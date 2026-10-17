//! Clips written as lists of notes, for melodies, basslines and chords.
//!
//! The agent names each note by pitch, voice or number and times it in
//! beats. [`clip_from_notes`] checks the notes and builds the clip; `dto`
//! reads clips back as notes.

use thiserror::Error;
use treason_core::midi::U7;
use treason_core::model::{Clip, ClipError, ClipEvent, EventKind};
use treason_core::time::{PPQN, Ticks};
use treason_devices::DeviceProfile;

use crate::dto::{NoteRef, NoteSpec};
use crate::notation::{self, NotationError};
use crate::pattern::HIT_VELOCITY;

/// The most notes one clip can hold.
pub const MAX_NOTES: usize = 1024;

/// The longest loop, in beats: 256 bars of 4/4.
pub const MAX_LOOP_BEATS: u32 = 1024;

/// The velocity of a note that doesn't give one, the same as a step's hit.
pub const DEFAULT_VELOCITY: U7 = HIT_VELOCITY;

/// Notes that can't become a clip. The messages tell the agent how to fix
/// the call.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NotesError {
    /// No notes.
    #[error(r#"give at least one note, such as {{"note": "C3", "start": 0, "length": 1}}"#)]
    NoNotes,
    /// More notes than a clip can hold.
    #[error("{0} notes is more than the {MAX_NOTES} a clip can hold")]
    TooMany(usize),
    /// A loop length out of range.
    #[error("loop_beats must be more than 0 and at most {MAX_LOOP_BEATS}, not {0}")]
    LoopBeats(f64),
    /// Notes that end past the longest loop, when the loop length is left
    /// to the notes.
    #[error("the notes end at beat {0}, past the longest loop of {MAX_LOOP_BEATS} beats")]
    PastLongestLoop(f64),
    /// One note is wrong.
    #[error("note {position} (`{note}`): {error}")]
    Note {
        /// Which note in the list, counting from 1.
        position: usize,
        /// The note as written.
        note: String,
        /// What's wrong with it.
        error: NoteError,
    },
    /// Two notes of the same pitch that sound at once. A key can't be held
    /// down twice.
    #[error(
        "notes {first} and {second} are both `{note}` and overlap: end the first before the second starts"
    )]
    Overlap {
        /// The earlier of the two in the list, counting from 1.
        first: usize,
        /// The later of the two.
        second: usize,
        /// The note as written.
        note: String,
    },
    /// The clip itself was rejected.
    #[error(transparent)]
    Clip(#[from] ClipError),
}

/// What's wrong with one note.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum NoteError {
    /// A name that is neither a voice of the track's device nor a pitch.
    #[error(
        "not a voice of {device} or a pitch such as C4 or F#2; its voices are {voices} (list_devices shows their aliases)"
    )]
    UnknownVoice {
        /// The device's id.
        device: String,
        /// Its voice names.
        voices: String,
    },
    /// A name that isn't a pitch, on a track without a device to name
    /// voices.
    #[error(
        "not a pitch such as C4 or F#2, and the track has no device to name voices: write a pitch, or give the track a device with edit_track (see list_devices)"
    )]
    NoDevice,
    /// A pitch past either end of MIDI's range.
    #[error(transparent)]
    Pitch(#[from] NotationError),
    /// A number that isn't a MIDI note.
    #[error("not a MIDI note: notes are 0-127")]
    NumberOutOfRange,
    /// A start that isn't a beat in range.
    #[error("start must be from 0 to {MAX_LOOP_BEATS} beats, not {0}")]
    Start(f64),
    /// A length that isn't positive, or too short to play.
    #[error("length must be more than 0 and at most {MAX_LOOP_BEATS} beats, not {0}")]
    Length(f64),
    /// A velocity that isn't 1-127. 0 would be silent.
    #[error("velocity must be 1-127, not {0}")]
    Velocity(u8),
    /// A note that starts at or after the end of the loop.
    #[error(
        "it starts at beat {start}, but the loop is {loop_beats} beats long: start it earlier, or make loop_beats longer"
    )]
    PastLoop {
        /// The note's start, in beats.
        start: f64,
        /// The loop's length, in beats.
        loop_beats: f64,
    },
    /// A note longer than the loop, which would overlap its own next pass.
    #[error(
        "it lasts {length} beats, longer than the {loop_beats}-beat loop: shorten it, or make loop_beats longer"
    )]
    LongerThanLoop {
        /// The note's length, in beats.
        length: f64,
        /// The loop's length, in beats.
        loop_beats: f64,
    },
}

