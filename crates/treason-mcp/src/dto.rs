//! What the agent sends and gets back.
//!
//! Designed for the model rather than mirroring the core types: channels
//! count from 1, positions are bars and beats, notes go by voice or pitch
//! name, patterns are step strings, and controls go by name. The doc
//! comments become the descriptions in the tools' JSON schemas, so they are
//! written for the agent.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use treason_core::model::{Clip, EventKind, PortId, Project, Track};
use treason_core::playback::{PlaybackStatus, TransportState};
use treason_core::time::{PPQN, Ticks};
use treason_devices::{DeviceProfile, DeviceRegistry};

use crate::controls;
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
    /// The loop's notes as step strings, in the form set_steps takes. Shown
    /// when every note sits on a step grid at set_steps' velocities; missing
    /// for other clips, such as melodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<PatternView>,
    /// The loop's notes in the form set_notes takes, by start. Shown for
    /// clips that aren't step patterns, such as melodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<NoteView>>,
    /// The loop's controls (MIDI CC) in the form set_controls takes, by
    /// name. Missing when it sends none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controls: Option<BTreeMap<String, ControlView>>,
    /// How many notes, control changes and other events the loop holds.
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
        let controls = Some(ControlView::all(&track.clip, device)).filter(|all| !all.is_empty());
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
            controls,
            events: track.clip.events().len(),
        }
    }
}

/// One note of a loop.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct NoteView {
    /// The device's voice for the note if it has one, otherwise its pitch,
    /// such as "C4". A decimal MIDI number if the pitch name would resolve
    /// to a different device voice or alias.
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
                        .map_or_else(
                            || {
                                let pitch = notation::format_pitch(note);
                                if device
                                    .and_then(|device| device.voice(&pitch))
                                    .is_some_and(|voice| voice != note)
                                {
                                    note.get().to_string()
                                } else {
                                    pitch
                                }
                            },
                            str::to_owned,
                        ),
                    start: beats(event.start),
                    length: beats(length),
                    velocity: velocity.get(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// One control of a loop, as points in the form set_controls takes.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct ControlView {
    /// The control's values, by time. Written back with set_controls and
    /// the track's loop_beats, they play the same, though they may not be
    /// the points first written: a repeat reads back as every pass of it.
    pub points: Vec<PointView>,
}

impl ControlView {
    /// The controls `clip` sends, by name: `device`'s name for the
    /// controller where it has one, then the standard name, then the
    /// number.
    fn all(clip: &Clip, device: Option<&DeviceProfile>) -> BTreeMap<String, Self> {
        controls::controls_from_clip(clip)
            .into_iter()
            .map(|(controller, points)| {
                let points = points
                    .into_iter()
                    .map(|point| PointView {
                        at: beats(point.at),
                        value: point.value.get(),
                        ramp: point.ramp,
                    })
                    .collect();
                (controls::control_name(controller, device), Self { points })
            })
            .collect()
    }
}

/// A control's value at a point in the loop.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct PointView {
    /// When, in beats from the start of the loop.
    pub at: f64,
    /// The value, 0-127.
    pub value: u8,
    /// How the value gets here from the point before. Missing where it
    /// jumps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ramp: Option<Ramp>,
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

/// Where playback is, and whether the project is saved.
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
    /// The name the project was last saved as or loaded from, which
    /// save_project uses when given no name. Missing until then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Whether the project has changed since it was last saved or loaded.
    pub unsaved_changes: bool,
}

impl StatusView {
    /// Describes `status` in `project`'s bars and beats. `name` is what the
    /// project was last saved as or loaded from, and `unsaved_changes` says
    /// whether it has changed since.
    pub fn new(
        status: PlaybackStatus,
        project: &Project,
        name: Option<&str>,
        unsaved_changes: bool,
    ) -> Self {
        let bar = project.time_signature.bar_length().0;
        let beat = PPQN * 4 / u64::from(project.time_signature.denominator());
        let position = status.position.0;
        Self {
            playing: status.state == TransportState::Playing,
            bar: position / bar + 1,
            beat: position % bar / beat + 1,
            tempo: project.tempo.get(),
            project: name.map(str::to_owned),
            unsaved_changes,
        }
    }
}

/// The saved projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProjectList {
    /// Project names for load_project, newest first.
    pub projects: Vec<String>,
    /// How many older projects there are beyond the list.
    pub more: usize,
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

