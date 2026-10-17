//! Standard MIDI File export.
//!
//! [`write`](fn@write) takes a [`Song`]: what the tempo track says, and
//! each track's messages at their ticks. The engine's `render_song` renders
//! a project into those messages, and the binary sorts them into tracks.

use std::io;

use midly::num::{u4, u7, u14, u15, u24, u28};
use midly::{Format, Header, MetaMessage, Timing, TrackEvent, TrackEventKind};
use thiserror::Error;
use treason_core::midi::MidiMessage;
use treason_core::time::{Bpm, PPQN, Ticks, TimeSignature};

/// What [`write`](fn@write) puts in a file.
#[derive(Debug, Clone, Copy)]
pub struct Song<'a> {
    /// The song's name, which the tempo track carries.
    pub title: &'a str,
    /// The tempo, for the whole song.
    pub tempo: Bpm,
    /// The time signature, for the whole song.
    pub time_signature: TimeSignature,
    /// Where every track ends. No message may come later.
    pub length: Ticks,
    /// The tracks after the tempo track, in order.
    pub tracks: &'a [SongTrack<'a>],
}

/// One track of a [`Song`].
#[derive(Debug, Clone, Copy)]
pub struct SongTrack<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its channel messages in the order they play, each at its tick.
    pub messages: &'a [(Ticks, MidiMessage)],
}

/// Why a song can't be written.
#[derive(Debug, Error)]
pub enum SmfError {
    /// A message a MIDI file can't hold. Files take channel messages, not
    /// MIDI clock, transport or Song Position Pointer.
    #[error("track `{track}`: a MIDI file can't hold {message:?}")]
    Unsupported {
        /// The track's name.
        track: String,
        /// The message.
        message: MidiMessage,
    },
    /// A message earlier than the one before it.
    #[error("track `{track}`: the message at {tick} comes after one at {previous}")]
    OutOfOrder {
        /// The track's name.
        track: String,
        /// The message's tick.
        tick: Ticks,
        /// The tick of the message before it.
        previous: Ticks,
    },
    /// A message after the song's end.
    #[error("track `{track}`: the message at {tick} comes after the end, at {length}")]
    PastTheEnd {
        /// The track's name.
        track: String,
        /// The message's tick.
        tick: Ticks,
        /// The song's length.
        length: Ticks,
    },
    /// A song longer than the time between two events in a MIDI file may
    /// be.
    #[error("a song {0} long is longer than a MIDI file can hold")]
    TooLong(Ticks),
    /// Writing failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Writes `song` to `out` as a format 1 MIDI file at [`PPQN`] ticks per
/// quarter note: a tempo track with the title, time signature and tempo,
/// then each track with its name and messages. Every track ends at the
/// song's length.
///
/// Checks the whole song before writing, so a song it refuses leaves `out`
/// untouched.
pub fn write(song: &Song<'_>, out: impl io::Write) -> Result<(), SmfError> {
    // Every gap between events fits once the whole song does.
    u32::try_from(song.length.0)
        .ok()
        .and_then(u28::try_from)
        .ok_or(SmfError::TooLong(song.length))?;
    let mut tracks = Vec::with_capacity(song.tracks.len() + 1);
    tracks.push(tempo_track(song));
    for track in song.tracks {
        tracks.push(channel_track(track, song.length)?);
    }
    let ticks_per_quarter = u15::new(PPQN as u16);
    let header = Header::new(Format::Parallel, Timing::Metrical(ticks_per_quarter));
    midly::write_std(&header, &tracks, out)?;
    Ok(())
}

/// The first track: the title, time signature and tempo, at the start.
fn tempo_track<'a>(song: &Song<'a>) -> Vec<TrackEvent<'a>> {
    let time_signature = song.time_signature;
    let denominator = time_signature.denominator();
    // The metronome clicks once a beat. MIDI clock runs at 24 pulses per
    // quarter note, 96 per whole note.
    let clocks_per_click = 96 / denominator;
    let micros_per_quarter = (60_000_000.0 / song.tempo.get()).round() as u32;
    vec![
        at(0, MetaMessage::TrackName(song.title.as_bytes())),
        at(
            0,
            MetaMessage::TimeSignature(
                time_signature.numerator(),
                denominator.trailing_zeros() as u8,
                clocks_per_click,
                8,
            ),
        ),
        at(0, MetaMessage::Tempo(u24::new(micros_per_quarter))),
        at(song.length.0, MetaMessage::EndOfTrack),
    ]
}

