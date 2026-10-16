//! The engine thread.

use std::io;
use std::mem;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use arc_swap::Guard;
use thiserror::Error;
use treason_core::midi::{Channel, MidiMessage, U7};
use treason_core::model::{PortId, Project, Track, TrackId};
use treason_core::output::{Delivery, MidiOutput, TimedMessage};
use treason_core::playback::{TransportCommand, TransportState};
use treason_core::time::Ticks;

use crate::clock::Clock;
use crate::link::EngineLink;
use crate::notes::OwnedNotes;
use crate::render::{ClockPulses, RenderedEvent, render_bounded};
use crate::timebase::Timebase;
use crate::transport::{self, Transport, TransportEffect};

/// How many rendered events can wait for submission, without allocating.
const RENDER_CAPACITY: usize = 4096;

/// The longest the engine sleeps before checking for commands and new
/// snapshots again. A shorter lookahead shortens it.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Time for clock receivers to prepare after Start or Continue.
const CLOCK_SETUP: Duration = Duration::from_millis(1);

/// All Notes Off, which panic sends on every channel.
const ALL_NOTES_OFF: U7 = U7::saturating(123);

/// A snapshot staged for a playback bar, including any early submission.
struct SnapshotSwitch {
    line: Ticks,
    project: Arc<Project>,
    submitted: bool,
}

