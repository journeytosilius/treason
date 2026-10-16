//! What the agent sends and gets back.
//!
//! Designed for the model rather than mirroring the core types: channels
//! count from 1, positions are bars and beats, notes go by voice or pitch
//! name, and patterns are step strings. The doc comments become the
//! descriptions in the tools' JSON schemas, so they are written for the agent.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use treason_core::model::{Clip, EventKind, PortId, Project, Track};
use treason_core::playback::{PlaybackStatus, TransportState};
use treason_core::time::{PPQN, Ticks};
use treason_devices::{DeviceProfile, DeviceRegistry};

use crate::notation;
use crate::pattern::{self, StepPattern};

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
    /// Describes `project`. `ports` names the output ports, and `devices`
    /// the voices in its patterns.
    pub fn new(project: &Project, ports: &[String], devices: &DeviceRegistry) -> Self {
        Self {
            tempo: project.tempo.get(),
            time_signature: project.time_signature.to_string(),
            tracks: project
                .tracks
                .iter()
                .map(|track| TrackView::new(track, ports, devices))
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
    /// The loop as step strings, in the form set_steps takes. Shown when
    /// every note sits on a step grid at set_steps' velocities; missing for
    /// other clips, such as melodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<PatternView>,
    /// The loop's notes in the form set_notes takes, by start. Shown for
    /// clips that aren't step patterns, such as melodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<NoteView>>,
    /// How many notes and other events the loop holds.
    pub events: usize,
}

impl TrackView {
    /// Describes `track`. `ports` names the output ports, and `devices` the
    /// voices in its pattern.
    pub fn new(track: &Track, ports: &[String], devices: &DeviceRegistry) -> Self {
        let device = track.device.as_deref().and_then(|id| devices.get(id));
        let pattern = pattern::steps_from_clip(&track.clip, device).map(PatternView::from);
        let notes = match pattern {
            None => Some(NoteView::all(&track.clip, device)).filter(|notes| !notes.is_empty()),
            Some(_) => None,
        };
        Self {
            id: track.id.0,
            name: track.name.clone(),
            device: track.device.clone(),
            port: port_name(ports, track.target.port),
            channel: track.target.channel.number(),
            muted: track.muted,
            loop_beats: beats(track.clip.length()),
            pattern,
            notes,
            events: track.clip.events().len(),
        }
    }
}

/// One note of a loop.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct NoteView {
    /// The device's voice for the note if it has one, otherwise its pitch,
    /// such as "C4".
    pub note: String,
    /// When it starts, in beats from the start of the loop.
    pub start: f64,
    /// How long it lasts, in beats.
    pub length: f64,
    /// How hard, 1-127.
    pub velocity: u8,
}

impl NoteView {
    /// The notes `clip` plays, by start, named after `device`'s voices where
    /// it has them.
    fn all(clip: &Clip, device: Option<&DeviceProfile>) -> Vec<Self> {
        clip.events()
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::Note {
                    note,
                    velocity,
                    length,
                } => Some(Self {
                    note: device
                        .and_then(|device| device.voice_name(note))
                        .map_or_else(|| notation::format_pitch(note), str::to_owned),
                    start: beats(event.start),
                    length: beats(length),
                    velocity: velocity.get(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// A loop written as step strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PatternView {
    /// Steps per beat, as in set_steps.
    pub steps_per_beat: u32,
    /// One step string per voice, with a space between beats: "x" a hit,
    /// "X" an accent, "." a rest.
    pub steps: BTreeMap<String, String>,
}

impl From<StepPattern> for PatternView {
    fn from(pattern: StepPattern) -> Self {
        Self {
            steps_per_beat: pattern.steps_per_beat,
            steps: pattern.steps,
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

/// Input for edit_track.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditTrack {
    /// The track's id.
    pub track: u32,
    /// A new name.
    pub name: Option<String>,
    /// Another output port, from list_ports.
    pub port: Option<String>,
    /// Another MIDI channel, 1-16. Without it the channel stays, even when
    /// the device changes.
    pub channel: Option<u8>,
    /// Another device profile id, from list_devices. Later set_steps calls
    /// use its voice names; the notes already in the clip stay as they are.
    pub device: Option<String>,
}

/// Input for set_steps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetSteps {
    /// The track's id.
    pub track: u32,
    /// One step string per voice, keyed by the device's voice names (or
    /// their aliases) or by MIDI note number. Each character is a step: "x"
    /// a hit, "X" an accent, "." or "-" a rest. Spaces and "|" are ignored.
    /// Example: {"kick": "x...x...x...x...", "snare": "....x.......x...",
    /// "closed_hat": "..x...x...x...x."}. The loop is as long as the longest
    /// string. A shorter one repeats to fill it, so {"closed_hat": "..x."}
    /// plays an off-beat hat in every beat; its length must divide the
    /// loop's.
    pub steps: BTreeMap<String, String>,
    /// Steps per beat (a quarter note): 1, 2, 3, 4, 6, 8, 12, 16, 24 or 32.
    /// Defaults to 4, so each step is a sixteenth note; 3 gives eighth-note
    /// triplets.
    pub steps_per_beat: Option<u32>,
}

/// Input for set_notes.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetNotes {
    /// The track's id.
    pub track: u32,
    /// The notes, in any order, at most 1024. Notes that start together
    /// make a chord. Two notes of the same pitch can't overlap.
    pub notes: Vec<NoteSpec>,
    /// The loop length in beats, at most 1024. Defaults to the end of the
    /// last note, rounded up to a whole bar. A note can run past the end of
    /// the loop into the next pass.
    pub loop_beats: Option<f64>,
}

/// One note.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteSpec {
    /// Which note.
    pub note: NoteRef,
    /// When it starts, in beats from the start of the loop: 0 is the first
    /// beat, 1.5 the offbeat after the second. Rounds to 1/960 of a beat.
    pub start: f64,
    /// How long it lasts, in beats: 1 is a quarter note in 4/4, 0.5 an
    /// eighth.
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
