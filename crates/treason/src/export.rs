//! MIDI file export: `treason export`, and the [`MidiExport`] the MCP
//! server uses. The engine renders the project, and `treason-io` writes the
//! file.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use treason_control::{ExportStore, ProjectName};
use treason_core::export::{self, ExportError, MAX_EVENTS, MidiExport};
use treason_core::midi::MidiMessage;
use treason_core::model::{OutputTarget, Project};
use treason_core::time::Ticks;
use treason_engine::render::render_song;
use treason_io::smf::{self, Song, SongTrack};

use crate::config::Config;

/// Writes projects as Standard MIDI Files.
#[derive(Debug, Clone, Copy, Default)]
pub struct SmfExport;

impl MidiExport for SmfExport {
    fn export(&self, project: &Project, title: &str, bars: u32) -> Result<Vec<u8>, ExportError> {
        let bars = export::bars(project, Some(bars))?;
        let length = Ticks(project.time_signature.bar_length().0 * u64::from(bars));
        let events = render_song(project, length, MAX_EVENTS)
            .map_err(|too_large| ExportError::TooManyEvents(too_large.events))?;

        // A file track for each track that plays, in the project's order.
        let playing: Vec<_> = project.tracks.iter().filter(|track| !track.muted).collect();
        let mut position = HashMap::new();
        let mut first_on_target = HashMap::new();
        for (index, track) in playing.iter().enumerate() {
            position.insert(track.id, index);
            first_on_target.entry(track.target).or_insert(index);
        }
        let mut messages = vec![Vec::new(); playing.len()];
        for event in events {
            // Keep startup channel setup ahead of every track's notes when
            // a player merges file tracks in order. This also carries muted
            // tracks' programs and bend ranges to the first unmuted track
            // on their target.
            let startup_channel = match event.message {
                _ if event.is_setup() && event.tick == Ticks::ZERO => event.message.channel(),
                MidiMessage::ProgramChange { channel, .. } if event.tick == Ticks::ZERO => {
                    Some(channel)
                }
                MidiMessage::ControlChange {
                    channel,
                    controller,
                    ..
                } if event.tick == Ticks::ZERO && matches!(controller.get(), 0 | 32) => {
                    Some(channel)
                }
                _ => None,
            };
            let index = match startup_channel {
                Some(channel) => first_on_target.get(&OutputTarget {
                    port: event.port,
                    channel,
                }),
                None => event.track.and_then(|id| position.get(&id)),
            };
            if let Some(&index) = index {
                messages[index].push((event.tick, event.message));
            }
        }
        let tracks: Vec<_> = playing
            .iter()
            .zip(&messages)
            .map(|(track, messages)| SongTrack {
                name: &track.name,
                messages,
            })
            .collect();
        let song = Song {
            title,
            tempo: project.tempo,
            time_signature: project.time_signature,
            length,
            tracks: &tracks,
        };
        let mut file = Vec::new();
        smf::write(&song, &mut file).map_err(|error| ExportError::Write(error.to_string()))?;
        Ok(file)
    }
}

/// Options for `treason export`.
#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    /// The saved project.
    #[arg(value_name = "PROJECT")]
    project: ProjectName,
    /// How many bars to export, from bar 1 [default: until every track's
    /// loop ends on the same bar line]
    #[arg(long, value_name = "N")]
    bars: Option<u32>,
    /// Where to write the MIDI file [default: PROJECT.mid in the exports
    /// directory]
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
}

