//! MIDI file export: `treason export`, and the [`MidiExport`] the MCP
//! server uses. The engine renders the project, and `treason-io` writes the
//! file.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use treason_control::{ExportStore, ProjectName};
use treason_core::export::{self, ExportError, MAX_EVENTS, MidiExport};
use treason_core::model::Project;
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
        let position: HashMap<_, _> = playing
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id, index))
            .collect();
        let mut messages = vec![Vec::new(); playing.len()];
        for event in events {
            if let Some(&index) = event.track.and_then(|id| position.get(&id)) {
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
    use treason_core::midi::{Channel, U7};
    use treason_core::model::{Clip, ClipEvent, EventKind, OutputTarget, PortId, Track, TrackId};
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
            clip: Clip::new(Ticks(length), events).unwrap(),
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
