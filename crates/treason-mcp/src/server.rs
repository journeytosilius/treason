//! The tools.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use treason_control::{Edit, Session};
use treason_core::midi::Channel;
use treason_core::model::{Clip, OutputTarget, PortId, Track, TrackId};
use treason_core::playback::{Playback, TransportCommand};
use treason_core::time::{Bpm, Ticks, TimeSignature};

use crate::dto::{
    AddTrack, DeviceList, DeviceView, Play, PortList, ProjectView, SetMuted, SetNotes, SetSteps,
    SetTempo, SetTimeSignature, StatusView, TrackRef, TrackView,
};

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
        self.session()
            .apply(edit)
            .map_err(|error| error.to_string())
    }

    fn command(&self, command: TransportCommand) -> Result<(), String> {
        self.session()
            .command(command)
            .map_err(|error| error.to_string())
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

#[tool_router]
impl TreasonServer {
    /// Shows the project: tempo, time signature and every track.
    #[tool(annotations(read_only_hint = true))]
    fn get_project(&self) -> Json<ProjectView> {
        Json(ProjectView::new(self.session().project(), &self.ports))
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
            Some(number) => Channel::from_number(number)
                .ok_or_else(|| format!("MIDI channel must be 1-16, got {number}"))?,
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
        let view = TrackView::new(&track, &self.ports);
        session
            .apply(Edit::AddTrack(track))
            .map_err(|error| error.to_string())?;
        Ok(Json(view))
    }

    /// Removes a track. undo brings it back.
    #[tool]
    fn remove_track(
        &self,
        Parameters(TrackRef { track }): Parameters<TrackRef>,
    ) -> Result<String, String> {
        self.apply(Edit::RemoveTrack(TrackId(track)))?;
        Ok(format!("Removed track {track}."))
    }

    /// Mutes or unmutes a track. A muted track keeps its clip but plays
    /// nothing.
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
    /// starts at the next bar.
    #[tool]
    fn set_steps(&self, Parameters(_): Parameters<SetSteps>) -> Result<String, String> {
        Err(NOT_IMPLEMENTED.into())
    }

    /// Replaces a track's clip with notes, for basslines, chords and melodies.
    /// While playing, the new clip starts at the next bar.
    #[tool]
    fn set_notes(&self, Parameters(_): Parameters<SetNotes>) -> Result<String, String> {
        Err(NOT_IMPLEMENTED.into())
    }

    /// Starts playing from bar 1, or from `from_bar`. Every track loops its
    /// clip until stop.
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

    /// Stops playing and ends every sounding note. The position stays.
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
set_notes for melodic parts, then play. Edits apply while playing: tempo at once, clips at the \
next bar. undo reverts an edit."
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
}
