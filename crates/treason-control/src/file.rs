//! Project files: a project as TOML.
//!
//! The format is treason's own rather than the core model's serde form, so
//! the model can change without breaking saved projects, and each file says
//! which version of the format it uses. Tracks name their port, because a
//! port's number is its place in `treason.toml`, which can change between
//! saving and loading. Times are whole ticks, so a project reads back
//! exactly as it was written, and events are written one to a line, in the
//! clip's order.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml::ser::ValueSerializer;
use treason_core::midi::{Channel, U7};
use treason_core::model::{
    Clip, ClipError, ClipEvent, EventKind, OutputTarget, PortId, Program, Project, Track, TrackId,
};
use treason_core::time::{Bpm, ParseTimeSignatureError, Ticks};
use treason_devices::DeviceRegistry;

/// The version of the format this treason writes, and the only one it reads.
pub const FORMAT: u32 = 1;

/// The first line of every file.
const HEADER: &str = "# A treason project. Times are in ticks, 960 to a quarter note.\n";

/// Why a project couldn't be written or read.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum FileError {
    /// The text isn't TOML, or not the fields a project has.
    #[error("{0}")]
    Parse(String),
    /// The file doesn't say which format it uses.
    #[error("it has no `format` line, so it isn't a treason project")]
    NoFormat,
    /// The file uses a format this treason doesn't read.
    #[error("it uses format {0}, and this version of treason reads only format {FORMAT}")]
    Format(u32),
    /// The time signature doesn't parse.
    #[error(transparent)]
    TimeSignature(#[from] ParseTimeSignatureError),
    /// Two tracks have the same id.
    #[error("there are two tracks {0}")]
    DuplicateTrack(TrackId),
    /// A track plays on a port the configuration doesn't have.
    #[error("track {track} plays on port `{port}`, which treason.toml doesn't define")]
    UnknownPort {
        /// The track.
        track: TrackId,
        /// The port it names.
        port: String,
    },
    /// A track uses a device that has no profile.
    #[error("track {track} uses device `{device}`, which has no profile")]
    UnknownDevice {
        /// The track.
        track: TrackId,
        /// The device it names.
        device: String,
    },
    /// A track selects a bank but no program, which bank select needs.
    #[error("track {0} has a bank but no program")]
    BankWithoutProgram(TrackId),
    /// Two tracks set different programs on the same port and channel.
    #[error("tracks {0} and {1} set different programs on the same port and channel")]
    ProgramClash(TrackId, TrackId),
    /// A track's clip is invalid.
    #[error("track {track}: {error}")]
    Clip {
        /// The track.
        track: TrackId,
        /// What is wrong with its clip.
        error: ClipError,
    },
    /// Writing: a track plays on a port with no name.
    #[error("track {track} plays on port {port}, which has no name")]
    UnnamedPort {
        /// The track.
        track: TrackId,
        /// Its port.
        port: PortId,
    },
    /// Writing: the TOML writer refused a value.
    #[error("TOML can't hold the project: {0}")]
    Write(String),
}

/// A project file, but for its tracks, which [`write`] writes one by one.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    format: u32,
    tempo: Bpm,
    time_signature: String,
    #[serde(default, skip_serializing)]
    tracks: Vec<TrackFile>,
}

/// A track, but for its events, which [`write`] writes one to a line.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackFile {
    id: TrackId,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    port: String,
    channel: Channel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    program: Option<U7>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bank_msb: Option<U7>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bank_lsb: Option<U7>,
    #[serde(default)]
    muted: bool,
    loop_length: Ticks,
    #[serde(default, skip_serializing)]
    events: Vec<EventFile>,
}

/// A clip event.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum EventFile {
    Note {
        start: Ticks,
        note: U7,
        velocity: U7,
        length: Ticks,
    },
    Control {
        start: Ticks,
        controller: U7,
        value: U7,
    },
    Program {
        start: Ticks,
        program: U7,
    },
}

impl From<&ClipEvent> for EventFile {
    fn from(event: &ClipEvent) -> Self {
        let start = event.start;
        match event.kind {
            EventKind::Note {
                note,
                velocity,
                length,
            } => Self::Note {
                start,
                note,
                velocity,
                length,
            },
            EventKind::Control { controller, value } => Self::Control {
                start,
                controller,
                value,
            },
            EventKind::Program { program } => Self::Program { start, program },
        }
    }
}

