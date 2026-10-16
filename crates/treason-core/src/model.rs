//! The project model: what the engine plays and what the agent edits.
//!
//! A [`Project`] is an immutable snapshot. An edit makes a new snapshot that
//! shares every unchanged [`Track`] with the old one through [`Arc`]. That
//! makes publishing a change to the engine a pointer swap, and undo history
//! costs only the tracks that changed.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::midi::{Channel, U7};
use crate::time::{Bpm, Ticks, TimeSignature};

/// An output port: an index into the engine's table of open outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PortId(pub u16);

impl fmt::Display for PortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Identifies a track within a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrackId(pub u32);

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Where a track's MIDI goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputTarget {
    /// The output port.
    pub port: PortId,
    /// The channel on that port.
    pub channel: Channel,
}

/// Everything that plays.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// The current tempo.
    pub tempo: Bpm,
    /// The meter. It sets the bar length, and clip changes take effect on bar lines.
    pub time_signature: TimeSignature,
    /// The tracks, shared between snapshots.
    pub tracks: Vec<Arc<Track>>,
}

impl Project {
    /// The track with this id.
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks
            .iter()
            .map(Arc::as_ref)
            .find(|track| track.id == id)
    }

    /// An id no track has: one more than the highest, starting at 1.
    pub fn next_track_id(&self) -> TrackId {
        let highest = self.tracks.iter().map(|track| track.id.0).max();
        TrackId(highest.map_or(1, |id| id.saturating_add(1)))
    }
}

/// One stream of MIDI to one channel of one device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Unique within the project.
    pub id: TrackId,
    /// A name for people and agents.
    pub name: String,
    /// The id of the device profile (see `treason-devices`) that names this
    /// track's notes and controls. The engine never reads it.
    pub device: Option<String>,
    /// Where the MIDI goes.
    pub target: OutputTarget,
    /// Muted tracks play nothing.
    pub muted: bool,
    /// What the track loops.
    pub clip: Clip,
}

/// A loop of events.
///
/// Every track loops its own clip, so clips of different lengths give
/// polymeter. Events are kept sorted by start, which lets the engine find the
/// events in a time window with a binary search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawClip")]
pub struct Clip {
    length: Ticks,
    events: Vec<ClipEvent>,
}

impl Clip {
    /// A clip of `length` holding `events`, given in any order.
    pub fn new(length: Ticks, mut events: Vec<ClipEvent>) -> Result<Self, ClipError> {
        if length == Ticks::ZERO {
            return Err(ClipError::ZeroLength);
        }
        for event in &events {
            if event.start >= length {
                return Err(ClipError::EventOutsideClip {
                    start: event.start,
                    length,
                });
            }
            if let EventKind::Note {
                length: Ticks::ZERO,
                ..
            } = event.kind
            {
                return Err(ClipError::ZeroLengthNote { start: event.start });
            }
        }
        // A stable sort keeps the given order of events that start together.
        events.sort_by_key(|event| event.start);
        Ok(Self { length, events })
    }

    /// The loop length.
    pub const fn length(&self) -> Ticks {
        self.length
    }

    /// All events, sorted by start.
    pub fn events(&self) -> &[ClipEvent] {
        &self.events
    }

    /// The events that start within `range`, for one pass through the loop.
    pub fn events_in(&self, range: Range<Ticks>) -> &[ClipEvent] {
        let first = self
            .events
            .partition_point(|event| event.start < range.start);
        let end = self.events.partition_point(|event| event.start < range.end);
        &self.events[first..end]
    }
}

impl Default for Clip {
    /// An empty 4/4 bar.
    fn default() -> Self {
        Self {
            length: TimeSignature::COMMON.bar_length(),
            events: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct RawClip {
    length: Ticks,
    events: Vec<ClipEvent>,
}

impl TryFrom<RawClip> for Clip {
    type Error = ClipError;

    fn try_from(raw: RawClip) -> Result<Self, Self::Error> {
        Self::new(raw.length, raw.events)
    }
}

/// Why [`Clip::new`] rejected a clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClipError {
    /// The clip has no length.
    #[error("a clip must be longer than zero ticks")]
    ZeroLength,
    /// An event starts at or after the end of the clip.
    #[error("the event at {start} starts outside the clip, which is {length} long")]
    EventOutsideClip { start: Ticks, length: Ticks },
    /// A note has no length.
    #[error("the note at {start} has zero length")]
    ZeroLengthNote { start: Ticks },
}

/// Something that happens at a point in a clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipEvent {
    /// When, from the start of the clip.
    pub start: Ticks,
    /// What.
    pub kind: EventKind,
}

/// What a clip event does. The port and channel come from the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A note, held for `length`.
    Note {
        note: U7,
        velocity: U7,
        length: Ticks,
    },
    /// A control change.
    Control { controller: U7, value: U7 },
    /// A program change.
    Program { program: U7 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kick(sixteenth: u64) -> ClipEvent {
        ClipEvent {
            start: Ticks::sixteenths(sixteenth),
            kind: EventKind::Note {
                note: U7::new(36).unwrap(),
                velocity: U7::MAX,
                length: Ticks::sixteenths(1),
            },
        }
    }

    #[test]
    fn clips_sort_events_and_find_windows() {
        let clip = Clip::new(
            Ticks::quarters(4),
            vec![kick(8), kick(0), kick(4), kick(12)],
        )
        .unwrap();
        let starts: Vec<u64> = clip.events().iter().map(|event| event.start.0).collect();
        assert_eq!(starts, [0, 960, 1920, 2880]);
        assert_eq!(clip.events_in(Ticks(900)..Ticks(2000)).len(), 2);
        assert!(clip.events_in(Ticks(1)..Ticks(960)).is_empty());
    }

    #[test]
    fn clips_reject_events_past_the_end() {
        let length = Ticks::quarters(4);
        assert_eq!(
            Clip::new(length, vec![kick(16)]),
            Err(ClipError::EventOutsideClip {
                start: length,
                length
            })
        );
        assert_eq!(Clip::new(Ticks::ZERO, vec![]), Err(ClipError::ZeroLength));
    }

    #[test]
    fn new_track_ids_follow_the_highest() {
        let track = |id| {
            Arc::new(Track {
                id: TrackId(id),
                name: format!("track {id}"),
                device: None,
                target: OutputTarget {
                    port: PortId(0),
                    channel: Channel::DRUMS,
                },
                muted: false,
                clip: Clip::default(),
            })
        };
        let mut project = Project::default();
        assert_eq!(project.next_track_id(), TrackId(1));
        project.tracks = vec![track(5), track(2)];
        assert_eq!(project.next_track_id(), TrackId(6));
    }
}
