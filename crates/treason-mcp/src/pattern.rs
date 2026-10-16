//! Step patterns: clips written as one step string per voice.
//!
//! [`clip_from_steps`] turns what set_steps receives into a [`Clip`], naming
//! voices through the track's device profile. [`steps_from_clip`] goes the
//! other way, so the agent reads back any clip that fits a step grid in the
//! form it writes.

use std::collections::BTreeMap;

use thiserror::Error;
use treason_core::midi::U7;
use treason_core::model::{Clip, ClipError, ClipEvent, EventKind};
use treason_core::time::{PPQN, Ticks};
use treason_devices::DeviceProfile;

use crate::notation::{self, NotationError, Step};

/// The step sizes on offer, in steps per beat (a quarter note): from whole
/// beats to 128th notes, with triplets. Each divides [`PPQN`].
pub const STEPS_PER_BEAT: [u32; 10] = [1, 2, 3, 4, 6, 8, 12, 16, 24, 32];

/// Steps per beat when the agent doesn't say: sixteenth notes.
pub const DEFAULT_STEPS_PER_BEAT: u32 = 4;

/// The most steps a pattern can have: 64 bars of sixteenths in 4/4.
pub const MAX_STEPS: usize = 1024;

/// The velocity of `x`.
pub const HIT_VELOCITY: U7 = U7::saturating(100);

/// The velocity of `X`.
pub const ACCENT_VELOCITY: U7 = U7::MAX;

/// A clip written as step strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepPattern {
    /// Steps per beat.
    pub steps_per_beat: u32,
    /// One step string per voice, grouped by beat.
    pub steps: BTreeMap<String, String>,
}

/// A step pattern that can't become a clip. The messages tell the agent how
/// to fix the call.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatternError {
    /// No step strings.
    #[error(r#"give at least one voice, such as {{"kick": "x...x...x...x..."}}"#)]
    NoVoices,
    /// A step size that isn't on offer.
    #[error(
        "steps_per_beat must be 1, 2, 3, 4, 6, 8, 12, 16, 24 or 32, not {0}: 4 makes sixteenth notes, 3 eighth-note triplets"
    )]
    StepsPerBeat(u32),
    /// A number that isn't a MIDI note.
    #[error("`{0}` is not a MIDI note: notes are 0-127")]
    NoteOutOfRange(String),
    /// A voice name on a track without a device.
    #[error(
        "`{0}` is not a MIDI note number, and the track has no device to name its voices: use note numbers 0-127, or give the track a device with edit_track (see list_devices)"
    )]
    NoDevice(String),
    /// A voice the device doesn't have.
    #[error(
        "{device} has no voice `{voice}`; its voices are {voices} (list_devices shows their aliases)"
    )]
    UnknownVoice {
        voice: String,
        device: String,
        voices: String,
    },
    /// Two voices that are the same note, such as a voice and its alias.
    #[error("`{first}` and `{second}` are both note {note}: give each note once")]
    SameNote {
        first: String,
        second: String,
        note: U7,
    },
    /// A step string that doesn't parse.
    #[error("`{voice}`: {error}")]
    Notation { voice: String, error: NotationError },
    /// A step string with no steps.
    #[error("`{0}` has no steps")]
    NoSteps(String),
    /// A step string over the limit.
    #[error(
        "`{voice}` has {steps} steps, more than the {} a pattern can hold: shorten it, or use fewer steps per beat",
        MAX_STEPS
    )]
    TooLong { voice: String, steps: usize },
    /// A shorter string that can't repeat evenly through the loop.
    #[error(
        "`{voice}` has {steps} steps, which doesn't divide the {loop_steps}-step loop: give it {loop_steps} steps, or a length that divides {loop_steps} so it repeats"
    )]
    Uneven {
        voice: String,
        steps: usize,
        loop_steps: usize,
    },
    /// The clip itself was rejected.
    #[error(transparent)]
    Clip(#[from] ClipError),
}

