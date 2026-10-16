//! The session: the current project, its history and the link to playback.

use std::collections::VecDeque;
use std::sync::Arc;

use thiserror::Error;
use treason_core::model::{Project, Track, TrackId};
use treason_core::playback::{Playback, PlaybackError, PlaybackStatus, TransportCommand};
use treason_devices::DeviceRegistry;

use crate::edit::Edit;

/// How many past snapshots undo keeps.
const UNDO_LIMIT: usize = 256;

/// Why an edit or command was rejected. A rejected edit changes nothing.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ControlError {
    /// No track has this id.
    #[error("there is no track {0}")]
    UnknownTrack(TrackId),
    /// A track already has this id.
    #[error("there is already a track {0}")]
    DuplicateTrack(TrackId),
    /// No device profile has this id.
    #[error("there is no device profile `{0}`")]
    UnknownDevice(String),
    /// Playback refused the request.
    #[error(transparent)]
    Playback(#[from] PlaybackError),
}

/// The project being worked on and everything that changes it.
///
/// Holds the current snapshot, undo and redo history, the device profiles and
/// the [`Playback`] that plays it. Single-threaded by design: an adapter that
/// serves several clients puts it behind a mutex. None of this runs on the
/// engine thread, so holding that lock never delays playback.
pub struct Session<P> {
    project: Arc<Project>,
    undo: VecDeque<Arc<Project>>,
    redo: Vec<Arc<Project>>,
    devices: DeviceRegistry,
    playback: P,
}

impl<P: Playback> Session<P> {
    /// Starts a session and publishes `project` to `playback`.
    pub fn new(project: Arc<Project>, devices: DeviceRegistry, mut playback: P) -> Self {
        playback.publish(Arc::clone(&project));
        Self {
            project,
            undo: VecDeque::new(),
            redo: Vec::new(),
            devices,
            playback,
        }
    }

    /// The current snapshot.
    pub fn project(&self) -> &Arc<Project> {
        &self.project
    }

    /// The device profiles.
    pub fn devices(&self) -> &DeviceRegistry {
        &self.devices
    }

    /// What plays the project.
    pub fn playback(&self) -> &P {
        &self.playback
    }

    /// Validates `edit`, applies it and publishes the result.
    pub fn apply(&mut self, edit: Edit) -> Result<(), ControlError> {
        let next = edited(&self.project, edit, &self.devices)?;
        let previous = std::mem::replace(&mut self.project, Arc::new(next));
        if self.undo.len() == UNDO_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(previous);
        self.redo.clear();
        self.publish();
        Ok(())
    }

    /// Steps back one edit. Returns `false` if there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop_back() else {
            return false;
        };
        self.redo
            .push(std::mem::replace(&mut self.project, previous));
        self.publish();
        true
    }

    /// Re-applies the last undone edit. Returns `false` if there is nothing to
    /// redo.
    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo
            .push_back(std::mem::replace(&mut self.project, next));
        self.publish();
        true
    }

    /// Sends a transport command.
    pub fn command(&mut self, command: TransportCommand) -> Result<(), ControlError> {
        self.playback.command(command).map_err(ControlError::from)
    }

    /// Where playback is.
    pub fn status(&self) -> PlaybackStatus {
        self.playback.status()
    }

    fn publish(&mut self) {
        self.playback.publish(Arc::clone(&self.project));
    }
}

/// `project` with `edit` applied. Unchanged tracks stay shared.
fn edited(
    project: &Project,
    edit: Edit,
    devices: &DeviceRegistry,
) -> Result<Project, ControlError> {
    let mut next = project.clone();
    match edit {
        Edit::SetTempo(tempo) => next.tempo = tempo,
        Edit::SetTimeSignature(time_signature) => next.time_signature = time_signature,
        Edit::AddTrack(track) => {
            if next.track(track.id).is_some() {
                return Err(ControlError::DuplicateTrack(track.id));
            }
            check_device(&track, devices)?;
            next.tracks.push(Arc::new(track));
        }
        Edit::RemoveTrack(id) => {
            let index = next
                .tracks
                .iter()
                .position(|track| track.id == id)
                .ok_or(ControlError::UnknownTrack(id))?;
            next.tracks.remove(index);
        }
        Edit::ReplaceTrack(track) => {
            let slot = next
                .tracks
                .iter_mut()
                .find(|old| old.id == track.id)
                .ok_or(ControlError::UnknownTrack(track.id))?;
            check_device(&track, devices)?;
            *slot = Arc::new(track);
        }
        Edit::SetClip { track, clip } => track_mut(&mut next, track)?.clip = clip,
        Edit::SetMuted { track, muted } => track_mut(&mut next, track)?.muted = muted,
    }
    Ok(next)
}