/// One note, checked and in ticks.
struct Placed {
    note: U7,
    start: Ticks,
    length: Ticks,
    velocity: U7,
}

/// The clip `notes` play.
///
/// A note is a pitch name such as `C4`, a MIDI note number, or, when the
/// track has a `device`, one of its voice names or aliases. Starts and
/// lengths are in beats and round to the nearest tick. The loop lasts
/// `loop_beats`, or by default until the end of the last note, rounded up
/// to whole bars of `bar`. A note can run past the end of the loop into
/// the next pass, but not overlap another note of the same pitch there or
/// anywhere else.
pub fn clip_from_notes(
    notes: &[NoteSpec],
    loop_beats: Option<f64>,
    bar: Ticks,
    device: Option<&DeviceProfile>,
) -> Result<Clip, NotesError> {
    if notes.is_empty() {
        return Err(NotesError::NoNotes);
    }
    if notes.len() > MAX_NOTES {
        return Err(NotesError::TooMany(notes.len()));
    }
    let wrong = |index: usize, error| NotesError::Note {
        position: index + 1,
        note: written(&notes[index].note),
        error,
    };
    let placed = notes
        .iter()
        .enumerate()
        .map(|(index, spec)| place(spec, device).map_err(|error| wrong(index, error)))
        .collect::<Result<Vec<_>, _>>()?;

    let length = match loop_beats {
        Some(beats) => ticks(beats)
            .filter(|&length| length > Ticks::ZERO)
            .ok_or(NotesError::LoopBeats(beats))?,
        None => {
            let end = placed
                .iter()
                .map(|note| note.start.0 + note.length.0)
                .max()
                .unwrap_or(0);
            let length = end.div_ceil(bar.0).max(1) * bar.0;
            if length > u64::from(MAX_LOOP_BEATS) * PPQN {
                return Err(NotesError::PastLongestLoop(beats(Ticks(end))));
            }
            Ticks(length)
        }
    };
    for (index, note) in placed.iter().enumerate() {
        if note.start >= length {
            return Err(wrong(
                index,
                NoteError::PastLoop {
                    start: beats(note.start),
                    loop_beats: beats(length),
                },
            ));
        }
        if note.length > length {
            return Err(wrong(
                index,
                NoteError::LongerThanLoop {
                    length: beats(note.length),
                    loop_beats: beats(length),
                },
            ));
        }
    }
    check_overlaps(notes, &placed, length)?;

    let events = placed
        .iter()
        .map(|note| ClipEvent {
            start: note.start,
            kind: EventKind::Note {
                note: note.note,
                velocity: note.velocity,
                length: note.length,
            },
        })
        .collect();
    Ok(Clip::new(length, events)?)
}

/// Checks one note on its own.
fn place(spec: &NoteSpec, device: Option<&DeviceProfile>) -> Result<Placed, NoteError> {
    let note = resolve(&spec.note, device)?;
    let velocity = match spec.velocity {
        None => DEFAULT_VELOCITY,
        Some(velocity) => U7::new(velocity)
            .filter(|&velocity| velocity > U7::MIN)
            .ok_or(NoteError::Velocity(velocity))?,
    };
    let start = ticks(spec.start).ok_or(NoteError::Start(spec.start))?;
    let length = ticks(spec.length)
        .filter(|&length| length > Ticks::ZERO)
        .ok_or(NoteError::Length(spec.length))?;
    Ok(Placed {
        note,
        start,
        length,
        velocity,
    })
}

