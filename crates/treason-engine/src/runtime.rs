//! The engine thread.

use std::io;
use std::mem;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::Guard;
use thiserror::Error;
use treason_core::midi::{Channel, MidiMessage, U7};
use treason_core::model::{PortId, Project, Track};
use treason_core::output::{Delivery, MidiOutput, TimedMessage};
use treason_core::playback::{TransportCommand, TransportState};
use treason_core::time::Ticks;

use crate::clock::Clock;
use crate::link::EngineLink;
use crate::notes::ActiveNotes;
use crate::render::{RenderedEvent, render};
use crate::timebase::Timebase;
use crate::transport::{Transport, TransportEffect};

/// How many events one window can hold before rendering has to allocate.
const RENDER_CAPACITY: usize = 4096;

/// The longest the engine sleeps before checking for commands and new
/// snapshots again. A shorter lookahead shortens it.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// All Notes Off, which panic sends on every channel.
const ALL_NOTES_OFF: U7 = U7::saturating(123);

/// The engine thread's `SCHED_FIFO` priority on Unix, from 1 to 99.
#[cfg(unix)]
const REALTIME_PRIORITY: u8 = 70;

/// Settings for one output port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortSettings {
    /// How long the device takes to respond. Its messages are sent this much
    /// earlier, so every port sounds in time.
    pub latency: Duration,
    /// Whether the port gets MIDI clock and transport messages.
    pub clock: bool,
}

/// How the engine runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// How far ahead the engine renders. Longer rides out scheduling hiccups;
    /// shorter makes commands take effect sooner.
    pub lookahead: Duration,
    /// One entry per output port, in `PortId` order.
    pub ports: Vec<PortSettings>,
    /// Whether to ask the OS for real-time scheduling for the engine thread.
    pub realtime: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            lookahead: Duration::from_millis(20),
            ports: Vec::new(),
            realtime: true,
        }
    }
}

/// Everything the engine thread owns.
///
/// Generic over the output and the clock: [`SystemClock`] and a MIDI port
/// for live playback, `ManualClock` and a recording to render offline or to
/// test.
///
/// [`SystemClock`]: crate::SystemClock
pub struct Engine<O, C> {
    link: EngineLink,
    output: O,
    clock: C,
    config: EngineConfig,
    transport: Transport,
    /// The notes sounding on each port.
    sounding: Vec<ActiveNotes>,
    /// Rendered events not sent yet, in send order.
    rendered: Vec<RenderedEvent>,
    /// Ticks to time, anchored where playback last started.
    timebase: Timebase,
    /// The next tick to render.
    cursor: Ticks,
    /// The snapshot playing.
    project: Arc<Project>,
    /// A newer snapshot, waiting for the next bar line.
    waiting: Option<Arc<Project>>,
    /// The bar line where the playing snapshot takes over, and the snapshot
    /// it replaces, until playback reaches that line.
    switch: Option<(Ticks, Arc<Project>)>,
}

impl<O: MidiOutput, C: Clock> Engine<O, C> {
    /// An engine that plays what arrives through `link` on `output`, timed by
    /// `clock`.
    pub fn new(link: EngineLink, output: O, clock: C, config: EngineConfig) -> Self {
        let project = Guard::into_inner(link.project());
        let timebase = Timebase::new(clock.now(), Ticks::ZERO, project.tempo);
        Self {
            link,
            output,
            clock,
            transport: Transport::new(),
            sounding: vec![ActiveNotes::new(); config.ports.len()],
            rendered: Vec::with_capacity(RENDER_CAPACITY),
            timebase,
            cursor: Ticks::ZERO,
            project,
            waiting: None,
            switch: None,
            config,
        }
    }