/// Ownership follows a submission through delivery and cancellation.
#[derive(Clone, Copy)]
struct SubmittedEvent {
    timed: TimedMessage,
    track: Option<TrackId>,
    cancelled: bool,
}

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
    /// The notes sounding after all submitted messages have been delivered.
    sounding: Vec<OwnedNotes>,
    /// Delivered notes, conservatively including note-ons racing cancellation.
    delivered: Vec<OwnedNotes>,
    /// The last delivered pulse on each port, retained across cancellation.
    delivered_pulses: Vec<Option<Ticks>>,
    /// A bounded mirror of scheduled work, for restoring state on cancel.
    scheduled: Vec<SubmittedEvent>,
    /// Rendered events not sent yet, in send order.
    rendered: Vec<RenderedEvent>,
    /// Ticks to time, anchored at the last start, locate or tempo change.
    timebase: Timebase,
    /// The next tick to render.
    cursor: Ticks,
    /// The last event rendered when a full buffer splits a tick.
    render_after: Option<RenderedEvent>,
    /// The snapshot playing.
    project: Arc<Project>,
    /// A newer snapshot, waiting for the next bar line.
    waiting: Option<SnapshotSwitch>,
    /// The ports that get MIDI clock and transport messages.
    clock_ports: Vec<PortId>,
    /// The first tick with a clock pulse since playback last started.
    pulses_from: Ticks,
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
            sounding: vec![OwnedNotes::new(); config.ports.len()],
            delivered: vec![OwnedNotes::new(); config.ports.len()],
            delivered_pulses: vec![None; config.ports.len()],
            // A full render batch plus one switch's note-offs on every port.
            scheduled: Vec::with_capacity(RENDER_CAPACITY + config.ports.len() * 16 * 128),
            rendered: Vec::with_capacity(RENDER_CAPACITY),
            timebase,
            cursor: Ticks::ZERO,
            render_after: None,
            project,
            waiting: None,
            clock_ports: (0..)
                .map(PortId)
                .zip(&config.ports)
                .filter_map(|(port, settings)| settings.clock.then_some(port))
                .collect(),
            pulses_from: Ticks::ZERO,
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
        self.advance_delivery(now);
        if self.link.is_abandoned() {
            self.cancel_scheduled(now + Duration::from_nanos(1))?;
            self.end_notes(now, self.transport.status().position)?;
            if self.playing() {
                self.cue(TransportEffect::Stop, now)?;
            }
            self.output.flush()?;
            return Ok(false);
        }
        if self.playing() {
            // Finish work due under the previous snapshot and tempo before
            // applying changes. A submitted switch remains replaceable until
            // playback actually reaches its boundary.
            self.render_and_send_until(Ticks(self.timebase.tick_at(now).0 + 1), now)?;
            self.complete_switch(now);
        }
        // The snapshot comes first, so a pattern published just before Play
        // plays from the top.
        self.check_snapshot(now)?;
        while let Some(command) = self.link.next_command() {
            self.command(command, self.clock.now())?;
        }
        let now = self.clock.now();
        if self.playing() {
            let ahead = self.ahead();
            let horizon = Ticks(self.timebase.tick_at(now + ahead).0 + 1);
            let limit = match self.output.delivery() {
                Delivery::Immediate => now,
                Delivery::Scheduled { .. } => now + ahead,
            };
            self.render_and_send_until(horizon, limit)?;
            self.complete_switch(now);
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
    fn check_snapshot(&mut self, now: Instant) -> Result<(), O::Error> {
        let latest = self.link.project();
        let newest = self.waiting.as_ref().map_or(&self.project, |s| &s.project);
        if Arc::ptr_eq(&*latest, newest) {
            return Ok(());
        }
        let latest = Guard::into_inner(latest);
        if !self.playing() {
            self.adopt(latest);
            return Ok(());
        }
        let position = self.timebase.tick_at(now);
        if latest.tempo != self.timebase.tempo() {
            // All due work was sent using the old mapping. Rebuild future
            // work, including backend submissions, from the current position
            // and time, rather than from the future render boundary.
            self.cancel_scheduled(now + Duration::from_nanos(1))?;
            self.rendered.clear();
            self.cursor = Ticks(position.0 + 1);
            self.render_after = None;
            self.timebase = Timebase::new(now, position, latest.tempo);
            if let Some(waiting) = &mut self.waiting {
                waiting.submitted = false;
            }
        }
        let bar = self.project.time_signature.bar_length().0;
        let line = Ticks((position.0 / bar + 1) * bar);
        // The next playback bar may already be rendered or submitted. Trim
        // both buffers there before staging the latest edit (or undo).
        self.cancel_scheduled(self.timebase.time_of(line))?;
        self.rendered.retain(|event| event.tick < line);
        if self.cursor >= line {
            self.cursor = line;
            self.render_after = None;
        }
        if let Some(superseded) = self.waiting.take() {
            self.link.retire(superseded.project);
        }
        // Publishing the playing snapshot again, as undo can, ends the wait.
        // `latest` is then a second reference to it, so dropping it frees
        // nothing.
        if !Arc::ptr_eq(&latest, &self.project) {
            self.waiting = Some(SnapshotSwitch {
                line,
                project: latest,
                submitted: false,
            });
        }
        Ok(())
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
        let effect = self.transport.apply(command);
        match effect {
            TransportEffect::None | TransportEffect::Locate(_) => {}
            TransportEffect::Start { at, .. } => {
                self.restart(at, now, position)?;
                self.pulses_from = transport::first_pulse(at);
                self.delivered_pulses.fill(None);
            }
            TransportEffect::Stop => {
                self.cancel_scheduled(now + Duration::from_nanos(1))?;
                self.end_notes(now, position)?;
                self.settle();
            }
            TransportEffect::Panic => self.panic(now, position)?,
        }
        // After any note-offs, so the receivers stop or move once our own
        // notes have ended.
        self.cue(effect, now)?;
        if let TransportEffect::Start { at, .. } = effect {
            if !self.clock_ports.is_empty() {
                // Flush the cue before measuring the lead, so buffered and
                // immediate outputs both give receivers the full interval.
                self.output.flush()?;
                self.clock.sleep_until(self.clock.now() + CLOCK_SETUP);
            }
            // Anchor clips and pulses together after the receivers are ready.
            self.timebase = Timebase::new(self.clock.now(), at, self.project.tempo);
        }
        Ok(())
    }

    /// Sends `effect`'s MIDI transport messages to the clock ports at `at`.
    fn cue(&mut self, effect: TransportEffect, at: Instant) -> Result<(), O::Error> {
        let tick = self.transport.status().position;
        for index in 0..self.clock_ports.len() {
            let port = self.clock_ports[index];
            for message in effect.clock_messages() {
                let message = TimedMessage {
                    at,
                    tick,
                    port,
                    message,
                };
                self.emit(message, None)?;
            }
        }
        Ok(())
    }

    /// Plays from `at`, starting now. Ends the notes sounding at `position`
    /// and takes the newest snapshot at once, since nothing carries over a
    /// jump.
    fn restart(&mut self, at: Ticks, now: Instant, position: Ticks) -> Result<(), O::Error> {
        self.cancel_scheduled(now + Duration::from_nanos(1))?;
        self.end_notes(now, position)?;
        self.settle();
        self.cursor = at;
        Ok(())
    }

    /// Drops the events not sent yet and moves to the newest snapshot at once.
    fn settle(&mut self) {
        self.rendered.clear();
        self.render_after = None;
        if let Some(newer) = self.waiting.take() {
            self.adopt(newer.project);
        }
    }

    /// Ends every sounding note at once. `tick` is where playback is.
    fn end_notes(&mut self, at: Instant, tick: Ticks) -> Result<(), O::Error> {
        for index in 0..self.sounding.len() {
            let port = PortId(index as u16);
            let mut notes = self.sounding[index].notes.clone();
            for message in notes.drain_note_offs() {
                let message = TimedMessage {
                    at,
                    tick,
                    port,
                    message,
                };
                self.emit(message, None)?;
            }
        }
        Ok(())
    }

    /// Ends every sounding note, then sends All Notes Off on every channel
    /// of every port, for notes the engine doesn't know about. Playback
    /// carries on.
    fn panic(&mut self, at: Instant, tick: Ticks) -> Result<(), O::Error> {
        self.cancel_scheduled(at + Duration::from_nanos(1))?;
        // Playback continues: render the cancelled future again, including
        // its note-offs, after ending the notes that really sounded.
        self.rendered.clear();
        self.cursor = Ticks(tick.0 + 1);
        self.render_after = None;
        if let Some(waiting) = &mut self.waiting {
            waiting.submitted = false;
        }
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
                self.emit(message, None)?;
            }
        }
        Ok(())
    }

    /// Drains oversized windows in bounded batches. Scheduled backpressure
    /// or events not due yet leave the rest for a later cycle.
    fn render_and_send_until(&mut self, horizon: Ticks, limit: Instant) -> Result<(), O::Error> {
        loop {
            self.render_until(horizon);
            self.send_until(limit)?;
            if self.cursor >= horizon
                || !self.rendered.is_empty()
                || self.scheduled.len() >= RENDER_CAPACITY
            {
                return Ok(());
            }
        }
    }

    /// Renders only what fits, preserving a continuation within a tick.
    /// Both sides of a staged bar use their own snapshot.
    fn render_until(&mut self, horizon: Ticks) {
        while self.cursor < horizon && self.rendered.len() < self.rendered.capacity() {
            let (project, end) = match &self.waiting {
                Some(waiting) if self.cursor >= waiting.line => (&waiting.project, horizon),
                Some(waiting) => (&self.project, horizon.min(waiting.line)),
                None => (&self.project, horizon),
            };
            let clock = ClockPulses {
                ports: &self.clock_ports,
                from: self.pulses_from,
            };
            self.render_after = render_bounded(
                project,
                clock,
                self.cursor..end,
                self.render_after,
                &mut self.rendered,
            );
            if let Some(last) = self.render_after {
                self.cursor = last.tick;
                return;
            }
            self.cursor = end;
        }
    }

    /// Sends the rendered events due by `limit`. On reaching a switch's bar
    /// line, first ends the notes of the tracks that changed.
    fn send_until(&mut self, limit: Instant) -> Result<(), O::Error> {
        let mut sent = 0;
        loop {
            // Leave room for a switch's forced note-offs. If a dense window
            // fills the mirror, keep the rest rendered until delivery frees
            // space; never grow the mirror on the engine thread.
            if self.scheduled.len() >= RENDER_CAPACITY {
                break;
            }
            let next = self.rendered.get(sent).copied();
            if let Some(waiting) = &self.waiting
                && !waiting.submitted
                && next.map_or(self.cursor >= waiting.line, |event| {
                    event.tick >= waiting.line
                })
                && self.timebase.time_of(waiting.line) <= limit
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

    /// Submits the forced note-offs before any new events at the boundary.
    /// Keeps the snapshot pending until actual playback reaches that line.
    fn finish_switch(&mut self) -> Result<(), O::Error> {
        let Some(waiting) = &self.waiting else {
            return Ok(());
        };
        let line = waiting.line;
        let at = self.timebase.time_of(line);
        for index in 0..self.project.tracks.len() {
            let track = &self.project.tracks[index];
            // A muted track has no notes of its own to end.
            if track.muted || carries_on(track, &self.waiting.as_ref().unwrap().project) {
                continue;
            }
            let port = track.target.port;
            let id = track.id;
            let Some(notes) = self.sounding.get(usize::from(port.0)) else {
                continue;
            };
            let mut notes = notes.for_track(id, track.target.channel);
            for message in notes.drain_channel(track.target.channel) {
                let message = TimedMessage {
                    at,
                    tick: line,
                    port,
                    message,
                };
                self.emit(message, Some(id))?;
            }
        }
        self.waiting.as_mut().unwrap().submitted = true;
        Ok(())
    }

    /// Only delivery, not early submission, makes a switch irreversible.
    fn complete_switch(&mut self, now: Instant) {
        if self
            .waiting
            .as_ref()
            .is_some_and(|waiting| waiting.submitted && self.timebase.time_of(waiting.line) <= now)
        {
            let waiting = self.waiting.take().unwrap();
            self.adopt(waiting.project);
        }
    }

    /// Sends one rendered event and keeps track of the notes it starts and
    /// ends. A note-off for a note this track no longer owns, such as one
    /// Stop ended or another track retriggered, isn't sent. Neither is
    /// anything for a port without settings in the configuration.
    fn send(&mut self, at: Instant, event: RenderedEvent) -> Result<(), O::Error> {
        let Some(notes) = self.sounding.get(usize::from(event.port.0)) else {
            return Ok(());
        };
        if event.message == MidiMessage::TimingClock
            && self.delivered_pulses[usize::from(event.port.0)]
                .is_some_and(|tick| event.tick <= tick)
        {
            return Ok(());
        }
        if let MidiMessage::NoteOff { channel, note, .. } = event.message
            && !event
                .track
                .is_some_and(|track| notes.owns(track, channel, note))
        {
            return Ok(());
        }
        self.emit(
            TimedMessage {
                at,
                tick: event.tick,
                port: event.port,
                message: event.message,
            },
            event.track,
        )
    }

    /// Submits a message, tracking delivered and projected notes separately.
    fn emit(&mut self, message: TimedMessage, track: Option<TrackId>) -> Result<(), O::Error> {
        self.output.send(&message)?;
        self.sounding[usize::from(message.port.0)].observe(message.message, track);
        if matches!(self.output.delivery(), Delivery::Scheduled { .. }) {
            let now = self.clock.now();
            if message.at > now {
                self.scheduled.push(SubmittedEvent {
                    timed: message,
                    track,
                    cancelled: false,
                });
                return Ok(());
            }
            // Submission can cross queued deadlines. Observe those older
            // deliveries before this message, especially an off followed
            // by a retrigger of the same note.
            self.advance_delivery(now);
        }
        self.delivered[usize::from(message.port.0)].observe(message.message, track);
        remember_pulse(&mut self.delivered_pulses, &message);
        Ok(())
    }

    /// Advances delivered notes and clock positions as timestamps elapse.
    fn advance_delivery(&mut self, now: Instant) {
        self.scheduled.retain(|event| {
            let message = &event.timed;
            if message.at <= now {
                self.delivered[usize::from(message.port.0)].observe(message.message, event.track);
                remember_pulse(&mut self.delivered_pulses, message);
                false
            } else {
                true
            }
        });
    }

    /// Invalidates backend work and reconstructs projected notes. Delivery
    /// can race cancellation: after it returns, any removed note-on due by
    /// then might have sounded. Keep those notes until an off ends them;
    /// never assume a removed note-off was delivered.
    fn cancel_scheduled(&mut self, from: Instant) -> Result<(), O::Error> {
        let scheduled = &mut self.scheduled;
        self.output.cancel_from(from, &mut |message| {
            // Only clocks need exact delivery accounting. Notes retain the
            // conservative cleanup below, including racing note-ons.
            if message.message == MidiMessage::TimingClock
                && let Some(event) = scheduled.iter_mut().find(|event| event.timed == *message)
            {
                event.cancelled = true;
            }
        })?;
        let now = self.clock.now();
        self.scheduled.retain(|event| {
            let message = &event.timed;
            if message.at < from {
                if message.at > now {
                    return true;
                }
                self.delivered[usize::from(message.port.0)].observe(message.message, event.track);
                remember_pulse(&mut self.delivered_pulses, message);
            } else if message.message == MidiMessage::TimingClock && !event.cancelled {
                // The backend withdrew every undelivered pulse and reported
                // each one. This pulse therefore reached the receiver, even
                // if it raced cancellation. Do not render it a second time.
                remember_pulse(&mut self.delivered_pulses, message);
            } else if message.at <= now
                && matches!(message.message, MidiMessage::NoteOn { velocity, .. } if velocity != U7::MIN)
            {
                self.delivered[usize::from(message.port.0)].observe(message.message, event.track);
            }
            false
        });
        self.sounding.clone_from_slice(&self.delivered);
        for event in &self.scheduled {
            let message = &event.timed;
            self.sounding[usize::from(message.port.0)].observe(message.message, event.track);
        }
        Ok(())
    }

    /// When to wake up: at least every poll interval, to pick up commands
    /// and render ahead, and for an output that takes messages only when
    /// they are due, at the next event or bar-line switch.
    fn wake_time(&self, now: Instant) -> Instant {
        let mut wake = now + POLL_INTERVAL.min(self.config.lookahead);
        if let Some(message) = self.scheduled.first() {
            wake = wake.min(message.timed.at);
        }
        if self.playing() && self.output.delivery() == Delivery::Immediate {
            if let Some(event) = self.rendered.first() {
                wake = wake.min(self.timebase.time_of(event.tick));
            }
            if let Some(waiting) = &self.waiting {
                wake = wake.min(self.timebase.time_of(waiting.line));
            }
        }
        wake
    }
}

/// Retains a port's clock position without allocating on the engine thread.
fn remember_pulse(pulses: &mut [Option<Ticks>], message: &TimedMessage) {
    if message.message == MidiMessage::TimingClock {
        let last = &mut pulses[usize::from(message.port.0)];
        *last = Some(last.map_or(message.tick, |tick| tick.max(message.tick)));
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
    /// priority if the configuration asks for it and the OS allows it. If the
    /// output fails, the engine logs why and stops.
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
                let result = self.run();
                if let Err(error) = &result {
                    // Nothing is playing any more, so logging can't hold it up.
                    tracing::error!(%error, "MIDI output failed; the engine has stopped");
                }
                result
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
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    use treason_core::model::{Clip, ClipEvent, EventKind, OutputTarget, TrackId};
    use treason_core::playback::{Playback, PlaybackStatus};
    use treason_core::time::Bpm;
    use treason_io::RecordingOutput;

    use super::*;
    use crate::clock::ManualClock;
    use crate::link::{EngineHandle, link};
    use crate::notes::ActiveNotes;

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
    fn engine<O: MidiOutput>(
        project: Arc<Project>,
        output: O,
    ) -> (EngineHandle, Engine<O, ManualClock>) {
        engine_with(project, output, PortSettings::default())
    }

    /// An engine like [`engine`]'s whose first port, where [`track`]s play,
    /// also gets MIDI clock.
    fn clocked<O: MidiOutput>(
        project: Arc<Project>,
        output: O,
    ) -> (EngineHandle, Engine<O, ManualClock>) {
        let clock = PortSettings {
            clock: true,
            ..PortSettings::default()
        };
        engine_with(project, output, clock)
    }

    fn engine_with<O: MidiOutput>(
        project: Arc<Project>,
        output: O,
        first_port: PortSettings,
    ) -> (EngineHandle, Engine<O, ManualClock>) {
        let (handle, link) = link(project);
        let config = EngineConfig {
            lookahead: ms(20),
            ports: vec![first_port, PortSettings::default()],
            realtime: false,
        };
        (
            handle,
            Engine::new(link, output, ManualClock::new(), config),
        )
    }

    /// Runs the engine until its clock reaches `until`.
    fn run_until<O: MidiOutput>(engine: &mut Engine<O, ManualClock>, until: Instant) {
        while engine.clock.now() < until {
            assert!(engine.cycle().unwrap());
        }
    }

    /// Sends `command` and runs the cycle that applies it. Returns when that
    /// was.
    fn apply<O: MidiOutput>(
        handle: &mut EngineHandle,
        engine: &mut Engine<O, ManualClock>,
        command: TransportCommand,
    ) -> Instant {
        handle.command(command).unwrap();
        let now = engine.clock.now();
        assert!(engine.cycle().unwrap());
        now
    }

    /// Takes what the engine sent, one line per message with its time from
    /// `origin`, such as "125ms off 36" or "20.833334ms clock".
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
                    MidiMessage::TimingClock => "clock".to_owned(),
                    MidiMessage::SongPosition(beats) => format!("SPP {}", beats.get()),
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
    fn tempo_changes_preserve_position_and_retime_the_lookahead_in_both_directions() {
        for scheduled in [false, true] {
            for (tempo, first, second) in [
                (
                    240.0,
                    Duration::from_micros(106_250),
                    Duration::from_micros(112_500),
                ),
                (60.0, ms(125), ms(150)),
            ] {
                // Both the old off and new on at 112.5 ms are already
                // rendered (and, in scheduled mode, already submitted).
                let part = track(1, 10, 1920, &[(0, 36, 216), (216, 38, 24)]);
                let initial = project(vec![part]);
                let output = if scheduled {
                    RecordingOutput::scheduled(ms(50))
                } else {
                    RecordingOutput::new()
                };
                let (mut handle, mut engine) = engine(initial.clone(), output);
                let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
                run_until(&mut engine, origin + ms(100));
                let at = engine.clock.now();
                let position = engine.timebase.tick_at(at);
                assert_eq!(position, Ticks(192));
                handle.publish(Arc::new(Project {
                    tempo: Bpm::new(tempo).unwrap(),
                    ..(*initial).clone()
                }));
                assert!(engine.cycle().unwrap());
                assert_eq!(engine.timebase.tick_at(at), position);
                assert_eq!(handle.status().position, position);
                run_until(&mut engine, origin + ms(200));
                let actual: Vec<_> = engine
                    .output
                    .take()
                    .iter()
                    .map(|message| (message.at - origin, message.tick))
                    .collect();
                assert_eq!(
                    actual,
                    [
                        (Duration::ZERO, Ticks::ZERO),
                        (first, Ticks(216)),
                        (first, Ticks(216)),
                        (second, Ticks(240)),
                    ],
                    "tempo {tempo}, scheduled {scheduled}"
                );
            }
        }
    }

    #[test]
    fn a_tempo_change_and_stop_in_one_cycle_keep_the_actual_position() {
        for tempo in [60.0, 240.0, 400.0] {
            let initial = project(vec![track(1, 10, 1920, &[(0, 60, 240)])]);
            let (mut handle, mut engine) = engine(initial.clone(), ScheduledOutput::new());
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(110));
            let position = engine.timebase.tick_at(engine.clock.now());
            assert_eq!(position, Ticks(211));
            handle.publish(Arc::new(Project {
                tempo: Bpm::new(tempo).unwrap(),
                ..(*initial).clone()
            }));
            apply(&mut handle, &mut engine, TransportCommand::Stop);
            assert_eq!(
                handle.status(),
                PlaybackStatus {
                    state: TransportState::Stopped,
                    position,
                }
            );
            engine.output.deliver_until(origin + ms(1000));
            assert!(engine.output.notes.is_empty());
        }
    }

    #[test]
    fn edits_inside_the_lookahead_replace_the_next_playback_bar() {
        for scheduled in [false, true] {
            let beat = track(1, 1, 3840, &[(0, 36, 240)]);
            let held = track(2, 2, 7680, &[(3000, 60, 1680)]);
            let unchanged = track(3, 3, 7680, &[(3000, 62, 1680)]);
            let output = if scheduled {
                RecordingOutput::scheduled(ms(50))
            } else {
                RecordingOutput::new()
            };
            let (mut handle, mut engine) =
                engine(project(vec![beat, held.clone(), unchanged.clone()]), output);
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(1985));
            engine.clock.sleep_until(origin + ms(1990));
            assert_eq!(engine.clock.now(), origin + ms(1990));
            assert!(
                engine.cursor > Ticks(3840),
                "the next bar is already rendered"
            );
            let mut muted = (*held).clone();
            muted.muted = true;
            handle.publish(project(vec![
                track(1, 1, 3840, &[(0, 38, 240)]),
                Arc::new(muted),
                unchanged,
            ]));
            run_until(&mut engine, origin + ms(2500));
            let future: Vec<_> = sent(&mut engine, origin)
                .into_iter()
                .filter(|line| line.starts_with('2'))
                .collect();
            assert_eq!(
                future,
                ["2s off 60", "2s on 38", "2.125s off 38", "2.4375s off 62"],
                "scheduled {scheduled}"
            );
        }
    }

    #[test]
    fn an_edit_or_undo_can_replace_a_switch_already_submitted_for_the_next_bar() {
        for undo in [false, true] {
            let initial = project(vec![track(1, 10, 3840, &[(0, 36, 240)])]);
            let (mut handle, mut engine) =
                engine(initial.clone(), RecordingOutput::scheduled(ms(50)));
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(1985));
            handle.publish(project(vec![track(1, 10, 3840, &[(0, 38, 240)])]));
            assert!(engine.cycle().unwrap());
            assert!(engine.waiting.as_ref().unwrap().submitted);
            // The original snapshot must survive until delivery, so a later
            // edit or undo still targets the same upcoming bar.
            assert!(Arc::ptr_eq(&engine.project, &initial));
            handle.publish(if undo {
                initial
            } else {
                project(vec![track(1, 10, 3840, &[(0, 40, 240)])])
            });
            run_until(&mut engine, origin + ms(2200));
            let future: Vec<_> = sent(&mut engine, origin)
                .into_iter()
                .filter(|line| line.starts_with('2'))
                .collect();
            let expected = if undo {
                ["2s on 36", "2.125s off 36"]
            } else {
                ["2s on 40", "2.125s off 40"]
            };
            assert_eq!(future, expected);
        }
    }

    #[test]
    fn tempo_changes_retime_a_pending_bar_switch_and_its_forced_note_offs() {
        let initial = project(vec![track(1, 10, 7680, &[(3000, 60, 1680)])]);
        let (mut handle, mut engine) = engine(initial, RecordingOutput::scheduled(ms(50)));
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(1980));
        let edited = project(vec![track(1, 10, 3840, &[(0, 38, 240)])]);
        handle.publish(edited.clone());
        assert!(engine.cycle().unwrap());
        assert!(engine.waiting.as_ref().unwrap().submitted);
        engine
            .clock
            .sleep_until(origin + Duration::from_micros(1_987_500));
        handle.publish(Arc::new(Project {
            tempo: Bpm::new(60.0).unwrap(),
            ..(*edited).clone()
        }));
        run_until(&mut engine, origin + ms(2400));
        let actual: Vec<_> = engine
            .output
            .take()
            .iter()
            .map(|message| (message.at - origin, message.message))
            .collect();
        assert_eq!(actual.len(), 4);
        assert_eq!(actual[1].0, Duration::from_micros(2_012_500));
        assert!(matches!(actual[1].1, MidiMessage::NoteOff { note, .. } if note.get() == 60));
        assert_eq!(actual[2].0, Duration::from_micros(2_012_500));
        assert!(matches!(actual[2].1, MidiMessage::NoteOn { note, .. } if note.get() == 38));
        assert_eq!(actual[3].0, Duration::from_micros(2_262_500));
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
    fn changing_a_track_preserves_held_notes_on_its_shared_channel() {
        for scheduled in [false, true] {
            for edit in ["mute", "remove", "clip", "port", "channel"] {
                let changed = track(7, 10, 7680, &[(3000, 60, 1680)]);
                let unchanged = track(42, 10, 7680, &[(3000, 62, 1680)]);
                let output = if scheduled {
                    RecordingOutput::scheduled(ms(50))
                } else {
                    RecordingOutput::new()
                };
                let (mut handle, mut engine) =
                    engine(project(vec![changed.clone(), unchanged.clone()]), output);
                let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
                run_until(&mut engine, origin + ms(1990));
                // The bar is already rendered/submitted. Reorder tracks as
                // well, so ownership must use stable IDs, not vector indices.
                let mut newer = (*changed).clone();
                match edit {
                    "mute" => newer.muted = true,
                    "remove" => {}
                    "clip" => newer.clip = Clip::default(),
                    "port" => newer.target.port = PortId(1),
                    "channel" => newer.target.channel = Channel::from_number(2).unwrap(),
                    _ => unreachable!(),
                }
                let mut tracks = vec![unchanged];
                if edit != "remove" {
                    tracks.push(Arc::new(newer));
                }
                handle.publish(project(tracks));
                run_until(&mut engine, origin + ms(2500));
                let messages = engine.output.messages();
                assert_eq!(messages.len(), 4, "{edit}, scheduled {scheduled}");
                assert_eq!(messages[2].port, PortId(0));
                assert_eq!(messages[2].at, origin + ms(2000));
                assert!(matches!(messages[2].message,
                    MidiMessage::NoteOff { channel: Channel::DRUMS, note, .. } if note.get() == 60
                ));
                assert_eq!(messages[3].at, origin + Duration::from_micros(2_437_500));
                assert!(
                    matches!(messages[3].message,
                        MidiMessage::NoteOff { channel: Channel::DRUMS, note, .. } if note.get() == 62
                    ),
                    "{edit}, scheduled {scheduled}"
                );
                assert!(engine.sounding.iter().all(|notes| notes.notes.is_empty()));
            }
        }
    }

    #[test]
    fn replacing_or_undoing_a_submitted_switch_restores_track_ownership() {
        for undo in [false, true] {
            let changed = track(7, 10, 7680, &[(3000, 60, 1680)]);
            let unchanged = track(42, 10, 7680, &[(3000, 62, 1680)]);
            let initial = project(vec![changed.clone(), unchanged.clone()]);
            let (mut handle, mut engine) = engine(initial.clone(), ScheduledOutput::new());
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(1980));
            let mut muted = (*changed).clone();
            muted.muted = true;
            handle.publish(project(vec![Arc::new(muted), unchanged.clone()]));
            assert!(engine.cycle().unwrap());
            assert!(engine.waiting.as_ref().unwrap().submitted);
            // Cancel the queued cleanup, then either restore the original
            // snapshot or replace it with another edit at that same bar.
            handle.publish(if undo {
                initial
            } else {
                project(vec![unchanged])
            });
            run_until(&mut engine, origin + ms(2010));
            engine.output.deliver_until(origin + ms(2010));
            assert_eq!(
                engine
                    .output
                    .notes
                    .is_sounding(Channel::DRUMS, U7::saturating(60)),
                undo
            );
            assert!(
                engine
                    .output
                    .notes
                    .is_sounding(Channel::DRUMS, U7::saturating(62))
            );
            run_until(&mut engine, origin + ms(2500));
            engine.output.deliver_until(origin + ms(2500));
            assert!(engine.output.notes.is_empty());
            assert_eq!(engine.output.delivered.len(), 4);
            let off = engine.output.delivered[2];
            assert_eq!(
                off.at,
                origin
                    + if undo {
                        Duration::from_micros(2_437_500)
                    } else {
                        ms(2000)
                    }
            );
        }
    }

    #[test]
    fn an_older_tracks_note_off_cannot_end_another_tracks_retrigger() {
        for scheduled in [false, true] {
            let first = track(7, 10, 7680, &[(3000, 60, 960)]);
            let retrigger = track(42, 10, 7680, &[(3360, 60, 1320)]);
            let output = if scheduled {
                RecordingOutput::scheduled(ms(50))
            } else {
                RecordingOutput::new()
            };
            let (mut handle, mut engine) = engine(project(vec![first, retrigger]), output);
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(2500));
            assert_eq!(
                sent(&mut engine, origin),
                ["1.5625s on 60", "1.75s on 60", "2.4375s off 60",]
            );
        }
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
    fn transport_cues_lead_aligned_notes_and_pulses_on_every_start() {
        for scheduled in [false, true] {
            for (playing, command) in [
                (false, TransportCommand::Play),
                (true, TransportCommand::Play),
                (false, TransportCommand::Continue),
                (true, TransportCommand::Locate(Ticks(240))),
            ] {
                let output = if scheduled {
                    RecordingOutput::scheduled(ms(50))
                } else {
                    RecordingOutput::new()
                };
                let beat = track(1, 10, 240, &[(0, 36, 120)]);
                let (mut handle, mut engine) = clocked(project(vec![beat]), output);
                if command == TransportCommand::Continue {
                    apply(
                        &mut handle,
                        &mut engine,
                        TransportCommand::Locate(Ticks(240)),
                    );
                } else if playing {
                    apply(&mut handle, &mut engine, TransportCommand::Play);
                }
                engine.output.take();
                let cue_at = apply(&mut handle, &mut engine, command);
                run_until(&mut engine, cue_at + ms(10));
                let messages = engine.output.messages();
                let cue = messages
                    .iter()
                    .find(|sent| matches!(sent.message, MidiMessage::Start | MidiMessage::Continue))
                    .unwrap();
                let pulse = messages
                    .iter()
                    .find(|sent| sent.message == MidiMessage::TimingClock && sent.at > cue.at)
                    .unwrap();
                let note = messages
                    .iter()
                    .find(|sent| {
                        matches!(sent.message, MidiMessage::NoteOn { .. }) && sent.at > cue.at
                    })
                    .unwrap();
                assert_eq!(
                    pulse.at - cue.at,
                    CLOCK_SETUP,
                    "{command:?}, scheduled {scheduled}"
                );
                assert_eq!((note.tick, note.at), (pulse.tick, pulse.at));
            }
        }
    }

    #[test]
    fn clock_ports_get_start_a_pulse_every_40_ticks_and_stop() {
        let kick = track(1, 10, 960, &[(0, 36, 240)]);
        let mut bass = (*track(2, 1, 960, &[(0, 40, 240)])).clone();
        bass.target.port = PortId(1);
        let (mut handle, mut engine) =
            clocked(project(vec![kick, Arc::new(bass)]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(45));
        engine.clock.sleep_until(origin + ms(50));
        apply(&mut handle, &mut engine, TransportCommand::Stop);
        // Stopped at tick 94, between 16ths. The receivers resume at the
        // next one, tick 240, after Continue and its setup interval.
        engine.clock.sleep_until(origin + ms(60));
        apply(&mut handle, &mut engine, TransportCommand::Continue);
        run_until(&mut engine, origin + ms(160));

        let notes = |sent: &&TimedMessage| {
            matches!(
                sent.message,
                MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. }
            )
        };
        let mut clock = engine.output.messages().iter().filter(|sent| !notes(sent));
        assert!(
            clock.all(|sent| sent.port == PortId(0)),
            "only the clock port"
        );
        assert_eq!(
            sent(&mut engine, origin),
            [
                "0ns Start",
                "1ms clock",
                "1ms on 36",
                "1ms on 40",
                "21.833334ms clock",
                "42.666667ms clock",
                "50ms off 36",
                "50ms off 40",
                "50ms Stop",
                "60ms SPP 1",
                "60ms Continue",
                "137.041667ms clock",
                "157.875ms clock",
            ]
        );
    }

    #[test]
    fn restarting_or_locating_stops_the_clock_before_moving_it() {
        let kick = track(1, 10, 960, &[(0, 36, 480)]);
        let (mut handle, mut engine) = clocked(project(vec![kick]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(25));
        engine.clock.sleep_until(origin + ms(30));
        apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(55));
        engine.clock.sleep_until(origin + ms(60));
        // Tick 1000 is between 16ths, so the clock resumes at tick 1200.
        apply(
            &mut handle,
            &mut engine,
            TransportCommand::Locate(Ticks(1000)),
        );
        run_until(&mut engine, origin + ms(165));
        engine.clock.sleep_until(origin + ms(170));
        apply(&mut handle, &mut engine, TransportCommand::Stop);
        engine.clock.sleep_until(origin + ms(180));
        apply(
            &mut handle,
            &mut engine,
            TransportCommand::Locate(Ticks(2400)),
        );
        assert_eq!(
            sent(&mut engine, origin),
            [
                "0ns Start",
                "1ms clock",
                "1ms on 36",
                "21.833334ms clock",
                "30ms off 36",
                "30ms Stop",
                "30ms Start",
                "31ms clock",
                "31ms on 36",
                "51.833334ms clock",
                "60ms off 36",
                "60ms Stop",
                "60ms SPP 5",
                "60ms Continue",
                "165.166667ms clock",
                "170ms Stop",
                "180ms SPP 10",
            ]
        );
    }

    #[test]
    fn dropping_the_handle_stops_the_clock_after_ending_notes() {
        for playing in [true, false] {
            let held = track(1, 10, 1920, &[(0, 60, 960)]);
            let (mut handle, mut engine) = clocked(project(vec![held]), RecordingOutput::new());
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            if !playing {
                apply(&mut handle, &mut engine, TransportCommand::Stop);
            }
            run_until(&mut engine, origin + ms(10));
            engine.output.take();
            drop(handle);
            assert!(!engine.cycle().unwrap());
            let expected: &[&str] = if playing {
                &["10ms off 60", "10ms Stop"]
            } else {
                &[]
            };
            assert_eq!(sent(&mut engine, origin), expected);
        }
    }

    /// The ticks and times of the clock pulses sent.
    fn pulses(sent: &[TimedMessage]) -> Vec<(u64, Instant)> {
        sent.iter()
            .filter(|sent| sent.message == MidiMessage::TimingClock)
            .map(|sent| (sent.tick.0, sent.at))
            .collect()
    }

    #[test]
    fn panic_leaves_the_clock_running() {
        let held = track(1, 10, 1920, &[(0, 60, 960)]);
        let (mut handle, mut engine) = clocked(project(vec![held]), RecordingOutput::new());
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(100));
        apply(&mut handle, &mut engine, TransportCommand::Panic);
        run_until(&mut engine, origin + ms(300));
        let sent = engine.output.take();
        // Every pulse up to tick 576, at 300 ms, once and in order.
        let ticks: Vec<u64> = pulses(&sent).iter().map(|pulse| pulse.0).collect();
        assert_eq!(ticks, (0..15).map(|pulse| pulse * 40).collect::<Vec<_>>());
        let transport = sent.iter().filter(|sent| {
            matches!(
                sent.message,
                MidiMessage::Start
                    | MidiMessage::Stop
                    | MidiMessage::Continue
                    | MidiMessage::SongPosition(_)
            )
        });
        assert_eq!(transport.count(), 1, "only the Start");
    }

    #[test]
    fn scheduled_clock_counts_survive_both_cancellation_orderings() {
        for cancel_first in [false, true] {
            for change in ["panic", "tempo", "bar edit"] {
                let (mut handle, link) = link(project(vec![]));
                let origin = Instant::now();
                let clock = SharedClock {
                    origin,
                    nanos: Arc::new(AtomicU64::new(0)),
                };
                let mut engine = Engine::new(
                    link,
                    ScheduledOutput::new(),
                    clock,
                    EngineConfig {
                        ports: vec![
                            PortSettings {
                                clock: true,
                                ..PortSettings::default()
                            };
                            2
                        ],
                        realtime: false,
                        ..EngineConfig::default()
                    },
                );
                handle.command(TransportCommand::Play).unwrap();
                assert!(engine.cycle().unwrap());
                // Panic/tempo crosses tick 240; the bar edit crosses tick
                // 3840. Each pulse is queued but not yet due at cancellation.
                let (before, after, racing_tick) = if change == "bar edit" {
                    (1999, 2003, 3840)
                } else {
                    (124, 128, 240)
                };
                while engine.clock.now() < origin + ms(before - 10) {
                    assert!(engine.cycle().unwrap());
                }
                engine.clock.sleep_until(origin + ms(before));
                engine.output.deliver_until(engine.clock.now());
                assert!(
                    engine
                        .scheduled
                        .iter()
                        .any(|event| event.timed.tick == Ticks(racing_tick))
                );
                engine.output.cancel_advance =
                    Some((engine.clock.clone(), origin + ms(after), cancel_first));
                match change {
                    "panic" => handle.command(TransportCommand::Panic).unwrap(),
                    "tempo" => handle.publish(Arc::new(Project {
                        tempo: Bpm::new(240.0).unwrap(),
                        ..Project::default()
                    })),
                    "bar edit" => handle.publish(project(vec![track(1, 10, 960, &[(0, 36, 120)])])),
                    _ => unreachable!(),
                }
                assert!(engine.cycle().unwrap());
                assert!(
                    engine.output.cancel_advance.is_none(),
                    "the race must occur"
                );
                while engine.clock.now() < origin + ms(after + 100) {
                    assert!(engine.cycle().unwrap());
                }
                engine.output.deliver_until(engine.clock.now());
                for port in [PortId(0), PortId(1)] {
                    let delivered: Vec<_> = engine
                        .output
                        .delivered
                        .iter()
                        .filter(|message| message.port == port)
                        .copied()
                        .collect();
                    let ticks: Vec<_> = pulses(&delivered).iter().map(|pulse| pulse.0).collect();
                    assert!(ticks.contains(&racing_tick), "the racing pulse must arrive");
                    assert_eq!(
                        ticks,
                        (0..ticks.len() as u64).map(|n| n * 40).collect::<Vec<_>>(),
                        "{change}, cancellation first: {cancel_first}, {port:?}"
                    );
                    assert_eq!(
                        delivered
                            .iter()
                            .filter(|message| matches!(
                                message.message,
                                MidiMessage::Start
                                    | MidiMessage::Stop
                                    | MidiMessage::Continue
                                    | MidiMessage::SongPosition(_)
                            ))
                            .count(),
                        1,
                        "only the initial Start"
                    );
                }
            }
        }
    }

    #[test]
    fn stopping_withdraws_pulses_queued_on_a_scheduled_output() {
        let (mut handle, mut engine) = clocked(project(vec![]), RecordingOutput::scheduled(ms(50)));
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(100));
        let stopped = engine.clock.now();
        assert!(
            engine
                .output
                .messages()
                .iter()
                .any(|sent| sent.at > stopped),
            "pulses are queued ahead"
        );
        apply(&mut handle, &mut engine, TransportCommand::Stop);
        let sent = engine.output.take();
        let last = sent.last().unwrap();
        assert_eq!((last.at, last.message), (stopped, MidiMessage::Stop));
        assert!(sent.iter().all(|sent| sent.at <= stopped));
    }

    #[test]
    fn the_clock_runs_on_through_bar_switches_and_tempo_changes() {
        for scheduled in [false, true] {
            let kicks = |note| track(1, 10, 960, &[(0, note, 240)]);
            let output = if scheduled {
                RecordingOutput::scheduled(ms(50))
            } else {
                RecordingOutput::new()
            };
            let (mut handle, mut engine) = clocked(project(vec![kicks(36)]), output);
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(1990));
            handle.publish(project(vec![kicks(38)]));
            run_until(&mut engine, origin + ms(2500));
            handle.publish(Arc::new(Project {
                tempo: Bpm::new(240.0).unwrap(),
                tracks: vec![kicks(38)],
                ..Project::default()
            }));
            run_until(&mut engine, origin + ms(3000));

            let pulses = pulses(engine.output.messages());
            let ticks: Vec<u64> = pulses.iter().map(|pulse| pulse.0).collect();
            let every_40: Vec<u64> = (0..ticks.len() as u64).map(|n| n * 40).collect();
            assert_eq!(ticks, every_40, "scheduled {scheduled}");
            // 24 a beat: 20.8 ms apart at 120 BPM, then 10.4 ms at 240.
            let gaps: Vec<u128> = pulses
                .windows(2)
                .map(|pair| (pair[1].1 - pair[0].1).as_micros())
                .collect();
            assert_eq!(gaps[0], 20_833, "scheduled {scheduled}");
            assert_eq!(gaps[gaps.len() - 1], 10_416, "scheduled {scheduled}");
            assert!(ticks.len() > 150, "scheduled {scheduled}: {}", ticks.len());
        }
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

    /// A backend with separate submission and actual delivery. Advancing it
    /// explicitly lets tests issue commands while future messages are queued.
    struct ScheduledOutput {
        queue: RecordingOutput,
        delivered: Vec<TimedMessage>,
        notes: ActiveNotes,
        cancel_advance: Option<(SharedClock, Instant, bool)>,
        send_advance: Option<(SharedClock, Instant)>,
    }

    impl ScheduledOutput {
        fn new() -> Self {
            Self {
                queue: RecordingOutput::scheduled(ms(50)),
                delivered: Vec::new(),
                notes: ActiveNotes::new(),
                cancel_advance: None,
                send_advance: None,
            }
        }

        fn deliver_until(&mut self, now: Instant) {
            for message in self.queue.take() {
                if message.at <= now {
                    self.notes.observe(message.message);
                    self.delivered.push(message);
                } else {
                    self.queue.send(&message).unwrap();
                }
            }
        }
    }

    impl MidiOutput for ScheduledOutput {
        type Error = std::convert::Infallible;

        fn delivery(&self) -> Delivery {
            self.queue.delivery()
        }

        fn send(&mut self, message: &TimedMessage) -> Result<(), Self::Error> {
            self.queue.send(message)?;
            if let Some((mut clock, until)) = self.send_advance.take() {
                clock.sleep_until(until);
                self.deliver_until(clock.now());
            }
            Ok(())
        }

        fn cancel_from(
            &mut self,
            from: Instant,
            cancelled: &mut dyn FnMut(&TimedMessage),
        ) -> Result<(), Self::Error> {
            if let Some((mut clock, until, cancel_first)) = self.cancel_advance.take() {
                if cancel_first {
                    self.queue.cancel_from(from, cancelled)?;
                }
                clock.sleep_until(until);
                self.deliver_until(clock.now());
            }
            self.queue.cancel_from(from, cancelled)
        }
    }

    /// Shared with the backend so time can pass inside output calls.
    #[derive(Clone)]
    struct SharedClock {
        origin: Instant,
        nanos: Arc<AtomicU64>,
    }

    impl Clock for SharedClock {
        fn now(&self) -> Instant {
            self.origin + Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
        }

        fn sleep_until(&mut self, deadline: Instant) {
            let nanos = deadline.saturating_duration_since(self.origin).as_nanos();
            self.nanos
                .fetch_max(u64::try_from(nanos).unwrap(), Ordering::Relaxed);
        }
    }

    #[test]
    fn cleanup_ends_a_retrigger_delivered_while_submitting_it() {
        for stop in [true, false] {
            let initial = project(vec![track(1, 10, 1920, &[(0, 60, 216), (240, 60, 216)])]);
            let (mut handle, link) = link(initial);
            let origin = Instant::now();
            let clock = SharedClock {
                origin,
                nanos: Arc::new(AtomicU64::new(0)),
            };
            let mut engine = Engine::new(
                link,
                ScheduledOutput::new(),
                clock,
                EngineConfig {
                    ports: vec![PortSettings::default()],
                    realtime: false,
                    ..EngineConfig::default()
                },
            );
            handle.command(TransportCommand::Play).unwrap();
            while engine.clock.now() < origin + ms(105) {
                assert!(engine.cycle().unwrap());
            }
            assert_eq!(engine.clock.now(), origin + ms(105));
            assert_eq!(engine.scheduled.len(), 1);
            assert_eq!(
                engine.scheduled[0].timed.at,
                origin + Duration::from_micros(112_500)
            );
            // The next cycle submits the retrigger at 125 ms. Time passes
            // inside send, delivering the older off and then the new on
            // before the engine gets its post-submission clock sample.
            engine.output.send_advance = Some((engine.clock.clone(), origin + ms(126)));
            assert!(engine.cycle().unwrap());
            assert_eq!(engine.clock.now(), origin + ms(126));
            let delivered: Vec<_> = engine
                .output
                .delivered
                .iter()
                .map(|message| (message.at - origin, message.tick))
                .collect();
            assert_eq!(
                delivered,
                [
                    (Duration::ZERO, Ticks::ZERO),
                    (Duration::from_micros(112_500), Ticks(216)),
                    (ms(125), Ticks(240)),
                ]
            );
            assert!(
                engine
                    .output
                    .notes
                    .is_sounding(Channel::DRUMS, U7::saturating(60))
            );
            if stop {
                handle.command(TransportCommand::Stop).unwrap();
                assert!(engine.cycle().unwrap());
            } else {
                drop(handle);
                assert!(!engine.cycle().unwrap());
            }
            // Neither cleanup path can rely on the natural off at 237.5 ms:
            // it has not been submitted and playback has now ended.
            engine.output.deliver_until(origin + ms(1000));
            assert!(engine.output.notes.is_empty(), "Stop: {stop}");
            let last = engine.output.delivered.last().unwrap();
            assert_eq!(last.at, origin + ms(126));
            assert!(matches!(last.message, MidiMessage::NoteOff { note, .. } if note.get() == 60));
        }
    }

    #[test]
    fn transport_cleanup_ends_notes_that_race_cancellation() {
        for cancel_first in [false, true] {
            for command in [
                Some(TransportCommand::Stop),
                Some(TransportCommand::Play),
                Some(TransportCommand::Locate(Ticks(960))),
                Some(TransportCommand::Panic),
                None,
            ] {
                let initial = project(vec![track(1, 10, 1920, &[(0, 60, 216), (216, 62, 240)])]);
                let (mut handle, link) = link(initial);
                let origin = Instant::now();
                let clock = SharedClock {
                    origin,
                    nanos: Arc::new(AtomicU64::new(0)),
                };
                let mut engine = Engine::new(
                    link,
                    ScheduledOutput::new(),
                    clock,
                    EngineConfig {
                        ports: vec![PortSettings::default()],
                        realtime: false,
                        ..EngineConfig::default()
                    },
                );
                handle.command(TransportCommand::Play).unwrap();
                while engine.clock.now() < origin + ms(110) {
                    assert!(engine.cycle().unwrap());
                }
                engine.output.deliver_until(engine.clock.now());
                assert_eq!(engine.scheduled.len(), 2);
                // Stop samples 110 ms. Inside cancel_from, time reaches
                // 113 ms, crossing the queued off/on pair at 112.5 ms.
                engine.output.cancel_advance =
                    Some((engine.clock.clone(), origin + ms(113), cancel_first));
                if let Some(command) = command {
                    handle.command(command).unwrap();
                    assert!(engine.cycle().unwrap());
                    engine.output.deliver_until(engine.clock.now());
                    if command != TransportCommand::Panic {
                        assert!(
                            !engine
                                .output
                                .notes
                                .is_sounding(Channel::DRUMS, U7::saturating(62)),
                            "{command:?}"
                        );
                    }
                    while engine.clock.now() < origin + ms(400) {
                        assert!(engine.cycle().unwrap());
                    }
                    handle.command(TransportCommand::Stop).unwrap();
                    assert!(engine.cycle().unwrap());
                } else {
                    drop(handle);
                    assert!(!engine.cycle().unwrap());
                }
                engine.output.deliver_until(origin + ms(1000));
                assert!(
                    engine.output.notes.is_empty(),
                    "{command:?}, cancellation first: {cancel_first}"
                );
                if !cancel_first {
                    assert!(engine.output.delivered.iter().any(|message| {
                        message.at == origin + Duration::from_micros(112_500)
                            && matches!(message.message, MidiMessage::NoteOn { note, .. } if note.get() == 62)
                    }), "the backend must actually deliver the racing note-on");
                }
            }
        }
    }

    #[test]
    fn transport_changes_cancel_queued_work_before_ending_delivered_notes() {
        // At 110 ms, note 60 really sounds, but its off and note 62's on at
        // 125 ms have both been submitted. Projected note state alone would
        // miss note 60 and send an off for note 62 before it ever plays.
        for command in [
            Some(TransportCommand::Stop),
            Some(TransportCommand::Play),
            Some(TransportCommand::Locate(Ticks(960))),
            Some(TransportCommand::Panic),
            None, // handle dropped: shutdown
        ] {
            let part = track(1, 10, 1920, &[(0, 60, 240), (240, 62, 240)]);
            let (mut handle, mut engine) = engine(project(vec![part]), ScheduledOutput::new());
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(110));
            engine.output.deliver_until(engine.clock.now());
            assert_eq!(engine.scheduled.len(), 2);
            assert!(
                engine
                    .output
                    .notes
                    .is_sounding(Channel::DRUMS, U7::saturating(60))
            );
            if let Some(command) = command {
                apply(&mut handle, &mut engine, command);
                run_until(&mut engine, origin + ms(400));
                apply(&mut handle, &mut engine, TransportCommand::Stop);
            } else {
                drop(handle);
                assert!(!engine.cycle().unwrap());
            }
            engine.output.deliver_until(origin + ms(1000));
            assert!(engine.output.notes.is_empty(), "{command:?}");
            assert!(engine.output.delivered.iter().any(|message| {
                message.at == origin + ms(110)
                    && matches!(message.message, MidiMessage::NoteOff { note, .. } if note.get() == 60)
            }), "the actually sounding note must end: {command:?}");
            if command != Some(TransportCommand::Panic) {
                assert!(!engine.output.delivered.iter().any(|message| {
                    message.at == origin + ms(125)
                        && matches!(message.message, MidiMessage::NoteOn { note, .. } if note.get() == 62)
                }), "stale note-on: {command:?}");
            }
        }
    }

    #[test]
    fn oversized_and_retained_render_batches_are_bounded_without_losing_events() {
        for scheduled in [false, true] {
            for (first, second) in [
                (RENDER_CAPACITY + 1, 0),                       // One oversized tick.
                (RENDER_CAPACITY - 1, RENDER_CAPACITY + 11),    // Retained + another batch.
                (RENDER_CAPACITY * 2 + 5, RENDER_CAPACITY + 5), // Queue backpressure.
            ] {
                let count = first + second;
                let dense = Arc::new(Track {
                    clip: Clip::new(
                        Ticks(1920),
                        (0..count)
                            .map(|index| ClipEvent {
                                start: Ticks(if index < first { 240 } else { 249 }),
                                kind: EventKind::Control {
                                    controller: U7::saturating(7),
                                    value: U7::saturating((index % 128) as u8),
                                },
                            })
                            .collect(),
                    )
                    .unwrap(),
                    ..(*track(1, 10, 1920, &[])).clone()
                });
                let output = if scheduled {
                    RecordingOutput::scheduled(ms(50))
                } else {
                    RecordingOutput::new()
                };
                let (mut handle, mut engine) = engine(project(vec![dense]), output);
                let capacity = engine.scheduled.capacity();
                let render_capacity = engine.rendered.capacity();
                let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
                run_until(&mut engine, origin + ms(115));
                if scheduled {
                    assert_eq!(engine.scheduled.len(), RENDER_CAPACITY);
                } else {
                    assert_eq!(engine.rendered.len(), RENDER_CAPACITY);
                }
                run_until(&mut engine, origin + ms(150));
                let messages = engine.output.messages();
                assert_eq!(messages.len(), count);
                for (index, message) in messages.iter().enumerate() {
                    assert_eq!(message.tick, Ticks(if index < first { 240 } else { 249 }));
                    assert!(matches!(message.message,
                        MidiMessage::ControlChange { value, .. }
                            if value.get() == (index % 128) as u8
                    ));
                }
                assert_eq!(engine.scheduled.capacity(), capacity);
                assert_eq!(engine.rendered.capacity(), render_capacity);
                assert!(engine.rendered.is_empty());
            }
        }
    }

    /// Hands what the engine sends to the test, across threads.
    struct ChannelOutput(mpsc::Sender<MidiMessage>);

    impl MidiOutput for ChannelOutput {
        type Error = Infallible;

        fn delivery(&self) -> Delivery {
            Delivery::Immediate
        }

        fn send(&mut self, message: &TimedMessage) -> Result<(), Infallible> {
            // The test may have stopped listening.
            let _ = self.0.send(message.message);
            Ok(())
        }

        fn cancel_from(
            &mut self,
            _from: Instant,
            _cancelled: &mut dyn FnMut(&TimedMessage),
        ) -> Result<(), Infallible> {
            Ok(())
        }
    }

    #[test]
    fn a_spawned_engine_ends_its_notes_and_returns_when_the_handle_is_dropped() {
        // A note held the whole beat, every beat, so one is always sounding.
        // The manual clock skips the waiting: the engine plays as fast as it
        // can until it sees the handle go.
        let held = track(1, 10, 960, &[(0, 60, 960)]);
        let (mut handle, link) = link(project(vec![held]));
        let (output, sent) = mpsc::channel();
        let config = EngineConfig {
            lookahead: ms(20),
            ports: vec![PortSettings::default()],
            realtime: false,
        };
        let engine = Engine::new(link, ChannelOutput(output), ManualClock::new(), config)
            .spawn()
            .unwrap();
        handle.command(TransportCommand::Play).unwrap();
        let first = sent.recv().unwrap();
        drop(handle);
        engine.join().unwrap();

        // The engine thread has dropped the output, so this ends.
        let messages: Vec<MidiMessage> = std::iter::once(first).chain(sent.iter()).collect();
        // Every note-on is followed by its note-off, however far it got.
        assert!(messages.len().is_multiple_of(2), "{messages:?}");
        for pair in messages.chunks(2) {
            assert!(
                matches!(
                    pair,
                    [MidiMessage::NoteOn { .. }, MidiMessage::NoteOff { .. }]
                ),
                "{messages:?}"
            );
        }
    }
}
