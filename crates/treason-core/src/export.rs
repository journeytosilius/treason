//! The contract between the agent's tools and whatever writes MIDI files.
//!
//! `treason-mcp` and the `treason export` command ask a [`MidiExport`] for a
//! project as a Standard MIDI File. The binary provides one that renders
//! with the engine and writes with `treason-io`, crates that neither of the
//! others may depend on.

use thiserror::Error;

use crate::midi::U7;
use crate::model::{EventKind, Project};

/// The most bars an export may span.
pub const MAX_BARS: u32 = 1000;

/// The most MIDI events an export may hold, which bounds the memory and the
/// file it takes.
pub const MAX_EVENTS: u64 = 1_000_000;

/// Why a project can't be exported.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExportError {
    /// Zero bars.
    #[error("an export needs at least one bar")]
    NoBars,
    /// More than [`MAX_BARS`] bars.
    #[error("an export may span at most {MAX_BARS} bars, not {0}")]
    TooManyBars(u32),
    /// No length was given, and the loops take more than [`MAX_BARS`] bars
    /// to line up.
    #[error(
        "the tracks' loops line up only after more than {MAX_BARS} bars, so choose how many \
         bars to export"
    )]
    NoCycle,
    /// More than [`MAX_EVENTS`] events.
    #[error("the export would hold {0} MIDI events, more than the {MAX_EVENTS} a file may")]
    TooManyEvents(u64),
    /// The MIDI file couldn't be written.
    #[error("writing the MIDI file failed: {0}")]
    Write(String),
}

/// Writes projects as Standard MIDI Files.
pub trait MidiExport {
    /// `project` as a MIDI file `bars` bars long, playing from bar 1 as it
    /// plays live: a tempo track titled `title` with the tempo and time
    /// signature, then a track for each unmuted track, named after it.
    /// Notes still sounding at the end stop there.
    fn export(&self, project: &Project, title: &str, bars: u32) -> Result<Vec<u8>, ExportError>;
}

/// How many bars to export: `requested`, checked, or by default enough for
/// every track that plays something to finish its loop on a bar line, so
/// that each pattern is complete. A project where nothing plays takes one
/// bar.
pub fn bars(project: &Project, requested: Option<u32>) -> Result<u32, ExportError> {
    match requested {
        Some(0) => Err(ExportError::NoBars),
        Some(bars) if bars > MAX_BARS => Err(ExportError::TooManyBars(bars)),
        Some(bars) => Ok(bars),
        None => cycle_bars(project).ok_or(ExportError::NoCycle),
    }
}

/// The bars until the bar line and every loop that plays something meet,
/// or `None` if that is more than [`MAX_BARS`].
fn cycle_bars(project: &Project) -> Option<u32> {
    let bar = project.time_signature.bar_length().0;
    let limit = bar * u64::from(MAX_BARS);
    let mut cycle = bar;
    let playing = project.tracks.iter().filter(|track| !track.muted);
    for track in playing.filter(|track| {
        track.clip.events().iter().any(|event| match event.kind {
            EventKind::Note { velocity, .. } => velocity != U7::MIN,
            EventKind::Control { .. }
            | EventKind::Program { .. }
            | EventKind::PitchBend { .. }
            | EventKind::Pressure { .. }
            | EventKind::NotePressure { .. } => true,
        })
    }) {
        let length = track.clip.length().0;
        cycle = (cycle / gcd(cycle, length)).checked_mul(length)?;
        if cycle > limit {
            return None;
        }
    }
    // A multiple of the bar, since it started as one.
    u32::try_from(cycle / bar).ok()
}

