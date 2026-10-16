//! Turning clips into MIDI events.

use std::ops::Range;

use treason_core::midi::{MidiMessage, U7};
use treason_core::model::{EventKind, PortId, Project, Track};
use treason_core::time::{TICKS_PER_CLOCK_PULSE, Ticks};

/// A message to send at a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderedEvent {
    /// When, in song ticks.
    pub tick: Ticks,
    /// Where.
    pub port: PortId,
    /// What.
    pub message: MidiMessage,
}

/// Appends to `out` everything `project` plays in `window`, in send order.
///
/// Every unmuted track loops its clip from tick 0, so an event at clip
/// position `p` plays at `p + k × length` for every `k`. A note's note-off
/// lands at its start plus its length, which may be in a later window: each
/// window holds the note-offs that fall inside it. Notes with velocity 0
/// play nothing.
///
/// Events are ordered by tick. At the same tick, note-offs come first, then
/// program changes, control changes and note-ons, so a retriggered note isn't
/// cut short and a note plays with the program and controller values set on
/// its tick.
///
/// `out` is reused from window to window: with its capacity reserved up
/// front, rendering doesn't allocate.
pub fn render(project: &Project, window: Range<Ticks>, out: &mut Vec<RenderedEvent>) {
    if window.start >= window.end {
        return;
    }
    let first = out.len();
    for track in project.tracks.iter().filter(|track| !track.muted) {
        render_track(track, window.clone(), out);
    }
    out[first..].sort_unstable_by_key(send_order);
}

/// The ticks in `window` that get a MIDI clock pulse: every 40th tick,
/// counting from 0, which is 24 pulses per quarter note.
pub fn clock_pulses(window: Range<Ticks>) -> impl Iterator<Item = Ticks> {
    let first = window.start.0.div_ceil(TICKS_PER_CLOCK_PULSE);
    (first..)
        .map(|pulse| Ticks(pulse * TICKS_PER_CLOCK_PULSE))
        .take_while(move |tick| *tick < window.end)
}

/// Appends one track's events in a non-empty `window`, unsorted.
fn render_track(track: &Track, window: Range<Ticks>, out: &mut Vec<RenderedEvent>) {
    let clip = &track.clip;
    if clip.events().is_empty() {
        return;
    }
    let (start, end) = (window.start.0, window.end.0);
    let loop_length = clip.length().0;
    let port = track.target.port;
    let channel = track.target.channel;
    let mut emit = |tick, message| {
        out.push(RenderedEvent {
            tick: Ticks(tick),
            port,
            message,
        });
    };
    // A note that ends in the window started at most the longest note
    // earlier, and no note is longer than the loop, so this looks back at
    // most one pass.
    let from = start.saturating_sub(clip.longest_note().0);
    for pass in from / loop_length..=(end - 1) / loop_length {
        let offset = pass * loop_length;
        let span = Ticks(from.max(offset) - offset)..Ticks(end.min(offset + loop_length) - offset);
        for event in clip.events_in(span) {
            let at = offset + event.start.0;
            match event.kind {
                EventKind::Note { velocity, .. } if velocity == U7::MIN => {}
                EventKind::Note {
                    note,
                    velocity,
                    length,
                } => {
                    if at >= start {
                        let on = MidiMessage::NoteOn {
                            channel,
                            note,
                            velocity,
                        };
                        emit(at, on);
                    }
                    let off_at = at + length.0;
                    if (start..end).contains(&off_at) {
                        let off = MidiMessage::NoteOff {
                            channel,
                            note,
                            velocity: U7::MIN,
                        };
                        emit(off_at, off);
                    }
                }
                EventKind::Control { controller, value } if at >= start => {
                    let message = MidiMessage::ControlChange {
                        channel,
                        controller,
                        value,
                    };
                    emit(at, message);
                }
                EventKind::Program { program } if at >= start => {
                    emit(at, MidiMessage::ProgramChange { channel, program });
                }
                // Before the window, so already sent.
                EventKind::Control { .. } | EventKind::Program { .. } => {}
            }
        }
    }
}