/// Exports a saved project, replacing any earlier export of it, or the
/// file `--output` names.
pub fn run(config: &Config, args: &ExportArgs) -> anyhow::Result<()> {
    let name = &args.project;
    let devices = config.load_devices()?;
    let projects = config.project_store();
    let project = projects
        .load(name, &config.port_names(), &devices)
        .with_context(|| format!("loading project `{name}` from {}", projects.dir().display()))?;
    let bars = export::bars(&project, args.bars)?;
    let file = SmfExport.export(&project, name.as_str(), bars)?;
    let path = match &args.output {
        Some(path) => {
            ExportStore::save_to(path, &file)
                .with_context(|| format!("writing {}", path.display()))?;
            path.clone()
        }
        None => config.export_store().save(name, &file, true)?,
    };
    let unit = if bars == 1 { "bar" } else { "bars" };
    println!("Exported {bars} {unit} of `{name}` to {}.", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::Path;
    use std::sync::Arc;

    use midly::num::u15;
    use midly::{Format, MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
    use treason_core::midi::{Channel, U7, U14};
    use treason_core::model::{
        Clip, ClipEvent, DEFAULT_RELEASE, EventKind, OutputTarget, PortId, Program, Track, TrackId,
    };
    use treason_core::time::Bpm;

    use super::*;

    fn track(id: u32, name: &str, channel: u8, length: u64, notes: &[(u64, u8, u64)]) -> Track {
        let events = notes
            .iter()
            .map(|&(start, note, length)| ClipEvent {
                start: Ticks(start),
                kind: EventKind::Note {
                    note: U7::new(note).unwrap(),
                    velocity: U7::new(100).unwrap(),
                    length: Ticks(length),
                    release: DEFAULT_RELEASE,
                },
            })
            .collect();
        Track {
            id: TrackId(id),
            name: name.into(),
            device: None,
            target: OutputTarget {
                port: PortId(0),
                channel: Channel::from_number(channel).unwrap(),
            },
            muted: false,
            program: None,
            bend_range: None,
            clip: Clip::new(Ticks(length), events).unwrap(),
        }
    }

    #[test]
    fn programs_and_bend_ranges_go_first_on_the_tracks_that_play_on_their_channel() {
        let program = |number| Some(Program::new(U7::new(number).unwrap()));
        let with = |program, muted, track: Track| Track {
            program,
            muted,
            ..track
        };
        let ranged = |semitones, track: Track| Track {
            bend_range: U7::new(semitones),
            ..track
        };
        let project = Project {
            tracks: [
                with(
                    program(25),
                    false,
                    track(1, "drums", 10, 3840, &[(0, 36, 240)]),
                ),
                // Muted, so not in the file, but its program and bend range
                // set channel 2 for the sub, which leaves them to it.
                ranged(
                    12,
                    with(
                        program(38),
                        true,
                        track(2, "bass", 2, 3840, &[(0, 33, 240)]),
                    ),
                ),
                with(None, false, track(3, "sub", 2, 3840, &[(0, 21, 240)])),
                // Nothing plays on channel 3 in the file.
                ranged(
                    2,
                    with(
                        program(80),
                        true,
                        track(4, "lead", 3, 3840, &[(0, 60, 240)]),
                    ),
                ),
            ]
            .into_iter()
            .map(Arc::new)
            .collect(),
            ..Project::default()
        };
        let file = SmfExport.export(&project, "jam", 1).unwrap();
        assert_eq!(
            tracks(&file)[1..],
            [
                vec![
                    "0 name drums",
                    "0 pc 25 ch10",
                    "0 on 36 ch10",
                    "240 off 36 ch10",
                    "3840 end"
                ],
                vec![
                    "0 name sub",
                    "0 pc 38 ch2",
                    // RPN 0, the bend range, then RPN null.
                    "0 cc101 0 ch2",
                    "0 cc100 0 ch2",
                    "0 cc6 12 ch2",
                    "0 cc38 0 ch2",
                    "0 cc101 127 ch2",
                    "0 cc100 127 ch2",
                    "0 on 21 ch2",
                    "240 off 21 ch2",
                    "3840 end"
                ],
            ]
        );
    }

    #[test]
    fn clip_programs_keep_their_restored_range_in_the_export() {
        let first = track(1, "first", 1, 3840, &[(0, 60, 240)]);
        let mut setter = track(2, "setter", 1, 960, &[]);
        setter.bend_range = U7::new(12);
        setter.clip = Clip::new(
            Ticks(960),
            vec![ClipEvent {
                start: Ticks::ZERO,
                kind: EventKind::Program {
                    program: U7::new(5).unwrap(),
                },
            }],
        )
        .unwrap();
        let project = Project {
            tracks: [first, setter].map(Arc::new).into(),
            ..Project::default()
        };
        let file = SmfExport.export(&project, "shared", 1).unwrap();
        let range = |at| {
            [(101, 0), (100, 0), (6, 12), (38, 0), (101, 127), (100, 127)]
                .map(|(cc, value)| format!("{at} cc{cc} {value} ch1"))
        };
        let mut first = vec!["0 name first".to_owned()];
        first.extend(range(0));
        first.push("0 pc 5 ch1".to_owned());
        first.extend(range(0));
        first.extend(["0 on 60 ch1", "240 off 60 ch1", "3840 end"].map(String::from));
        let mut setter = vec!["0 name setter".to_owned()];
        for at in [960, 1920, 2880] {
            setter.push(format!("{at} pc 5 ch1"));
            setter.extend(range(at));
        }
        setter.push("3840 end".to_owned());
        assert_eq!(tracks(&file)[1..], [first, setter]);
    }

    #[test]
    fn later_unmuted_setters_put_startup_programs_before_shared_channel_notes() {
        for (msb, lsb) in [
            (None, None),
            (Some(1), None),
            (None, Some(2)),
            (Some(1), Some(2)),
        ] {
            for populated in [false, true] {
                let mut other_port = track(1, "other port", 2, 3840, &[(0, 48, 240)]);
                other_port.target.port = PortId(1);
                let first = track(2, "first", 2, 3840, &[(0, 21, 240)]);
                let notes = if populated { &[(0, 33, 240)][..] } else { &[] };
                let mut setter = track(3, "setter", 2, 3840, notes);
                setter.program = Some(Program {
                    number: U7::new(33).unwrap(),
                    bank_msb: msb.map(|value| U7::new(value).unwrap()),
                    bank_lsb: lsb.map(|value| U7::new(value).unwrap()),
                });
                if populated {
                    let mut events = setter.clip.events().to_vec();
                    events.extend([
                        ClipEvent {
                            start: Ticks::ZERO,
                            kind: EventKind::Control {
                                controller: U7::new(1).unwrap(),
                                value: U7::new(64).unwrap(),
                            },
                        },
                        ClipEvent {
                            start: Ticks(960),
                            kind: EventKind::Program {
                                program: U7::new(34).unwrap(),
                            },
                        },
                    ]);
                    setter.clip = Clip::new(Ticks(3840), events).unwrap();
                }
                let project = Project {
                    tracks: [other_port, first, setter].map(Arc::new).into(),
                    ..Project::default()
                };
                let file = SmfExport.export(&project, "shared", 1).unwrap();
                let mut first = vec!["0 name first"];
                if msb.is_some() {
                    first.push("0 cc0 1 ch2");
                }
                if lsb.is_some() {
                    first.push("0 cc32 2 ch2");
                }
                first.extend(["0 pc 33 ch2", "0 on 21 ch2", "240 off 21 ch2", "3840 end"]);
                let setter = if populated {
                    vec![
                        "0 name setter",
                        "0 cc1 64 ch2",
                        "0 on 33 ch2",
                        "240 off 33 ch2",
                        "960 pc 34 ch2",
                        "3840 end",
                    ]
                } else {
                    vec!["0 name setter", "3840 end"]
                };
                assert_eq!(
                    tracks(&file)[1..],
                    [
                        vec![
                            "0 name other port",
                            "0 on 48 ch2",
                            "240 off 48 ch2",
                            "3840 end",
                        ],
                        first,
                        setter,
                    ],
                    "banks {msb:?}/{lsb:?}, populated {populated}",
                );
            }
        }
    }

    /// A kick every beat, a muted bass, and a lead note held over the bar
    /// line.
    fn jam() -> Project {
        let mut bass = track(2, "bass", 2, 3840, &[(0, 33, 960)]);
        bass.muted = true;
        Project {
            tempo: Bpm::new(124.0).unwrap(),
            tracks: [
                track(1, "drums", 10, 960, &[(0, 36, 240)]),
                bass,
                track(3, "lead", 1, 3840, &[(2880, 60, 1920)]),
            ]
            .into_iter()
            .map(Arc::new)
            .collect(),
            ..Project::default()
        }
    }

    /// Each track in `file`, one line per event, such as "240 off 36 ch10".
    fn tracks(file: &[u8]) -> Vec<Vec<String>> {
        let smf = Smf::parse(file).unwrap();
        assert_eq!(smf.header.format, Format::Parallel);
        assert_eq!(smf.header.timing, Timing::Metrical(u15::new(960)));
        smf.tracks
            .iter()
            .map(|track| {
                let mut tick = 0;
                track
                    .iter()
                    .map(|event| {
                        tick += event.delta.as_int();
                        let what = match event.kind {
                            TrackEventKind::Midi {
                                channel,
                                message: MidiMessage::NoteOn { key, .. },
                            } => format!("on {key} ch{}", channel.as_int() + 1),
                            TrackEventKind::Midi {
                                channel,
                                message: MidiMessage::NoteOff { key, .. },
                            } => format!("off {key} ch{}", channel.as_int() + 1),
                            TrackEventKind::Midi {
                                channel,
                                message: MidiMessage::Controller { controller, value },
                            } => format!("cc{controller} {value} ch{}", channel.as_int() + 1),
                            TrackEventKind::Midi {
                                channel,
                                message: MidiMessage::ProgramChange { program },
                            } => format!("pc {program} ch{}", channel.as_int() + 1),
                            TrackEventKind::Midi {
                                channel,
                                message: MidiMessage::PitchBend { bend },
                            } => format!("bend {} ch{}", bend.0, channel.as_int() + 1),
                            TrackEventKind::Midi {
                                channel,
                                message: MidiMessage::ChannelAftertouch { vel },
                            } => format!("pressure {vel} ch{}", channel.as_int() + 1),
                            TrackEventKind::Meta(MetaMessage::TrackName(name)) => {
                                format!("name {}", String::from_utf8_lossy(name))
                            }
                            TrackEventKind::Meta(MetaMessage::TimeSignature(n, d, ..)) => {
                                format!("time {n}/{}", 1 << d)
                            }
                            TrackEventKind::Meta(MetaMessage::Tempo(micros)) => {
                                format!("tempo {micros}")
                            }
                            TrackEventKind::Meta(MetaMessage::EndOfTrack) => "end".into(),
                            other => format!("{other:?}"),
                        };
                        format!("{tick} {what}")
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn files_end_with_bends_centred_and_pressure_lifted() {
        let mut lead = track(1, "lead", 1, 3840, &[(0, 60, 960)]);
        let mut events = lead.clip.events().to_vec();
        events.extend([
            ClipEvent {
                start: Ticks(480),
                kind: EventKind::PitchBend {
                    value: U14::new(0).unwrap(),
                },
            },
            ClipEvent {
                start: Ticks(480),
                kind: EventKind::Pressure {
                    value: U7::new(90).unwrap(),
                },
            },
        ]);
        lead.clip = Clip::new(Ticks(3840), events).unwrap();
        let project = Project {
            tracks: vec![Arc::new(lead)],
            ..Project::default()
        };
        let file = SmfExport.export(&project, "bent", 1).unwrap();
        assert_eq!(
            tracks(&file)[1],
            [
                "0 name lead",
                "0 on 60 ch1",
                "480 bend 0 ch1",
                "480 pressure 90 ch1",
                "960 off 60 ch1",
                "3840 bend 8192 ch1",
                "3840 pressure 0 ch1",
                "3840 end",
            ]
        );
    }

    #[test]
    fn exports_hold_each_unmuted_track_as_it_plays() {
        let file = SmfExport.export(&jam(), "jam", 1).unwrap();
        assert_eq!(
            tracks(&file),
            [
                vec!["0 name jam", "0 time 4/4", "0 tempo 483871", "3840 end"],
                vec![
                    "0 name drums",
                    "0 on 36 ch10",
                    "240 off 36 ch10",
                    "960 on 36 ch10",
                    "1200 off 36 ch10",
                    "1920 on 36 ch10",
                    "2160 off 36 ch10",
                    "2880 on 36 ch10",
                    "3120 off 36 ch10",
                    "3840 end",
                ],
                // Held over the end, the lead note stops there.
                vec![
                    "0 name lead",
                    "2880 on 60 ch1",
                    "3840 off 60 ch1",
                    "3840 end"
                ],
            ]
        );
        // With a second bar, it ends on time and starts again.
        let file = SmfExport.export(&jam(), "jam", 2).unwrap();
        assert_eq!(
            tracks(&file)[2],
            [
                "0 name lead",
                "2880 on 60 ch1",
                "4800 off 60 ch1",
                "6720 on 60 ch1",
                "7680 off 60 ch1",
                "7680 end",
            ]
        );
    }

    #[test]
    fn tracks_of_controls_alone_export_their_control_changes() {
        let control = |start, value| ClipEvent {
            start: Ticks(start),
            kind: EventKind::Control {
                controller: U7::new(1).unwrap(),
                value: U7::new(value).unwrap(),
            },
        };
        let mut modulation = track(1, "modulation", 3, 3840, &[]);
        modulation.clip = Clip::new(Ticks(3840), vec![control(0, 0), control(1920, 127)]).unwrap();
        let project = Project {
            tracks: vec![Arc::new(modulation)],
            ..Project::default()
        };
        assert_eq!(export::bars(&project, None), Ok(1));
        let file = SmfExport.export(&project, "wheel", 2).unwrap();
        assert_eq!(
            tracks(&file)[1],
            [
                "0 name modulation",
                "0 cc1 0 ch3",
                "1920 cc1 127 ch3",
                "3840 cc1 0 ch3",
                "5760 cc1 127 ch3",
                "7680 end",
            ]
        );
    }

    #[test]
    fn exports_release_pedal_holds_even_after_the_normal_note_off() {
        for pedal in [64, 66] {
            for note_length in [960, 5760] {
                let mut synth = track(1, "synth", 3, 7680, &[(0, 60, note_length)]);
                let mut events = synth.clip.events().to_vec();
                // Sostenuto captures keys already down. Sustain can be
                // pressed before the note starts.
                let pedal_at = if pedal == 64 { 0 } else { 240 };
                for (at, value) in [(pedal_at, 127), (6720, 0)] {
                    events.push(ClipEvent {
                        start: Ticks(at),
                        kind: EventKind::Control {
                            controller: U7::new(pedal).unwrap(),
                            value: U7::new(value).unwrap(),
                        },
                    });
                }
                synth.clip = Clip::new(Ticks(7680), events).unwrap();
                let project = Project {
                    tracks: vec![Arc::new(synth)],
                    ..Project::default()
                };
                // Export only the first bar: the authored pedal-off is
                // outside the file, and the short note's key is up well
                // before the cutoff, while the pedal still holds it.
                let file = SmfExport.export(&project, "pedals", 1).unwrap();
                let mut expected = vec!["0 name synth".to_owned()];
                if pedal == 64 {
                    expected.extend(["0 cc64 127 ch3".into(), "0 on 60 ch3".into()]);
                } else {
                    expected.extend(["0 on 60 ch3".into(), "240 cc66 127 ch3".into()]);
                }
                expected.extend([
                    format!("{} off 60 ch3", note_length.min(3840)),
                    format!("3840 cc{pedal} 0 ch3"),
                    "3840 end".into(),
                ]);
                assert_eq!(
                    tracks(&file)[1],
                    expected,
                    "pedal {pedal}, note {note_length}"
                );
            }
        }
    }

    #[test]
    fn partial_control_updates_preserve_the_rendered_rpn_sequence() {
        let mut synth = track(1, "synth", 3, 3840, &[]);
        synth.clip = Clip::new(
            Ticks(3840),
            [(101, 0), (100, 0), (6, 12)]
                .map(|(controller, value)| ClipEvent {
                    start: Ticks::ZERO,
                    kind: EventKind::Control {
                        controller: U7::new(controller).unwrap(),
                        value: U7::new(value).unwrap(),
                    },
                })
                .to_vec(),
        )
        .unwrap();
        for controllers in [vec![100], vec![100, 101]] {
            let controls = controllers
                .into_iter()
                .map(|controller| {
                    (
                        controller.to_string(),
                        treason_mcp::dto::ControlLane {
                            points: vec![treason_mcp::dto::ControlPoint {
                                at: 0.0,
                                value: if controller == 100 { 2 } else { 0 },
                                ramp: None,
                            }],
                            repeat_beats: None,
                        },
                    )
                })
                .collect();
            let mut updated = synth.clone();
            updated.clip = treason_mcp::controls::clip_with_controls(
                &synth.clip,
                &controls,
                Some(4.0),
                Ticks(3840),
                None,
            )
            .unwrap();
            let project = Project {
                tracks: vec![Arc::new(updated)],
                ..Project::default()
            };
            // The complete file stream goes through the live renderer;
            // checking each controller separately would miss the bug.
            let file = SmfExport.export(&project, "rpn", 1).unwrap();
            assert_eq!(
                tracks(&file)[1],
                [
                    "0 name synth",
                    "0 cc101 0 ch3",
                    "0 cc100 2 ch3",
                    "0 cc6 12 ch3",
                    "3840 end",
                ]
            );
        }
    }

    #[test]
    fn exports_too_big_are_refused_before_rendering() {
        // A note on every tick: 7,680 note-ons and note-offs a bar.
        let project = Project {
            tracks: vec![Arc::new(track(1, "buzz", 1, 1, &[(0, 60, 1)]))],
            ..Project::default()
        };
        let export = |bars| SmfExport.export(&project, "buzz", bars);
        assert_eq!(export(1000), Err(ExportError::TooManyEvents(7_680_000)));
        assert_eq!(export(0), Err(ExportError::NoBars));
        assert_eq!(export(1001), Err(ExportError::TooManyBars(1001)));
        assert_eq!(tracks(&export(1).unwrap())[1].len(), 7682);
    }

    #[test]
    fn the_export_command_writes_a_saved_project() {
        let dir = std::env::temp_dir().join(format!("treason-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = Config {
            devices: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../devices"),
            projects: dir.join("projects"),
            exports: dir.join("exports"),
            ..Config::default()
        };
        let name: ProjectName = "jam".parse().unwrap();
        let args = |bars, output| ExportArgs {
            project: name.clone(),
            bars,
            output,
        };
        let error = run(&config, &args(None, None)).unwrap_err();
        assert!(
            format!("{error:#}").contains("no project `jam`"),
            "{error:#}"
        );

        config
            .project_store()
            .save(&name, &jam(), &config.port_names(), false)
            .unwrap();
        // Every loop ends with the first bar, and a second export replaces
        // the first.
        for _ in 0..2 {
            run(&config, &args(None, None)).unwrap();
        }
        let file = std::fs::read(dir.join("exports").join("jam.mid")).unwrap();
        assert_eq!(file, SmfExport.export(&jam(), "jam", 1).unwrap());

        let output = dir.join("elsewhere.mid");
        run(&config, &args(Some(2), Some(output.clone()))).unwrap();
        assert_eq!(tracks(&std::fs::read(&output).unwrap())[0][3], "7680 end");

        // Replacing --output publishes a new file. A reader of the earlier
        // export still sees its complete two bars.
        let mut previous = std::fs::File::open(&output).unwrap();
        run(&config, &args(Some(1), Some(output.clone()))).unwrap();
        let mut before = Vec::new();
        previous.read_to_end(&mut before).unwrap();
        assert_eq!(tracks(&before)[0][3], "7680 end");
        assert_eq!(tracks(&std::fs::read(&output).unwrap())[0][3], "3840 end");
        drop(previous);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
