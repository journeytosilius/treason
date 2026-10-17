//! The tools.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use treason_control::{
    ControlError, Edit, ExportStore, ProjectError, ProjectName, ProjectStore, Session,
};
use treason_core::export::{self, ExportError, MAX_BARS, MidiExport};
use treason_core::midi::{Channel, U7};
use treason_core::model::{Clip, OutputTarget, PortId, Project, Track, TrackId};
use treason_core::playback::{Playback, TransportCommand};
use treason_core::time::{Bpm, Ticks, TimeSignature};
use treason_devices::DeviceProfile;

use crate::dto::{
    AddTrack, DeviceList, DeviceView, EditTrack, ExportMidi, LoadProject, Play, PortList,
    ProjectList, ProjectView, SaveProject, SetControls, SetMuted, SetNotes, SetSteps, SetTempo,
    SetTimeSignature, StatusView, TrackRef, TrackView,
};
use crate::{controls, notes, pattern, programs};

/// The guide for agents, `docs/guide.md`, which `get_guide` returns.
const GUIDE: &str = include_str!("../../../docs/guide.md");

/// The session behind the server: one project, shared by every agent.
pub type SharedSession = Arc<Mutex<Session<Box<dyn Playback + Send>>>>;

/// treason's MCP server.
///
/// Cheap to clone. Clones serve the same session, so every connected agent
/// sees and edits the same project.
#[derive(Clone)]
pub struct TreasonServer {
    session: SharedSession,
    ports: Arc<[String]>,
    projects: Arc<ProjectStore>,
    exports: Arc<ExportStore>,
    exporter: Arc<dyn MidiExport + Send + Sync>,
    tool_router: ToolRouter<Self>,
}

impl TreasonServer {
    /// Serves `session`. `ports` names the output ports, in `PortId` order,
    /// `projects` is where the project is saved, and `exporter` writes the
    /// MIDI files that go in `exports`.
    pub fn new(
        session: Session<Box<dyn Playback + Send>>,
        ports: Vec<String>,
        projects: ProjectStore,
        exports: ExportStore,
        exporter: Arc<dyn MidiExport + Send + Sync>,
    ) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            ports: ports.into(),
            projects: Arc::new(projects),
            exports: Arc::new(exports),
            exporter,
            tool_router: Self::tool_router(),
        }
    }

    fn session(&self) -> MutexGuard<'_, Session<Box<dyn Playback + Send>>> {
        lock(&self.session)
    }

    /// Runs `work` with the session, the project store and the port names
    /// on a thread that may block, since saving and loading wait for the
    /// disk.
    async fn on_disk<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Session<Box<dyn Playback + Send>>, &ProjectStore, &[String]) -> T
        + Send
        + 'static,
    ) -> Result<T, String> {
        let session = Arc::clone(&self.session);
        let projects = Arc::clone(&self.projects);
        let ports = Arc::clone(&self.ports);
        tokio::task::spawn_blocking(move || work(&mut lock(&session), &projects, &ports))
            .await
            .map_err(|error| format!("the project store failed: {error}"))
    }

    fn apply(&self, edit: Edit) -> Result<(), String> {
        self.session().apply(edit).map_err(describe)
    }

    fn command(&self, command: TransportCommand) -> Result<(), String> {
        self.session().command(command).map_err(describe)
    }

    /// Replaces a track's clip with the one `clip` makes from the clip it
    /// has, the track's device and the project, and describes the track as
    /// it will play.
    fn replace_clip(
        &self,
        track: u32,
        clip: impl FnOnce(&Clip, Option<&DeviceProfile>, &Project) -> Result<Clip, String>,
    ) -> Result<Json<TrackView>, String> {
        let mut session = self.session();
        let mut track = find_track(&session, track)?;
        let device = track
            .device
            .as_deref()
            .and_then(|id| session.devices().get(id));
        track.clip = clip(&track.clip, device, session.project())?;
        let view = TrackView::new(&track, &self.ports, session.devices());
        session
            .apply(Edit::SetClip {
                track: track.id,
                clip: track.clip,
            })
            .map_err(describe)?;
        Ok(Json(view))
    }

    /// The port called `name`, or the first port.
    fn port_id(&self, name: Option<&str>) -> Result<PortId, String> {
        let index = match name {
            Some(name) => self
                .ports
                .iter()
                .position(|port| port == name)
                .ok_or_else(|| format!("there is no port `{name}`; see list_ports"))?,
            None if self.ports.is_empty() => return Err("no output ports are configured".into()),
            None => 0,
        };
        u16::try_from(index)
            .map(PortId)
            .map_err(|_| "too many ports".into())
    }
}

fn lock(session: &SharedSession) -> MutexGuard<'_, Session<Box<dyn Playback + Send>>> {
    // An edit swaps in a whole new project, so a panic while the lock was
    // held can't have left the session half-changed.
    session.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A failed save or load, worded so the agent can fix the call.
fn describe_project(error: ProjectError) -> String {
    match error {
        ProjectError::Unnamed => "the project has no name yet; give save_project a name".into(),
        ProjectError::Exists(name) => format!(
            "there is already a project `{name}`; choose another name, or pass overwrite: true \
             to replace it"
        ),
        ProjectError::ExportExists(name) => format!(
            "there is already a MIDI file `{name}`; choose another name, or pass overwrite: true \
             to replace it"
        ),
        ProjectError::NotFound(name) => format!("there is no project `{name}`; see list_projects"),
        ProjectError::UnsavedChanges => "the project has unsaved changes, which loading would \
            lose; save them first with save_project, or pass discard_changes: true to load anyway"
            .into(),
        other => other.to_string(),
    }
}

/// A refused export, worded so the agent can fix the call.
fn describe_export(error: ExportError) -> String {
    match error {
        ExportError::NoBars => format!("bars must be 1 to {MAX_BARS}, not 0"),
        ExportError::TooManyBars(bars) => format!("bars must be 1 to {MAX_BARS}, not {bars}"),
        ExportError::NoCycle => format!(
            "the tracks' loops line up only after more than {MAX_BARS} bars; pass bars to say how \
             many to export"
        ),
        ExportError::TooManyEvents(_) => format!("{error}; pass fewer bars"),
        ExportError::Write(_) => error.to_string(),
    }
}

/// `count` of `thing`, such as "1 track" or "3 tracks".
fn count(count: usize, thing: &str) -> String {
    let s = if count == 1 { "" } else { "s" };
    format!("{count} {thing}{s}")
}

/// A rejected edit or command, worded so the agent can fix the call.
fn describe(error: ControlError) -> String {
    match error {
        ControlError::UnknownTrack(id) => format!("there is no track {id}; see get_project"),
        ControlError::UnknownDevice(id) => format!("there is no device `{id}`; see list_devices"),
        ControlError::ProgramClash(first, second) => format!(
            "tracks {first} and {second} play on the same port and channel, which has one \
             program: give them the same one, leave it out of one (program null in edit_track \
             clears it), or move one to another channel"
        ),
        ControlError::BendRangeClash(first, second) => format!(
            "tracks {first} and {second} play on the same port and channel, which has one bend \
             range: give them the same one, leave it out of one (bend_range null in edit_track \
             clears it), or move one to another channel"
        ),
        other => other.to_string(),
    }
}

/// The profile of the device with `id`, if any.
fn find_device<'a>(
    session: &'a Session<Box<dyn Playback + Send>>,
    id: Option<&str>,
) -> Result<Option<&'a DeviceProfile>, String> {
    id.map(|id| {
        session
            .devices()
            .get(id)
            .ok_or_else(|| describe(ControlError::UnknownDevice(id.to_owned())))
    })
    .transpose()
}

/// A copy of the track with `id`, to edit.
fn find_track(session: &Session<Box<dyn Playback + Send>>, id: u32) -> Result<Track, String> {
    session
        .project()
        .track(TrackId(id))
        .cloned()
        .ok_or_else(|| describe(ControlError::UnknownTrack(TrackId(id))))
}

/// The channel numbered `number`, 1-16.
fn parse_channel(number: u8) -> Result<Channel, String> {
    Channel::from_number(number).ok_or_else(|| format!("MIDI channel must be 1-16, got {number}"))
}

fn parse_bend_range(semitones: u8) -> Result<U7, String> {
    U7::new(semitones).ok_or_else(|| {
        format!("bend_range must be 0-127 semitones, not {semitones}; most synths take up to 24")
    })
}

#[tool_router]
impl TreasonServer {
    /// The guide to driving treason: how projects, tracks and loops work,
    /// what each tool is for, the names and notations, the limits, what the
    /// errors mean and how to connect gear, with examples of every call.
    /// Read it once before you start.
    #[tool(annotations(read_only_hint = true))]
    fn get_guide(&self) -> String {
        GUIDE.into()
    }

    /// Shows the project: tempo, time signature and every track, with each
    /// clip as step strings or notes, in the form set_steps or set_notes
    /// takes, and its controls in the form set_controls takes.
    #[tool(annotations(read_only_hint = true))]
    fn get_project(&self) -> Json<ProjectView> {
        let session = self.session();
        Json(ProjectView::new(
            session.project(),
            &self.ports,
            session.devices(),
        ))
    }

    /// Lists the device profiles. A track with a device names its notes by
    /// voice, such as "kick" or "closed_hat", its controls by parameter,
    /// such as "cutoff", and its sound by program, such as
    /// "electric_bass_finger" on "gm", the General MIDI sounds.
    #[tool(annotations(read_only_hint = true))]
    fn list_devices(&self) -> Json<DeviceList> {
        let devices = self
            .session()
            .devices()
            .iter()
            .map(DeviceView::from)
            .collect();
        Json(DeviceList { devices })
    }