/// A track with its name and messages, ending at `length`. Each event
/// waits the ticks since the one before it.
fn channel_track<'a>(
    track: &SongTrack<'a>,
    length: Ticks,
) -> Result<Vec<TrackEvent<'a>>, SmfError> {
    let mut events = Vec::with_capacity(track.messages.len() + 2);
    events.push(at(0, MetaMessage::TrackName(track.name.as_bytes())));
    let mut previous = Ticks::ZERO;
    for &(tick, message) in track.messages {
        if tick < previous {
            return Err(SmfError::OutOfOrder {
                track: track.name.to_owned(),
                tick,
                previous,
            });
        }
        if tick > length {
            return Err(SmfError::PastTheEnd {
                track: track.name.to_owned(),
                tick,
                length,
            });
        }
        let kind = channel_event(message).ok_or_else(|| SmfError::Unsupported {
            track: track.name.to_owned(),
            message,
        })?;
        events.push(TrackEvent {
            delta: delta(tick - previous),
            kind,
        });
        previous = tick;
    }
    events.push(TrackEvent {
        delta: delta(length - previous),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    Ok(events)
}

/// A meta event `ticks` after the one before it.
fn at(ticks: u64, message: MetaMessage<'_>) -> TrackEvent<'_> {
    TrackEvent {
        delta: delta(Ticks(ticks)),
        kind: TrackEventKind::Meta(message),
    }
}

/// The time between two events, which [`write`](fn@write) has checked fits.
fn delta(ticks: Ticks) -> u28 {
    u28::new(ticks.0 as u32)
}

/// `message` as a file holds it, or `None` for a system message.
fn channel_event(message: MidiMessage) -> Option<TrackEventKind<'static>> {
    use midly::MidiMessage as Smf;
    let (channel, message) = match message {
        MidiMessage::NoteOff {
            channel,
            note,
            velocity,
        } => (
            channel,
            Smf::NoteOff {
                key: u7::new(note.get()),
                vel: u7::new(velocity.get()),
            },
        ),
        MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        } => (
            channel,
            Smf::NoteOn {
                key: u7::new(note.get()),
                vel: u7::new(velocity.get()),
            },
        ),
        MidiMessage::PolyPressure {
            channel,
            note,
            pressure,
        } => (
            channel,
            Smf::Aftertouch {
                key: u7::new(note.get()),
                vel: u7::new(pressure.get()),
            },
        ),
        MidiMessage::ControlChange {
            channel,
            controller,
            value,
        } => (
            channel,
            Smf::Controller {
                controller: u7::new(controller.get()),
                value: u7::new(value.get()),
            },
        ),
        MidiMessage::ProgramChange { channel, program } => (
            channel,
            Smf::ProgramChange {
                program: u7::new(program.get()),
            },
        ),
        MidiMessage::ChannelPressure { channel, pressure } => (
            channel,
            Smf::ChannelAftertouch {
                vel: u7::new(pressure.get()),
            },
        ),
        MidiMessage::PitchBend { channel, value } => (
            channel,
            Smf::PitchBend {
                bend: midly::PitchBend(u14::new(value.get())),
            },
        ),
        MidiMessage::SongPosition(_)
        | MidiMessage::TimingClock
        | MidiMessage::Start
        | MidiMessage::Continue
        | MidiMessage::Stop => return None,
    };
    Some(TrackEventKind::Midi {
        channel: u4::new(channel.index()),
        message,
    })
}

#[cfg(test)]
mod tests {
    use midly::Smf;
    use treason_core::midi::{Channel, U7, U14};

    use super::*;