/// The greatest common divisor of two numbers, not both zero.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::midi::{Channel, U7, U14};
    use crate::model::{
        Clip, ClipEvent, DEFAULT_RELEASE, EventKind, OutputTarget, PortId, Track, TrackId,
    };
    use crate::time::{Ticks, TimeSignature};

    /// A track whose clip is `length` long and plays one note, unless
    /// `empty`.
    fn track(id: u32, length: Ticks, muted: bool, empty: bool) -> Arc<Track> {
        let note = ClipEvent {
            start: Ticks::ZERO,
            kind: EventKind::Note {
                note: U7::new(60).unwrap(),
                velocity: U7::MAX,
                length: Ticks(1),
                release: DEFAULT_RELEASE,
            },
        };
        let events = if empty { Vec::new() } else { vec![note] };
        Arc::new(Track {
            id: TrackId(id),
            name: format!("track {id}"),
            device: None,
            target: OutputTarget {
                port: PortId(0),
                channel: Channel::from_number(1).unwrap(),
            },
            muted,
            program: None,
            bend_range: None,
            clip: Clip::new(length, events).unwrap(),
        })
    }

    fn project(time_signature: TimeSignature, tracks: Vec<Arc<Track>>) -> Project {
        Project {
            time_signature,
            tracks,
            ..Project::default()
        }
    }

    #[test]
    fn requested_bars_are_checked() {
        let project = Project::default();
        assert_eq!(bars(&project, Some(1)), Ok(1));
        assert_eq!(bars(&project, Some(MAX_BARS)), Ok(MAX_BARS));
        assert_eq!(bars(&project, Some(0)), Err(ExportError::NoBars));
        assert_eq!(
            bars(&project, Some(MAX_BARS + 1)),
            Err(ExportError::TooManyBars(MAX_BARS + 1))
        );
    }

    #[test]
    fn by_default_every_loop_ends_on_the_same_bar_line() {
        let four_four = TimeSignature::COMMON;
        let seven_eight = TimeSignature::new(7, 8).unwrap();
        let beats = Ticks::quarters;
        let cases = [
            ("nothing plays", four_four, vec![], 1),
            (
                "one bar",
                four_four,
                vec![track(1, beats(4), false, false)],
                1,
            ),
            (
                "three bars and four",
                four_four,
                vec![
                    track(1, beats(12), false, false),
                    track(2, beats(16), false, false),
                ],
                12,
            ),
            // 5 beats meets the 4-beat bar after 20 beats.
            (
                "five beats",
                four_four,
                vec![track(1, beats(5), false, false)],
                5,
            ),
            (
                "a 4/4 bar in 7/8",
                seven_eight,
                vec![track(1, beats(4), false, false)],
                8,
            ),
            (
                "muted and empty tracks don't count",
                four_four,
                vec![
                    track(1, beats(4), false, false),
                    track(2, beats(3), true, false),
                    track(3, beats(5), false, true),
                ],
                1,
            ),
        ];
        for (case, time_signature, tracks, expected) in cases {
            assert_eq!(
                bars(&project(time_signature, tracks), None),
                Ok(expected),
                "{case}"
            );
        }
    }

    #[test]
    fn loops_that_line_up_too_late_need_a_length() {
        // Two loops one tick apart line up only after thousands of bars.
        let close = project(
            TimeSignature::COMMON,
            vec![
                track(1, Ticks(3840), false, false),
                track(2, Ticks(3839), false, false),
            ],
        );
        assert_eq!(bars(&close, None), Err(ExportError::NoCycle));
        assert_eq!(bars(&close, Some(4)), Ok(4));

        // A length whose product with the bar overflows is refused, not
        // wrapped.
        let huge = project(
            TimeSignature::COMMON,
            vec![track(1, Ticks(u64::MAX / 2), false, false)],
        );
        assert_eq!(bars(&huge, None), Err(ExportError::NoCycle));
    }

    #[test]
    fn silent_clips_do_not_extend_the_default_cycle() {
        // This clip would need 3,839 bars to meet a 4/4 bar line, but its
        // velocity-zero note emits no messages.
        let mut silent = track(2, Ticks(3839), false, true);
        Arc::make_mut(&mut silent).clip = Clip::new(
            Ticks(3839),
            vec![ClipEvent {
                start: Ticks::ZERO,
                kind: EventKind::Note {
                    note: U7::new(60).unwrap(),
                    velocity: U7::MIN,
                    length: Ticks(1),
                    release: DEFAULT_RELEASE,
                },
            }],
        )
        .unwrap();
        let cases = [
            (
                "silent track beside a one-bar part",
                vec![track(1, Ticks::quarters(4), false, false), silent.clone()],
                1,
            ),
            (
                "silent track beside a three-bar part",
                vec![silent.clone(), track(1, Ticks::quarters(12), false, false)],
                3,
            ),
            ("wholly silent project", vec![silent], 1),
        ];
        for (case, tracks, expected) in cases {
            assert_eq!(
                bars(&project(TimeSignature::COMMON, tracks), None),
                Ok(expected),
                "{case}"
            );
        }
    }

    #[test]
    fn messages_other_than_notes_still_extend_the_default_cycle() {
        for kind in [
            EventKind::Control {
                controller: U7::MIN,
                value: U7::MIN,
            },
            EventKind::Program { program: U7::MIN },
            EventKind::PitchBend { value: U14::CENTER },
            EventKind::Pressure { value: U7::MIN },
        ] {
            let mut messages = track(1, Ticks::quarters(5), false, true);
            Arc::make_mut(&mut messages).clip = Clip::new(
                Ticks::quarters(5),
                vec![
                    ClipEvent {
                        start: Ticks::ZERO,
                        kind: EventKind::Note {
                            note: U7::new(60).unwrap(),
                            velocity: U7::MIN,
                            length: Ticks(1),
                            release: DEFAULT_RELEASE,
                        },
                    },
                    ClipEvent {
                        start: Ticks(1),
                        kind,
                    },
                ],
            )
            .unwrap();
            assert_eq!(
                bars(&project(TimeSignature::COMMON, vec![messages]), None),
                Ok(5),
                "{kind:?}"
            );
        }
    }
}