/// The clip a step pattern plays: one note per hit, one step long.
///
/// `steps` maps voices to step strings. A voice is a MIDI note number or,
/// when the track has a `device`, one of its voice names or aliases. The
/// loop is as long as the longest string. A shorter string repeats to fill
/// it, so its length must divide the loop's.
pub fn clip_from_steps(
    steps: &BTreeMap<String, String>,
    steps_per_beat: Option<u32>,
    device: Option<&DeviceProfile>,
) -> Result<Clip, PatternError> {
    let per_beat = steps_per_beat.unwrap_or(DEFAULT_STEPS_PER_BEAT);
    if !STEPS_PER_BEAT.contains(&per_beat) {
        return Err(PatternError::StepsPerBeat(per_beat));
    }
    if steps.is_empty() {
        return Err(PatternError::NoVoices);
    }
    let step = PPQN / u64::from(per_beat);

    let mut voices: Vec<(&str, U7, Vec<Step>)> = Vec::with_capacity(steps.len());
    for (voice, text) in steps {
        let note = resolve(voice, device)?;
        if let Some((first, ..)) = voices.iter().find(|(_, other, _)| *other == note) {
            return Err(PatternError::SameNote {
                first: (*first).to_owned(),
                second: voice.clone(),
                note,
            });
        }
        let parsed = notation::parse_steps(text).map_err(|error| PatternError::Notation {
            voice: voice.clone(),
            error,
        })?;
        if parsed.is_empty() {
            return Err(PatternError::NoSteps(voice.clone()));
        }
        if parsed.len() > MAX_STEPS {
            return Err(PatternError::TooLong {
                voice: voice.clone(),
                steps: parsed.len(),
            });
        }
        voices.push((voice, note, parsed));
    }

    let loop_steps = voices
        .iter()
        .map(|(_, _, parsed)| parsed.len())
        .max()
        .unwrap_or_default();
    let mut events = Vec::new();
    for (voice, note, parsed) in &voices {
        if !loop_steps.is_multiple_of(parsed.len()) {
            return Err(PatternError::Uneven {
                voice: (*voice).to_owned(),
                steps: parsed.len(),
                loop_steps,
            });
        }
        for (index, current) in parsed.iter().cycle().take(loop_steps).enumerate() {
            let velocity = match current {
                Step::Rest => continue,
                Step::Hit => HIT_VELOCITY,
                Step::Accent => ACCENT_VELOCITY,
            };
            events.push(ClipEvent {
                start: Ticks(index as u64 * step),
                kind: EventKind::Note {
                    note: *note,
                    velocity,
                    length: Ticks(step),
                },
            });
        }
    }
    Ok(Clip::new(Ticks(loop_steps as u64 * step), events)?)
}

/// A clip as step strings, if it fits a step grid: every event a note one
/// step long that starts on a step, at the velocity of `x` or `X`, with a
/// step size on offer. Notes go by their voice name in `device` where it has
/// one, and by number otherwise. An empty clip has no pattern.
pub fn steps_from_clip(clip: &Clip, device: Option<&DeviceProfile>) -> Option<StepPattern> {
    let mut step = None;
    let mut hits = Vec::with_capacity(clip.events().len());
    for event in clip.events() {
        let EventKind::Note {
            note,
            velocity,
            length,
        } = event.kind
        else {
            return None;
        };
        if *step.get_or_insert(length) != length {
            return None;
        }
        let hit = match velocity {
            HIT_VELOCITY => Step::Hit,
            ACCENT_VELOCITY => Step::Accent,
            _ => return None,
        };
        hits.push((event.start, note, hit));
    }

    let step = step?.0;
    let per_beat = u32::try_from(PPQN.checked_div(step)?).ok()?;
    if !PPQN.is_multiple_of(step)
        || !STEPS_PER_BEAT.contains(&per_beat)
        || !clip.length().0.is_multiple_of(step)
    {
        return None;
    }
    let loop_steps = usize::try_from(clip.length().0 / step).ok()?;
    if loop_steps > MAX_STEPS {
        return None;
    }

    let mut rows: BTreeMap<U7, Vec<Step>> = BTreeMap::new();
    for (start, note, hit) in hits {
        if !start.0.is_multiple_of(step) {
            return None;
        }
        let index = usize::try_from(start.0 / step).ok()?;
        let row = rows
            .entry(note)
            .or_insert_with(|| vec![Step::Rest; loop_steps]);
        match row.get_mut(index) {
            // Two notes on the same step can't be written as one character.
            Some(slot @ Step::Rest) => *slot = hit,
            _ => return None,
        }
    }

    let group = per_beat as usize;
    let steps = rows
        .into_iter()
        .map(|(note, row)| {
            let name = device
                .and_then(|device| device.voice_name(note))
                .map_or_else(|| note.to_string(), str::to_owned);
            (name, notation::format_steps(&row, group))
        })
        .collect();
    Some(StepPattern {
        steps_per_beat: per_beat,
        steps,
    })
}