impl From<EventFile> for ClipEvent {
    fn from(event: EventFile) -> Self {
        match event {
            EventFile::Note {
                start,
                note,
                velocity,
                length,
            } => Self {
                start,
                kind: EventKind::Note {
                    note,
                    velocity,
                    length,
                },
            },
            EventFile::Control {
                start,
                controller,
                value,
            } => Self {
                start,
                kind: EventKind::Control { controller, value },
            },
            EventFile::Program { start, program } => Self {
                start,
                kind: EventKind::Program { program },
            },
        }
    }
}

/// `project` as the text of a project file. `ports` names the output ports,
/// in `PortId` order.
pub fn write(project: &Project, ports: &[String]) -> Result<String, FileError> {
    let mut text = String::from(HEADER);
    let file = ProjectFile {
        format: FORMAT,
        tempo: project.tempo,
        time_signature: project.time_signature.to_string(),
        tracks: Vec::new(),
    };
    text.push_str(&toml::to_string(&file).map_err(write_error)?);
    for track in &project.tracks {
        let port = ports
            .get(usize::from(track.target.port.0))
            .ok_or(FileError::UnnamedPort {
                track: track.id,
                port: track.target.port,
            })?;
        let file = TrackFile {
            id: track.id,
            name: track.name.clone(),
            device: track.device.clone(),
            port: port.clone(),
            channel: track.target.channel,
            program: track.program.map(|program| program.number),
            bank_msb: track.program.and_then(|program| program.bank_msb),
            bank_lsb: track.program.and_then(|program| program.bank_lsb),
            muted: track.muted,
            loop_length: track.clip.length(),
            events: Vec::new(),
        };
        text.push_str("\n[[tracks]]\n");
        text.push_str(&toml::to_string(&file).map_err(write_error)?);
        let events = track.clip.events();
        if events.is_empty() {
            text.push_str("events = []\n");
            continue;
        }
        text.push_str("events = [\n");
        for event in events {
            text.push_str("    ");
            EventFile::from(event)
                .serialize(ValueSerializer::new(&mut text))
                .map_err(write_error)?;
            text.push_str(",\n");
        }
        text.push_str("]\n");
    }
    Ok(text)
}

/// Reads a project from the text of a project file. `ports` names the
/// output ports, in `PortId` order, and every device a track names must have
/// a profile in `devices`.
pub fn read(text: &str, ports: &[String], devices: &DeviceRegistry) -> Result<Project, FileError> {
    // The version comes first, so that a newer file is reported as one
    // rather than as fields this version doesn't know.
    #[derive(Deserialize)]
    struct Version {
        format: Option<u32>,
    }
    let Version { format } = toml::from_str(text).map_err(parse_error)?;
    match format {
        Some(FORMAT) => {}
        Some(format) => return Err(FileError::Format(format)),
        None => return Err(FileError::NoFormat),
    }
    let file: ProjectFile = toml::from_str(text).map_err(parse_error)?;
    let time_signature = file.time_signature.parse()?;
    let mut ids = BTreeSet::new();
    let tracks = file
        .tracks
        .into_iter()
        .map(|track| {
            if !ids.insert(track.id) {
                return Err(FileError::DuplicateTrack(track.id));
            }
            read_track(track, ports, devices).map(Arc::new)
        })
        .collect::<Result<_, _>>()?;
    let project = Project {
        tempo: file.tempo,
        time_signature,
        tracks,
    };
    if let Some((first, second)) = project.program_clash() {
        return Err(FileError::ProgramClash(first, second));
    }
    Ok(project)
}