    fn note_on(note: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            channel: Channel::from_number(1).unwrap(),
            note: U7::new(note).unwrap(),
            velocity: U7::new(100).unwrap(),
        }
    }

    fn note_off(note: u8) -> MidiMessage {
        MidiMessage::NoteOff {
            channel: Channel::from_number(1).unwrap(),
            note: U7::new(note).unwrap(),
            velocity: U7::MIN,
        }
    }

    fn song<'a>(tracks: &'a [SongTrack<'a>], length: u64) -> Song<'a> {
        Song {
            title: "t",
            tempo: Bpm::new(120.0).unwrap(),
            time_signature: TimeSignature::COMMON,
            length: Ticks(length),
            tracks,
        }
    }

    fn bytes(song: &Song<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        write(song, &mut out).unwrap();
        out
    }

    /// Each event of each track, at its tick from the start.
    fn timeline<'a>(smf: &'a Smf<'a>) -> Vec<Vec<(u32, TrackEventKind<'a>)>> {
        smf.tracks
            .iter()
            .map(|track| {
                let mut tick = 0;
                track
                    .iter()
                    .map(|event| {
                        tick += event.delta.as_int();
                        (tick, event.kind)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn files_look_like_this() {
        let messages = [(Ticks(0), note_on(60)), (Ticks(480), note_off(60))];
        let tracks = [SongTrack {
            name: "a",
            messages: &messages,
        }];
        #[rustfmt::skip]
        let expected: &[u8] = &[
            // Format 1, two tracks, 960 ticks per quarter note.
            b'M', b'T', b'h', b'd', 0, 0, 0, 6, 0, 1, 0, 2, 0x03, 0xc0,
            // The tempo track, 25 bytes.
            b'M', b'T', b'r', b'k', 0, 0, 0, 25,
            // Its name, "t".
            0x00, 0xff, 0x03, 1, b't',
            // 4/4, a click every 24 MIDI clocks, eight 32nds a quarter.
            0x00, 0xff, 0x58, 4, 4, 2, 24, 8,
            // 500,000 microseconds a quarter note: 120 BPM.
            0x00, 0xff, 0x51, 3, 0x07, 0xa1, 0x20,
            // The end, 960 ticks (0x87 0x40) in.
            0x87, 0x40, 0xff, 0x2f, 0,
            // Track "a", 19 bytes.
            b'M', b'T', b'r', b'k', 0, 0, 0, 19,
            0x00, 0xff, 0x03, 1, b'a',
            // A note-on at 0 and its note-off 480 ticks (0x83 0x60) later.
            0x00, 0x90, 60, 100,
            0x83, 0x60, 0x80, 60, 0,
            // The end, 480 ticks after that.
            0x83, 0x60, 0xff, 0x2f, 0,
        ];
        assert_eq!(bytes(&song(&tracks, 960)), expected);
    }

    #[test]
    fn songs_read_back_as_they_were_written() {
        let channel = Channel::from_number(10).unwrap();
        let drums = [
            (Ticks(0), note_on(36)),
            (Ticks(0), note_on(42)),
            (Ticks(240), note_off(36)),
            (Ticks(240), note_off(42)),
        ];
        let everything = [
            (
                Ticks(0),
                MidiMessage::ProgramChange {
                    channel,
                    program: U7::new(5).unwrap(),
                },
            ),
            (
                Ticks(10),
                MidiMessage::ControlChange {
                    channel,
                    controller: U7::new(74).unwrap(),
                    value: U7::new(64).unwrap(),
                },
            ),
            (
                Ticks(20),
                MidiMessage::PolyPressure {
                    channel,
                    note: U7::new(60).unwrap(),
                    pressure: U7::new(30).unwrap(),
                },
            ),
            (
                Ticks(30),
                MidiMessage::ChannelPressure {
                    channel,
                    pressure: U7::new(40).unwrap(),
                },
            ),
            (
                Ticks(40),
                MidiMessage::PitchBend {
                    channel,
                    value: U14::new(12_000).unwrap(),
                },
            ),
            // A note-off may land exactly on the end.
            (Ticks(3360), note_off(60)),
        ];
        let tracks = [
            SongTrack {
                name: "drums",
                messages: &drums,
            },
            SongTrack {
                name: "lead ✓",
                messages: &everything,
            },
            SongTrack {
                name: "empty",
                messages: &[],
            },
        ];
        let song = Song {
            title: "jam",
            tempo: Bpm::new(124.0).unwrap(),
            time_signature: TimeSignature::new(7, 8).unwrap(),
            length: Ticks(3360),
            tracks: &tracks,
        };
        let bytes = bytes(&song);
        let smf = Smf::parse(&bytes).unwrap();
        assert_eq!(smf.header.format, Format::Parallel);
        assert_eq!(smf.header.timing, Timing::Metrical(u15::new(960)));

        let midi = |channel: u8, message| TrackEventKind::Midi {
            channel: u4::new(channel),
            message,
        };
        let meta = TrackEventKind::Meta;
        let on = |key: u8| midly::MidiMessage::NoteOn {
            key: u7::new(key),
            vel: u7::new(100),
        };
        let off = |key: u8| midly::MidiMessage::NoteOff {
            key: u7::new(key),
            vel: u7::new(0),
        };
        assert_eq!(
            timeline(&smf),
            [
                vec![
                    (0, meta(MetaMessage::TrackName(b"jam"))),
                    // 7/8: a click every eighth note, 12 MIDI clocks.
                    (0, meta(MetaMessage::TimeSignature(7, 3, 12, 8))),
                    // 60,000,000 / 124, rounded.
                    (0, meta(MetaMessage::Tempo(u24::new(483_871)))),
                    (3360, meta(MetaMessage::EndOfTrack)),
                ],
                vec![
                    (0, meta(MetaMessage::TrackName(b"drums"))),
                    (0, midi(0, on(36))),
                    (0, midi(0, on(42))),
                    (240, midi(0, off(36))),
                    (240, midi(0, off(42))),
                    (3360, meta(MetaMessage::EndOfTrack)),
                ],
                vec![
                    (0, meta(MetaMessage::TrackName("lead ✓".as_bytes()))),
                    (
                        0,
                        midi(
                            9,
                            midly::MidiMessage::ProgramChange {
                                program: u7::new(5)
                            }
                        )
                    ),
                    (
                        10,
                        midi(
                            9,
                            midly::MidiMessage::Controller {
                                controller: u7::new(74),
                                value: u7::new(64),
                            }
                        )
                    ),
                    (
                        20,
                        midi(
                            9,
                            midly::MidiMessage::Aftertouch {
                                key: u7::new(60),
                                vel: u7::new(30),
                            }
                        )
                    ),
                    (
                        30,
                        midi(
                            9,
                            midly::MidiMessage::ChannelAftertouch { vel: u7::new(40) }
                        )
                    ),
                    (
                        40,
                        midi(
                            9,
                            midly::MidiMessage::PitchBend {
                                bend: midly::PitchBend(u14::new(12_000)),
                            }
                        )
                    ),
                    (3360, midi(0, off(60))),
                    (3360, meta(MetaMessage::EndOfTrack)),
                ],
                vec![
                    (0, meta(MetaMessage::TrackName(b"empty"))),
                    (3360, meta(MetaMessage::EndOfTrack)),
                ],
            ]
        );
    }

    #[test]
    fn time_signatures_click_once_a_beat() {
        let cases = [
            (1, 1, 0, 96),
            (3, 2, 1, 48),
            (6, 8, 3, 12),
            (5, 16, 4, 6),
            (32, 32, 5, 3),
        ];
        for (numerator, denominator, power, clocks) in cases {
            let song = Song {
                time_signature: TimeSignature::new(numerator, denominator).unwrap(),
                ..song(&[], 960)
            };
            let bytes = bytes(&song);
            let smf = Smf::parse(&bytes).unwrap();
            assert_eq!(
                smf.tracks[0][1].kind,
                TrackEventKind::Meta(MetaMessage::TimeSignature(numerator, power, clocks, 8)),
                "{numerator}/{denominator}"
            );
        }
    }

    #[test]
    fn tempos_round_to_the_microsecond() {
        for (bpm, micros) in [
            (Bpm::MIN, 3_000_000),
            (120.0, 500_000),
            (133.0, 451_128),
            (Bpm::MAX, 150_000),
        ] {
            let song = Song {
                tempo: Bpm::new(bpm).unwrap(),
                ..song(&[], 960)
            };
            let bytes = bytes(&song);
            let smf = Smf::parse(&bytes).unwrap();
            assert_eq!(
                smf.tracks[0][2].kind,
                TrackEventKind::Meta(MetaMessage::Tempo(u24::new(micros))),
                "{bpm}"
            );
        }
    }

    #[test]
    fn songs_a_file_cant_hold_are_refused_before_anything_is_written() {
        let clock = [(Ticks(0), MidiMessage::TimingClock)];
        let backwards = [(Ticks(480), note_on(60)), (Ticks(240), note_off(60))];
        let late = [(Ticks(961), note_off(60))];
        let cases = [
            (
                &clock[..],
                960,
                "track `a`: a MIDI file can't hold TimingClock",
            ),
            (
                &backwards[..],
                960,
                "track `a`: the message at 240 ticks comes after one at 480 ticks",
            ),
            (
                &late[..],
                960,
                "track `a`: the message at 961 ticks comes after the end, at 960 ticks",
            ),
            (
                &[][..],
                1 << 28,
                "a song 268435456 ticks long is longer than a MIDI file can hold",
            ),
        ];
        for (messages, length, expected) in cases {
            let tracks = [SongTrack {
                name: "a",
                messages,
            }];
            let mut out = Vec::new();
            let error = write(&song(&tracks, length), &mut out).unwrap_err();
            assert_eq!(error.to_string(), expected);
            assert!(out.is_empty(), "{expected}");
        }
        // The longest song a file can hold is fine.
        let longest = (1 << 28) - 1;
        let bytes = bytes(&song(&[], longest));
        let smf = Smf::parse(&bytes).unwrap();
        assert_eq!(smf.tracks[0][3].delta.as_int() as u64, longest);
    }
}