    /// Lists the MIDI output ports that tracks can play on.
    #[tool(annotations(read_only_hint = true))]
    fn list_ports(&self) -> Json<PortList> {
        Json(PortList {
            ports: self.ports.to_vec(),
        })
    }

    /// Shows whether the sequencer is playing, where, and how fast, and the
    /// project's name and whether it has unsaved changes.
    #[tool(annotations(read_only_hint = true))]
    fn status(&self) -> Json<StatusView> {
        let session = self.session();
        Json(StatusView::new(
            session.status(),
            session.project(),
            session.name().map(ProjectName::as_str),
            session.has_unsaved_changes(),
        ))
    }

    /// Lists the saved projects, newest first, for load_project.
    #[tool(annotations(read_only_hint = true))]
    async fn list_projects(&self) -> Result<Json<ProjectList>, String> {
        let projects = Arc::clone(&self.projects);
        let list = tokio::task::spawn_blocking(move || projects.list())
            .await
            .map_err(|error| format!("the project store failed: {error}"))?
            .map_err(describe_project)?;
        Ok(Json(ProjectList {
            projects: list.names.iter().map(ToString::to_string).collect(),
            more: list.more,
        }))
    }

    /// Saves the project under a name, so that load_project can bring it
    /// back, even after treason restarts. It keeps everything: the tempo,
    /// the time signature, and every track with its clip, port, channel,
    /// device and mute. Without a name, saves under the project's own name,
    /// the one it was last saved as or loaded from. Saving over a different
    /// saved project needs overwrite: true.
    #[tool]
    async fn save_project(
        &self,
        Parameters(SaveProject { name, overwrite }): Parameters<SaveProject>,
    ) -> Result<String, String> {
        let name = name
            .map(ProjectName::new)
            .transpose()
            .map_err(describe_project)?;
        let saved = self
            .on_disk(move |session, projects, ports| session.save(projects, ports, name, overwrite))
            .await?
            .map_err(describe_project)?;
        Ok(format!("Saved the project as `{saved}`."))
    }

    /// Loads a project saved with save_project. It replaces the current
    /// project and its undo history, so it refuses while the current
    /// project has unsaved changes, unless discard_changes is true. While
    /// playing, the loaded project takes over like an edit: its tempo at
    /// once, its tracks at the next bar. Returns the project, as get_project
    /// shows it.
    #[tool]
    async fn load_project(
        &self,
        Parameters(LoadProject {
            name,
            discard_changes,
        }): Parameters<LoadProject>,
    ) -> Result<Json<ProjectView>, String> {
        let name = ProjectName::new(name).map_err(describe_project)?;
        self.on_disk(move |session, projects, ports| {
            session
                .load(projects, ports, name, discard_changes)
                .map_err(describe_project)?;
            Ok(Json(ProjectView::new(
                session.project(),
                ports,
                session.devices(),
            )))
        })
        .await?
    }

    /// Exports the project as a Standard MIDI File that a DAW can open: a
    /// track for each unmuted track, named after it, with the tempo and
    /// time signature. Muted tracks are left out, as they are from playback.
    /// The file plays from bar 1 for `bars` bars, by default until every
    /// track's loop ends on the same bar line. It goes in the exports
    /// directory as <name>.mid, by default under the project's own name;
    /// replacing a different MIDI file needs overwrite: true. Returns where
    /// the file is.
    #[tool]
    async fn export_midi(
        &self,
        Parameters(ExportMidi {
            name,
            bars,
            overwrite,
        }): Parameters<ExportMidi>,
    ) -> Result<String, String> {
        let name = name
            .map(ProjectName::new)
            .transpose()
            .map_err(describe_project)?;
        let (project, own) = {
            let session = self.session();
            (Arc::clone(session.project()), session.name().cloned())
        };
        let Some(name) = name.or_else(|| own.clone()) else {
            return Err("the project has no name yet; give export_midi a name".into());
        };
        let overwrite = overwrite || own.as_ref() == Some(&name);
        let bars = export::bars(&project, bars).map_err(describe_export)?;
        let playing = project.tracks.iter().filter(|track| !track.muted).count();
        let muted = project.tracks.len() - playing;

        // Rendering a long song takes a while, so it runs on a thread that
        // may block, with a snapshot rather than the session.
        let exporter = Arc::clone(&self.exporter);
        let exports = Arc::clone(&self.exports);
        let path = tokio::task::spawn_blocking(move || {
            let file = exporter
                .export(&project, name.as_str(), bars)
                .map_err(describe_export)?;
            exports
                .save(&name, &file, overwrite)
                .map_err(describe_project)
        })
        .await
        .map_err(|error| format!("the export failed: {error}"))??;
        let path = std::path::absolute(&path).unwrap_or(path);
        let mut reply = format!(
            "Exported {} with {} to {}.",
            count(bars as usize, "bar"),
            count(playing, "track"),
            path.display()
        );
        if muted > 0 {
            reply.push_str(&format!(" Left out {}.", count(muted, "muted track")));
        }
        Ok(reply)
    }

    /// Sets the tempo. It changes at once, even while playing.
    #[tool]
    fn set_tempo(
        &self,
        Parameters(SetTempo { bpm }): Parameters<SetTempo>,
    ) -> Result<String, String> {
        let tempo = Bpm::try_from(bpm).map_err(|error| error.to_string())?;
        self.apply(Edit::SetTempo(tempo))?;
        Ok(format!("The tempo is {bpm} BPM."))
    }

    /// Sets the time signature, and with it the bar length. Changes to
    /// clips take effect on bar lines.
    #[tool]
    fn set_time_signature(
        &self,
        Parameters(SetTimeSignature {
            numerator,
            denominator,
        }): Parameters<SetTimeSignature>,
    ) -> Result<String, String> {
        let time_signature = TimeSignature::new(numerator, denominator).ok_or_else(|| {
            format!(
                "{numerator}/{denominator} is not a time signature: beats per bar must be 1-32 \
                 and the beat 1, 2, 4, 8, 16 or 32"
            )
        })?;
        self.apply(Edit::SetTimeSignature(time_signature))?;
        Ok(format!("The time signature is {time_signature}."))
    }

    /// Adds a track that loops one empty bar, with a program to set the
    /// sound of its channel, and a bend range for its pitch bend, if you
    /// like. Returns the track: pass its id to the other tools.
    #[tool]
    fn add_track(
        &self,
        Parameters(request): Parameters<AddTrack>,
    ) -> Result<Json<TrackView>, String> {
        let port = self.port_id(request.port.as_deref())?;
        let mut session = self.session();
        let device = find_device(&session, request.device.as_deref())?;
        let channel = match request.channel {
            Some(number) => parse_channel(number)?,
            None => device
                .map(|device| device.default_channel)
                .unwrap_or_default(),
        };
        let program = programs::requested(
            request.program.as_ref(),
            request.bank_msb,
            request.bank_lsb,
            device,
        )
        .map_err(|error| error.to_string())?;
        let bend_range = request.bend_range.map(parse_bend_range).transpose()?;
        let bar = session.project().time_signature.bar_length();
        let track = Track {
            id: session.project().next_track_id(),
            name: request.name,
            device: request.device,
            target: OutputTarget { port, channel },
            muted: false,
            program,
            bend_range,
            clip: Clip::new(bar, Vec::new()).map_err(|error| error.to_string())?,
        };
        let view = TrackView::new(&track, &self.ports, session.devices());
        session.apply(Edit::AddTrack(track)).map_err(describe)?;
        Ok(Json(view))
    }

    /// Renames a track, moves it to another port, channel or device, or
    /// changes its program or bend range, as one edit. Its clip stays as it
    /// is. While playing, the change takes effect at the next bar. Returns
    /// the track.
    #[tool]
    fn edit_track(
        &self,
        Parameters(request): Parameters<EditTrack>,
    ) -> Result<Json<TrackView>, String> {
        let EditTrack {
            track,
            name,
            port,
            channel,
            device,
            program,
            bank_msb,
            bank_lsb,
            bend_range,
        } = request;
        let banked = bank_msb.is_some() || bank_lsb.is_some();
        let unchanged = [
            name.is_none(),
            port.is_none(),
            channel.is_none(),
            device.is_none(),
            program.is_none() && !banked,
            bend_range.is_none(),
        ];
        if unchanged.into_iter().all(|unchanged| unchanged) {
            return Err(
                "give at least one of name, port, channel, device, program or \
                 bend_range to change"
                    .into(),
            );
        }
        let port = port
            .as_deref()
            .map(|port| self.port_id(Some(port)))
            .transpose()?;
        let channel = channel.map(parse_channel).transpose()?;
        let bend_range = bend_range
            .map(|range| range.map(parse_bend_range).transpose())
            .transpose()?;
        let mut session = self.session();
        let mut edited = find_track(&session, track)?;
        if let Some(name) = name {
            edited.name = name;
        }
        if let Some(port) = port {
            edited.target.port = port;
        }
        if let Some(channel) = channel {
            edited.target.channel = channel;
        }
        if device.is_some() {
            edited.device = device;
        }
        // Names are the track's device's, the new one if it changes.
        let profile = find_device(&session, edited.device.as_deref())?;
        let requested = programs::requested(
            program.as_ref().and_then(Option::as_ref),
            bank_msb,
            bank_lsb,
            profile,
        )
        .map_err(|error| error.to_string())?;
        // Missing keeps the program and the bend range; null clears them.
        if program.is_some() {
            edited.program = requested;
        }
        if let Some(bend_range) = bend_range {
            edited.bend_range = bend_range;
        }
        let view = TrackView::new(&edited, &self.ports, session.devices());
        session
            .apply(Edit::ReplaceTrack(edited))
            .map_err(describe)?;
        Ok(Json(view))
    }

