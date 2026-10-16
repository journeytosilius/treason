//! The tools.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use treason_control::{ControlError, Edit, Session};
use treason_core::midi::Channel;
use treason_core::model::{Clip, OutputTarget, PortId, Track, TrackId};
use treason_core::playback::{Playback, TransportCommand};
use treason_core::time::{Bpm, Ticks, TimeSignature};

use crate::dto::{
    AddTrack, DeviceList, DeviceView, EditTrack, Play, PortList, ProjectView, SetMuted, SetNotes,
    SetSteps, SetTempo, SetTimeSignature, StatusView, TrackRef, TrackView,
};
use crate::pattern;

/// What placeholder tools answer.
const NOT_IMPLEMENTED: &str = "not implemented yet in this build of treason";

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
    tool_router: ToolRouter<Self>,
}

impl TreasonServer {
    /// Serves `session`. `ports` names the output ports, in `PortId` order.
    pub fn new(session: Session<Box<dyn Playback + Send>>, ports: Vec<String>) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            ports: ports.into(),
            tool_router: Self::tool_router(),
        }
    }

    fn session(&self) -> MutexGuard<'_, Session<Box<dyn Playback + Send>>> {
        // An edit swaps in a whole new project, so a panic while the lock was
        // held can't have left the session half-changed.
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn apply(&self, edit: Edit) -> Result<(), String> {
        self.session().apply(edit).map_err(describe)
    }

    fn command(&self, command: TransportCommand) -> Result<(), String> {
        self.session().command(command).map_err(describe)
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

/// A rejected edit or command, worded so the agent can fix the call.
fn describe(error: ControlError) -> String {
    match error {
        ControlError::UnknownTrack(id) => format!("there is no track {id}; see get_project"),
        ControlError::UnknownDevice(id) => format!("there is no device `{id}`; see list_devices"),
        other => other.to_string(),
    }
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

#[tool_router]
impl TreasonServer {
    /// Shows the project: tempo, time signature and every track.
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
    /// voice, such as "kick" or "closed_hat".
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

    /// Shows whether the sequencer is playing, where, and how fast.
    #[tool(annotations(read_only_hint = true))]
    fn status(&self) -> Json<StatusView> {
        let session = self.session();
        Json(StatusView::new(session.status(), session.project()))
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

    /// Adds a track that loops one empty bar. Returns the track: pass its id
    /// to the other tools.
    #[tool]
    fn add_track(
        &self,
        Parameters(request): Parameters<AddTrack>,
    ) -> Result<Json<TrackView>, String> {
        let port = self.port_id(request.port.as_deref())?;
        let mut session = self.session();
        let device_channel = match request.device.as_deref() {
            Some(id) => {
                let device = session
                    .devices()
                    .get(id)
                    .ok_or_else(|| format!("there is no device `{id}`; see list_devices"))?;
                Some(device.default_channel)
            }
            None => None,
        };
        let channel = match request.channel {
            Some(number) => parse_channel(number)?,
            None => device_channel.unwrap_or_default(),
        };
        let bar = session.project().time_signature.bar_length();
        let track = Track {
            id: session.project().next_track_id(),
            name: request.name,
            device: request.device,
            target: OutputTarget { port, channel },
            muted: false,
            clip: Clip::new(bar, Vec::new()).map_err(|error| error.to_string())?,
        };
        let view = TrackView::new(&track, &self.ports, session.devices());
        session.apply(Edit::AddTrack(track)).map_err(describe)?;
        Ok(Json(view))
    }

    /// Renames a track or moves it to another port, channel or device, as
    /// one edit. Its clip stays as it is. While playing, the change takes
    /// effect at the next bar. Returns the track.
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
        } = request;
        if name.is_none() && port.is_none() && channel.is_none() && device.is_none() {
            return Err("give at least one of name, port, channel or device to change".into());
        }
        let port = port
            .as_deref()
            .map(|port| self.port_id(Some(port)))
            .transpose()?;
        let channel = channel.map(parse_channel).transpose()?;
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

    /// Replaces a track's clip with a step pattern: one step string per
    /// voice, the usual way to write drums. While playing, the new pattern
    /// starts at the next bar. Returns the track with its pattern as it will
    /// play.
    #[tool]
    fn set_steps(
        &self,
        Parameters(request): Parameters<SetSteps>,
    ) -> Result<Json<TrackView>, String> {
        let mut session = self.session();
        let mut track = find_track(&session, request.track)?;
        let device = track
            .device
            .as_deref()
            .and_then(|id| session.devices().get(id));
        track.clip = pattern::clip_from_steps(&request.steps, request.steps_per_beat, device)
            .map_err(|error| error.to_string())?;
        let view = TrackView::new(&track, &self.ports, session.devices());
        session
            .apply(Edit::SetClip {
                track: track.id,
                clip: track.clip,
            })
            .map_err(describe)?;
        Ok(Json(view))
    }

    /// Replaces a track's clip with notes, for basslines, chords and melodies.
    /// While playing, the new clip starts at the next bar.
    #[tool]
    fn set_notes(&self, Parameters(_): Parameters<SetNotes>) -> Result<String, String> {
        Err(NOT_IMPLEMENTED.into())
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

    /// Stops playing and ends every sounding note. The position stays. Gear
    /// on ports that get MIDI clock stops too.
    #[tool]
    fn stop(&self) -> Result<String, String> {
        self.command(TransportCommand::Stop)?;
        Ok("Stopped.".into())
    }

    /// Ends every sounding note on every port at once, for stuck notes.
    #[tool]
    fn panic(&self) -> Result<String, String> {
        self.command(TransportCommand::Panic)?;
        Ok("Sent note-offs and All Notes Off on every channel.".into())
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
    instructions = "treason is a MIDI sequencer that plays external drum machines and synths; \
it makes no sound itself. A project has a tempo, a time signature and tracks. Each track loops \
its own clip on one MIDI port and channel, so clips of different lengths give polymeter. \
Typical flow: list_devices and list_ports, add_track with a device, set_steps for drums or \
set_notes for melodic parts, then play. Edits apply while playing: tempo at once, everything \
else at the next bar. undo reverts an edit."
)]
impl ServerHandler for TreasonServer {}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use treason_core::model::Project;
    use treason_core::playback::{PlaybackError, PlaybackStatus};
    use treason_devices::DeviceRegistry;

    use super::*;

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

    fn server() -> TreasonServer {
        let devices_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../devices");
        let devices = DeviceRegistry::load_dir(&devices_dir).unwrap();
        let playback: Box<dyn Playback + Send> = Box::new(Silent);
        let session = Session::new(Arc::new(Project::default()), devices, playback);
        TreasonServer::new(session, vec!["out".into(), "synth".into()])
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
                "get_project",
                "list_devices",
                "list_ports",
                "panic",
                "play",
                "redo",
                "remove_track",
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
    fn tracks_take_the_device_channel_and_undo() {
        let server = server();
        let request = AddTrack {
            name: "drums".into(),
            port: None,
            device: Some("gm-drums".into()),
            channel: None,
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
}
