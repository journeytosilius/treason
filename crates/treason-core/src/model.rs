//! The project model: what the engine plays and what the agent edits.
//!
//! A [`Project`] is an immutable snapshot. An edit makes a new snapshot that
//! shares every unchanged [`Track`] with the old one through [`Arc`]. That
//! makes publishing a change to the engine a pointer swap, and undo history
//! costs only the tracks that changed.

use std::collections::HashMap;
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

    /// Two tracks that set different programs on the same port and channel,
    /// in project order, if any. A channel plays one program at a time, so
    /// tracks that share one must agree on it, or leave it to the others.
    pub fn program_clash(&self) -> Option<(TrackId, TrackId)> {
        let mut programs = HashMap::new();
        for track in &self.tracks {
            let Some(program) = track.program else {
                continue;
            };
            let (first, set) = *programs.entry(track.target).or_insert((track.id, program));
            if set != program {
                return Some((first, track.id));
            }
        }
        None
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
    /// The sound its channel plays: sent when playback starts, after a
    /// jump, and at the bar line where it changes, muted or not. `None`
    /// leaves the device's sound as it is.
    pub program: Option<Program>,
    /// What the track loops.
    pub clip: Clip,
}

/// A sound to select: a program change, after a bank select if the device
/// has banks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Program {
    /// The program change's value, 0-127, as sent.
    pub number: U7,
    /// Bank select's most significant byte, controller 0, sent first if
    /// set.
    pub bank_msb: Option<U7>,
    /// Bank select's least significant byte, controller 32, sent next if
    /// set.
    pub bank_lsb: Option<U7>,
}

impl Program {
    /// A program with no bank.
    pub const fn new(number: U7) -> Self {
        Self {
            number,
            bank_msb: None,
            bank_lsb: None,
        }
    }
}

/// A loop of events.
///
/// Every track loops its own clip, so clips of different lengths give
/// polymeter. Events are kept sorted by start, which lets the engine find the
/// events in a time window with a binary search. A note may run past the
/// loop point into the next pass, but no note is longer than the loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawClip")]
pub struct Clip {
    length: Ticks,
    events: Vec<ClipEvent>,
    /// Kept so the engine knows how far back a note ending now can start.
    #[serde(skip)]
    longest_note: Ticks,
}

impl Clip {
    /// A clip of `length` holding `events`, given in any order.
    pub fn new(length: Ticks, mut events: Vec<ClipEvent>) -> Result<Self, ClipError> {
        if length == Ticks::ZERO {
            return Err(ClipError::ZeroLength);
        }
        let mut longest_note = Ticks::ZERO;
        for event in &events {
            if event.start >= length {
                return Err(ClipError::EventOutsideClip {
                    start: event.start,
                    length,
                });
            }
            if let EventKind::Note {
                length: note_length,
                ..
            } = event.kind
            {
                if note_length == Ticks::ZERO {
                    return Err(ClipError::ZeroLengthNote { start: event.start });
                }
                if note_length > length {
                    return Err(ClipError::NoteLongerThanClip {
                        start: event.start,
                        note_length,
                        length,
                    });
                }
                longest_note = longest_note.max(note_length);
            }
        }
        // A stable sort keeps the given order of events that start together.
        events.sort_by_key(|event| event.start);
        Ok(Self {
            length,
            events,
            longest_note,
        })
    }

    /// The loop length.
    pub const fn length(&self) -> Ticks {
        self.length
    }

    /// All events, sorted by start.
    pub fn events(&self) -> &[ClipEvent] {
        &self.events
    }

    /// The length of the longest note, or zero if there are no notes.
    pub const fn longest_note(&self) -> Ticks {
        self.longest_note
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
            longest_note: Ticks::ZERO,
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
    /// A note outlasts the loop, so it would overlap its own next pass.
    #[error("the note at {start} lasts {note_length}, longer than the clip's {length}")]
    NoteLongerThanClip {
        start: Ticks,
        note_length: Ticks,
        length: Ticks,
    },
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
    fn notes_can_cross_the_loop_point_but_not_outlast_the_loop() {
        let note = |start, length| ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: U7::MAX,
                velocity: U7::MAX,
                length: Ticks(length),
            },
        };
        let clip = Clip::new(Ticks(960), vec![note(720, 480), note(0, 960)]).unwrap();
        assert_eq!(clip.longest_note(), Ticks(960));
        assert_eq!(
            Clip::new(Ticks(960), vec![note(0, 240), note(480, 961)]),
            Err(ClipError::NoteLongerThanClip {
                start: Ticks(480),
                note_length: Ticks(961),
                length: Ticks(960),
            })
        );
        assert_eq!(Clip::default().longest_note(), Ticks::ZERO);

        let json = serde_json::to_string(&clip).unwrap();
        assert!(!json.contains("longest"), "{json}");
        assert_eq!(serde_json::from_str::<Clip>(&json).unwrap(), clip);
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
                program: None,
                clip: Clip::default(),
            })
        };
        let mut project = Project::default();
        assert_eq!(project.next_track_id(), TrackId(1));
        project.tracks = vec![track(5), track(2)];
        assert_eq!(project.next_track_id(), TrackId(6));
    }

    #[test]
    fn tracks_on_one_channel_agree_on_its_program() {
        let program = |number| {
            Some(Program {
                number: U7::new(number).unwrap(),
                bank_msb: None,
                bank_lsb: None,
            })
        };
        let banked = Some(Program {
            bank_msb: U7::new(1),
            ..program(5).unwrap()
        });
        let track = |id, port, channel, program| {
            Arc::new(Track {
                id: TrackId(id),
                name: format!("track {id}"),
                device: None,
                target: OutputTarget {
                    port: PortId(port),
                    channel: Channel::from_number(channel).unwrap(),
                },
                muted: false,
                program,
                clip: Clip::default(),
            })
        };
        for (tracks, clash) in [
            (vec![], None),
            (
                vec![track(1, 0, 1, program(5)), track(2, 0, 1, program(5))],
                None,
            ),
            (vec![track(1, 0, 1, program(5)), track(2, 0, 1, None)], None),
            (
                vec![track(1, 0, 1, program(5)), track(2, 0, 2, program(6))],
                None,
            ),
            (
                vec![track(1, 0, 1, program(5)), track(2, 1, 1, program(6))],
                None,
            ),
            (
                vec![track(1, 0, 1, program(5)), track(2, 0, 1, program(6))],
                Some((TrackId(1), TrackId(2))),
            ),
            // The bank is part of the sound.
            (
                vec![track(1, 0, 1, program(5)), track(2, 0, 1, banked)],
                Some((TrackId(1), TrackId(2))),
            ),
            // Named by the first track to set the channel's program.
            (
                vec![
                    track(3, 0, 1, None),
                    track(1, 0, 1, program(5)),
                    track(2, 0, 1, program(5)),
                    track(4, 0, 1, program(7)),
                ],
                Some((TrackId(1), TrackId(4))),
            ),
        ] {
            let project = Project {
                tracks,
                ..Project::default()
            };
            assert_eq!(project.program_clash(), clash);
        }
    }
}