    /// Removes a track. While playing, it stops at the next bar. undo
    /// brings it back.
    #[tool]
    fn remove_track(
        &self,
        Parameters(TrackRef { track }): Parameters<TrackRef>,
    ) -> Result<String, String> {
        self.apply(Edit::RemoveTrack(TrackId(track)))?;
        Ok(format!("Removed track {track}."))
    }

    /// Mutes or unmutes a track, at the next bar while playing. A muted
    /// track keeps its clip but plays nothing.
    #[tool]
    fn set_muted(
        &self,
        Parameters(SetMuted { track, muted }): Parameters<SetMuted>,
    ) -> Result<String, String> {
        self.apply(Edit::SetMuted {
            track: TrackId(track),
            muted,
        })?;
        let state = if muted { "muted" } else { "unmuted" };
        Ok(format!("Track {track} is {state}."))
    }

    /// Replaces a track's notes with a step pattern: one step string per
    /// voice, the usual way to write drums. The track's controls stay where
    /// they are, cut off by a shorter loop; to fill a longer one, set them
    /// again with set_controls. While playing, the new pattern starts at the
    /// next bar. Returns the track with its pattern as it will play.
    #[tool]
    fn set_steps(
        &self,
        Parameters(request): Parameters<SetSteps>,
    ) -> Result<Json<TrackView>, String> {
        self.replace_clip(request.track, |clip, device, _| {
            let notes = pattern::clip_from_steps(&request.steps, request.steps_per_beat, device)
                .map_err(|error| error.to_string())?;
            controls::keep_controls(clip, notes).map_err(|error| error.to_string())
        })
    }

    /// Replaces a track's notes, for basslines, chords and melodies. Name
    /// each note by pitch, such as "C4" (middle C), "F#2" or "Bb3", by MIDI
    /// note number, or by a voice of the track's device, and time it in
    /// beats from the start of the loop. For example, a bass figure:
    /// {"track": 2, "notes": [{"note": "C2", "start": 0, "length": 0.5},
    /// {"note": "Eb2", "start": 1.5, "length": 0.5}]}. The track's controls
    /// stay where they are, cut off by a shorter loop; to fill a longer one,
    /// set them again with set_controls. While playing, the new clip starts
    /// at the next bar. Returns the track as it will play: as step strings
    /// if the notes fit a step grid, otherwise as notes.
    #[tool]
    fn set_notes(
        &self,
        Parameters(request): Parameters<SetNotes>,
    ) -> Result<Json<TrackView>, String> {
        self.replace_clip(request.track, |clip, device, project| {
            let bar = project.time_signature.bar_length();
            let notes = notes::clip_from_notes(&request.notes, request.loop_beats, bar, device)
                .map_err(|error| error.to_string())?;
            controls::keep_controls(clip, notes).map_err(|error| error.to_string())
        })
    }

    /// Sets how a track moves its synth's controls, such as the mod wheel or
    /// a filter's cutoff, by sending MIDI control changes (CC) through the
    /// loop, and how it bends pitch (pitch_bend) and presses on the keys
    /// (pressure, channel aftertouch). A track can send controls alone,
    /// with no notes: modulation for a synth that plays its own notes, for
    /// example. Each control is a list of points, values at times in beats,
    /// that it jumps to or ramps to from the point before. A two-bar
    /// mod-wheel swell: {"track": 3, "controls": {"modulation": {"points":
    /// [{"at": 0, "value": 0}, {"at": 8, "value": 127, "ramp": "linear"}]}}}.
    /// A sine-wave LFO on the cutoff, once a beat: {"track": 3, "controls":
    /// {"cutoff": {"points": [{"at": 0, "value": 32}, {"at": 0.5, "value":
    /// 96, "ramp": "smooth"}, {"at": 1, "value": 32, "ramp": "smooth"}],
    /// "repeat_beats": 1}}}. A bend up to the top of the bend range over
    /// beat 4, back at the centre for the next bar: {"track": 3, "controls":
    /// {"pitch_bend": {"points": [{"at": 0, "value": 0}, {"at": 3, "value":
    /// 0}, {"at": 4, "value": 8191, "ramp": "smooth"}]}}}. The track's notes
    /// stay, and so do the controls the call leaves out; a control with no
    /// points is removed. While playing, the change takes effect at the next
    /// bar. Returns the track as it will play.
    #[tool]
    fn set_controls(
        &self,
        Parameters(request): Parameters<SetControls>,
    ) -> Result<Json<TrackView>, String> {
        self.replace_clip(request.track, |clip, device, project| {
            let bar = project.time_signature.bar_length();
            controls::clip_with_controls(clip, &request.controls, request.loop_beats, bar, device)
                .map_err(|error| error.to_string())
        })
    }

    /// Starts playing from bar 1, or from `from_bar`. Every track loops its
    /// clip until stop. Gear on ports that get MIDI clock starts with it,
    /// from the same bar.
    #[tool]
    fn play(&self, Parameters(Play { from_bar }): Parameters<Play>) -> Result<String, String> {
        let bar = from_bar.unwrap_or(1);
        let mut session = self.session();
        let result = match bar {
            0 => return Err("bars count from 1".into()),
            1 => session.command(TransportCommand::Play),
            _ => {
                let bar_length = session.project().time_signature.bar_length().0;
                let start = Ticks(u64::from(bar - 1) * bar_length);
                session
                    .command(TransportCommand::Locate(start))
                    .and_then(|()| session.command(TransportCommand::Continue))
            }
        };
        result.map_err(|error| error.to_string())?;
        Ok(format!("Playing from bar {bar}."))
    }

    /// Stops playing and ends every sounding note, releasing pedals,
    /// recentring pitch bend and lifting pressure. The position stays. Gear
    /// on ports that get MIDI clock stops too.
    #[tool]
    fn stop(&self) -> Result<String, String> {
        self.command(TransportCommand::Stop)?;
        Ok("Stopped.".into())
    }

    /// Ends every sounding note on every port at once, and puts the pedals,
    /// pitch bend and pressure back at rest on every channel, for stuck
    /// notes and bends. Playback carries on.
    #[tool]
    fn panic(&self) -> Result<String, String> {
        self.command(TransportCommand::Panic)?;
        Ok(
            "Sent note-offs, then pedal releases, pitch bend and pressure at rest, All Sound Off \
            and All Notes Off on every channel."
                .into(),
        )
    }

    /// Undoes the last edit.
    #[tool]
    fn undo(&self) -> String {
        if self.session().undo() {
            "Undid the last edit.".into()
        } else {
            "Nothing to undo.".into()
        }
    }

    /// Redoes the last undone edit.
    #[tool]
    fn redo(&self) -> String {
        if self.session().redo() {
            "Redid the edit.".into()
        } else {
            "Nothing to redo.".into()
        }
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "treason",
    instructions = "treason is a MIDI sequencer that plays external drum machines and synths; it \
makes no sound itself. Call get_guide once before you start: it explains how everything works, \
with examples of every call. A project has a tempo, a time signature and tracks. Each track \
loops its own clip on one MIDI port and channel, so clips of different lengths give polymeter. \
Typical flow: list_devices and list_ports, add_track with a device and a program for its \
sound, set_steps for drums or set_notes for melodic parts, set_controls for modulation and \
other controls (MIDI CC), then play. Edits apply while playing: tempo at once, everything else at the next bar. undo reverts an \
edit. save_project keeps the work under a name, and load_project brings it back; status says \
whether there are unsaved changes. export_midi writes a MIDI file that a DAW can open."
)]
impl ServerHandler for TreasonServer {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use rmcp::ServiceExt;
    use tokio::io::{
        AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
    };
    use treason_core::midi::U7;
    use treason_core::model::Project;
    use treason_core::playback::{PlaybackError, PlaybackStatus};
    use treason_devices::DeviceRegistry;

    use super::*;
    use crate::dto::{NoteRef, NoteSpec, ProgramRef, ProgramView, Ramp};

    /// Accepts everything and plays nothing.
    struct Silent;

    impl Playback for Silent {
        fn publish(&mut self, _project: Arc<Project>) {}

        fn command(&mut self, _command: TransportCommand) -> Result<(), PlaybackError> {
            Ok(())
        }

        fn status(&self) -> PlaybackStatus {
            PlaybackStatus::default()
        }
    }

    /// Writes what it was asked to export instead of MIDI, and refuses a
    /// song titled "huge" as too big.
    struct FakeExport;

    impl MidiExport for FakeExport {
        fn export(
            &self,
            project: &Project,
            title: &str,
            bars: u32,
        ) -> Result<Vec<u8>, ExportError> {
            if title == "huge" {
                return Err(ExportError::TooManyEvents(2_000_000));
            }
            Ok(format!("{title}: {} tracks, {bars} bars", project.tracks.len()).into_bytes())
        }
    }

