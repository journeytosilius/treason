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

use crate::midi::{Channel, U7, U14};
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
        self.clash(|track| track.program)
    }

    /// Two tracks that set different bend ranges on the same port and
    /// channel, in project order, if any. Like a program, a channel has one.
    pub fn bend_range_clash(&self) -> Option<(TrackId, TrackId)> {
        self.clash(|track| track.bend_range)
    }

    /// Two tracks on one port and channel that set it up differently, by
    /// `setting`: the first to set it there, and the first to disagree.
    fn clash<T: Copy + PartialEq>(
        &self,
        setting: impl Fn(&Track) -> Option<T>,
    ) -> Option<(TrackId, TrackId)> {
        let mut set = HashMap::new();
        for track in &self.tracks {
            let Some(value) = setting(track) else {
                continue;
            };
            let (first, first_value) = *set.entry(track.target).or_insert((track.id, value));
            if first_value != value {
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
    /// How far its channel's pitch bend reaches, in semitones either way:
    /// sent as RPN 0, pitch bend sensitivity, along with the program, and
    /// after it, since some devices reset it on a program change. `None`
    /// leaves the device's range as it is.
    pub bend_range: Option<U7>,
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
/// loop point into the next pass, but no note is longer than the loop. A
/// key's pressure comes only while a note of that key sounds.
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
        check_key_pressure(length, &events)?;
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

/// Checks that each key pressure in `events` comes while a note of its key
/// sounds, in a loop `length` long, counting the notes that run past its
/// end into the start of the next pass.
fn check_key_pressure(length: Ticks, events: &[ClipEvent]) -> Result<(), ClipError> {
    let pressed = |event: &ClipEvent| matches!(event.kind, EventKind::NotePressure { .. });
    if !events.iter().any(pressed) {
        return Ok(());
    }
    // The ticks each key's notes sound for, as spans of the loop by key and
    // start, merged so that only the last to start at or before a tick can
    // cover it.
    let mut spans = Vec::new();
    for event in events {
        if let EventKind::Note {
            note, length: held, ..
        } = event.kind
        {
            let (start, end) = (event.start.0, event.start.0 + held.0);
            spans.push((note, start, end.min(length.0)));
            if end > length.0 {
                spans.push((note, 0, end - length.0));
            }
        }
    }
    spans.sort_unstable();
    let mut merged: Vec<(U7, u64, u64)> = Vec::with_capacity(spans.len());
    for (note, start, end) in spans {
        match merged.last_mut() {
            Some((key, _, last)) if *key == note && start <= *last => *last = end.max(*last),
            _ => merged.push((note, start, end)),
        }
    }
    for event in events.iter().filter(|event| pressed(event)) {
        let EventKind::NotePressure { note, .. } = event.kind else {
            continue;
        };
        let tick = event.start.0;
        let after = merged.partition_point(|&(key, start, _)| (key, start) <= (note, tick));
        let sounding = after
            .checked_sub(1)
            .is_some_and(|index| merged[index].0 == note && tick < merged[index].2);
        if !sounding {
            return Err(ClipError::PressureWithoutNote {
                start: event.start,
                note,
            });
        }
    }
    Ok(())
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
    /// Pressure on a key while no note of it sounds.
    #[error("the pressure on note {note} at {start} comes while no note of that key sounds")]
    PressureWithoutNote { start: Ticks, note: U7 },
}

/// Something that happens at a point in a clip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipEvent {
    /// When, from the start of the clip.
    pub start: Ticks,
    /// What.
    pub kind: EventKind,
}

/// The velocity of a note-off that doesn't say how fast its key comes up:
/// MIDI's default, for gear that can't sense it.
pub const DEFAULT_RELEASE: U7 = U7::saturating(64);

/// What a clip event does. The port and channel come from the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A note, held for `length`. Its note-off goes out with `release`, how
    /// fast its key comes up.
    Note {
        note: U7,
        velocity: U7,
        length: Ticks,
        release: U7,
    },
    /// A control change.
    Control { controller: U7, value: U7 },
    /// A program change.
    Program { program: U7 },
    /// Pitch bend, from 0 to 16383, with [`U14::CENTER`] at rest.
    PitchBend { value: U14 },
    /// Channel pressure, or aftertouch: how hard every key on the channel
    /// is pressed, 0 at rest.
    Pressure { value: U7 },
    /// Key pressure, or polyphonic aftertouch: how hard the key of one
    /// sounding note is pressed, 0 at rest.
    NotePressure { note: U7, value: U7 },
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
                release: DEFAULT_RELEASE,
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
                release: DEFAULT_RELEASE,
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
    fn key_pressure_comes_only_while_a_note_of_its_key_sounds() {
        let key = |number| U7::new(number).unwrap();
        let note = |number, start, length| ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: key(number),
                velocity: U7::MAX,
                length: Ticks(length),
                release: DEFAULT_RELEASE,
            },
        };
        let notes = [
            note(60, 240, 240),
            // Runs past the end of the loop, through its first 240 ticks.
            note(60, 720, 480),
            // Holds through the next, which ends first.
            note(67, 0, 600),
            note(67, 100, 100),
        ];
        for (number, start, sounding) in [
            (60, 240, true),
            (60, 479, true),
            (60, 480, false),
            (60, 600, false),
            (60, 720, true),
            (60, 0, true),
            (60, 239, true),
            (67, 300, true),
            (67, 600, false),
            (62, 300, false),
        ] {
            let mut events = notes.to_vec();
            events.push(ClipEvent {
                start: Ticks(start),
                kind: EventKind::NotePressure {
                    note: key(number),
                    value: U7::MAX,
                },
            });
            let expected = if sounding {
                Ok(())
            } else {
                Err(ClipError::PressureWithoutNote {
                    start: Ticks(start),
                    note: key(number),
                })
            };
            let clip = Clip::new(Ticks(960), events).map(|_| ());
            assert_eq!(clip, expected, "pressure on {number} at {start}");
        }
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
                bend_range: None,
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
                bend_range: None,
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

    #[test]
    fn tracks_on_one_channel_agree_on_its_bend_range() {
        let track = |id, channel, bend_range: Option<u8>| {
            Arc::new(Track {
                id: TrackId(id),
                name: format!("track {id}"),
                device: None,
                target: OutputTarget {
                    port: PortId(0),
                    channel: Channel::from_number(channel).unwrap(),
                },
                muted: false,
                program: None,
                bend_range: bend_range.and_then(U7::new),
                clip: Clip::default(),
            })
        };
        for (tracks, clash) in [
            (vec![track(1, 1, Some(2)), track(2, 1, Some(2))], None),
            (vec![track(1, 1, Some(2)), track(2, 1, None)], None),
            (vec![track(1, 1, Some(2)), track(2, 2, Some(12))], None),
            (
                vec![
                    track(3, 1, None),
                    track(1, 1, Some(12)),
                    track(2, 1, Some(2)),
                ],
                Some((TrackId(1), TrackId(2))),
            ),
            // Zero is a range: none at all.
            (
                vec![track(1, 1, Some(2)), track(2, 1, Some(0))],
                Some((TrackId(1), TrackId(2))),
            ),
        ] {
            let project = Project {
                tracks,
                ..Project::default()
            };
            assert_eq!(project.bend_range_clash(), clash);
            assert_eq!(project.program_clash(), None);
        }
    }
}