fn check_device(track: &Track, devices: &DeviceRegistry) -> Result<(), ControlError> {
    match &track.device {
        Some(id) if devices.get(id).is_none() => Err(ControlError::UnknownDevice(id.clone())),
        _ => Ok(()),
    }
}

/// The track with `id`, copied if it is shared, so it can be changed.
fn track_mut(project: &mut Project, id: TrackId) -> Result<&mut Track, ControlError> {
    project
        .tracks
        .iter_mut()
        .find(|track| track.id == id)
        .map(Arc::make_mut)
        .ok_or(ControlError::UnknownTrack(id))
}

#[cfg(test)]
mod tests {
    use treason_core::midi::Channel;
    use treason_core::model::{Clip, OutputTarget, PortId};
    use treason_core::time::Bpm;

    use super::*;

    #[derive(Default)]
    struct FakePlayback {
        published: Vec<Arc<Project>>,
    }

    impl Playback for FakePlayback {
        fn publish(&mut self, project: Arc<Project>) {
            self.published.push(project);
        }

        fn command(&mut self, _command: TransportCommand) -> Result<(), PlaybackError> {
            Ok(())
        }

        fn status(&self) -> PlaybackStatus {
            PlaybackStatus::default()
        }
    }

    fn track(id: u32) -> Track {
        Track {
            id: TrackId(id),
            name: format!("track {id}"),
            device: None,
            target: OutputTarget {
                port: PortId(0),
                channel: Channel::DRUMS,
            },
            muted: false,
            clip: Clip::default(),
        }
    }

    fn session(tracks: Vec<Track>) -> Session<FakePlayback> {
        let project = Project {
            tracks: tracks.into_iter().map(Arc::new).collect(),
            ..Project::default()
        };
        Session::new(
            Arc::new(project),
            DeviceRegistry::new(),
            FakePlayback::default(),
        )
    }

    #[test]
    fn edits_publish_and_undo_redo_restore() {
        let mut session = session(vec![]);
        let fast = Bpm::new(140.0).unwrap();

        session.apply(Edit::SetTempo(fast)).unwrap();
        assert_eq!(session.project().tempo, fast);

        assert!(session.undo());
        assert_eq!(session.project().tempo, Bpm::default());
        assert!(session.redo());
        assert_eq!(session.project().tempo, fast);

        // Initial publish, the edit, the undo and the redo.
        assert_eq!(session.playback().published.len(), 4);
    }

    #[test]
    fn rejected_edits_change_nothing() {
        let mut session = session(vec![track(1)]);
        let before = Arc::clone(session.project());

        let result = session.apply(Edit::SetMuted {
            track: TrackId(9),
            muted: true,
        });
        assert_eq!(result, Err(ControlError::UnknownTrack(TrackId(9))));

        let mut with_device = track(2);
        with_device.device = Some("nope".into());
        let result = session.apply(Edit::AddTrack(with_device));
        assert_eq!(result, Err(ControlError::UnknownDevice("nope".into())));

        assert!(Arc::ptr_eq(session.project(), &before));
        assert!(!session.undo());
        assert_eq!(session.playback().published.len(), 1);
    }

    #[test]
    fn replacing_a_track_keeps_its_place() {
        let mut session = session(vec![track(1), track(2)]);
        let mut moved = track(1);
        moved.name = "drums".into();
        moved.target.port = PortId(1);
        session.apply(Edit::ReplaceTrack(moved.clone())).unwrap();
        assert_eq!(*session.project().tracks[0], moved);
        assert_eq!(session.project().tracks[1].id, TrackId(2));

        let result = session.apply(Edit::ReplaceTrack(track(9)));
        assert_eq!(result, Err(ControlError::UnknownTrack(TrackId(9))));
        let mut unknown_device = track(2);
        unknown_device.device = Some("nope".into());
        let result = session.apply(Edit::ReplaceTrack(unknown_device));
        assert_eq!(result, Err(ControlError::UnknownDevice("nope".into())));

        assert!(session.undo());
        assert_eq!(session.project().tracks[0].name, "track 1");
    }

    #[test]
    fn edits_share_unchanged_tracks() {
        let mut session = session(vec![track(1), track(2)]);
        let before = Arc::clone(session.project());

        session
            .apply(Edit::SetMuted {
                track: TrackId(1),
                muted: true,
            })
            .unwrap();

        let after = session.project();
        assert!(!Arc::ptr_eq(&before.tracks[0], &after.tracks[0]));
        assert!(Arc::ptr_eq(&before.tracks[1], &after.tracks[1]));
    }
}
