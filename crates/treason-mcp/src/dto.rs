//! What the agent sends and gets back.
//!
//! Designed for the model rather than mirroring the core types: channels
//! count from 1, positions are bars and beats, notes go by voice or pitch
//! name, and patterns are step strings. The doc comments become the
//! descriptions in the tools' JSON schemas, so they are written for the agent.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use treason_core::model::{PortId, Project, Track};
use treason_core::playback::{PlaybackStatus, TransportState};
use treason_core::time::{PPQN, Ticks};
use treason_devices::DeviceProfile;

/// The project.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ProjectView {
    /// Beats per minute.
    pub tempo: f64,
    /// Such as "4/4".
    pub time_signature: String,
    /// Every track. Each loops its own clip.
    pub tracks: Vec<TrackView>,
}

impl ProjectView {
    /// Describes `project`. `ports` names the output ports.
    pub fn new(project: &Project, ports: &[String]) -> Self {
        Self {
            tempo: project.tempo.get(),
            time_signature: project.time_signature.to_string(),
            tracks: project
                .tracks
                .iter()
                .map(|track| TrackView::new(track, ports))
                .collect(),
        }
    }
}

/// A track.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct TrackView {
    /// Refers to the track in other tools.
    pub id: u32,
    /// The track's name.
    pub name: String,
    /// The device profile that names its voices and parameters, if any.
    pub device: Option<String>,
    /// The output port.
    pub port: String,
    /// The MIDI channel, 1-16.
    pub channel: u8,
    /// A muted track plays nothing.
    pub muted: bool,
    /// The length of the track's loop, in beats.
    pub loop_beats: f64,
    /// How many notes and other events the loop holds.
    pub events: usize,
}

impl TrackView {
    /// Describes `track`. `ports` names the output ports.
    pub fn new(track: &Track, ports: &[String]) -> Self {
        Self {
            id: track.id.0,
            name: track.name.clone(),
            device: track.device.clone(),
            port: port_name(ports, track.target.port),
            channel: track.target.channel.number(),
            muted: track.muted,
            loop_beats: beats(track.clip.length()),
            events: track.clip.events().len(),
        }
    }
}

/// The device profiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DeviceList {
    /// Every device profile.
    pub devices: Vec<DeviceView>,
}

/// What a synth or drum machine calls its notes and controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DeviceView {
    /// Use this id in add_track.
    pub id: String,
    /// The device's name.
    pub name: String,
    /// Who makes it.
    pub manufacturer: Option<String>,
    /// The MIDI channel it listens on by default, 1-16.
    pub default_channel: u8,
    /// Voice names and their MIDI notes. On a drum machine, one per sound.
    pub voices: BTreeMap<String, u8>,
    /// Other names for voices, such as "bd" for "kick".
    pub aliases: BTreeMap<String, String>,
    /// Parameter names and their MIDI CC numbers.
    pub params: BTreeMap<String, u8>,
}

impl From<&DeviceProfile> for DeviceView {
    fn from(profile: &DeviceProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            manufacturer: profile.manufacturer.clone(),
            default_channel: profile.default_channel.number(),
            voices: profile
                .voices
                .iter()
                .map(|(name, note)| (name.clone(), note.get()))
                .collect(),
            aliases: profile.aliases.clone(),
            params: profile
                .params
                .iter()
                .map(|(name, param)| (name.clone(), param.cc.get()))
                .collect(),
        }
    }
}

/// The output ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PortList {
    /// Port names, for add_track. The first is the default.
    pub ports: Vec<String>,
}

/// Where playback is.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct StatusView {
    /// Whether the sequencer is playing.
    pub playing: bool,
    /// The current bar, counting from 1.
    pub bar: u64,
    /// The beat within the bar, counting from 1.
    pub beat: u64,
    /// Beats per minute.
    pub tempo: f64,
}

