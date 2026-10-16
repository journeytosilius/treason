//! The contract between the control layer and whatever plays the project.
//!
//! `treason-control` drives a [`Playback`] and `treason-engine` provides one.
//! A test double works just as well, so the control layer can be tested
//! without threads or MIDI ports.

use std::sync::Arc;

use thiserror::Error;

use crate::model::Project;
use crate::time::Ticks;

/// Transport commands, with MIDI transport semantics. The MIDI transport
/// messages go to the ports that get MIDI clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportCommand {
    /// Play from the beginning, even while playing. Sends MIDI Start,
    /// after Stop while playing.
    Play,
    /// Stop and keep the position. Ends sounding notes, then sends MIDI
    /// Stop.
    Stop,
    /// Play from the current position. Sends Song Position Pointer and MIDI
    /// Continue, or Start at the beginning. Does nothing while playing.
    Continue,
    /// Move to a position, and while playing, play on from there. Sends Song
    /// Position Pointer, between Stop and Continue while playing.
    Locate(Ticks),
    /// Ends every sounding note on every port at once.
    Panic,
}

/// Whether the transport is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportState {
    /// Not playing.
    #[default]
    Stopped,
    /// Playing.
    Playing,
}

/// Where playback is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaybackStatus {
    /// Running or not.
    pub state: TransportState,
    /// The current position.
    pub position: Ticks,
}

/// Why a playback request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PlaybackError {
    /// Too many commands are waiting: the engine is not keeping up.
    #[error("the engine's command queue is full")]
    QueueFull,
    /// The engine has shut down.
    #[error("the engine is not running")]
    Disconnected,
}

/// Something that plays a project.
pub trait Playback {
    /// Makes `project` the version that plays. Tempo changes take effect at
    /// once and clip changes at the next bar.
    fn publish(&mut self, project: Arc<Project>);

    /// Sends a transport command.
    fn command(&mut self, command: TransportCommand) -> Result<(), PlaybackError>;

    /// Where playback is now.
    fn status(&self) -> PlaybackStatus;
}

/// Lets a session hold its playback as a trait object, such as
/// `Box<dyn Playback + Send>`, when the concrete type shouldn't leak.
impl<P: Playback + ?Sized> Playback for Box<P> {
    fn publish(&mut self, project: Arc<Project>) {
        (**self).publish(project);
    }

    fn command(&mut self, command: TransportCommand) -> Result<(), PlaybackError> {
        (**self).command(command)
    }

    fn status(&self) -> PlaybackStatus {
        (**self).status()
    }
}