fn read_track(
    track: TrackFile,
    ports: &[String],
    devices: &DeviceRegistry,
) -> Result<Track, FileError> {
    let port = ports
        .iter()
        .position(|port| *port == track.port)
        .and_then(|index| u16::try_from(index).ok())
        .ok_or_else(|| FileError::UnknownPort {
            track: track.id,
            port: track.port.clone(),
        })?;
    if let Some(device) = &track.device
        && devices.get(device).is_none()
    {
        return Err(FileError::UnknownDevice {
            track: track.id,
            device: device.clone(),
        });
    }
    let program = match track.program {
        Some(number) => Some(Program {
            number,
            bank_msb: track.bank_msb,
            bank_lsb: track.bank_lsb,
        }),
        None if track.bank_msb.is_some() || track.bank_lsb.is_some() => {
            return Err(FileError::BankWithoutProgram(track.id));
        }
        None => None,
    };
    let events = track.events.into_iter().map(ClipEvent::from).collect();
    let clip = Clip::new(track.loop_length, events).map_err(|error| FileError::Clip {
        track: track.id,
        error,
    })?;
    Ok(Track {
        id: track.id,
        name: track.name,
        device: track.device,
        target: OutputTarget {
            port: PortId(port),
            channel: track.channel,
        },
        muted: track.muted,
        program,
        clip,
    })
}

fn parse_error(error: toml::de::Error) -> FileError {
    FileError::Parse(error.to_string())
}