/// The note a voice names: a MIDI note number, or a voice or alias of the
/// device.
fn resolve(voice: &str, device: Option<&DeviceProfile>) -> Result<U7, PatternError> {
    if !voice.is_empty() && voice.bytes().all(|byte| byte.is_ascii_digit()) {
        return voice
            .parse()
            .ok()
            .and_then(U7::new)
            .ok_or_else(|| PatternError::NoteOutOfRange(voice.to_owned()));
    }
    let device = device.ok_or_else(|| PatternError::NoDevice(voice.to_owned()))?;
    device
        .voice(voice)
        .ok_or_else(|| PatternError::UnknownVoice {
            voice: voice.to_owned(),
            device: device.id.clone(),
            voices: device
                .voices
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use treason_devices::DeviceRegistry;

    use super::*;

    fn drums() -> DeviceProfile {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../devices");
        let registry = DeviceRegistry::load_dir(&dir).unwrap();
        registry.get("gm-drums").unwrap().clone()
    }

    fn pattern(voices: &[(&str, &str)]) -> BTreeMap<String, String> {
        voices
            .iter()
            .map(|(voice, steps)| ((*voice).to_owned(), (*steps).to_owned()))
            .collect()
    }

    fn note(start: u64, note: u8, velocity: U7, length: u64) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: U7::new(note).unwrap(),
                velocity,
                length: Ticks(length),
            },
        }
    }

    #[test]
    fn hits_become_one_step_notes() {
        let steps = pattern(&[("kick", "x..."), ("sd", ".X..")]);
        let clip = clip_from_steps(&steps, None, Some(&drums())).unwrap();
        assert_eq!(clip.length(), Ticks::quarters(1));
        assert_eq!(
            clip.events(),
            [
                note(0, 36, HIT_VELOCITY, 240),
                note(240, 38, ACCENT_VELOCITY, 240)
            ]
        );
    }

    #[test]
    fn shorter_strings_repeat_to_fill_the_loop() {
        let steps = pattern(&[("kick", "x..."), ("closed_hat", "x.x.x.x.x.x.x.x.")]);
        let clip = clip_from_steps(&steps, None, Some(&drums())).unwrap();
        assert_eq!(clip.length(), Ticks::quarters(4));
        let kicks: Vec<u64> = clip
            .events()
            .iter()
            .filter(|event| matches!(event.kind, EventKind::Note { note, .. } if note.get() == 36))
            .map(|event| event.start.0)
            .collect();
        assert_eq!(kicks, [0, 960, 1920, 2880]);
    }

    #[test]
    fn steps_per_beat_sets_the_step_length() {
        let steps = pattern(&[("60", "x..x..")]);
        let clip = clip_from_steps(&steps, Some(3), None).unwrap();
        assert_eq!(clip.length(), Ticks::quarters(2));
        assert_eq!(
            clip.events(),
            [
                note(0, 60, HIT_VELOCITY, 320),
                note(960, 60, HIT_VELOCITY, 320)
            ]
        );
    }

    #[test]
    fn bad_patterns_are_rejected_with_a_fix() {
        let device = drums();
        let drums = Some(&device);
        let cases = [
            (pattern(&[]), None, drums, "at least one voice"),
            (
                pattern(&[("kick", "x...")]),
                Some(5),
                drums,
                "steps_per_beat must be",
            ),
            (
                pattern(&[("kik", "x...")]),
                None,
                drums,
                "has no voice `kik`",
            ),
            (pattern(&[("kick", "x...")]), None, None, "has no device"),
            (
                pattern(&[("128", "x...")]),
                None,
                None,
                "`128` is not a MIDI note",
            ),
            (
                pattern(&[("kick", "x..."), ("bd", "x...")]),
                None,
                drums,
                "both note 36",
            ),
            (
                pattern(&[("kick", "x..o")]),
                None,
                drums,
                "unexpected `o` at step 4",
            ),
            (
                pattern(&[("kick", " | ")]),
                None,
                drums,
                "`kick` has no steps",
            ),
            (
                pattern(&[("kick", "x...x...x...x..."), ("snare", "x.....")]),
                None,
                drums,
                "doesn't divide the 16-step loop",
            ),
        ];
        for (steps, per_beat, device, expected) in cases {
            let error = clip_from_steps(&steps, per_beat, device).unwrap_err();
            assert!(error.to_string().contains(expected), "{steps:?}: {error}");
        }
        let long = "x".repeat(MAX_STEPS + 1);
        let error = clip_from_steps(&pattern(&[("1", &long)]), None, None).unwrap_err();
        assert!(matches!(error, PatternError::TooLong { steps: 1025, .. }));
    }

    #[test]
    fn steps_per_beat_error_lists_every_choice() {
        let message = PatternError::StepsPerBeat(5).to_string();
        let listed: Vec<u32> = message
            .split(", not")
            .next()
            .unwrap()
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|number| number.parse().ok())
            .collect();
        assert_eq!(listed, STEPS_PER_BEAT, "{message}");
    }

    #[test]
    fn patterns_read_back_as_written() {
        let device = drums();
        let steps = pattern(&[("bd", "x..."), ("snare", "....X..."), ("hh", "..x.")]);
        let clip = clip_from_steps(&steps, None, Some(&device)).unwrap();
        let read = steps_from_clip(&clip, Some(&device)).unwrap();
        assert_eq!(read.steps_per_beat, 4);
        assert_eq!(
            read.steps,
            pattern(&[
                ("kick", "x... x..."),
                ("snare", ".... X..."),
                ("closed_hat", "..x. ..x."),
            ])
        );
        let again = clip_from_steps(&read.steps, Some(read.steps_per_beat), Some(&device));
        assert_eq!(again.unwrap(), clip);

        let numbered = steps_from_clip(&clip, None).unwrap();
        assert_eq!(numbered.steps["36"], "x... x...");
    }

    #[test]
    fn clips_off_the_grid_have_no_pattern() {
        let bar = Ticks::quarters(4);
        let clip = |events| Clip::new(bar, events).unwrap();
        assert_eq!(steps_from_clip(&clip(vec![]), None), None);
        // Off a step.
        let off = clip(vec![
            note(0, 36, HIT_VELOCITY, 240),
            note(100, 38, HIT_VELOCITY, 240),
        ]);
        assert_eq!(steps_from_clip(&off, None), None);
        // Notes of different lengths.
        let held = clip(vec![
            note(0, 36, HIT_VELOCITY, 240),
            note(480, 38, HIT_VELOCITY, 960),
        ]);
        assert_eq!(steps_from_clip(&held, None), None);
        // A velocity that is neither x nor X.
        let soft = clip(vec![note(0, 36, U7::new(64).unwrap(), 240)]);
        assert_eq!(steps_from_clip(&soft, None), None);
        // A step size that isn't on offer.
        let odd = clip(vec![note(0, 36, HIT_VELOCITY, 192)]);
        assert_eq!(steps_from_clip(&odd, None), None);
        // A control change.
        let control = clip(vec![ClipEvent {
            start: Ticks::ZERO,
            kind: EventKind::Control {
                controller: U7::new(1).unwrap(),
                value: U7::MAX,
            },
        }]);
        assert_eq!(steps_from_clip(&control, None), None);
        // The same note twice on one step.
        let twice = clip(vec![
            note(0, 36, HIT_VELOCITY, 240),
            note(0, 36, ACCENT_VELOCITY, 240),
        ]);
        assert_eq!(steps_from_clip(&twice, None), None);
    }
}