/// The MIDI note a note names. Numbers, written as numbers or digits, are
/// note numbers. A name is a voice of the device first, then a pitch.
fn resolve(note: &NoteRef, device: Option<&DeviceProfile>) -> Result<U7, NoteError> {
    let name = match note {
        NoteRef::Number(number) => return U7::new(*number).ok_or(NoteError::NumberOutOfRange),
        NoteRef::Name(name) => name,
    };
    if !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()) {
        return name
            .parse()
            .ok()
            .and_then(U7::new)
            .ok_or(NoteError::NumberOutOfRange);
    }
    if let Some(note) = device.and_then(|device| device.voice(name)) {
        return Ok(note);
    }
    match notation::parse_pitch(name) {
        Err(NotationError::BadPitch(_)) => Err(match device {
            Some(device) => NoteError::UnknownVoice {
                device: device.id.clone(),
                voices: device
                    .voices
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
            },
            None => NoteError::NoDevice,
        }),
        pitch => Ok(pitch?),
    }
}

/// Finds two notes of the same pitch that sound at once, including a note
/// that runs past the end of the loop into the start of the next pass.
fn check_overlaps(notes: &[NoteSpec], placed: &[Placed], length: Ticks) -> Result<(), NotesError> {
    let mut order: Vec<usize> = (0..placed.len()).collect();
    order.sort_by_key(|&index| (placed[index].note, placed[index].start, index));
    let end = |index: usize| placed[index].start.0 + placed[index].length.0;
    let overlap = |a: usize, b: usize| NotesError::Overlap {
        first: a.min(b) + 1,
        second: a.max(b) + 1,
        note: written(&notes[a].note),
    };
    for group in order.chunk_by(|&a, &b| placed[a].note == placed[b].note) {
        for pair in group.windows(2) {
            if end(pair[0]) > placed[pair[1]].start.0 {
                return Err(overlap(pair[0], pair[1]));
            }
        }
        // Only the latest note of a pitch can reach into the next pass: the
        // others end before it starts.
        let (first, last) = (group[0], group[group.len() - 1]);
        if first != last && end(last) > length.0 + placed[first].start.0 {
            return Err(overlap(first, last));
        }
    }
    Ok(())
}

/// A time in beats as whole ticks, rounded to the nearest. `None` unless
/// it's a number from 0 to the longest loop.
pub(crate) fn ticks(beats: f64) -> Option<Ticks> {
    (0.0..=f64::from(MAX_LOOP_BEATS))
        .contains(&beats)
        .then(|| Ticks((beats * PPQN as f64).round() as u64))
}

/// A time in ticks as beats.
pub(crate) fn beats(ticks: Ticks) -> f64 {
    ticks.0 as f64 / PPQN as f64
}

