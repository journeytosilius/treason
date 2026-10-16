//! The connection between the control side and the engine thread.
//!
//! - Project snapshots go through an [`ArcSwap`]. Publishing is one atomic
//!   pointer swap, and the engine reads without waiting.
//! - Transport commands go through a bounded single-producer,
//!   single-consumer ring buffer ([`rtrb`]), lock-free at both ends.
//! - Status (playing or not, position) comes back through atomics.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arc_swap::{ArcSwap, Guard};
use rtrb::{Consumer, Producer, RingBuffer};
use treason_core::model::Project;
use treason_core::playback::{
    Playback, PlaybackError, PlaybackStatus, TransportCommand, TransportState,
};
use treason_core::time::Ticks;

/// How many transport commands can wait for the engine.
const COMMAND_CAPACITY: usize = 64;

struct Shared {
    project: ArcSwap<Project>,
    playing: AtomicBool,
    position: AtomicU64,
}

/// Creates a connected pair: the [`EngineHandle`] for the control side and
/// the [`EngineLink`] for the engine thread.
pub fn link(project: Arc<Project>) -> (EngineHandle, EngineLink) {
    let shared = Arc::new(Shared {
        project: ArcSwap::new(project),
        playing: AtomicBool::new(false),
        position: AtomicU64::new(0),
    });
    let (commands, inbox) = RingBuffer::new(COMMAND_CAPACITY);
    let handle = EngineHandle {
        shared: Arc::clone(&shared),
        commands,
    };
    let link = EngineLink {
        shared,
        commands: inbox,
    };
    (handle, link)
}

/// The control side's end. Implements [`Playback`].
pub struct EngineHandle {
    shared: Arc<Shared>,
    commands: Producer<TransportCommand>,
}

impl Playback for EngineHandle {
    fn publish(&mut self, project: Arc<Project>) {
        self.shared.project.store(project);
    }

    fn command(&mut self, command: TransportCommand) -> Result<(), PlaybackError> {
        if self.commands.is_abandoned() {
            return Err(PlaybackError::Disconnected);
        }
        self.commands
            .push(command)
            .map_err(|_| PlaybackError::QueueFull)
    }

    fn status(&self) -> PlaybackStatus {
        // Relaxed is enough: status is informational and each field stands alone.
        let state = if self.shared.playing.load(Ordering::Relaxed) {
            TransportState::Playing
        } else {
            TransportState::Stopped
        };
        PlaybackStatus {
            state,
            position: Ticks(self.shared.position.load(Ordering::Relaxed)),
        }
    }
}

/// The engine thread's end.
pub struct EngineLink {
    shared: Arc<Shared>,
    commands: Consumer<TransportCommand>,
}

impl EngineLink {
    /// The next waiting command, if any. Wait-free.
    pub fn next_command(&mut self) -> Option<TransportCommand> {
        self.commands.pop().ok()
    }

    /// The latest published project. Wait-free.
    pub fn project(&self) -> Guard<Arc<Project>> {
        self.shared.project.load()
    }

    /// Whether the [`EngineHandle`] has been dropped. The engine then ends the
    /// sounding notes and stops.
    pub fn is_abandoned(&self) -> bool {
        self.commands.is_abandoned()
    }

    /// Reports where playback is.
    pub fn set_status(&self, status: PlaybackStatus) {
        let playing = status.state == TransportState::Playing;
        self.shared.playing.store(playing, Ordering::Relaxed);
        self.shared
            .position
            .store(status.position.0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use treason_core::time::Bpm;

    use super::*;

    #[test]
    fn snapshots_and_commands_reach_the_engine() {
        let (mut handle, mut engine) = link(Arc::new(Project::default()));

        let faster = Project {
            tempo: Bpm::new(140.0).unwrap(),
            ..Project::default()
        };
        handle.publish(Arc::new(faster));
        assert_eq!(engine.project().tempo, Bpm::new(140.0).unwrap());

        handle.command(TransportCommand::Play).unwrap();
        assert_eq!(engine.next_command(), Some(TransportCommand::Play));
        assert_eq!(engine.next_command(), None);

        let status = PlaybackStatus {
            state: TransportState::Playing,
            position: Ticks(480),
        };
        engine.set_status(status);
        assert_eq!(handle.status(), status);
    }

    #[test]
    fn reports_a_stopped_engine() {
        let (mut handle, engine) = link(Arc::new(Project::default()));
        drop(engine);
        assert_eq!(
            handle.command(TransportCommand::Play),
            Err(PlaybackError::Disconnected)
        );
    }

    #[test]
    fn the_engine_sees_a_dropped_handle() {
        let (handle, engine) = link(Arc::new(Project::default()));
        assert!(!engine.is_abandoned());
        drop(handle);
        assert!(engine.is_abandoned());
    }
}