    /// A directory for one test, removed when the test ends.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(test: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("treason-mcp-{}-{test}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A server that saves projects in `dir`, and exports to `exports` in
    /// it.
    fn server_in(dir: &Path) -> TreasonServer {
        let devices_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../devices");
        let devices = DeviceRegistry::load_dir(&devices_dir).unwrap();
        let playback: Box<dyn Playback + Send> = Box::new(Silent);
        let session = Session::new(Arc::new(Project::default()), devices, playback);
        TreasonServer::new(
            session,
            vec!["out".into(), "synth".into()],
            ProjectStore::new(dir),
            ExportStore::new(dir.join("exports")),
            Arc::new(FakeExport),
        )
    }

    /// A server for tests that don't save.
    fn server() -> TreasonServer {
        server_in(&std::env::temp_dir().join("treason-mcp-unsaved"))
    }

    #[test]
    fn every_tool_is_described() {
        let tools = server().tool_router.list_all();
        let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "add_track",
                "edit_track",
                "export_midi",
                "get_guide",
                "get_project",
                "list_devices",
                "list_ports",
                "list_projects",
                "load_project",
                "panic",
                "play",
                "redo",
                "remove_track",
                "save_project",
                "set_controls",
                "set_muted",
                "set_notes",
                "set_steps",
                "set_tempo",
                "set_time_signature",
                "status",
                "stop",
                "undo",
            ]
        );
        for tool in &tools {
            assert!(
                tool.description.as_deref().is_some_and(|d| !d.is_empty()),
                "{} has no description",
                tool.name
            );
        }
        let get_project = tools
            .iter()
            .find(|tool| tool.name == "get_project")
            .unwrap();
        assert!(get_project.output_schema.is_some());
    }

    #[test]
    fn the_guide_names_every_tool() {
        for tool in server().tool_router.list_all() {
            assert!(
                GUIDE.contains(&format!("`{}`", tool.name)),
                "docs/guide.md doesn't mention `{}`",
                tool.name
            );
        }
    }

    #[test]
    fn the_guide_fits_in_one_tool_result() {
        // MCP clients cut long tool results short: Claude Code at 25,000
        // tokens by default, with a warning from 10,000. 40,000 bytes of
        // English is about 10,000 tokens.
        assert!(
            GUIDE.len() <= 40_000,
            "docs/guide.md has {} bytes",
            GUIDE.len()
        );
    }

    /// The guide's example calls, in order. They sit in ```mcp blocks, each
    /// a tool's name and then its arguments in JSON, which may run over
    /// several lines.
    fn guide_calls() -> Vec<(String, serde_json::Value)> {
        let mut calls = Vec::new();
        for block in GUIDE.split("```mcp\n").skip(1) {
            let block = &block[..block.find("```").expect("every ```mcp block ends")];
            let mut rest = block.trim_start();
            while !rest.is_empty() {
                let (tool, arguments) = rest
                    .split_once(' ')
                    .unwrap_or_else(|| panic!("`{rest}` is not a tool's name and arguments"));
                let mut values =
                    serde_json::Deserializer::from_str(arguments).into_iter::<serde_json::Value>();
                let value = values
                    .next()
                    .and_then(Result::ok)
                    .unwrap_or_else(|| panic!("{tool}'s arguments are not JSON"));
                calls.push((tool.to_owned(), value));
                rest = arguments[values.byte_offset()..].trim_start();
            }
        }
        calls
    }

    /// An agent's end of an MCP session: JSON-RPC messages, one to a line.
    struct Agent {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        writer: WriteHalf<DuplexStream>,
        id: u64,
    }

    impl Agent {
        async fn send(&mut self, message: serde_json::Value) {
            let line = format!("{message}\n");
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        /// Sends a request, and returns the result the server answers with.
        async fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            self.id += 1;
            let request = serde_json::json!({
                "jsonrpc": "2.0", "id": self.id, "method": method, "params": params,
            });
            self.send(request).await;
            loop {
                let line = self.lines.next_line().await.unwrap().expect("an answer");
                let answer: serde_json::Value = serde_json::from_str(&line).unwrap();
                if answer["id"] == self.id {
                    return answer
                        .get("result")
                        .cloned()
                        .unwrap_or_else(|| panic!("{method} {params}: {answer}"));
                }
            }
        }
    }

    #[tokio::test]
    async fn the_guides_examples_run_in_order() {
        let dir = TestDir::new("guide");
        let (agent, transport) = tokio::io::duplex(1 << 20);
        let server = server_in(&dir.0);
        let service =
            tokio::spawn(async move { server.serve(transport).await.unwrap().waiting().await });
        let (reader, writer) = tokio::io::split(agent);
        let mut agent = Agent {
            lines: BufReader::new(reader).lines(),
            writer,
            id: 0,
        };
        let client = serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "guide", "version": "1"},
        });
        agent.request("initialize", client).await;
        let initialized = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized",
        });
        agent.send(initialized).await;

        let calls = guide_calls();
        assert!(
            calls.len() >= 40,
            "found only {} example calls",
            calls.len()
        );
        for (tool, arguments) in calls {
            let call = serde_json::json!({"name": tool, "arguments": arguments});
            let result = agent.request("tools/call", call).await;
            assert_ne!(result["isError"], true, "{tool} {arguments}: {result}");
        }
        drop(agent);
        service.await.unwrap().unwrap();
    }

    #[test]
    fn the_tool_reference_is_up_to_date() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/tools.md");
        let reference = crate::reference::render(&server().tool_router.list_all());
        if std::env::var_os("TREASON_UPDATE_DOCS").is_some() {
            std::fs::write(&path, &reference).unwrap();
        }
        let written = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            written == reference,
            "docs/tools.md is out of date: update it with \
             TREASON_UPDATE_DOCS=1 cargo test -p treason-mcp"
        );
    }

    #[test]
    fn tracks_take_the_device_channel_and_undo() {
        let server = server();
        let request = AddTrack {
            name: "drums".into(),
            port: None,
            device: Some("gm-drums".into()),
            channel: None,
            program: None,
            bank_msb: None,
            bank_lsb: None,
            bend_range: None,
        };
        let Json(track) = server.add_track(Parameters(request)).unwrap();
        assert_eq!(
            (track.id, track.channel, track.port.as_str()),
            (1, 10, "out")
        );
        assert_eq!(track.loop_beats, 4.0);

        let Json(project) = server.get_project();
        assert_eq!(project.tracks, [track]);
        server.undo();
        assert!(server.get_project().0.tracks.is_empty());
    }

    #[test]
    fn bad_requests_say_how_to_fix_them() {
        let server = server();
        let error = server
            .set_tempo(Parameters(SetTempo { bpm: 1000.0 }))
            .unwrap_err();
        assert!(error.contains("between 20 and 400"), "{error}");

        let request = AddTrack {
            name: "bass".into(),
            port: Some("nowhere".into()),
            device: None,
            channel: None,
            program: None,
            bank_msb: None,
            bank_lsb: None,
            bend_range: None,
        };
        let Err(error) = server.add_track(Parameters(request)) else {
            panic!("added a track on a port that doesn't exist");
        };
        assert!(error.contains("list_ports"), "{error}");

        let error = server
            .play(Parameters(Play { from_bar: Some(0) }))
            .unwrap_err();
        assert!(error.contains("from 1"), "{error}");
    }

    /// The error a tool call failed with.
    fn rejected<T>(result: Result<T, String>) -> String {
        match result {
            Ok(_) => panic!("the call was accepted"),
            Err(error) => error,
        }
    }

    fn add(server: &TreasonServer, device: Option<&str>) -> u32 {
        let request = AddTrack {
            name: "drums".into(),
            port: None,
            device: device.map(str::to_owned),
            channel: None,
            program: None,
            bank_msb: None,
            bank_lsb: None,
            bend_range: None,
        };
        server.add_track(Parameters(request)).unwrap().0.id
    }

    fn steps(track: u32, voices: &[(&str, &str)]) -> Parameters<SetSteps> {
        Parameters(SetSteps {
            track,
            steps: voices
                .iter()
                .map(|(voice, steps)| ((*voice).to_owned(), (*steps).to_owned()))
                .collect(),
            steps_per_beat: None,
        })
    }

    #[test]
    fn step_patterns_replace_the_clip_and_read_back() {
        let server = server();
        let track = add(&server, Some("gm-drums"));
        let request = steps(
            track,
            &[
                ("kick", "x...x...x...x..."),
                ("sd", "....X.......X..."),
                ("hh", "..x."),
            ],
        );
        let Json(view) = server.set_steps(request).unwrap();
        assert_eq!((view.loop_beats, view.events), (4.0, 10));
        let pattern = view.pattern.clone().expect("a step pattern reads back");
        assert_eq!(pattern.steps_per_beat, 4);
        assert_eq!(pattern.steps["kick"], "x... x... x... x...");
        assert_eq!(pattern.steps["snare"], ".... X... .... X...");
        assert_eq!(pattern.steps["closed_hat"], "..x. ..x. ..x. ..x.");
        assert_eq!(server.get_project().0.tracks, [view]);

        server.undo();
        let empty = &server.get_project().0.tracks[0];
        assert_eq!((empty.events, empty.pattern.is_none()), (0, true));
    }

    #[test]
    fn tracks_without_a_device_take_note_numbers() {
        let server = server();
        let track = add(&server, None);
        let mut request = steps(track, &[("60", "x..x..x..x..")]);
        request.0.steps_per_beat = Some(3);
        let Json(view) = server.set_steps(request).unwrap();
        assert_eq!(view.loop_beats, 4.0);
        assert_eq!(view.pattern.unwrap().steps["60"], "x.. x.. x.. x..");

        let error = rejected(server.set_steps(steps(track, &[("kick", "x...")])));
        assert!(error.contains("edit_track"), "{error}");
    }

    #[test]
    fn bad_step_patterns_say_how_to_fix_them() {
        let server = server();
        let track = add(&server, Some("gm-drums"));
        let cases = [
            (steps(99, &[("kick", "x...")]), "see get_project"),
            (steps(track, &[("kik", "x...")]), "has no voice `kik`"),
            (steps(track, &[("kick", "x..o")]), "at step 4"),
            (
                steps(track, &[("kick", "x..."), ("bd", "x...")]),
                "both note 36",
            ),
            (
                steps(track, &[("kick", "x...x...x...x..."), ("snare", "x.....")]),
                "doesn't divide the 16-step loop",
            ),
        ];
        for (request, expected) in cases {
            let error = rejected(server.set_steps(request));
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        let mut request = steps(track, &[("kick", "x...")]);
        request.0.steps_per_beat = Some(7);
        assert!(rejected(server.set_steps(request)).contains("steps_per_beat must be"));
        assert_eq!(
            server.get_project().0.tracks[0].events,
            0,
            "nothing changed"
        );
    }

    fn notes(track: u32, notes: &[(&str, f64, f64)]) -> Parameters<SetNotes> {
        Parameters(SetNotes {
            track,
            notes: notes
                .iter()
                .map(|&(note, start, length)| NoteSpec {
                    note: NoteRef::Name(note.into()),
                    start,
                    length,
                    velocity: None,
                })
                .collect(),
            loop_beats: None,
        })
    }

    /// A track's notes as (note, start, length, velocity).
    fn read_notes(view: &TrackView) -> Vec<(&str, f64, f64, u8)> {
        view.notes
            .as_deref()
            .expect("notes read back")
            .iter()
            .map(|note| (note.note.as_str(), note.start, note.length, note.velocity))
            .collect()
    }

    #[test]
    fn melodies_replace_the_clip_and_read_back_as_notes() {
        let server = server();
        let track = add(&server, None);
        let mut request = notes(
            track,
            &[("C2", 0.0, 0.5), ("Eb2", 1.5, 0.5), ("G2", 3.0, 1.0)],
        );
        request.0.notes[2].velocity = Some(90);
        let Json(view) = server.set_notes(request).unwrap();
        assert_eq!((view.loop_beats, view.events), (4.0, 3));
        assert!(view.pattern.is_none());
        assert_eq!(
            read_notes(&view),
            [
                ("C2", 0.0, 0.5, 100),
                ("D#2", 1.5, 0.5, 100),
                ("G2", 3.0, 1.0, 90)
            ]
        );
        assert_eq!(server.get_project().0.tracks, [view]);

        server.undo();
        let empty = &server.get_project().0.tracks[0];
        assert_eq!((empty.events, empty.notes.is_none()), (0, true));
    }

    #[test]
    fn notes_read_back_with_the_device_voice_names() {
        let server = server();
        let track = add(&server, Some("gm-drums"));
        // A flam: off the step grid, so it reads back as notes. C4 is the
        // kit's high bongo, while C6 is past its last voice.
        let request = notes(
            track,
            &[
                ("sd", 1.0, 0.25),
                ("snare", 0.96875, 0.03125),
                ("C4", 2.0, 1.0),
                ("C6", 3.0, 1.0),
            ],
        );
        let Json(view) = server.set_notes(request).unwrap();
        assert_eq!(
            read_notes(&view),
            [
                ("snare", 0.96875, 0.03125, 100),
                ("snare", 1.0, 0.25, 100),
                ("high_bongo", 2.0, 1.0, 100),
                ("C6", 3.0, 1.0, 100),
            ]
        );
    }

    #[test]
    fn notes_round_trip_when_a_voice_or_alias_shadows_a_pitch() {
        for (voice, aliases) in [
            ("A1", BTreeMap::new()),
            ("low", BTreeMap::from([("A1".into(), "low".into())])),
        ] {
            let device = DeviceProfile {
                id: "pitched".into(),
                name: "Pitched".into(),
                manufacturer: None,
                default_channel: Channel::from_number(1).unwrap(),
                voices: BTreeMap::from([(voice.into(), U7::new(40).unwrap())]),
                aliases,
                programs: BTreeMap::new(),
                params: BTreeMap::new(),
            };
            device.validate().unwrap();
            let mut devices = DeviceRegistry::new();
            devices.insert(device);
            let playback: Box<dyn Playback + Send> = Box::new(Silent);
            let session = Session::new(Arc::new(Project::default()), devices, playback);
            let unsaved = std::env::temp_dir().join("treason-mcp-unsaved");
            let server = TreasonServer::new(
                session,
                vec!["out".into()],
                ProjectStore::new(&unsaved),
                ExportStore::new(unsaved.join("exports")),
                Arc::new(FakeExport),
            );
            let track = add(&server, Some("pitched"));
            let mut request = notes(
                track,
                &[("33", 0.0, 0.5), ("A1", 1.0, 0.5), ("A2", 2.0, 0.5)],
            );
            request.0.notes[0].note = NoteRef::Number(33);
            // Ensure the clip reads back as notes rather than step strings.
            request.0.notes[0].velocity = Some(90);
            server.set_notes(request).unwrap();
            let original = server.session().project().tracks[0].clip.clone();
            let view = server.get_project().0.tracks.remove(0);
            let notes = serde_json::from_str(
                &serde_json::to_string(view.notes.as_ref().expect("notes read back")).unwrap(),
            )
            .unwrap();
            let Json(rewritten) = server
                .set_notes(Parameters(SetNotes {
                    track,
                    notes,
                    loop_beats: Some(view.loop_beats),
                }))
                .unwrap();
            assert_eq!(
                server.session().project().tracks[0].clip,
                original,
                "voice {voice}: unchanged readback must preserve MIDI notes"
            );
            assert_eq!(rewritten, view);
            assert_eq!(
                read_notes(&view),
                [
                    ("33", 0.0, 0.5, 90),
                    (voice, 1.0, 0.5, 100),
                    ("A2", 2.0, 0.5, 100),
                ]
            );
        }
    }

    #[test]
    fn notes_on_a_step_grid_read_back_as_steps() {
        let server = server();
        let track = add(&server, None);
        let Json(view) = server
            .set_notes(notes(track, &[("C2", 0.0, 0.25), ("C2", 2.0, 0.25)]))
            .unwrap();
        assert!(view.notes.is_none());
        assert_eq!(view.pattern.unwrap().steps["36"], "x... .... x... ....");
    }

    #[test]
    fn bad_notes_say_how_to_fix_them() {
        let server = server();
        let track = add(&server, None);
        let cases = [
            (notes(99, &[("C2", 0.0, 1.0)]), "see get_project"),
            (notes(track, &[("kick", 0.0, 1.0)]), "edit_track"),
            (
                notes(track, &[("C2", 0.0, 1.0), ("H2", 1.0, 1.0)]),
                "note 2 (`H2`)",
            ),
            (
                notes(track, &[("C2", 0.0, 2.0), ("C2", 1.0, 1.0)]),
                "notes 1 and 2 are both `C2` and overlap",
            ),
            (notes(track, &[]), "give at least one note"),
        ];
        for (request, expected) in cases {
            let error = rejected(server.set_notes(request));
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        let mut request = notes(track, &[("C2", 0.0, 1.0)]);
        request.0.loop_beats = Some(0.0);
        assert!(rejected(server.set_notes(request)).contains("loop_beats must be"));
        assert_eq!(
            server.get_project().0.tracks[0].events,
            0,
            "nothing changed"
        );
    }

    fn set_controls(
        track: u32,
        controls: &str,
        loop_beats: Option<f64>,
    ) -> Parameters<SetControls> {
        Parameters(SetControls {
            track,
            controls: serde_json::from_str(controls).unwrap(),
            loop_beats,
        })
    }

    #[test]
    fn a_track_can_send_controls_alone() {
        let server = server();
        let track = add(&server, None);
        // The tool's own examples: a two-bar swell, then a sine-wave LFO.
        let swell = r#"{"modulation": {"points": [{"at": 0, "value": 0},
            {"at": 8, "value": 127, "ramp": "linear"}]}}"#;
        let Json(view) = server
            .set_controls(set_controls(track, swell, None))
            .unwrap();
        assert_eq!(view.loop_beats, 8.0);
        assert!(view.pattern.is_none() && view.notes.is_none());
        let controls = view.controls.as_ref().expect("controls read back");
        let points: Vec<_> = controls["modulation"]
            .points
            .iter()
            .map(|point| (point.at, point.value, point.ramp))
            .collect();
        assert_eq!(points, [(0.0, 0, None), (8.0, 127, Some(Ramp::Linear))]);
        assert_eq!(view.events, 128, "a change for each value of the swell");
        assert_eq!(server.get_project().0.tracks, [view]);

        let lfo = r#"{"cutoff": {"points": [{"at": 0, "value": 32},
            {"at": 0.5, "value": 96, "ramp": "smooth"},
            {"at": 1, "value": 32, "ramp": "smooth"}], "repeat_beats": 1}}"#;
        let Json(view) = server.set_controls(set_controls(track, lfo, None)).unwrap();
        // The swell's loop stays, and the LFO cycles through it.
        assert_eq!(view.loop_beats, 8.0);
        let controls = view.controls.unwrap();
        assert_eq!(controls.len(), 2);
        assert_eq!(controls["cutoff"].points.len(), 17);

        server.undo();
        server.undo();
        let empty = &server.get_project().0.tracks[0];
        assert_eq!((empty.events, empty.controls.is_none()), (0, true));
    }

    #[test]
    fn controls_read_back_in_the_form_set_controls_takes() {
        let server = server();
        let track = add(&server, Some("gm-drums"));
        server
            .set_steps(steps(track, &[("kick", "x...x...x...x...")]))
            .unwrap();
        let wobble = r#"{"volume": {"points": [{"at": 0, "value": 100},
            {"at": 0.75, "value": 40, "ramp": "smooth"}, {"at": 1, "value": 80},
            {"at": 2, "value": 100, "ramp": "linear"}], "repeat_beats": 2},
            "1": {"points": [{"at": 0.5, "value": 3}]}}"#;
        server
            .set_controls(set_controls(track, wobble, None))
            .unwrap();
        let original = server.session().project().tracks[0].clip.clone();
        let view = server.get_project().0.tracks.remove(0);
        let controls = view.controls.as_ref().expect("controls read back");
        // Named by the kit's parameter, then the standard name.
        assert_eq!(
            controls.keys().map(String::as_str).collect::<Vec<_>>(),
            ["modulation", "volume"]
        );
        let written = serde_json::to_string(controls).unwrap();
        let Json(rewritten) = server
            .set_controls(set_controls(track, &written, Some(view.loop_beats)))
            .unwrap();
        assert_eq!(server.session().project().tracks[0].clip, original);
        assert_eq!(rewritten, view);
        assert_eq!(view.pattern.unwrap().steps["kick"], "x... x... x... x...");
    }

    #[test]
    fn pitch_bend_and_pressure_read_back_in_the_form_set_controls_takes() {
        let server = server();
        let track = add(&server, None);
        // The tool's own bend, and pressure swelling over the bar.
        let expression = r#"{"pitch_bend": {"points": [{"at": 0, "value": 0},
            {"at": 3, "value": 0}, {"at": 4, "value": 8191, "ramp": "smooth"}]},
            "pressure": {"points": [{"at": 0, "value": 0},
            {"at": 2, "value": 127, "ramp": "linear"}, {"at": 3, "value": 0}]}}"#;
        let Json(view) = server
            .set_controls(set_controls(track, expression, None))
            .unwrap();
        assert_eq!(view.loop_beats, 4.0);
        let controls = view.controls.as_ref().expect("controls read back");
        let points = |name: &str| {
            controls[name]
                .points
                .iter()
                .map(|point| (point.at, point.value, point.ramp))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            points("pitch_bend"),
            [
                (0.0, 0, None),
                (3.0, 0, None),
                (4.0, 8191, Some(Ramp::Smooth))
            ]
        );
        assert_eq!(
            points("pressure"),
            [
                (0.0, 0, None),
                (2.0, 127, Some(Ramp::Linear)),
                (3.0, 0, None)
            ]
        );
        let original = server.session().project().tracks[0].clip.clone();
        let written = serde_json::to_string(controls).unwrap();
        let Json(rewritten) = server
            .set_controls(set_controls(track, &written, Some(view.loop_beats)))
            .unwrap();
        assert_eq!(server.session().project().tracks[0].clip, original);
        assert_eq!(rewritten, view);
        assert_eq!(server.get_project().0.tracks, [view]);
    }

    #[test]
    fn existing_same_tick_controls_round_trip_through_get_project_and_set_controls() {
        use treason_core::midi::U7;
        use treason_core::model::{ClipEvent, EventKind};

        let server = server();
        let track = add(&server, Some("gm-drums"));
        // A valid existing clip can interleave RPN sequences and repeat
        // the same controller and value on one tick.
        let original = Clip::new(
            Ticks(3840),
            [
                (101, 0),
                (100, 0),
                (6, 12),
                (7, 100),
                (100, 2),
                (6, 20),
                (7, 20),
                (7, 20),
            ]
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
        server
            .session()
            .apply(Edit::SetClip {
                track: TrackId(track),
                clip: original.clone(),
            })
            .unwrap();
        let view = server.get_project().0.tracks.remove(0);
        let controls = view.controls.as_ref().unwrap();
        let volume: Vec<_> = controls["volume"]
            .points
            .iter()
            .map(|point| (point.at, point.value))
            .collect();
        assert_eq!(volume, [(0.0, 100), (0.0, 20), (0.0, 20)]);
        let written = serde_json::to_string(controls).unwrap();
        let Json(rewritten) = server
            .set_controls(set_controls(track, &written, Some(view.loop_beats)))
            .unwrap();
        assert_eq!(server.session().project().tracks[0].clip, original);
        assert_eq!(rewritten, view);
    }

    #[test]
    fn writing_notes_or_steps_keeps_the_controls() {
        let server = server();
        let track = add(&server, Some("gm-drums"));
        let pan = r#"{"pan": {"points": [{"at": 0, "value": 0}, {"at": 3, "value": 127}]}}"#;
        server.set_controls(set_controls(track, pan, None)).unwrap();
        let pans = |view: &TrackView| {
            view.controls
                .as_ref()
                .map_or(0, |controls| controls["pan"].points.len())
        };
        let Json(view) = server
            .set_steps(steps(track, &[("kick", "x...x...x...x...")]))
            .unwrap();
        assert_eq!((pans(&view), view.events), (2, 6));
        // Two beats of notes cut off the change at beat 3.
        let mut melody = notes(track, &[("C2", 0.0, 1.0)]);
        melody.0.loop_beats = Some(2.0);
        let Json(view) = server.set_notes(melody).unwrap();
        assert_eq!((pans(&view), view.events), (1, 2));
    }

    #[test]
    fn bad_controls_say_how_to_fix_them() {
        let server = server();
        let track = add(&server, Some("gm-drums"));
        server.set_steps(steps(track, &[("kick", "x...")])).unwrap();
        let point = r#"{"points": [{"at": 0, "value": 64}]}"#;
        let cases = [
            (
                99,
                format!(r#"{{"pan": {point}}}"#),
                None,
                "see get_project",
            ),
            (
                track,
                format!(r#"{{"wobble": {point}}}"#),
                None,
                "not a parameter of gm-drums (chorus, pan, reverb, volume)",
            ),
            (
                track,
                format!(r#"{{"pan": {point}}}"#),
                Some(8.0),
                "leave out loop_beats, or change the loop with set_notes or set_steps",
            ),
            (track, "{}".to_owned(), None, "give at least one control"),
            (
                track,
                r#"{"pitch_bend": {"points": [{"at": 0, "value": 16383}]}}"#.to_owned(),
                None,
                "`pitch_bend`: point 1: value must be -8192 to 8191, with 0 the centre, not 16383",
            ),
        ];
        for (track, controls, loop_beats, expected) in cases {
            let error = rejected(server.set_controls(set_controls(track, &controls, loop_beats)));
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        assert_eq!(
            server.get_project().0.tracks[0].events,
            1,
            "nothing changed"
        );
    }

    #[test]
    fn the_schema_lists_every_standard_control_name() {
        let schema = serde_json::to_value(schemars::schema_for!(SetControls)).unwrap();
        let description = schema["properties"]["controls"]["description"]
            .as_str()
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for (name, number) in treason_devices::controllers::STANDARD {
            assert!(
                description.contains(&format!("{name} ({number}")),
                "{name} ({number}) is missing: {description}"
            );
        }
    }

    #[test]
    fn tracks_move_and_rename_in_one_edit() {
        let server = server();
        let track = add(&server, Some("gm-drums"));
        let bar = steps(track, &[("kick", "x...x...x...x...")]);
        server.set_steps(bar).unwrap();
        let request = EditTrack {
            track,
            name: Some("909".into()),
            port: Some("synth".into()),
            channel: Some(11),
            device: None,
            program: None,
            bank_msb: None,
            bank_lsb: None,
            bend_range: None,
        };
        let Json(view) = server.edit_track(Parameters(request)).unwrap();
        assert_eq!(
            (view.name.as_str(), view.port.as_str(), view.channel),
            ("909", "synth", 11)
        );
        assert_eq!(view.events, 4, "the clip stays");
        assert_eq!(server.get_project().0.tracks, [view]);

        server.undo();
        let view = &server.get_project().0.tracks[0];
        assert_eq!(
            (view.name.as_str(), view.port.as_str(), view.channel),
            ("drums", "out", 10)
        );
    }

    #[test]
    fn bad_track_edits_say_how_to_fix_them() {
        let server = server();
        let track = add(&server, None);
        let unchanged = |track| EditTrack {
            track,
            name: None,
            port: None,
            channel: None,
            device: None,
            program: None,
            bank_msb: None,
            bank_lsb: None,
            bend_range: None,
        };
        let cases = [
            (unchanged(track), "give at least one"),
            (
                EditTrack {
                    name: Some("ghost".into()),
                    ..unchanged(99)
                },
                "see get_project",
            ),
            (
                EditTrack {
                    port: Some("nowhere".into()),
                    ..unchanged(track)
                },
                "see list_ports",
            ),
            (
                EditTrack {
                    channel: Some(17),
                    ..unchanged(track)
                },
                "1-16",
            ),
            (
                EditTrack {
                    device: Some("nope".into()),
                    ..unchanged(track)
                },
                "see list_devices",
            ),
        ];
        for (request, expected) in cases {
            let error = rejected(server.edit_track(Parameters(request)));
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        assert_eq!(server.get_project().0.tracks[0].name, "drums");
    }

    /// A track called `name` on `channel` of device `gm`, with `program`.
    fn add_sound(server: &TreasonServer, name: &str, channel: u8, program: ProgramRef) -> u32 {
        let request = AddTrack {
            name: name.into(),
            port: None,
            device: Some("gm".into()),
            channel: Some(channel),
            program: Some(program),
            bank_msb: None,
            bank_lsb: None,
            bend_range: None,
        };
        server.add_track(Parameters(request)).unwrap().0.id
    }

    /// An edit of `track`'s program, and nothing else.
    fn reprogram(
        track: u32,
        program: Option<Option<ProgramRef>>,
        bank_msb: Option<u8>,
    ) -> EditTrack {
        EditTrack {
            track,
            name: None,
            port: None,
            channel: None,
            device: None,
            program,
            bank_msb,
            bank_lsb: None,
            bend_range: None,
        }
    }

    #[test]
    fn tracks_pick_their_sound_by_program_name_or_number() {
        let server = server();
        let name = |name: &str| ProgramRef::Name(name.into());
        let bass = add_sound(&server, "bass", 2, name("electric_bass_finger"));
        let shown = |server: &TreasonServer| {
            let track = server.get_project().0.tracks[0].clone();
            (track.program, track.bank_msb, track.bank_lsb)
        };
        assert_eq!(
            shown(&server),
            (Some(name("electric_bass_finger")), None, None)
        );

        let cases = [
            // A number reads back by the device's name for it.
            (
                reprogram(bass, Some(Some(ProgramRef::Number(34))), None),
                (Some(name("electric_bass_pick")), None, None),
            ),
            // In a bank, it's another sound, so it stays a number.
            (
                reprogram(bass, Some(Some(ProgramRef::Number(34))), Some(3)),
                (Some(ProgramRef::Number(34)), Some(3), None),
            ),
            // Leaving the program out keeps it.
            (
                EditTrack {
                    name: Some("low end".into()),
                    ..reprogram(bass, None, None)
                },
                (Some(ProgramRef::Number(34)), Some(3), None),
            ),
            (reprogram(bass, Some(None), None), (None, None, None)),
        ];
        for (request, expected) in cases {
            let Json(view) = server.edit_track(Parameters(request)).unwrap();
            assert_eq!((view.program, view.bank_msb, view.bank_lsb), expected);
            assert_eq!(shown(&server), expected);
        }
        server.undo();
        assert_eq!(
            shown(&server),
            (Some(ProgramRef::Number(34)), Some(3), None)
        );

        // A track on the same channel can leave the program to the first.
        let double = AddTrack {
            name: "double".into(),
            port: None,
            device: None,
            channel: Some(2),
            program: None,
            bank_msb: None,
            bank_lsb: None,
            bend_range: None,
        };
        assert!(server.add_track(Parameters(double)).is_ok());

        let Json(devices) = server.list_devices();
        let gm = devices
            .devices
            .iter()
            .find(|device| device.id == "gm")
            .unwrap();
        let program = |program| ProgramView {
            program,
            bank_msb: None,
            bank_lsb: None,
        };
        assert_eq!(gm.programs.len(), 128);
        assert_eq!(gm.programs["electric_bass_finger"], program(33));
        assert_eq!(gm.programs["gunshot"], program(127));
    }

    #[test]
    fn bad_programs_say_how_to_fix_the_call() {
        let server = server();
        let lead = add_sound(&server, "lead", 1, ProgramRef::Name("lead_1_square".into()));
        let drums = add(&server, None);
        let named = |name: &str| Some(Some(ProgramRef::Name(name.into())));
        let cases = [
            // The same channel can have only one program.
            (
                AddTrack {
                    name: "other".into(),
                    port: None,
                    device: Some("gm".into()),
                    channel: Some(1),
                    program: Some(ProgramRef::Number(81)),
                    bank_msb: None,
                    bank_lsb: None,
                    bend_range: None,
                },
                "tracks 1 and 3 play on the same port and channel",
            ),
            (
                AddTrack {
                    name: "other".into(),
                    port: None,
                    device: Some("gm".into()),
                    channel: Some(3),
                    program: Some(ProgramRef::Name("electric_bass".into())),
                    bank_msb: None,
                    bank_lsb: None,
                    bend_range: None,
                },
                "gm has no program `electric_bass`; similar ones are",
            ),
            (
                AddTrack {
                    name: "other".into(),
                    port: None,
                    device: None,
                    channel: Some(3),
                    program: None,
                    bank_msb: Some(1),
                    bank_lsb: None,
                    bend_range: None,
                },
                "give the program too",
            ),
        ];
        for (request, expected) in cases {
            let error = rejected(server.add_track(Parameters(request)));
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        let cases = [
            (reprogram(lead, None, Some(1)), "give the program too"),
            (reprogram(lead, Some(None), Some(1)), "give the program too"),
            (
                reprogram(lead, Some(Some(ProgramRef::Number(128))), None),
                "a manual that numbers programs 1-128",
            ),
            (
                reprogram(lead, named("lead_2_sawtooth"), Some(1)),
                "brings its own bank",
            ),
            (
                reprogram(drums, named("lead_2_sawtooth"), None),
                "the track has no device to name programs",
            ),
            // Names go by the device the edit gives the track.
            (
                EditTrack {
                    device: Some("gm-drums".into()),
                    ..reprogram(lead, named("lead_2_sawtooth"), None)
                },
                "gm-drums has no program `lead_2_sawtooth`",
            ),
            (
                EditTrack {
                    device: Some("nope".into()),
                    ..reprogram(lead, named("lead_2_sawtooth"), None)
                },
                "there is no device `nope`",
            ),
            (
                EditTrack {
                    channel: Some(1),
                    ..reprogram(drums, Some(Some(ProgramRef::Number(81))), None)
                },
                "tracks 1 and 2 play on the same port and channel",
            ),
        ];
        for (request, expected) in cases {
            let error = rejected(server.edit_track(Parameters(request)));
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        let Json(project) = server.get_project();
        let programs: Vec<_> = project
            .tracks
            .iter()
            .map(|track| track.program.clone())
            .collect();
        assert_eq!(
            programs,
            [Some(ProgramRef::Name("lead_1_square".into())), None],
            "nothing changed"
        );
    }

    /// A new track on `channel` with this bend range.
    fn ranged(name: &str, channel: u8, bend_range: Option<u8>) -> AddTrack {
        AddTrack {
            name: name.into(),
            port: None,
            device: None,
            channel: Some(channel),
            program: None,
            bank_msb: None,
            bank_lsb: None,
            bend_range,
        }
    }

    /// An edit of `track`'s bend range, and nothing else.
    fn rerange(track: u32, bend_range: Option<Option<u8>>) -> EditTrack {
        EditTrack {
            bend_range,
            ..reprogram(track, None, None)
        }
    }

    #[test]
    fn tracks_set_their_bend_range_and_null_clears_it() {
        let server = server();
        let Json(view) = server
            .add_track(Parameters(ranged("lead", 1, Some(12))))
            .unwrap();
        assert_eq!(view.bend_range, Some(12));
        let lead = view.id;
        let shown = |server: &TreasonServer| server.get_project().0.tracks[0].bend_range;
        assert_eq!(shown(&server), Some(12));
        let cases = [
            (rerange(lead, Some(Some(2))), Some(2)),
            // Leaving it out keeps it.
            (
                EditTrack {
                    name: Some("solo".into()),
                    ..rerange(lead, None)
                },
                Some(2),
            ),
            (rerange(lead, Some(None)), None),
            (rerange(lead, Some(Some(48))), Some(48)),
        ];
        for (request, expected) in cases {
            let Json(view) = server.edit_track(Parameters(request)).unwrap();
            assert_eq!(view.bend_range, expected);
            assert_eq!(shown(&server), expected);
        }
        server.undo();
        assert_eq!(shown(&server), None);

        // Tracks on the same channel can leave the range to one of them, or
        // give the same one.
        server
            .edit_track(Parameters(rerange(lead, Some(Some(12)))))
            .unwrap();
        for range in [None, Some(12)] {
            assert!(
                server
                    .add_track(Parameters(ranged("double", 1, range)))
                    .is_ok()
            );
        }
    }

    #[test]
    fn bad_bend_ranges_say_how_to_fix_the_call() {
        let server = server();
        let lead = server
            .add_track(Parameters(ranged("lead", 1, Some(12))))
            .unwrap()
            .0
            .id;
        let pad = server
            .add_track(Parameters(ranged("pad", 2, Some(2))))
            .unwrap()
            .0
            .id;
        let clash = |first, second| {
            format!(
                "tracks {first} and {second} play on the same port and channel, which has one \
                 bend range: give them the same one, leave it out of one (bend_range null in \
                 edit_track clears it), or move one to another channel"
            )
        };
        let cases = [
            (
                ranged("other", 3, Some(128)),
                "bend_range must be 0-127 semitones, not 128; most synths take up to 24".to_owned(),
            ),
            (ranged("other", 1, Some(24)), clash(lead, 3)),
        ];
        for (request, expected) in cases {
            let error = rejected(server.add_track(Parameters(request)));
            assert!(error.contains(&expected), "expected {expected:?}: {error}");
        }
        let cases = [
            (
                rerange(lead, Some(Some(200))),
                "bend_range must be 0-127 semitones, not 200".to_owned(),
            ),
            // Moving onto a channel with another range clashes too.
            (
                EditTrack {
                    channel: Some(1),
                    ..rerange(pad, None)
                },
                clash(lead, pad),
            ),
        ];
        for (request, expected) in cases {
            let error = rejected(server.edit_track(Parameters(request)));
            assert!(error.contains(&expected), "expected {expected:?}: {error}");
        }
        let ranges: Vec<_> = server
            .get_project()
            .0
            .tracks
            .iter()
            .map(|track| track.bend_range)
            .collect();
        assert_eq!(ranges, [Some(12), Some(2)], "nothing changed");
    }

    #[test]
    fn edit_track_tells_null_from_a_missing_program_or_bend_range() {
        let range = |json: &str| serde_json::from_str::<EditTrack>(json).unwrap().bend_range;
        assert_eq!(range(r#"{"track": 1}"#), None);
        assert_eq!(range(r#"{"track": 1, "bend_range": null}"#), Some(None));
        assert_eq!(range(r#"{"track": 1, "bend_range": 12}"#), Some(Some(12)));
        let edit = |json: &str| serde_json::from_str::<EditTrack>(json).unwrap().program;
        assert_eq!(edit(r#"{"track": 1}"#), None);
        assert_eq!(edit(r#"{"track": 1, "program": null}"#), Some(None));
        assert_eq!(
            edit(r#"{"track": 1, "program": 5}"#),
            Some(Some(ProgramRef::Number(5)))
        );
        assert_eq!(
            edit(r#"{"track": 1, "program": "pad_2_warm"}"#),
            Some(Some(ProgramRef::Name("pad_2_warm".into())))
        );
        let schema = serde_json::to_value(schemars::schema_for!(EditTrack)).unwrap();
        for field in ["program", "bend_range"] {
            let property = schema["properties"][field].to_string();
            assert!(
                property.contains("null"),
                "agents must see that {field} can be null: {property}"
            );
        }
    }

    fn save(name: Option<&str>, overwrite: bool) -> Parameters<SaveProject> {
        Parameters(SaveProject {
            name: name.map(str::to_owned),
            overwrite,
        })
    }

    fn load(name: &str, discard_changes: bool) -> Parameters<LoadProject> {
        Parameters(LoadProject {
            name: name.to_owned(),
            discard_changes,
        })
    }

    #[tokio::test]
    async fn a_saved_project_loads_back_as_it_was() {
        let dir = TestDir::new("save-load");
        let server = server_in(&dir.0);
        let drums = add(&server, Some("gm-drums"));
        server
            .set_steps(steps(
                drums,
                &[("kick", "x...x...x...x..."), ("snare", "....X...")],
            ))
            .unwrap();
        let request = AddTrack {
            name: "bass".into(),
            port: Some("synth".into()),
            device: None,
            channel: Some(2),
            program: Some(ProgramRef::Number(38)),
            bank_msb: Some(121),
            bank_lsb: Some(1),
            bend_range: None,
        };
        let bass = server.add_track(Parameters(request)).unwrap().0.id;
        server
            .set_notes(notes(
                bass,
                &[("C2", 0.0, 0.75), ("Eb2", 1.5, 0.5), ("G1", 3.25, 0.5)],
            ))
            .unwrap();
        server
            .set_muted(Parameters(SetMuted {
                track: bass,
                muted: true,
            }))
            .unwrap();
        server
            .set_tempo(Parameters(SetTempo { bpm: 124.5 }))
            .unwrap();
        let status = server.status().0;
        assert_eq!((status.project, status.unsaved_changes), (None, true));

        let saved = server
            .save_project(save(Some("acid jam"), false))
            .await
            .unwrap();
        assert_eq!(saved, "Saved the project as `acid jam`.");
        let status = server.status().0;
        assert_eq!(
            (status.project.as_deref(), status.unsaved_changes),
            (Some("acid jam"), false)
        );
        let Json(project) = server.get_project();

        server
            .remove_track(Parameters(TrackRef { track: drums }))
            .unwrap();
        let error = rejected(server.load_project(load("acid jam", false)).await);
        assert!(
            error.contains("save_project") && error.contains("discard_changes: true"),
            "{error}"
        );
        let Json(loaded) = server.load_project(load("acid jam", true)).await.unwrap();
        assert_eq!(loaded, project);
        assert_eq!(server.get_project().0, project);
        assert!(!server.status().0.unsaved_changes);
        assert_eq!(server.undo(), "Nothing to undo.");

        let Json(list) = server.list_projects().await.unwrap();
        assert_eq!((list.projects, list.more), (vec!["acid jam".to_owned()], 0));
    }

    #[tokio::test]
    async fn saving_and_loading_say_how_to_fix_the_call() {
        let dir = TestDir::new("save-load-errors");
        let server = server_in(&dir.0);
        add(&server, None);
        let cases = [
            (
                server.save_project(save(None, false)).await,
                "give save_project a name",
            ),
            (
                server.save_project(save(Some("a/b"), false)).await,
                "letters, digits",
            ),
        ];
        for (result, expected) in cases {
            let error = rejected(result);
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        let error = rejected(server.load_project(load("nothing", true)).await);
        assert!(error.contains("see list_projects"), "{error}");
        let error = rejected(server.load_project(load("../escape", true)).await);
        assert!(error.contains("letters, digits"), "{error}");

        server
            .save_project(save(Some("first"), false))
            .await
            .unwrap();
        server
            .save_project(save(Some("second"), false))
            .await
            .unwrap();
        let error = rejected(server.save_project(save(Some("first"), false)).await);
        assert!(error.contains("overwrite: true"), "{error}");
        server
            .save_project(save(Some("first"), true))
            .await
            .unwrap();
        assert_eq!(server.status().0.project.as_deref(), Some("first"));
        // Saving under the project's own name needs nothing more.
        server.save_project(save(None, false)).await.unwrap();
    }

    fn export(name: Option<&str>, bars: Option<u32>, overwrite: bool) -> Parameters<ExportMidi> {
        Parameters(ExportMidi {
            name: name.map(str::to_owned),
            bars,
            overwrite,
        })
    }

    /// A melody of one note on track `track`, looping every `beats` beats.
    fn melody(track: u32, beats: f64) -> Parameters<SetNotes> {
        Parameters(SetNotes {
            loop_beats: Some(beats),
            ..notes(track, &[("C4", 0.0, 1.0)]).0
        })
    }

    #[tokio::test]
    async fn exports_complete_every_loop_under_the_projects_name() {
        let dir = TestDir::new("export");
        let server = server_in(&dir.0);
        let exports = dir.0.join("exports");
        let file =
            |name: &str| std::fs::read_to_string(exports.join(format!("{name}.mid"))).unwrap();

        // A three-beat loop meets the 4/4 bar line after three bars.
        let lead = add(&server, None);
        server.set_notes(melody(lead, 3.0)).unwrap();
        let reply = server
            .export_midi(export(Some("demo"), None, false))
            .await
            .unwrap();
        let path = exports.join("demo.mid");
        assert_eq!(
            reply,
            format!("Exported 3 bars with 1 track to {}.", path.display())
        );
        assert_eq!(file("demo"), "demo: 1 tracks, 3 bars");

        // The project isn't called demo, so replacing the file takes
        // overwrite.
        let error = rejected(
            server
                .export_midi(export(Some("demo"), Some(8), false))
                .await,
        );
        assert!(error.contains("pass overwrite: true"), "{error}");
        assert_eq!(file("demo"), "demo: 1 tracks, 3 bars");
        server
            .export_midi(export(Some("demo"), Some(8), true))
            .await
            .unwrap();
        assert_eq!(file("demo"), "demo: 1 tracks, 8 bars");

        // Once saved, the project exports under its own name, as often as
        // it likes. The exporter gets muted tracks too, and leaves them out.
        server.save_project(save(Some("jam"), false)).await.unwrap();
        let drums = add(&server, None);
        server
            .set_muted(Parameters(SetMuted {
                track: drums,
                muted: true,
            }))
            .unwrap();
        for bars in [2, 1] {
            let reply = server
                .export_midi(export(None, Some(bars), false))
                .await
                .unwrap();
            assert!(
                reply.ends_with("jam.mid. Left out 1 muted track."),
                "{reply}"
            );
            assert_eq!(
                reply.starts_with("Exported 1 bar with 1 track to "),
                bars == 1,
                "{reply}"
            );
        }
        assert_eq!(file("jam"), "jam: 2 tracks, 1 bars");
    }

    #[tokio::test]
    async fn exporting_says_how_to_fix_the_call() {
        let dir = TestDir::new("export-errors");
        let server = server_in(&dir.0);
        // A 1023-beat loop meets the 4/4 bar line only after 1023 bars.
        let lead = add(&server, None);
        server.set_notes(melody(lead, 1023.0)).unwrap();
        let cases = [
            (export(None, Some(4), false), "give export_midi a name"),
            (export(Some("a/b"), Some(4), false), "letters, digits"),
            (
                export(Some("x"), Some(0), false),
                "bars must be 1 to 1000, not 0",
            ),
            (
                export(Some("x"), Some(1001), false),
                "bars must be 1 to 1000, not 1001",
            ),
            (export(Some("x"), None, false), "pass bars to say how many"),
            (export(Some("huge"), Some(4), false), "pass fewer bars"),
        ];
        for (call, expected) in cases {
            let error = rejected(server.export_midi(call).await);
            assert!(error.contains(expected), "expected {expected:?}: {error}");
        }
        assert!(!dir.0.join("exports").exists());
        server
            .export_midi(export(Some("x"), Some(4), false))
            .await
            .unwrap();
    }
}