    /// Runs on the calling thread until the
    /// [`EngineHandle`](crate::EngineHandle) is dropped, then ends every
    /// sounding note and returns. Returns early if the output fails.
    ///
    /// Each cycle: pick up a new snapshot (tempo at once, track changes at
    /// the next bar line), apply waiting commands, render the lookahead
    /// window, send what is due (or hand it over early to a scheduled
    /// output), report the position, and sleep until the next event or
    /// check.
    pub fn run(mut self) -> Result<(), O::Error> {
        while self.cycle()? {}
        Ok(())
    }

    /// One turn of the loop. Returns false once the handle is gone and
    /// every note has ended.
    fn cycle(&mut self) -> Result<bool, O::Error> {
        let now = self.clock.now();
        if self.link.is_abandoned() {
            self.end_notes(now, self.transport.status().position)?;
            self.output.flush()?;
            return Ok(false);
        }
        // The snapshot comes first, so a pattern published just before Play
        // plays from the top.
        self.check_snapshot();
        while let Some(command) = self.link.next_command() {
            self.command(command, now)?;
        }
        if self.playing() {
            let ahead = self.ahead();
            let horizon = Ticks(self.timebase.tick_at(now + ahead).0 + 1);
            self.render_until(horizon);
            let limit = match self.output.delivery() {
                Delivery::Immediate => now,
                Delivery::Scheduled { .. } => now + ahead,
            };
            self.send_until(limit)?;
            self.transport.advance_to(self.timebase.tick_at(now));
        }
        self.output.flush()?;
        self.link.set_status(self.transport.status());
        let wake = self.wake_time(now);
        self.clock.sleep_until(wake);
        Ok(true)
    }

    fn playing(&self) -> bool {
        self.transport.status().state == TransportState::Playing
    }

    /// How far ahead to render, and to hand over to a scheduled output.
    fn ahead(&self) -> Duration {
        match self.output.delivery() {
            Delivery::Immediate => self.config.lookahead,
            Delivery::Scheduled { lookahead } => self.config.lookahead.min(lookahead),
        }
    }

    /// Picks up a newly published snapshot. While playing, its tempo applies
    /// at once and the rest waits for the next bar line. While stopped, all
    /// of it applies at once.
    fn check_snapshot(&mut self) {
        let latest = self.link.project();
        let newest = self.waiting.as_ref().unwrap_or(&self.project);
        if Arc::ptr_eq(&*latest, newest) {
            return;
        }
        let latest = Guard::into_inner(latest);
        if let Some(superseded) = self.waiting.take() {
            self.link.retire(superseded);
        }
        if !self.playing() {
            self.adopt(latest);
            return;
        }
        if latest.tempo != self.timebase.tempo() {
            self.timebase.retempo(self.cursor, latest.tempo);
        }
        // Publishing the playing snapshot again, as undo can, ends the wait.
        // `latest` is then a second reference to it, so dropping it frees
        // nothing.
        if !Arc::ptr_eq(&latest, &self.project) {
            self.waiting = Some(latest);
        }
    }

    /// Plays `newer` from now on, handing back the snapshot it replaces.
    fn adopt(&mut self, newer: Arc<Project>) {
        let old = mem::replace(&mut self.project, newer);
        self.link.retire(old);
    }

    fn command(&mut self, command: TransportCommand, now: Instant) -> Result<(), O::Error> {
        if self.playing() {
            // So that Stop keeps, and Continue resumes from, where playback is.
            self.transport.advance_to(self.timebase.tick_at(now));
        }
        let position = self.transport.status().position;
        match self.transport.apply(command) {
            TransportEffect::None => {}
            TransportEffect::Start { at, .. } => self.restart(at, now, position)?,
            TransportEffect::Stop => {
                self.end_notes(now, position)?;
                self.settle();
            }
            TransportEffect::Locate(to) => {
                if self.playing() {
                    self.restart(to, now, position)?;
                }
            }
            TransportEffect::Panic => self.panic(now, position)?,
        }
        Ok(())
    }