/// Where an event goes in the send order: by tick, then note-offs, program
/// changes, control changes and note-ons, then by port and message. Events
/// that tie are identical, so the order is the same on every run.
fn send_order(event: &RenderedEvent) -> (Ticks, u8, PortId, [u8; 3]) {
    let rank = match event.message {
        MidiMessage::NoteOff { .. } => 0,
        MidiMessage::ProgramChange { .. } => 1,
        MidiMessage::ControlChange { .. } => 2,
        _ => 3,
    };
    let mut bytes = [0; 3];
    event.message.encode(&mut bytes);
    (event.tick, rank, event.port, bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use treason_core::midi::Channel;
    use treason_core::model::{Clip, ClipEvent, OutputTarget, TrackId};

    use super::*;

    fn note(start: u64, note: u8, length: u64) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: U7::new(note).unwrap(),
                velocity: U7::MAX,
                length: Ticks(length),
            },
        }
    }

    fn track(id: u32, length: u64, events: Vec<ClipEvent>) -> Track {
        Track {
            id: TrackId(id),
            name: format!("track {id}"),
            device: None,
            target: OutputTarget {
                port: PortId(0),
                channel: Channel::DRUMS,
            },
            muted: false,
            clip: Clip::new(Ticks(length), events).unwrap(),
        }
    }

    fn project(tracks: Vec<Track>) -> Project {
        Project {
            tracks: tracks.into_iter().map(Arc::new).collect(),
            ..Project::default()
        }
    }

    /// What plays in `window`, one line per event, such as "240 off 36".
    fn play(project: &Project, window: Range<u64>) -> Vec<String> {
        let mut out = Vec::new();
        render(project, Ticks(window.start)..Ticks(window.end), &mut out);
        out.iter()
            .map(|event| {
                let what = match event.message {
                    MidiMessage::NoteOn { note, .. } => format!("on {}", note.get()),
                    MidiMessage::NoteOff { note, .. } => format!("off {}", note.get()),
                    MidiMessage::ControlChange {
                        controller, value, ..
                    } => format!("cc {}={}", controller.get(), value.get()),
                    MidiMessage::ProgramChange { program, .. } => {
                        format!("pc {}", program.get())
                    }
                    other => format!("{other:?}"),
                };
                format!("{} {what}", event.tick.0)
            })
            .collect()
    }

    /// A beat with a kick on the downbeat and a snare on the off-beat, each
    /// a 16th long.
    fn beat() -> Project {
        project(vec![track(
            1,
            960,
            vec![note(480, 38, 240), note(0, 36, 240)],
        )])
    }

    #[test]
    fn notes_end_after_their_length() {
        assert_eq!(
            play(&beat(), 0..960),
            ["0 on 36", "240 off 36", "480 on 38", "720 off 38"]
        );
    }

    #[test]
    fn clips_loop_from_tick_zero() {
        assert_eq!(
            play(&beat(), 1920..2880),
            ["1920 on 36", "2160 off 36", "2400 on 38", "2640 off 38"]
        );
        // Across the loop point.
        assert_eq!(play(&beat(), 720..1200), ["720 off 38", "960 on 36"]);
    }

    #[test]
    fn each_window_holds_the_events_inside_it() {
        let project = beat();
        let cases: [(Range<u64>, &[&str]); 4] = [
            (0..100, &["0 on 36"]),
            (100..240, &[]),
            (240..241, &["240 off 36"]),
            (960..960, &[]),
        ];
        for (window, expected) in cases {
            assert_eq!(play(&project, window.clone()), expected, "{window:?}");
        }
        let (start, end) = (961, 960);
        assert!(play(&project, start..end).is_empty(), "a backward window");
        // Windows that tile the timeline play everything exactly once.
        let tiled: Vec<String> = (0..48)
            .flat_map(|window| play(&project, window * 80..(window + 1) * 80))
            .collect();
        assert_eq!(tiled, play(&project, 0..3840));
        assert_eq!(tiled.len(), 16);
    }

    #[test]
    fn retriggered_notes_end_before_they_start_again() {
        let project = project(vec![track(
            1,
            480,
            vec![note(0, 36, 240), note(240, 36, 240)],
        )]);
        assert_eq!(
            play(&project, 0..720),
            [
                "0 on 36",
                "240 off 36",
                "240 on 36",
                "480 off 36",
                "480 on 36",
            ]
        );
    }

    #[test]
    fn notes_held_over_the_loop_point_end_in_the_next_pass() {
        // A note tied over the loop point, and one as long as the loop.
        let tied = project(vec![track(1, 960, vec![note(720, 60, 480)])]);
        assert_eq!(play(&tied, 0..960), ["720 on 60"]);
        assert_eq!(play(&tied, 960..1920), ["1200 off 60", "1680 on 60"]);
        let drone = project(vec![track(1, 960, vec![note(0, 48, 960)])]);
        assert_eq!(
            play(&drone, 0..1920),
            ["0 on 48", "960 off 48", "960 on 48"]
        );
        assert_eq!(play(&drone, 1000..1920), Vec::<String>::new());
    }

    #[test]
    fn programs_and_controls_come_before_notes_on_their_tick() {
        let controls = vec![
            note(240, 40, 240),
            ClipEvent {
                start: Ticks(240),
                kind: EventKind::Control {
                    controller: U7::new(74).unwrap(),
                    value: U7::new(20).unwrap(),
                },
            },
            ClipEvent {
                start: Ticks(240),
                kind: EventKind::Program {
                    program: U7::new(5).unwrap(),
                },
            },
            note(0, 40, 240),
        ];
        let project = project(vec![track(1, 960, controls)]);
        assert_eq!(
            play(&project, 0..480),
            [
                "0 on 40",
                "240 off 40",
                "240 pc 5",
                "240 cc 74=20",
                "240 on 40",
            ]
        );
        // Controls before the window aren't sent again.
        assert_eq!(play(&project, 241..480), Vec::<String>::new());
    }

    #[test]
    fn tracks_play_on_their_own_port_and_channel_and_loop() {
        let mut bass = track(2, 2880, vec![note(0, 40, 240)]);
        bass.target = OutputTarget {
            port: PortId(1),
            channel: Channel::from_number(2).unwrap(),
        };
        let mut muted = track(3, 960, vec![note(0, 50, 240)]);
        muted.muted = true;
        // Four-beat kicks against a three-beat bass line: polymeter.
        let project = project(vec![track(1, 3840, vec![note(0, 36, 240)]), bass, muted]);
        let mut out = Vec::new();
        render(&project, Ticks(0)..Ticks(7680), &mut out);
        let ons: Vec<(u64, u16, u8, u8)> = out
            .iter()
            .filter_map(|event| match event.message {
                MidiMessage::NoteOn { channel, note, .. } => {
                    Some((event.tick.0, event.port.0, channel.number(), note.get()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            ons,
            [
                (0, 0, 10, 36),
                (0, 1, 2, 40),
                (2880, 1, 2, 40),
                (3840, 0, 10, 36),
                (5760, 1, 2, 40),
            ]
        );
    }

    #[test]
    fn silent_notes_play_nothing() {
        let mut silent = note(0, 36, 240);
        silent.kind = EventKind::Note {
            note: U7::new(36).unwrap(),
            velocity: U7::MIN,
            length: Ticks(240),
        };
        let project = project(vec![track(1, 960, vec![silent])]);
        assert_eq!(play(&project, 0..960), Vec::<String>::new());
    }

    #[test]
    fn rendering_appends_without_reallocating() {
        let mut out = Vec::with_capacity(64);
        let earlier = RenderedEvent {
            tick: Ticks(5000),
            port: PortId(0),
            message: MidiMessage::Stop,
        };
        out.push(earlier);
        let buffer = out.as_ptr();
        render(&beat(), Ticks(0)..Ticks(1920), &mut out);
        assert_eq!(out.as_ptr(), buffer);
        assert_eq!(out.len(), 9);
        assert_eq!(out[0], earlier, "what was there stays first");
    }

    #[test]
    fn clock_pulses_fall_every_40_ticks() {
        let pulses: Vec<u64> = clock_pulses(Ticks(0)..Ticks(100)).map(|t| t.0).collect();
        assert_eq!(pulses, [0, 40, 80]);
        let pulses: Vec<u64> = clock_pulses(Ticks(41)..Ticks(121)).map(|t| t.0).collect();
        assert_eq!(pulses, [80, 120]);
        assert_eq!(clock_pulses(Ticks(1)..Ticks(40)).count(), 0);
    }
}