fn write_error(error: toml::ser::Error) -> FileError {
    FileError::Write(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use treason_core::time::TimeSignature;
    use treason_devices::DeviceProfile;

    use super::*;

    fn u7(value: u8) -> U7 {
        U7::new(value).unwrap()
    }

    fn note(start: u64, note: u8, velocity: u8, length: u64) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Note {
                note: u7(note),
                velocity: u7(velocity),
                length: Ticks(length),
            },
        }
    }

    fn control(start: u64, controller: u8, value: u8) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Control {
                controller: u7(controller),
                value: u7(value),
            },
        }
    }

    fn program(start: u64, program: u8) -> ClipEvent {
        ClipEvent {
            start: Ticks(start),
            kind: EventKind::Program {
                program: u7(program),
            },
        }
    }

    fn track(id: u32, port: u16, events: Vec<ClipEvent>) -> Track {
        Track {
            id: TrackId(id),
            name: format!("track {id}"),
            device: None,
            target: OutputTarget {
                port: PortId(port),
                channel: Channel::DRUMS,
            },
            muted: false,
            program: None,
            clip: Clip::new(Ticks::quarters(4), events).unwrap(),
        }
    }

    fn devices() -> DeviceRegistry {
        let mut devices = DeviceRegistry::new();
        devices.insert(DeviceProfile {
            id: "kit".into(),
            name: "Kit".into(),
            manufacturer: None,
            default_channel: Channel::DRUMS,
            voices: BTreeMap::from([("kick".into(), u7(36))]),
            aliases: BTreeMap::new(),
            programs: BTreeMap::new(),
            params: BTreeMap::new(),
        });
        devices
    }

    fn ports() -> Vec<String> {
        vec!["out".into(), "synth".into()]
    }

    /// A project with every kind of event and setting a file holds.
    fn everything() -> Project {
        let mut drums = track(
            3,
            0,
            vec![
                note(0, 36, 127, 240),
                note(960, 38, 100, 240),
                note(0, 42, 64, 120),
            ],
        );
        drums.name = "drums \"909\" \\ ünïcode\nsecond line".into();
        drums.device = Some("kit".into());
        let mut bass = track(
            1,
            1,
            // A bank select and program change, then an RPN sequence, all on
            // one tick, and a note across the loop point.
            vec![
                control(0, 0, 1),
                control(0, 32, 0),
                program(0, 81),
                control(0, 101, 0),
                control(0, 100, 0),
                control(0, 6, 12),
                note(3600, 33, 90, 480),
            ],
        );
        bass.target.channel = Channel::from_number(2).unwrap();
        bass.muted = true;
        bass.program = Some(Program {
            number: u7(33),
            bank_msb: Some(u7(121)),
            bank_lsb: Some(u7(0)),
        });
        drums.program = Some(Program::new(u7(25)));
        let mut empty = track(7, 0, vec![]);
        empty.clip = Clip::new(Ticks(3360), vec![]).unwrap();
        Project {
            // A tempo whose shortest decimal form has 17 digits.
            tempo: Bpm::new(400.0 / 3.0).unwrap(),
            time_signature: TimeSignature::new(7, 8).unwrap(),
            tracks: vec![Arc::new(drums), Arc::new(bass), Arc::new(empty)],
        }
    }

    #[test]
    fn a_project_reads_back_exactly_as_it_was_written() {
        let project = everything();
        let text = write(&project, &ports()).unwrap();
        assert_eq!(read(&text, &ports(), &devices()), Ok(project));
    }

    #[test]
    fn events_on_the_same_tick_keep_their_order() {
        let project = everything();
        let text = write(&project, &ports()).unwrap();
        let read = read(&text, &ports(), &devices()).unwrap();
        let controllers: Vec<u8> = read.tracks[1]
            .clip
            .events()
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::Control { controller, .. } => Some(controller.get()),
                _ => None,
            })
            .collect();
        assert_eq!(controllers, [0, 32, 101, 100, 6]);
    }

    #[test]
    fn files_look_like_this() {
        let mut drums = track(1, 0, vec![note(0, 36, 100, 240), control(480, 74, 64)]);
        drums.name = "drums".into();
        drums.device = Some("kit".into());
        let mut lead = track(2, 1, vec![program(0, 5)]);
        lead.name = "lead".into();
        lead.target.channel = Channel::from_number(1).unwrap();
        lead.muted = true;
        lead.program = Some(Program {
            number: u7(80),
            bank_msb: None,
            bank_lsb: Some(u7(2)),
        });
        let project = Project {
            tempo: Bpm::new(124.0).unwrap(),
            tracks: vec![Arc::new(drums), Arc::new(lead)],
            ..Project::default()
        };
        let mut silent = track(3, 0, vec![]);
        silent.name = "silent".into();
        let mut with_empty = project.clone();
        with_empty.tracks.push(Arc::new(silent));
        assert_eq!(
            write(&with_empty, &ports()).unwrap(),
            r#"# A treason project. Times are in ticks, 960 to a quarter note.
format = 1
tempo = 124.0
time_signature = "4/4"

[[tracks]]
id = 1
name = "drums"
device = "kit"
port = "out"
channel = 10
muted = false
loop_length = 3840
events = [
    { type = "note", start = 0, note = 36, velocity = 100, length = 240 },
    { type = "control", start = 480, controller = 74, value = 64 },
]

[[tracks]]
id = 2
name = "lead"
port = "synth"
channel = 1
program = 80
bank_lsb = 2
muted = true
loop_length = 3840
events = [
    { type = "program", start = 0, program = 5 },
]

[[tracks]]
id = 3
name = "silent"
port = "out"
channel = 10
muted = false
loop_length = 3840
events = []
"#
        );
    }

    #[test]
    fn tracks_find_their_port_by_name() {
        let project = everything();
        let text = write(&project, &ports()).unwrap();
        let reordered = vec!["synth".to_owned(), "out".to_owned()];
        let read = read(&text, &reordered, &devices()).unwrap();
        let ports: Vec<u16> = read
            .tracks
            .iter()
            .map(|track| track.target.port.0)
            .collect();
        assert_eq!(ports, [1, 0, 1]);

        let error = super::read(&text, &["out".to_owned()], &devices()).unwrap_err();
        assert_eq!(
            error,
            FileError::UnknownPort {
                track: TrackId(1),
                port: "synth".into()
            }
        );
    }

    #[test]
    fn a_track_on_a_port_without_a_name_cant_be_written() {
        let project = Project {
            tracks: vec![Arc::new(track(1, 2, vec![]))],
            ..Project::default()
        };
        assert_eq!(
            write(&project, &ports()),
            Err(FileError::UnnamedPort {
                track: TrackId(1),
                port: PortId(2)
            })
        );
    }

    #[test]
    fn the_longest_times_read_back() {
        let mut long = track(u32::MAX, 0, vec![]);
        long.clip = Clip::new(Ticks(u64::MAX), vec![note(u64::MAX - 1, 127, 127, 1)]).unwrap();
        let project = Project {
            tracks: vec![Arc::new(long)],
            ..Project::default()
        };
        let text = write(&project, &ports()).unwrap();
        assert_eq!(read(&text, &ports(), &devices()), Ok(project));
    }

    /// A one-track project file with `track` in its track table and `events`
    /// as its events.
    fn file(top: &str, track: &str, events: &str) -> String {
        format!(
            "format = 1\ntempo = 120.0\ntime_signature = \"4/4\"\n{top}\n[[tracks]]\nid = 1\n\
             name = \"t\"\nport = \"out\"\nchannel = 1\nloop_length = 960\n{track}\n\
             events = [{events}]\n"
        )
    }

    #[test]
    fn hand_written_files_may_leave_out_what_has_a_default() {
        let text = "format = 1\ntempo = 90\ntime_signature = \"3/4\"\n\n[[tracks]]\nid = 4\n\
                    name = \"t\"\nport = \"synth\"\nchannel = 16\nloop_length = 960\n";
        let project = read(text, &ports(), &devices()).unwrap();
        assert_eq!(project.tempo, Bpm::new(90.0).unwrap());
        let track = &project.tracks[0];
        assert_eq!(
            (
                track.device.as_deref(),
                track.muted,
                track.program,
                track.clip.events().len()
            ),
            (None, false, None, 0)
        );
        assert_eq!(track.target.channel.number(), 16);

        let empty = read(
            "format = 1\ntempo = 120.0\ntime_signature = \"4/4\"\n",
            &[],
            &devices(),
        );
        assert_eq!(empty, Ok(Project::default()));
    }

    #[test]
    fn bad_files_say_what_is_wrong() {
        let note = r#"{ type = "note", start = 0, note = 36, velocity = 100, length = 240 }"#;
        let cases = [
            ("tempo = 120.0\n".to_owned(), "no `format` line"),
            (
                "format = 2\nnew_setting = true\n".to_owned(),
                "reads only format 1",
            ),
            ("format = 1\ntempo = \n".to_owned(), "line 2"),
            (file("tempp = 3", "", ""), "unknown field `tempp`"),
            (
                file("", "", "").replace("tempo = 120.0", "tempo = 1000.0"),
                "between 20 and 400",
            ),
            (
                file("", "", "").replace("4/4", "4/3"),
                "`4/3` is not a time signature",
            ),
            (
                file("", "device = \"tr8\"", ""),
                "device `tr8`, which has no profile",
            ),
            (
                file("", "", "").replace("\"out\"", "\"elsewhere\""),
                "port `elsewhere`",
            ),
            (
                file("", "", "").replace("channel = 1", "channel = 17"),
                "1-16",
            ),
            (
                file("", "", "").replace("channel = 1", "channel = 0"),
                "1-16",
            ),
            (
                file("", "", "").replace("loop_length = 960", "loop_length = 0"),
                "longer than zero ticks",
            ),
            (
                file("", "", &note.replace("start = 0", "start = 960")),
                "track 1: the event at 960 ticks starts outside the clip",
            ),
            (
                file("", "", &note.replace("length = 240", "length = 961")),
                "longer than the clip's 960 ticks",
            ),
            (
                file("", "", &note.replace("length = 240", "length = 0")),
                "zero length",
            ),
            (
                file("", "", &note.replace("velocity = 100", "velocity = 128")),
                "128",
            ),
            (
                file("", "", &note.replace("velocity = 100, ", "")),
                "missing field `velocity`",
            ),
            (
                file("", "", &note.replace("type = \"note\"", "type = \"bend\"")),
                "unknown variant `bend`",
            ),
            (file("", "", &note.replace("start = 0", "start = -1")), "-1"),
            (
                file("", "", "")
                    + "[[tracks]]\nid = 1\nname = \"u\"\nport = \"out\"\nchannel = 2\n\
                                    loop_length = 960\n",
                "there are two tracks 1",
            ),
            (file("", "program = 128", ""), "128"),
            (
                file("", "bank_lsb = 1", ""),
                "track 1 has a bank but no program",
            ),
            (
                file("", "program = 5", "")
                    + "[[tracks]]\nid = 2\nname = \"u\"\nport = \"out\"\nchannel = 1\n\
                       program = 5\nbank_msb = 1\nloop_length = 960\n",
                "tracks 1 and 2 set different programs on the same port and channel",
            ),
        ];
        for (text, expected) in cases {
            let error = read(&text, &ports(), &devices()).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {expected:?}: {error}\n{text}"
            );
        }
    }
}