    /// Plays from `at`, starting now. Ends the notes sounding at `position`
    /// and takes the newest snapshot at once, since nothing carries over a
    /// jump.
    fn restart(&mut self, at: Ticks, now: Instant, position: Ticks) -> Result<(), O::Error> {
        self.end_notes(now, position)?;
        self.settle();
        self.timebase = Timebase::new(now, at, self.project.tempo);
        self.cursor = at;
        Ok(())
    }

    /// Drops the events not sent yet and moves to the newest snapshot at once.
    fn settle(&mut self) {
        self.rendered.clear();
        if let Some((_, old)) = self.switch.take() {
            self.link.retire(old);
        }
        if let Some(newer) = self.waiting.take() {
            self.adopt(newer);
        }
    }

    /// Ends every sounding note at once. `tick` is where playback is.
    fn end_notes(&mut self, at: Instant, tick: Ticks) -> Result<(), O::Error> {
        for (port, notes) in (0..).map(PortId).zip(&mut self.sounding) {
            for message in notes.drain_note_offs() {
                let message = TimedMessage {
                    at,
                    tick,
                    port,
                    message,
                };
                self.output.send(&message)?;
            }
        }
        Ok(())
    }

    /// Ends every sounding note, then sends All Notes Off on every channel
    /// of every port, for notes the engine doesn't know about. Playback
    /// carries on.
    fn panic(&mut self, at: Instant, tick: Ticks) -> Result<(), O::Error> {
        self.end_notes(at, tick)?;
        for port in (0..).map(PortId).take(self.sounding.len()) {
            for channel in (0..16).filter_map(Channel::from_index) {
                let message = TimedMessage {
                    at,
                    tick,
                    port,
                    message: MidiMessage::ControlChange {
                        channel,
                        controller: ALL_NOTES_OFF,
                        value: U7::MIN,
                    },
                };
                self.output.send(&message)?;
            }
        }
        Ok(())
    }

    /// Renders up to `horizon`. A waiting snapshot takes over at the first
    /// bar line on the way, once playback has reached the previous switch.
    fn render_until(&mut self, horizon: Ticks) {
        if self.cursor >= horizon {
            return;
        }
        if self.switch.is_none() {
            let bar = self.project.time_signature.bar_length().0;
            let line = Ticks(self.cursor.0.div_ceil(bar) * bar);
            if line < horizon
                && let Some(newer) = self.waiting.take()
            {
                render(&self.project, self.cursor..line, &mut self.rendered);
                let old = mem::replace(&mut self.project, newer);
                self.switch = Some((line, old));
                self.cursor = line;
            }
        }
        render(&self.project, self.cursor..horizon, &mut self.rendered);
        self.cursor = horizon;
    }

    /// Sends the rendered events due by `limit`. On reaching a switch's bar
    /// line, first ends the notes of the tracks that changed.
    fn send_until(&mut self, limit: Instant) -> Result<(), O::Error> {
        let mut sent = 0;
        loop {
            let next = self.rendered.get(sent).copied();
            if let Some((line, _)) = self.switch
                && next.is_none_or(|event| event.tick >= line)
                && self.timebase.time_of(line) <= limit
            {
                self.finish_switch()?;
                continue;
            }
            let Some(event) = next else { break };
            let at = self.timebase.time_of(event.tick);
            if at > limit {
                break;
            }
            sent += 1;
            self.send(at, event)?;
        }
        self.rendered.drain(..sent);
        Ok(())
    }

    /// Playback has reached the bar line where the playing snapshot took
    /// over. Ends the notes of tracks that changed or went away, and hands
    /// back the snapshot they came from.
    fn finish_switch(&mut self) -> Result<(), O::Error> {
        let Some((line, old)) = self.switch.take() else {
            return Ok(());
        };
        let at = self.timebase.time_of(line);
        for track in &old.tracks {
            // A muted track has no notes of its own to end.
            if track.muted || carries_on(track, &self.project) {
                continue;
            }
            let port = track.target.port;
            let Some(notes) = self.sounding.get_mut(usize::from(port.0)) else {
                continue;
            };
            for message in notes.drain_channel(track.target.channel) {
                let message = TimedMessage {
                    at,
                    tick: line,
                    port,
                    message,
                };
                self.output.send(&message)?;
            }
        }
        self.link.retire(old);
        Ok(())
    }