/// A note as the agent wrote it, for messages.
fn written(note: &NoteRef) -> String {
    match note {
        NoteRef::Number(number) => number.to_string(),
        NoteRef::Name(name) => name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use treason_core::midi::Channel;
    use treason_core::time::TimeSignature;
    use treason_devices::DeviceRegistry;

    use super::*;

    const BAR: Ticks = Ticks(4 * PPQN);

    fn drums() -> DeviceProfile {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../devices");
        DeviceRegistry::load_dir(&dir)
            .unwrap()
            .get("gm-drums")
            .unwrap()
            .clone()
    }

    fn note(name: &str, start: f64, length: f64) -> NoteSpec {
        NoteSpec {
            note: NoteRef::Name(name.into()),
            start,
            length,
            velocity: None,
        }
    }

    /// The clip's notes as (note, start, length, velocity) in ticks.
    fn played(clip: &Clip) -> Vec<(u8, u64, u64, u8)> {
        clip.events()
            .iter()
            .map(|event| match event.kind {
                EventKind::Note {
                    note,
                    velocity,
                    length,
                } => (note.get(), event.start.0, length.0, velocity.get()),
                other => panic!("not a note: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn notes_become_ticks_at_960_per_beat_to_the_nearest() {
        let notes = [
            note("C3", 0.0, 0.5),
            // A hair before beat 1 and a hair short of an eighth.
            note("A2", 0.9999, 0.4999),
            NoteSpec {
                velocity: Some(64),
                ..note("Eb3", 1.5, 0.25)
            },
            note("G3", 2.0 / 3.0, 1.0 / 3.0),
            NoteSpec {
                note: NoteRef::Number(36),
                ..note("", 3.0, 1.0)
            },
            note("38", 3.0, 1.0),
        ];
        let clip = clip_from_notes(&notes, None, BAR, None).unwrap();
        assert_eq!(clip.length(), BAR);
        assert_eq!(
            played(&clip),
            [
                (48, 0, 480, 100),
                (55, 640, 320, 100),
                (45, 960, 480, 100),
                (51, 1440, 240, 64),
                (36, 2880, 960, 100),
                (38, 2880, 960, 100),
            ]
        );
    }

    #[test]
    fn the_loop_defaults_to_whole_bars_after_the_last_note() {
        let three_four = TimeSignature::new(3, 4).unwrap().bar_length();
        for (notes, bar, beats) in [
            (vec![note("C3", 0.0, 1.0)], BAR, 4),
            (vec![note("C3", 3.0, 1.0)], BAR, 4),
            (vec![note("C3", 3.5, 1.0)], BAR, 8),
            (vec![note("C3", 0.0, 0.5), note("D3", 6.0, 0.5)], BAR, 8),
            (vec![note("C3", 3.0, 1.0)], three_four, 6),
        ] {
            let clip = clip_from_notes(&notes, None, bar, None).unwrap();
            assert_eq!(clip.length(), Ticks::quarters(beats), "{notes:?}");
        }
        let long = clip_from_notes(&[note("C3", 0.0, 1.0)], Some(6.0), BAR, None).unwrap();
        assert_eq!(long.length(), Ticks::quarters(6));
    }

    #[test]
    fn names_are_voices_of_the_device_before_pitches() {
        let drums = drums();
        let notes = [
            note("kick", 0.0, 0.25),
            note("sd", 1.0, 0.25),
            note("C4", 2.0, 0.25),
        ];
        let clip = clip_from_notes(&notes, None, BAR, Some(&drums)).unwrap();
        let pitches: Vec<u8> = played(&clip).iter().map(|note| note.0).collect();
        assert_eq!(pitches, [36, 38, 60]);

        // A voice named like a pitch wins over the pitch.
        let pitched = DeviceProfile {
            id: "pitched".into(),
            name: "Pitched".into(),
            manufacturer: None,
            default_channel: Channel::from_number(1).unwrap(),
            voices: BTreeMap::from([("A1".to_owned(), U7::new(40).unwrap())]),
            aliases: BTreeMap::new(),
            programs: BTreeMap::new(),
            params: BTreeMap::new(),
        };
        let notes = [note("A1", 0.0, 1.0), note("A2", 1.0, 1.0)];
        let clip = clip_from_notes(&notes, None, BAR, Some(&pitched)).unwrap();
        let pitches: Vec<u8> = played(&clip).iter().map(|note| note.0).collect();
        assert_eq!(pitches, [40, 45]);
    }

    #[test]
    fn notes_can_cross_the_loop_point_but_not_into_the_same_pitch() {
        // The held B2 ends one beat into the next pass, before the next B2.
        let crossing = [note("B2", 3.0, 2.0), note("B2", 1.0, 1.0)];
        assert!(clip_from_notes(&crossing, Some(4.0), BAR, None).is_ok());
        // Back to back is fine, and so is another pitch at the same time.
        let fine = [
            note("C3", 0.0, 1.0),
            note("C3", 1.0, 1.0),
            note("C3", 2.0, 2.0),
            note("E3", 0.5, 1.0),
        ];
        assert!(clip_from_notes(&fine, None, BAR, None).is_ok());

        for (notes, first, second) in [
            (vec![note("C3", 0.0, 2.0), note("C3", 1.0, 1.0)], 1, 2),
            // One tick of overlap is still an overlap.
            (
                vec![note("C3", 0.0, 961.0 / 960.0), note("C3", 1.0, 1.0)],
                1,
                2,
            ),
            (
                vec![
                    note("D3", 1.0, 1.0),
                    note("C3", 0.0, 1.0),
                    note("C3", 0.0, 0.5),
                ],
                2,
                3,
            ),
            (vec![note("B2", 0.5, 1.0), note("B2", 3.0, 2.0)], 1, 2),
        ] {
            let error = clip_from_notes(&notes, Some(4.0), BAR, None).unwrap_err();
            let NotesError::Overlap {
                first: found_first,
                second: found_second,
                ..
            } = error
            else {
                panic!("{error:?}");
            };
            assert_eq!((found_first, found_second), (first, second), "{notes:?}");
        }
    }

    #[test]
    fn bad_notes_say_which_and_why() {
        let drums = drums();
        for (spec, device, error) in [
            (note("H2", 0.0, 1.0), None, NoteError::NoDevice),
            (note("kick", 0.0, 1.0), None, NoteError::NoDevice),
            (
                note("C10", 0.0, 1.0),
                Some(&drums),
                NoteError::Pitch(NotationError::PitchOutOfRange("C10".into())),
            ),
            (note("128", 0.0, 1.0), None, NoteError::NumberOutOfRange),
            (
                NoteSpec {
                    note: NoteRef::Number(200),
                    ..note("", 0.0, 1.0)
                },
                None,
                NoteError::NumberOutOfRange,
            ),
            (note("C3", -1.0, 1.0), None, NoteError::Start(-1.0)),
            (note("C3", 2000.0, 1.0), None, NoteError::Start(2000.0)),
            (note("C3", 0.0, 0.0), None, NoteError::Length(0.0)),
            (note("C3", 0.0, 0.0001), None, NoteError::Length(0.0001)),
            (
                NoteSpec {
                    velocity: Some(0),
                    ..note("C3", 0.0, 1.0)
                },
                None,
                NoteError::Velocity(0),
            ),
            (
                NoteSpec {
                    velocity: Some(128),
                    ..note("C3", 0.0, 1.0)
                },
                None,
                NoteError::Velocity(128),
            ),
        ] {
            let notes = [note("C1", 0.0, 1.0), spec];
            let expected = NotesError::Note {
                position: 2,
                note: written(&notes[1].note),
                error,
            };
            assert_eq!(clip_from_notes(&notes, None, BAR, device), Err(expected));
        }

        let unknown = clip_from_notes(&[note("cowbel", 0.0, 1.0)], None, BAR, Some(&drums));
        let message = unknown.unwrap_err().to_string();
        assert!(
            message.starts_with("note 1 (`cowbel`): not a voice of gm-drums"),
            "{message}"
        );
        assert!(message.contains("cowbell"), "{message}");
        let nan = clip_from_notes(&[note("C3", f64::NAN, 1.0)], None, BAR, None);
        assert!(
            matches!(nan, Err(NotesError::Note { error: NoteError::Start(start), .. }) if start.is_nan())
        );
    }

    #[test]
    fn notes_must_fit_the_loop() {
        let past = clip_from_notes(&[note("C3", 4.0, 1.0)], Some(4.0), BAR, None);
        assert_eq!(
            past.unwrap_err().to_string(),
            "note 1 (`C3`): it starts at beat 4, but the loop is 4 beats long: start it earlier, or make loop_beats longer"
        );
        let longer = clip_from_notes(&[note("C3", 0.0, 4.5)], Some(4.0), BAR, None);
        assert!(matches!(
            longer,
            Err(NotesError::Note {
                error: NoteError::LongerThanLoop { .. },
                ..
            })
        ));
        for loop_beats in [0.0, -4.0, 1025.0, f64::INFINITY] {
            assert_eq!(
                clip_from_notes(&[note("C3", 0.0, 1.0)], Some(loop_beats), BAR, None),
                Err(NotesError::LoopBeats(loop_beats))
            );
        }
        let too_late = clip_from_notes(&[note("C3", 1023.0, 2.0)], None, BAR, None);
        assert_eq!(too_late, Err(NotesError::PastLongestLoop(1025.0)));
    }

    #[test]
    fn a_clip_holds_up_to_1024_notes() {
        let many = |count: usize| -> Vec<NoteSpec> {
            (0..count)
                .map(|index| note("C3", index as f64 / 4.0, 0.25))
                .collect()
        };
        assert_eq!(
            clip_from_notes(&many(MAX_NOTES), None, BAR, None)
                .unwrap()
                .events()
                .len(),
            MAX_NOTES
        );
        assert_eq!(
            clip_from_notes(&many(MAX_NOTES + 1), None, BAR, None),
            Err(NotesError::TooMany(MAX_NOTES + 1))
        );
        assert_eq!(
            clip_from_notes(&[], None, BAR, None),
            Err(NotesError::NoNotes)
        );
    }
}