impl StatusView {
    /// Describes `status` in `project`'s bars and beats.
    pub fn new(status: PlaybackStatus, project: &Project) -> Self {
        let bar = project.time_signature.bar_length().0;
        let beat = PPQN * 4 / u64::from(project.time_signature.denominator());
        let position = status.position.0;
        Self {
            playing: status.state == TransportState::Playing,
            bar: position / bar + 1,
            beat: position % bar / beat + 1,
            tempo: project.tempo.get(),
        }
    }
}

/// Input for set_tempo.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTempo {
    /// Beats per minute, 20 to 400.
    pub bpm: f64,
}

/// Input for set_time_signature.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTimeSignature {
    /// Beats per bar, 1 to 32.
    pub numerator: u8,
    /// The note value of a beat: 1, 2, 4, 8, 16 or 32.
    pub denominator: u8,
}

/// Input for add_track.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddTrack {
    /// A name for the track, such as "drums" or "bass".
    pub name: String,
    /// The output port, from list_ports. Defaults to the first port.
    pub port: Option<String>,
    /// A device profile id, from list_devices. It lets set_steps and
    /// set_notes use the device's voice names.
    pub device: Option<String>,
    /// The MIDI channel, 1-16. Defaults to the device's channel, or 1.
    pub channel: Option<u8>,
}

/// Names a track.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TrackRef {
    /// The track's id.
    pub track: u32,
}

/// Input for set_muted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetMuted {
    /// The track's id.
    pub track: u32,
    /// True to silence the track, false to hear it again.
    pub muted: bool,
}

/// Input for set_steps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetSteps {
    /// The track's id.
    pub track: u32,
    /// One step string per voice, keyed by voice name or MIDI note number.
    /// Each character is a step: "x" a hit, "X" an accent, "." a rest. Spaces
    /// and "|" are ignored. Example: {"kick": "x...x...x...x...",
    /// "closed_hat": "..x...x...x...x."}. The loop is as long as the longest
    /// string.
    pub steps: BTreeMap<String, String>,
    /// Steps per beat. Defaults to 4, so each step is a sixteenth note.
    pub steps_per_beat: Option<u32>,
}

/// Input for set_notes.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetNotes {
    /// The track's id.
    pub track: u32,
    /// The notes, in any order.
    pub notes: Vec<NoteSpec>,
    /// The loop length in beats. Defaults to the end of the last note,
    /// rounded up to a whole bar.
    pub loop_beats: Option<f64>,
}

/// One note.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteSpec {
    /// Which note.
    pub note: NoteRef,
    /// When it starts, in beats from the start of the loop.
    pub start: f64,
    /// How long it lasts, in beats.
    pub length: f64,
    /// How hard, 1-127. Defaults to 100.
    pub velocity: Option<u8>,
}

/// A note: a MIDI note number, or a name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NoteRef {
    /// A MIDI note number, 0-127.
    Number(u8),
    /// A voice of the track's device, such as "kick", or a pitch such as
    /// "C4" (middle C, note 60), "F#2" or "Bb3".
    Name(String),
}

/// Input for play.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Play {
    /// The bar to start from, counting from 1. Defaults to 1.
    pub from_bar: Option<u32>,
}

/// The name of a port, falling back to its number.
fn port_name(ports: &[String], port: PortId) -> String {
    ports
        .get(usize::from(port.0))
        .cloned()
        .unwrap_or_else(|| format!("port {port}"))
}

fn beats(ticks: Ticks) -> f64 {
    ticks.0 as f64 / PPQN as f64
}

#[cfg(test)]
mod tests {
    use treason_core::time::TimeSignature;

    use super::*;

    #[test]
    fn status_counts_bars_and_beats_from_one() {
        let project = Project {
            time_signature: TimeSignature::new(7, 8).unwrap(),
            ..Project::default()
        };
        let status = PlaybackStatus {
            state: TransportState::Playing,
            // Bar 3 (two 7/8 bars are 6720 ticks), plus five eighth notes.
            position: Ticks(6720 + 5 * 480),
        };
        let view = StatusView::new(status, &project);
        assert_eq!((view.bar, view.beat), (3, 6));
        assert!(view.playing);
    }
}