    /// Sends one rendered event and keeps track of the notes it starts and
    /// ends. A note-off for a note that isn't sounding, such as one Stop
    /// already ended, isn't sent. Neither is anything for a port without
    /// settings in the configuration.
    fn send(&mut self, at: Instant, event: RenderedEvent) -> Result<(), O::Error> {
        let Some(notes) = self.sounding.get_mut(usize::from(event.port.0)) else {
            return Ok(());
        };
        if let MidiMessage::NoteOff { channel, note, .. } = event.message
            && !notes.is_sounding(channel, note)
        {
            return Ok(());
        }
        notes.observe(event.message);
        self.output.send(&TimedMessage {
            at,
            tick: event.tick,
            port: event.port,
            message: event.message,
        })
    }

    /// When to wake up: at least every poll interval, to pick up commands
    /// and render ahead, and for an output that takes messages only when
    /// they are due, at the next event or bar-line switch.
    fn wake_time(&self, now: Instant) -> Instant {
        let mut wake = now + POLL_INTERVAL.min(self.config.lookahead);
        if self.playing() && self.output.delivery() == Delivery::Immediate {
            if let Some(event) = self.rendered.first() {
                wake = wake.min(self.timebase.time_of(event.tick));
            }
            if let Some((line, _)) = &self.switch {
                wake = wake.min(self.timebase.time_of(*line));
            }
        }
        wake
    }
}

/// Whether `track` carries on in `project` as it was, so its sounding notes
/// can end as written. A new name or device doesn't matter here.
fn carries_on(track: &Arc<Track>, project: &Project) -> bool {
    project.tracks.iter().any(|new| {
        Arc::ptr_eq(new, track)
            || (new.id == track.id
                && new.target == track.target
                && new.muted == track.muted
                && new.clip == track.clip)
    })
}

impl<O, C> Engine<O, C>
where
    O: MidiOutput + 'static,
    C: Clock + Send + 'static,
{
    /// Runs the engine on a new thread named `treason-engine`, at real-time
    /// priority if the configuration asks for it and the OS allows it.
    pub fn spawn(self) -> io::Result<EngineThread<O::Error>> {
        let thread = thread::Builder::new()
            .name("treason-engine".to_owned())
            .spawn(move || {
                if self.config.realtime
                    && let Err(error) = raise_priority()
                {
                    tracing::warn!(
                        %error,
                        "no real-time priority for the engine thread; timing may suffer under load"
                    );
                }
                self.run()
            })?;
        Ok(EngineThread { thread })
    }
}

/// The running engine thread.
#[derive(Debug)]
pub struct EngineThread<E> {
    thread: JoinHandle<Result<(), E>>,
}

impl<E> EngineThread<E> {
    /// Waits for the engine to finish. It finishes once the
    /// [`EngineHandle`](crate::EngineHandle) is dropped.
    pub fn join(self) -> Result<(), EngineError<E>> {
        match self.thread.join() {
            Ok(result) => result.map_err(EngineError::Output),
            Err(_) => Err(EngineError::Panicked),
        }
    }
}

/// Why the engine stopped before it was asked to.
#[derive(Debug, Error)]
pub enum EngineError<E> {
    /// The MIDI output failed.
    #[error("MIDI output failed")]
    Output(#[source] E),
    /// The engine thread panicked.
    #[error("the engine thread panicked")]
    Panicked,
}

#[cfg(unix)]
fn raise_priority() -> Result<(), thread_priority::Error> {
    use thread_priority::{
        RealtimeThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    };

    let priority = ThreadPriorityValue::try_from(REALTIME_PRIORITY)
        .map_err(|_| thread_priority::Error::Priority("real-time priority out of range"))?;
    thread_priority::set_thread_priority_and_policy(
        thread_priority::thread_native_id(),
        ThreadPriority::Crossplatform(priority),
        ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
    )
}

#[cfg(not(unix))]
fn raise_priority() -> Result<(), thread_priority::Error> {
    thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max)
}