/// Input for set_controls.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetControls {
    /// The track's id.
    pub track: u32,
    /// The controls to set, keyed by a parameter of the track's device (see
    /// list_devices), a standard name, or a controller number, 0-119. The
    /// standard names, with their numbers: modulation (1, the mod wheel),
    /// breath (2), foot (4), portamento_time (5), volume (7), balance (8),
    /// pan (10), expression (11), sustain (64), portamento (65), sostenuto
    /// (66), soft_pedal (67), legato (68), resonance (71), release (72),
    /// attack (73), cutoff (74), decay (75), vibrato_rate (76),
    /// vibrato_depth (77), vibrato_delay (78), reverb (91), tremolo (92),
    /// chorus (93), detune (94) and phaser (95).
    pub controls: BTreeMap<String, ControlLane>,
    /// The loop length in beats, at most 1024, on a track without notes:
    /// on a track with notes, they set the loop. Defaults to the track's
    /// loop, or on a track with nothing else, to the end of the last point
    /// or repeat, rounded up to a whole bar. A shorter loop cuts off the
    /// track's other controls at its end.
    pub loop_beats: Option<f64>,
}

/// How one control moves through the loop.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlLane {
    /// The control's values, in any order, at most 8192. An empty list
    /// removes the control. Successive jumps at the same time are sent in
    /// their list order, including repeated values.
    pub points: Vec<ControlPoint>,
    /// Repeat the points every this many beats through the loop, such as 1
    /// for an LFO that cycles once a beat. Defaults to the whole loop. A
    /// repeat that doesn't divide the loop starts again with it.
    pub repeat_beats: Option<f64>,
}

/// A control's value at a point in the loop.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlPoint {
    /// When, in beats from the start of the loop, or of each repeat: 0 is
    /// the first beat, 1.5 the offbeat after the second. Rounds to 1/960 of
    /// a beat. A ramp can end at the end of the loop or repeat, which is
    /// where the next pass starts again from the first point.
    pub at: f64,
    /// The value, 0-127.
    pub value: u8,
    /// How the value gets here from the point before: "linear" ramps
    /// evenly, and "smooth" eases out and in, so smooth ramps between a low
    /// and a high make a sine wave. Without it, the value jumps here. The
    /// first point can't ramp, since nothing comes before it. A ramp must
    /// end later than the point before; same-time points must jump.
    pub ramp: Option<Ramp>,
}

/// How a control moves from one point to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ramp {
    /// At an even rate.
    Linear,
    /// Slowly at both ends and fastest midway, along half a sine wave.
    Smooth,
}

/// Input for play.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Play {
    /// The bar to start from, counting from 1. Defaults to 1.
    pub from_bar: Option<u32>,
}

/// Input for save_project.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveProject {
    /// The name to save under, such as "acid jam" or "dub-techno_2": up to
    /// 64 letters, digits, spaces, dashes and underscores, beginning with a
    /// letter or digit. Defaults to the project's own name, which status
    /// shows.
    pub name: Option<String>,
    /// Replace the saved project of that name. Not needed to save again
    /// under the project's own name.
    #[serde(default)]
    pub overwrite: bool,
}

/// Input for load_project.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoadProject {
    /// The saved project's name, from list_projects.
    pub name: String,
    /// Load even if the current project has unsaved changes, which are
    /// then lost.
    #[serde(default)]
    pub discard_changes: bool,
}

/// Input for export_midi.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportMidi {
    /// The MIDI file's name, without ".mid": up to 64 letters, digits,
    /// spaces, dashes and underscores, beginning with a letter or digit.
    /// Defaults to the project's own name, which status shows.
    pub name: Option<String>,
    /// How many bars to export, from bar 1: 1 to 1000. Defaults to enough
    /// for every track's loop to end on the same bar line, so that each
    /// pattern is complete.
    pub bars: Option<u32>,
    /// Replace the MIDI file of that name. Not needed for the project's own
    /// name.
    #[serde(default)]
    pub overwrite: bool,
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
        let view = StatusView::new(status, &project, Some("jam"), true);
        assert_eq!((view.bar, view.beat), (3, 6));
        assert!(view.playing);
        assert_eq!(
            (view.project.as_deref(), view.unsaved_changes),
            (Some("jam"), true)
        );
    }
}