#[cfg(test)]
mod tests {
    use treason_core::model::{Clip, ClipEvent, EventKind, OutputTarget, TrackId};
    use treason_core::playback::{Playback, PlaybackStatus};
    use treason_core::time::Bpm;
    use treason_io::RecordingOutput;

    use super::*;
    use crate::clock::ManualClock;
    use crate::link::{EngineHandle, link};

    // At the default 120 BPM, a beat (960 ticks) is 500 ms and a 4/4 bar two
    // seconds.

    type TestEngine = Engine<RecordingOutput, ManualClock>;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// A track on port 0 and `channel`, looping `length` ticks of notes
    /// given as (start, note, length).
    fn track(id: u32, channel: u8, length: u64, notes: &[(u64, u8, u64)]) -> Arc<Track> {
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
        Arc::new(Track {
            id: TrackId(id),
            name: format!("track {id}"),
            device: None,
            target: OutputTarget {
                port: PortId(0),
                channel: Channel::from_number(channel).unwrap(),
            },
            muted: false,
            clip: Clip::new(Ticks(length), events).unwrap(),
        })
    }

    fn project(tracks: Vec<Arc<Track>>) -> Arc<Project> {
        Arc::new(Project {
            tracks,
            ..Project::default()
        })
    }

    /// An engine with two ports and a 20 ms lookahead, and its handle.
    fn engine(project: Arc<Project>, output: RecordingOutput) -> (EngineHandle, TestEngine) {
        let (handle, link) = link(project);
        let config = EngineConfig {
            lookahead: ms(20),
            ports: vec![PortSettings::default(); 2],
            realtime: false,
        };
        (
            handle,
            Engine::new(link, output, ManualClock::new(), config),
        )
    }

    /// Runs the engine until its clock reaches `until`.
    fn run_until(engine: &mut TestEngine, until: Instant) {
        while engine.clock.now() < until {
            assert!(engine.cycle().unwrap());
        }
    }

    /// Sends `command` and runs the cycle that applies it. Returns when that
    /// was.
    fn apply(
        handle: &mut EngineHandle,
        engine: &mut TestEngine,
        command: TransportCommand,
    ) -> Instant {
        handle.command(command).unwrap();
        let now = engine.clock.now();
        assert!(engine.cycle().unwrap());
        now
    }

    /// Takes what the engine sent, one line per message with its time from
    /// `origin`, such as "125ms off 36".
    fn sent(engine: &mut TestEngine, origin: Instant) -> Vec<String> {
        engine
            .output
            .take()
            .iter()
            .map(|sent| {
                let what = match sent.message {
                    MidiMessage::NoteOn { note, .. } => format!("on {}", note.get()),
                    MidiMessage::NoteOff { note, .. } => format!("off {}", note.get()),
                    MidiMessage::ControlChange {
                        channel,
                        controller,
                        value,
                    } => format!(
                        "port {} channel {} cc {}={}",
                        sent.port.0,
                        channel.number(),
                        controller.get(),
                        value.get()
                    ),
                    other => format!("{other:?}"),
                };
                format!("{:?} {what}", sent.at - origin)
            })
            .collect()
    }

    #[test]
    fn plays_clips_on_time_and_reports_the_position() {
        let beat = track(1, 10, 960, &[(0, 36, 240), (480, 38, 240)]);
        let (mut handle, mut engine) = engine(project(vec![beat]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(1000));
        assert!(engine.cycle().unwrap());
        assert_eq!(
            sent(&mut engine, origin),
            [
                "0ns on 36",
                "125ms off 36",
                "250ms on 38",
                "375ms off 38",
                "500ms on 36",
                "625ms off 36",
                "750ms on 38",
                "875ms off 38",
                "1s on 36",
            ]
        );
        let status = PlaybackStatus {
            state: TransportState::Playing,
            position: Ticks(1920),
        };
        assert_eq!(handle.status(), status);
    }

    #[test]
    fn a_pattern_published_just_before_play_plays_from_the_top() {
        let (mut handle, mut engine) = engine(project(vec![]), RecordingOutput::new());
        handle.publish(project(vec![track(1, 10, 960, &[(0, 36, 240)])]));
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        assert_eq!(sent(&mut engine, origin), ["0ns on 36"]);
    }

    #[test]
    fn stop_ends_notes_and_continue_resumes_where_it_stopped() {
        // A note held for a beat, every two beats.
        let held = track(1, 10, 1920, &[(0, 60, 960)]);
        let (mut handle, mut engine) = engine(project(vec![held]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(250));
        apply(&mut handle, &mut engine, TransportCommand::Stop);
        let stopped = PlaybackStatus {
            state: TransportState::Stopped,
            position: Ticks(480),
        };
        assert_eq!(handle.status(), stopped);

        run_until(&mut engine, origin + ms(400));
        apply(&mut handle, &mut engine, TransportCommand::Continue);
        run_until(&mut engine, origin + ms(1400));
        // The held note's own note-off isn't sent, since Stop ended the note.
        // The next note comes 1440 ticks (750 ms) after where it stopped.
        assert_eq!(
            sent(&mut engine, origin),
            ["0ns on 60", "250ms off 60", "1.15s on 60"]
        );
    }

    #[test]
    fn tempo_changes_apply_at_once() {
        let kicks = track(1, 10, 960, &[(0, 36, 240)]);
        let (mut handle, mut engine) = engine(project(vec![kicks.clone()]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(1100));
        let faster = Project {
            tempo: Bpm::new(240.0).unwrap(),
            tracks: vec![kicks],
            ..Project::default()
        };
        handle.publish(Arc::new(faster));
        run_until(&mut engine, origin + ms(2000));

        let kicks: Vec<Duration> = engine
            .output
            .take()
            .iter()
            .filter(|sent| matches!(sent.message, MidiMessage::NoteOn { .. }))
            .map(|sent| sent.at - origin)
            .collect();
        assert_eq!(kicks[..3], [ms(0), ms(500), ms(1000)]);
        // The next beat comes sooner than 1.5 s, without waiting for the bar
        // line at 2 s, and the beats after it are 250 ms apart.
        assert!(kicks[3] < ms(1500), "{kicks:?}");
        assert!(kicks.len() > 5, "{kicks:?}");
        for pair in kicks[3..].windows(2) {
            assert_eq!(pair[1] - pair[0], ms(250), "{kicks:?}");
        }
    }

    #[test]
    fn new_snapshots_take_over_at_the_next_bar_line() {
        let kicks = project(vec![track(1, 10, 960, &[(0, 36, 240)])]);
        let first = Arc::downgrade(&kicks);
        let (mut handle, mut engine) = engine(kicks, RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(600));
        handle.publish(project(vec![track(1, 10, 960, &[(0, 38, 240)])]));
        run_until(&mut engine, origin + ms(2600));

        let notes: Vec<String> = sent(&mut engine, origin)
            .into_iter()
            .filter(|line| line.contains(" on "))
            .collect();
        assert_eq!(
            notes,
            [
                "0ns on 36",
                "500ms on 36",
                "1s on 36",
                "1.5s on 36",
                "2s on 38",
                "2.5s on 38"
            ]
        );
        // The engine handed the old snapshot back instead of freeing it on
        // its thread. The next publish frees it.
        assert!(first.upgrade().is_some());
        handle.publish(project(vec![]));
        assert!(first.upgrade().is_none());
    }

    #[test]
    fn changed_tracks_end_their_notes_at_the_bar_line() {
        // Three tracks, on channels 1 to 3, each holding a note over the bar
        // line at 2 s: from 1.5625 s to 2.4375 s.
        let held = |id, note| track(id, id as u8, 7680, &[(3000, note, 1680)]);
        let (mut handle, mut engine) = engine(
            project(vec![held(1, 60), held(2, 62), held(3, 64)]),
            RecordingOutput::new(),
        );
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(1000));

        // Track 1 gets a new clip, track 2 only a new name, and track 3 is
        // muted.
        let new_clip = track(1, 1, 7680, &[(4320, 70, 240)]);
        let mut renamed = (*held(2, 62)).clone();
        renamed.name = "pad".into();
        let mut muted = (*held(3, 64)).clone();
        muted.muted = true;
        handle.publish(project(vec![new_clip, Arc::new(renamed), Arc::new(muted)]));
        run_until(&mut engine, origin + ms(3000));

        assert_eq!(
            sent(&mut engine, origin),
            [
                "1.5625s on 60",
                "1.5625s on 62",
                "1.5625s on 64",
                "2s off 60",
                "2s off 64",
                "2.25s on 70",
                "2.375s off 70",
                "2.4375s off 62",
            ]
        );
    }

    #[test]
    fn panic_ends_notes_and_sends_all_notes_off_everywhere() {
        let held = track(1, 10, 1920, &[(0, 60, 960)]);
        let (mut handle, mut engine) = engine(project(vec![held]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(100));
        apply(&mut handle, &mut engine, TransportCommand::Panic);

        let sent_at_panic = sent(&mut engine, origin);
        let all_notes_off: Vec<String> = (0..2)
            .flat_map(|port| {
                (1..=16).map(move |channel| format!("100ms port {port} channel {channel} cc 123=0"))
            })
            .collect();
        assert_eq!(sent_at_panic[..2], ["0ns on 60", "100ms off 60"]);
        assert_eq!(sent_at_panic[2..], all_notes_off);

        // Playing carries on. The held note's own note-off isn't sent again.
        assert_eq!(handle.status().state, TransportState::Playing);
        run_until(&mut engine, origin + ms(1100));
        assert_eq!(sent(&mut engine, origin), ["1s on 60"]);
    }

    #[test]
    fn play_restarts_from_the_top_and_locate_jumps() {
        let kicks = track(1, 10, 960, &[(0, 36, 480)]);
        let (mut handle, mut engine) = engine(project(vec![kicks]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(100));
        apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(200));
        apply(
            &mut handle,
            &mut engine,
            TransportCommand::Locate(Ticks(3840)),
        );
        assert_eq!(
            sent(&mut engine, origin),
            [
                "0ns on 36",
                "100ms off 36",
                "100ms on 36",
                "200ms off 36",
                "200ms on 36"
            ]
        );
        assert_eq!(handle.status().position, Ticks(3840));
    }

    #[test]
    fn dropping_the_handle_ends_notes_and_stops_the_engine() {
        let held = track(1, 10, 1920, &[(0, 60, 960)]);
        let (mut handle, mut engine) = engine(project(vec![held]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(100));
        drop(handle);
        assert!(!engine.cycle().unwrap());
        assert_eq!(sent(&mut engine, origin), ["0ns on 60", "100ms off 60"]);
    }

    #[test]
    fn scheduled_outputs_get_messages_ahead_of_time() {
        let beat = track(1, 10, 960, &[(0, 36, 240)]);
        let output = RecordingOutput::scheduled(ms(50));
        let (mut handle, mut engine) = engine(project(vec![beat]), output);
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(110));
        // The note-off due at 125 ms has been handed over already, up to the
        // engine's 20 ms lookahead early.
        assert_eq!(sent(&mut engine, origin), ["0ns on 36", "125ms off 36"]);
    }
}
