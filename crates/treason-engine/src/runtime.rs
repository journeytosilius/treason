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
use crate::notes::{HOLD_PEDALS, OwnedNotes};
use crate::render::{ClockPulses, RenderedEvent, render_bounded};
use crate::timebase::Timebase;
use crate::transport::{self, Transport, TransportEffect};

/// How many rendered events can wait for submission on each port, without
/// allocating.
const RENDER_CAPACITY: usize = 4096;

/// The longest the engine sleeps before checking for commands and new
/// snapshots again. A shorter lookahead shortens it.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Time for clock receivers to prepare after Start or Continue.
const CLOCK_SETUP: Duration = Duration::from_millis(1);

/// All Notes Off, which panic sends on every channel.
const ALL_NOTES_OFF: U7 = U7::saturating(123);

/// All Sound Off also silences voices that ignore All Notes Off.
const ALL_SOUND_OFF: U7 = U7::saturating(120);

/// A snapshot staged for a playback bar.
struct SnapshotSwitch {
    line: Ticks,
    project: Arc<Project>,
}

/// One port's share of playback. Each port renders and sends at its own
/// pace, its latency ahead of when its messages are heard.
struct PortStream {
    /// How much earlier than they are heard its messages go out.
    latency: Duration,
    /// Rendered events not sent yet, in send order.
    rendered: Vec<RenderedEvent>,
    /// The next tick to render.
    cursor: Ticks,
    /// The last event rendered when a full buffer splits a tick.
    render_after: Option<RenderedEvent>,
    /// Whether this port has submitted its side of the waiting switch,
    /// perhaps early. Only delivery makes the switch irreversible.
    switched: bool,
}

impl PortStream {
    fn new(settings: &PortSettings) -> Self {
        Self {
            latency: settings.latency,
            rendered: Vec::with_capacity(RENDER_CAPACITY),
            cursor: Ticks::ZERO,
            render_after: None,
            switched: false,
        }
    }

    /// Drops the events not sent yet.
    fn clear(&mut self) {
        self.rendered.clear();
        self.render_after = None;
        self.switched = false;
    }

    /// Drops what was rendered from `tick` on, to render it again.
    fn rewind(&mut self, tick: Ticks) {
        self.rendered.retain(|event| event.tick < tick);
        if self.cursor >= tick {
            self.cursor = tick;
            self.render_after = None;
        }
    }
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
/// The timebase says when each tick is heard. Each port sends its messages
/// its latency earlier, so a port with more latency runs ahead of the others.
/// Playback starts, and changes take effect, no earlier than the port with
/// the most latency can still play them, so every port plays them together.
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
    /// Each port's rendering and sending, in `PortId` order.
    streams: Vec<PortStream>,
    /// The most latency of any port.
    max_latency: Duration,
    /// How long after a start its first tick is heard: long enough for the
    /// port with the most latency to send it, and for clock receivers to
    /// prepare.
    start_lead: Duration,
    /// When ticks are heard, anchored at the last start, locate or tempo
    /// change.
    timebase: Timebase,
    /// Where playback last started. Nothing before it plays.
    started: Ticks,
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
        let latencies = config.ports.iter().map(|port| port.latency);
        let max_latency = latencies.max().unwrap_or_default();
        let clock_latencies = config.ports.iter().filter(|port| port.clock);
        let start_lead = clock_latencies
            .map(|port| port.latency + CLOCK_SETUP)
            .fold(max_latency, Duration::max);
        Self {
            link,
            output,
            clock,
            transport: Transport::new(),
            sounding: vec![OwnedNotes::new(); config.ports.len()],
            delivered: vec![OwnedNotes::new(); config.ports.len()],
            delivered_pulses: vec![None; config.ports.len()],
            // A full render batch plus one switch's note-offs and pedal
            // releases on every port.
            scheduled: Vec::with_capacity(
                RENDER_CAPACITY + config.ports.len() * 16 * (128 + HOLD_PEDALS.len()),
            ),
            streams: config.ports.iter().map(PortStream::new).collect(),
            max_latency,
            start_lead,
            timebase,
            started: Ticks::ZERO,
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
            self.render_and_send_until(now, Duration::ZERO, now)?;
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
            let limit = match self.output.delivery() {
                Delivery::Immediate => now,
                Delivery::Scheduled { .. } => now + ahead,
            };
            self.render_and_send_until(now, ahead, limit)?;
            self.complete_switch(now);
            self.transport.advance_to(self.playhead(now));
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

    /// The tick heard at `now`, or where playback started until that is
    /// heard.
    fn playhead(&self, now: Instant) -> Ticks {
        self.timebase.tick_at(now).max(self.started)
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
    ///
    /// While playing, a change takes effect after the last tick that the port
    /// with the most latency may have sent. The snapshot waits while a port
    /// already plays the waiting switch, or while the ticks before the last
    /// tempo change are still to be heard: both end within that latency.
    fn check_snapshot(&mut self, now: Instant) -> Result<(), O::Error> {
        let latest = self.link.project();
        let newest = self.waiting.as_ref().map_or(&self.project, |s| &s.project);
        if Arc::ptr_eq(&*latest, newest) {
            return Ok(());
        }
        // Later than anything any port has sent.
        let heard = now + self.max_latency;
        let retempo = latest.tempo != self.timebase.tempo();
        if self.playing()
            && (self
                .waiting
                .as_ref()
                .is_some_and(|waiting| self.timebase.time_of(waiting.line) <= heard)
                || (retempo && !self.timebase.settled(now)))
        {
            return Ok(());
        }
        let latest = Guard::into_inner(latest);
        if !self.playing() {
            self.adopt(latest);
            return Ok(());
        }
        let sent = self.timebase.tick_at(heard);
        if retempo {
            // All work sent so far keeps the old mapping. Rebuild everything
            // after it, including backend submissions, at the new tempo from
            // `heard`, rather than from the future render boundary.
            self.rewind_from(self.timebase.time_of(Ticks(sent.0 + 1)) - self.max_latency)?;
            self.timebase.retempo(sent, heard, latest.tempo);
        }
        let bar = self.project.time_signature.bar_length().0;
        let line = Ticks((sent.0 / bar + 1) * bar);
        // The next playback bar may already be rendered or submitted. Trim
        // both there on every port before staging the latest edit (or undo).
        self.rewind_from(self.timebase.time_of(line) - self.max_latency)?;
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
            self.transport.advance_to(self.playhead(now));
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
            }
            // Anchor clips and pulses together, once the port with the most
            // latency can send them and the receivers are ready.
            let heard = self.clock.now() + self.start_lead;
            self.timebase = Timebase::new(heard, at, self.project.tempo);
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

    /// Plays from `at`. Ends the notes sounding at `position` and takes the
    /// newest snapshot at once, since nothing carries over a jump.
    fn restart(&mut self, at: Ticks, now: Instant, position: Ticks) -> Result<(), O::Error> {
        self.cancel_scheduled(now + Duration::from_nanos(1))?;
        self.end_notes(now, position)?;
        self.settle();
        self.started = at;
        for stream in &mut self.streams {
            stream.cursor = at;
        }
        Ok(())
    }

    /// Drops the events not sent yet and moves to the newest snapshot at once.
    fn settle(&mut self) {
        for stream in &mut self.streams {
            stream.clear();
        }
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
            for channel in (0..16).filter_map(Channel::from_index) {
                self.release_pedals(port, channel, at, tick, None)?;
            }
        }
        Ok(())
    }

    /// Releases a channel's pedal holds, including when no keys remain.
    /// Pedals affect the whole MIDI channel, including other tracks on it.
    fn release_pedals(
        &mut self,
        port: PortId,
        channel: Channel,
        at: Instant,
        tick: Ticks,
        track: Option<TrackId>,
    ) -> Result<(), O::Error> {
        for controller in HOLD_PEDALS {
            if self.sounding[usize::from(port.0)].pedal_is_down(channel, controller) {
                self.emit(
                    TimedMessage {
                        at,
                        tick,
                        port,
                        message: MidiMessage::ControlChange {
                            channel,
                            controller,
                            value: U7::MIN,
                        },
                    },
                    track,
                )?;
            }
        }
        Ok(())
    }

    /// Ends notes and releases pedals, then sends All Sound Off and All
    /// Notes Off everywhere, including for untracked voices. Playback carries on.
    fn panic(&mut self, at: Instant, tick: Ticks) -> Result<(), O::Error> {
        // Playback continues: each port renders its cancelled future again,
        // including its note-offs, after ending the notes that really
        // sounded.
        self.rewind_from(at + Duration::from_nanos(1))?;
        self.end_notes(at, tick)?;
        for port in (0..).map(PortId).take(self.sounding.len()) {
            for channel in (0..16).filter_map(Channel::from_index) {
                for controller in HOLD_PEDALS
                    .into_iter()
                    .chain([ALL_SOUND_OFF, ALL_NOTES_OFF])
                {
                    let message = TimedMessage {
                        at,
                        tick,
                        port,
                        message: MidiMessage::ControlChange {
                            channel,
                            controller,
                            value: U7::MIN,
                        },
                    };
                    self.emit(message, None)?;
                }
            }
        }
        Ok(())
    }

    /// Renders each port up to what is heard `reach` after `now`, plus its
    /// latency, and sends what is due by `limit`. Drains oversized windows in
    /// bounded batches. Scheduled backpressure or events not due yet leave
    /// the rest for a later cycle.
    fn render_and_send_until(
        &mut self,
        now: Instant,
        reach: Duration,
        limit: Instant,
    ) -> Result<(), O::Error> {
        for index in 0..self.streams.len() {
            let heard = now + reach + self.streams[index].latency;
            let horizon = Ticks(self.timebase.tick_at(heard).0 + 1);
            loop {
                self.render_until(index, horizon);
                self.send_until(index, limit)?;
                let stream = &self.streams[index];
                if stream.cursor >= horizon
                    || !stream.rendered.is_empty()
                    || self.scheduled.len() >= RENDER_CAPACITY
                {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Renders only what fits, preserving a continuation within a tick.
    /// Both sides of a staged bar use their own snapshot.
    fn render_until(&mut self, index: usize, horizon: Ticks) {
        let port = PortId(index as u16);
        let stream = &mut self.streams[index];
        while stream.cursor < horizon && stream.rendered.len() < stream.rendered.capacity() {
            let (project, end) = match &self.waiting {
                Some(waiting) if stream.cursor >= waiting.line => (&waiting.project, horizon),
                Some(waiting) => (&self.project, horizon.min(waiting.line)),
                None => (&self.project, horizon),
            };
            let clock = ClockPulses {
                ports: &self.clock_ports,
                from: self.pulses_from,
            };
            stream.render_after = render_bounded(
                project,
                clock,
                port,
                stream.cursor..end,
                stream.render_after,
                &mut stream.rendered,
            );
            if let Some(last) = stream.render_after {
                stream.cursor = last.tick;
                return;
            }
            stream.cursor = end;
        }
    }

    /// Sends the port's rendered events due by `limit`. On reaching a
    /// switch's bar line, first ends the notes of its tracks that changed.
    fn send_until(&mut self, index: usize, limit: Instant) -> Result<(), O::Error> {
        let latency = self.streams[index].latency;
        let mut sent = 0;
        loop {
            // Leave room for a switch's forced note-offs. If a dense window
            // fills the mirror, keep the rest rendered until delivery frees
            // space; never grow the mirror on the engine thread.
            if self.scheduled.len() >= RENDER_CAPACITY {
                break;
            }
            let stream = &self.streams[index];
            let next = stream.rendered.get(sent).copied();
            if let Some(waiting) = &self.waiting
                && !stream.switched
                && next.map_or(stream.cursor >= waiting.line, |event| {
                    event.tick >= waiting.line
                })
                && self.timebase.time_of(waiting.line) - latency <= limit
            {
                self.finish_switch(index)?;
                continue;
            }
            let Some(event) = next else { break };
            let at = self.timebase.time_of(event.tick) - latency;
            if at > limit {
                break;
            }
            sent += 1;
            self.send(at, event)?;
        }
        self.streams[index].rendered.drain(..sent);
        Ok(())
    }

    /// Submits the port's forced note-offs before any new events at the
    /// boundary. Keeps the snapshot pending until actual playback reaches
    /// that line.
    fn finish_switch(&mut self, index: usize) -> Result<(), O::Error> {
        let Some(waiting) = &self.waiting else {
            return Ok(());
        };
        let line = waiting.line;
        let port = PortId(index as u16);
        let at = self.timebase.time_of(line) - self.streams[index].latency;
        for track_index in 0..self.project.tracks.len() {
            let track = &self.project.tracks[track_index];
            // A muted track has no notes of its own to end.
            if track.target.port != port
                || track.muted
                || carries_on(track, &self.waiting.as_ref().unwrap().project)
            {
                continue;
            }
            let id = track.id;
            let channel = track.target.channel;
            let mut notes = self.sounding[index].for_track(id, channel);
            for message in notes.drain_channel(channel) {
                let message = TimedMessage {
                    at,
                    tick: line,
                    port,
                    message,
                };
                self.emit(message, Some(id))?;
            }
            self.release_pedals(port, channel, at, line, Some(id))?;
        }
        self.streams[index].switched = true;
        Ok(())
    }

    /// Only delivery, not early submission, makes a switch irreversible:
    /// every port must have submitted its side and reached its send boundary.
    fn complete_switch(&mut self, now: Instant) {
        if self.waiting.as_ref().is_some_and(|waiting| {
            self.streams.iter().all(|stream| {
                stream.switched && self.timebase.time_of(waiting.line) - stream.latency <= now
            })
        }) {
            let waiting = self.waiting.take().unwrap();
            self.adopt(waiting.project);
        }
    }

    /// Sends one rendered event and keeps track of the notes it starts and
    /// ends. A note-off for a note this track no longer owns, such as one
    /// Stop ended or another track retriggered, isn't sent. Neither is a
    /// clock pulse the receiver already has.
    fn send(&mut self, at: Instant, event: RenderedEvent) -> Result<(), O::Error> {
        let port = usize::from(event.port.0);
        if event.message == MidiMessage::TimingClock
            && self.delivered_pulses[port].is_some_and(|tick| event.tick <= tick)
        {
            return Ok(());
        }
        if let MidiMessage::NoteOff { channel, note, .. } = event.message
            && !event
                .track
                .is_some_and(|track| self.sounding[port].owns(track, channel, note))
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

    /// Withdraws everything that goes out at or after `from` and rewinds
    /// each port to the first tick it would send from then on, to render it
    /// again. A port with more latency sends its ticks earlier, so it keeps
    /// more of them.
    fn rewind_from(&mut self, from: Instant) -> Result<(), O::Error> {
        self.cancel_scheduled(from)?;
        for stream in &mut self.streams {
            let before = from + stream.latency - Duration::from_nanos(1);
            let first = if before < self.timebase.time_of(self.started) {
                // tick_at saturates at zero before playback starts. No tick
                // has been sent yet, so the start tick must be rebuilt too.
                self.started
            } else {
                Ticks(self.timebase.tick_at(before).0 + 1).max(self.started)
            };
            stream.rewind(first);
            if self
                .waiting
                .as_ref()
                .is_some_and(|waiting| first <= waiting.line)
            {
                stream.switched = false;
            }
        }
        Ok(())
    }

    /// Invalidates backend work and reconstructs projected notes. Delivery
    /// can race cancellation: any removed note-on or pedal-on due by then
    /// might have sounded. Keep those notes and holds until cleanup;
    /// never assume a removed note-off or pedal-off was delivered.
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
                && (matches!(message.message, MidiMessage::NoteOn { velocity, .. } if velocity != U7::MIN)
                    || matches!(message.message, MidiMessage::ControlChange { controller, value, .. }
                        if HOLD_PEDALS.contains(&controller) && value.get() >= 64))
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
    /// and render ahead, at the next queued delivery, and for an output that
    /// takes messages only when they are due, at the next event or bar-line
    /// switch on any port.
    fn wake_time(&self, now: Instant) -> Instant {
        let mut wake = now + POLL_INTERVAL.min(self.config.lookahead);
        // Ports send at their own pace, so the queue isn't in time order.
        if let Some(next) = self.scheduled.iter().map(|event| event.timed.at).min() {
            wake = wake.min(next);
        }
        if self.playing() && self.output.delivery() == Delivery::Immediate {
            for stream in &self.streams {
                if let Some(event) = stream.rendered.first() {
                    wake = wake.min(self.timebase.time_of(event.tick) - stream.latency);
                }
                if let Some(waiting) = &self.waiting
                    && !stream.switched
                {
                    wake = wake.min(self.timebase.time_of(waiting.line) - stream.latency);
                }
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
        engine_with_ports(project, output, [first_port, PortSettings::default()])
    }

    /// An engine with a 20 ms lookahead and two ports with these settings.
    fn engine_with_ports<O: MidiOutput>(
        project: Arc<Project>,
        output: O,
        ports: [PortSettings; 2],
    ) -> (EngineHandle, Engine<O, ManualClock>) {
        let (handle, link) = link(project);
        let config = EngineConfig {
            lookahead: ms(20),
            ports: ports.to_vec(),
            realtime: false,
        };
        (
            handle,
            Engine::new(link, output, ManualClock::new(), config),
        )
    }

    /// A port with `millis` of latency.
    fn latency(millis: u64) -> PortSettings {
        PortSettings {
            latency: ms(millis),
            ..PortSettings::default()
        }
    }

    /// `track`, moved to port 1.
    fn on_port_1(track: Arc<Track>) -> Arc<Track> {
        let mut track = (*track).clone();
        track.target.port = PortId(1);
        Arc::new(track)
    }

    /// Whether every port has submitted its side of the waiting switch.
    fn submitted<O, C>(engine: &Engine<O, C>) -> bool {
        engine.waiting.is_some() && engine.streams.iter().all(|stream| stream.switched)
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
            .map(|sent| format!("{:?} {}", sent.at - origin, what(sent)))
            .collect()
    }

    /// Takes what the engine sent, as [`sent`] does, in one list per port.
    fn sent_per_port(engine: &mut TestEngine, origin: Instant) -> [Vec<String>; 2] {
        let mut ports = [Vec::new(), Vec::new()];
        for sent in engine.output.take() {
            let line = format!("{:?} {}", sent.at - origin, what(&sent));
            ports[usize::from(sent.port.0)].push(line);
        }
        ports
    }

    /// What a message is, such as "off 36" or "clock".
    fn what(sent: &TimedMessage) -> String {
        match sent.message {
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
        }
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
                engine.streams[0].cursor > Ticks(3840),
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
            assert!(submitted(&engine));
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
        assert!(submitted(&engine));
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
            assert!(submitted(&engine));
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
    fn cleanup_releases_pedal_held_notes_after_the_keys_have_ended() {
        // A one-key receiver that respects sustain and sostenuto: CC123
        // leaves a pedal-held voice sounding, whereas pedal release or
        // CC120 silences it. This deliberately differs from ActiveNotes.
        fn sounding(messages: &[TimedMessage], pedal: U7) -> bool {
            let (mut key, mut down, mut latched, mut voice) = (false, false, false, false);
            for sent in messages {
                if sent.port != PortId(0) {
                    continue;
                }
                match sent.message {
                    MidiMessage::NoteOn { velocity, .. } if velocity != U7::MIN => {
                        key = true;
                        voice = true;
                    }
                    MidiMessage::NoteOff { .. } => {
                        key = false;
                        voice &= down && (pedal.get() == 64 || latched);
                    }
                    MidiMessage::ControlChange {
                        controller, value, ..
                    } if controller == pedal => {
                        down = value.get() >= 64;
                        latched = down && key;
                        if !down && !key {
                            voice = false;
                        }
                    }
                    MidiMessage::ControlChange {
                        controller: ALL_SOUND_OFF,
                        ..
                    } => voice = false,
                    _ => {}
                }
            }
            voice
        }

        for scheduled in [false, true] {
            for pedal in HOLD_PEDALS {
                for cleanup in [
                    "stop", "restart", "locate", "panic", "shutdown", "mute", "remove", "clip",
                    "port", "channel",
                ] {
                    let mut held = (*track(1, 10, 7680, &[(0, 60, 960)])).clone();
                    let mut events = held.clip.events().to_vec();
                    for (tick, value) in [(480, U7::MAX), (6000, U7::MIN)] {
                        events.push(ClipEvent {
                            start: Ticks(tick),
                            kind: EventKind::Control {
                                controller: pedal,
                                value,
                            },
                        });
                    }
                    held.clip = Clip::new(held.clip.length(), events).unwrap();
                    let output = if scheduled {
                        RecordingOutput::scheduled(ms(50))
                    } else {
                        RecordingOutput::new()
                    };
                    let (mut handle, mut engine) =
                        engine(project(vec![Arc::new(held.clone())]), output);
                    let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
                    run_until(&mut engine, origin + ms(1000));
                    assert!(engine.sounding[0].notes.is_empty());
                    assert!(sounding(engine.output.messages(), pedal));
                    match cleanup {
                        "stop" => {
                            apply(&mut handle, &mut engine, TransportCommand::Stop);
                        }
                        "restart" => {
                            // Inspect cleanup before the next cycle plays
                            // the new note at the top of the clip.
                            engine
                                .command(TransportCommand::Play, engine.clock.now())
                                .unwrap();
                        }
                        "locate" => {
                            apply(
                                &mut handle,
                                &mut engine,
                                TransportCommand::Locate(Ticks(1920)),
                            );
                        }
                        "panic" => {
                            apply(&mut handle, &mut engine, TransportCommand::Panic);
                        }
                        "shutdown" => {
                            drop(handle);
                            assert!(!engine.cycle().unwrap());
                        }
                        _ => {
                            match cleanup {
                                "mute" => held.muted = true,
                                "clip" => held.clip = Clip::new(Ticks(7680), Vec::new()).unwrap(),
                                "port" => held.target.port = PortId(1),
                                "channel" => held.target.channel = Channel::from_number(3).unwrap(),
                                _ => {}
                            }
                            let tracks = if cleanup == "remove" {
                                vec![]
                            } else {
                                vec![Arc::new(held)]
                            };
                            handle.publish(project(tracks));
                            run_until(&mut engine, origin + ms(2100));
                        }
                    }
                    assert!(
                        !sounding(engine.output.messages(), pedal),
                        "{cleanup}, pedal {pedal}, scheduled {scheduled}"
                    );
                }
            }
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
                (1..=16).flat_map(move |channel| {
                    [64, 66, 120, 123].map(move |controller| {
                        format!("100ms port {port} channel {channel} cc {controller}=0")
                    })
                })
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
                // The 1 ms setup lead shifts the 5 ms polling deadlines.
                &["11ms off 60", "11ms Stop"]
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
    fn slower_ports_send_earlier_so_every_port_sounds_in_time() {
        let kick = track(1, 10, 960, &[(0, 36, 240)]);
        let bass = on_port_1(track(2, 1, 960, &[(0, 40, 240)]));
        let (mut handle, mut engine) = engine_with_ports(
            project(vec![kick, bass]),
            RecordingOutput::new(),
            [latency(30), PortSettings::default()],
        );
        // From beat 2, heard 30 ms later, once the slower port can play it.
        apply(
            &mut handle,
            &mut engine,
            TransportCommand::Locate(Ticks(960)),
        );
        let origin = apply(&mut handle, &mut engine, TransportCommand::Continue);
        run_until(&mut engine, origin + ms(20));
        assert!(engine.cycle().unwrap());
        assert_eq!(handle.status().position, Ticks(960), "until it's heard");
        run_until(&mut engine, origin + ms(280));
        assert!(engine.cycle().unwrap());
        assert_eq!(handle.status().position, Ticks(1440));
        run_until(&mut engine, origin + ms(700));
        assert_eq!(
            sent_per_port(&mut engine, origin),
            [
                ["0ns on 36", "125ms off 36", "500ms on 36", "625ms off 36"],
                ["30ms on 40", "155ms off 40", "530ms on 40", "655ms off 40"],
            ]
        );
    }

    #[test]
    fn a_clock_port_with_latency_still_gets_time_to_prepare() {
        let kick = track(1, 10, 960, &[(0, 36, 240)]);
        let bass = on_port_1(track(2, 1, 960, &[(0, 40, 240)]));
        let slow_clock = PortSettings {
            latency: ms(30),
            clock: true,
        };
        let (mut handle, mut engine) = engine_with_ports(
            project(vec![kick, bass]),
            RecordingOutput::new(),
            [slow_clock, latency(10)],
        );
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(45));
        // Tick 0 is heard at 31 ms: the clock port's 30 ms of latency after
        // the receiver's 1 ms to prepare.
        assert_eq!(
            sent_per_port(&mut engine, origin),
            [
                vec![
                    "0ns Start",
                    "1ms clock",
                    "1ms on 36",
                    "21.833334ms clock",
                    "42.666667ms clock"
                ],
                vec!["21ms on 40"],
            ]
        );
    }

    #[test]
    fn a_bar_switch_reaches_each_port_as_it_plays_the_line() {
        for scheduled in [false, true] {
            // Both hold a note over the bar line, which is heard at 2.03 s,
            // and would play another on it.
            let first = track(1, 1, 7680, &[(3000, 60, 1680), (3840, 64, 120)]);
            let second = on_port_1(track(2, 2, 7680, &[(3000, 62, 1680), (3840, 65, 120)]));
            let output = if scheduled {
                RecordingOutput::scheduled(ms(50))
            } else {
                RecordingOutput::new()
            };
            let (mut handle, mut engine) = engine_with_ports(
                project(vec![first, second.clone()]),
                output,
                [latency(30), PortSettings::default()],
            );
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            // The slower port has rendered the next bar already.
            run_until(&mut engine, origin + ms(1985));
            let mut muted = (*second).clone();
            muted.muted = true;
            handle.publish(project(vec![
                track(1, 1, 7680, &[(3840, 71, 120), (4320, 70, 240)]),
                Arc::new(muted),
            ]));
            run_until(&mut engine, origin + ms(2500));
            assert_eq!(
                sent_per_port(&mut engine, origin),
                [
                    vec![
                        "1.5625s on 60",
                        "2s off 60",
                        "2s on 71",
                        "2.0625s off 71",
                        "2.25s on 70",
                        "2.375s off 70"
                    ],
                    vec!["1.5925s on 62", "2.03s off 62"],
                ],
                "scheduled {scheduled}"
            );
        }
    }

    /// When each message is heard: when it was sent plus its port's latency,
    /// from `origin`, with what it is. Only up to `until`.
    fn heard(
        sent: &[TimedMessage],
        port: u16,
        latency: Duration,
        origin: Instant,
        until: Duration,
    ) -> Vec<(Duration, String)> {
        sent.iter()
            .filter(|sent| sent.port == PortId(port))
            .map(|sent| (sent.at + latency - origin, what(sent)))
            .filter(|(heard, _)| *heard <= until)
            .collect()
    }

    #[test]
    fn a_tempo_change_keeps_queued_switch_cleanup_on_lower_latency_ports() {
        let held = on_port_1(track(1, 1, 7680, &[(3000, 60, 1680)]));
        let (mut handle, mut engine) = engine_with_ports(
            project(vec![held.clone()]),
            RecordingOutput::scheduled(ms(50)),
            [latency(30), PortSettings::default()],
        );
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(1985));
        let mut muted = (*held).clone();
        muted.muted = true;
        let edited = project(vec![Arc::new(muted)]);
        handle.publish(edited.clone());
        // Both ports have submitted the switch by 2010 ms, but port 1's
        // forced note-off at 2030 ms has not reached its send boundary.
        run_until(&mut engine, origin + ms(2015));
        assert!(engine.output.messages().iter().any(|message| {
            message.at == origin + ms(2030)
                && matches!(message.message, MidiMessage::NoteOff { .. })
        }));
        handle.publish(Arc::new(Project {
            tempo: Bpm::new(240.0).unwrap(),
            ..(*edited).clone()
        }));
        run_until(&mut engine, origin + ms(2600));
        assert_eq!(engine.timebase.tempo(), Bpm::new(240.0).unwrap());
        assert_eq!(
            sent_per_port(&mut engine, origin),
            [vec![], vec!["1.5925s on 60", "2.03s off 60"]]
        );
    }

    #[test]
    fn a_tempo_change_is_heard_from_the_same_tick_on_every_port() {
        for scheduled in [false, true] {
            // The kick heard at 1.03 s ends just after the change takes
            // effect, and the slower port has already handed that note-off
            // to a scheduled output: the change has to take it back.
            let kicks = |id| track(id, 10, 960, &[(0, 36, 220)]);
            let tracks = vec![kicks(1), on_port_1(kicks(2))];
            let output = if scheduled {
                RecordingOutput::scheduled(ms(50))
            } else {
                RecordingOutput::new()
            };
            let (mut handle, mut engine) = engine_with_ports(
                project(tracks.clone()),
                output,
                [latency(30), PortSettings::default()],
            );
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(1100));
            handle.publish(Arc::new(Project {
                tempo: Bpm::new(240.0).unwrap(),
                tracks,
                ..Project::default()
            }));
            run_until(&mut engine, origin + ms(2000));
            let sent = engine.output.messages();
            let slower = heard(sent, 0, ms(30), origin, ms(1900));
            let faster = heard(sent, 1, Duration::ZERO, origin, ms(1900));
            assert_eq!(slower, faster, "scheduled {scheduled}");
            let kicks: Vec<Duration> = faster
                .iter()
                .filter(|(_, what)| what.starts_with("on"))
                .map(|(heard, _)| *heard)
                .collect();
            // The change is heard from 1.13 s: the first kick after it comes
            // 200 ms later, and the kicks are 250 ms apart from then on.
            assert_eq!(
                kicks,
                [30, 530, 1030, 1330, 1580, 1830].map(ms),
                "scheduled {scheduled}"
            );
        }
    }

    #[test]
    fn tempo_changes_close_together_keep_every_port_in_time() {
        for scheduled in [false, true] {
            // A 16th every 16th, so every change cuts close to a note.
            let sixteenths = |id| track(id, 10, 240, &[(0, 42, 120)]);
            let tracks = vec![sixteenths(1), on_port_1(sixteenths(2))];
            let output = if scheduled {
                RecordingOutput::scheduled(ms(50))
            } else {
                RecordingOutput::new()
            };
            let (mut handle, mut engine) = engine_with_ports(
                project(tracks.clone()),
                output,
                [latency(30), PortSettings::default()],
            );
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            // The first change is heard from 1.16 s. The second comes before
            // that, and before the faster port sends the hat heard at 1.155 s
            // at the first tempo.
            for (at, tempo) in [(1130, 240.0), (1140, 60.0)] {
                run_until(&mut engine, origin + ms(at));
                handle.publish(Arc::new(Project {
                    tempo: Bpm::new(tempo).unwrap(),
                    tracks: tracks.clone(),
                    ..Project::default()
                }));
            }
            run_until(&mut engine, origin + ms(2500));
            assert_eq!(engine.timebase.tempo(), Bpm::new(60.0).unwrap());
            let sent = engine.output.messages();
            let slower = heard(sent, 0, ms(30), origin, ms(2400));
            let faster = heard(sent, 1, Duration::ZERO, origin, ms(2400));
            assert_eq!(slower, faster, "scheduled {scheduled}");
            let ons: Vec<Duration> = faster
                .iter()
                .filter(|(_, what)| what.starts_with("on"))
                .map(|(heard, _)| *heard)
                .collect();
            let gaps: Vec<Duration> = ons.windows(2).map(|pair| pair[1] - pair[0]).collect();
            assert_eq!(gaps[0], ms(125), "scheduled {scheduled}");
            assert_eq!(gaps[gaps.len() - 1], ms(250), "scheduled {scheduled}");
        }
    }

    #[test]
    fn a_panic_before_the_start_is_heard_plays_nothing_before_it() {
        // A short note just before beat 2, which playback starts from.
        let notes = |id| track(id, 10, 960, &[(0, 36, 240), (940, 38, 10)]);
        let (mut handle, mut engine) = engine_with_ports(
            project(vec![notes(1), on_port_1(notes(2))]),
            RecordingOutput::new(),
            [latency(30), PortSettings::default()],
        );
        apply(
            &mut handle,
            &mut engine,
            TransportCommand::Locate(Ticks(960)),
        );
        let origin = apply(&mut handle, &mut engine, TransportCommand::Continue);
        run_until(&mut engine, origin + ms(10));
        apply(&mut handle, &mut engine, TransportCommand::Panic);
        run_until(&mut engine, origin + ms(200));
        let notes = sent_per_port(&mut engine, origin).map(|port| {
            port.into_iter()
                .filter(|line| !line.contains(" cc "))
                .collect::<Vec<_>>()
        });
        // The slower port's note ended with the panic. The faster port
        // hadn't started yet.
        assert_eq!(
            notes,
            [
                vec!["0ns on 36", "10ms off 36"],
                vec!["30ms on 36", "155ms off 36"],
            ]
        );
    }

    #[test]
    fn a_panic_before_tick_zero_is_sent_preserves_initial_notes_and_pulses() {
        for clock in [false, true] {
            let notes = |id| track(id, 10, 960, &[(0, 36, 240)]);
            let (mut handle, mut engine) = engine_with_ports(
                project(vec![notes(1), on_port_1(notes(2))]),
                RecordingOutput::scheduled(ms(50)),
                [
                    PortSettings {
                        latency: ms(30),
                        clock,
                    },
                    PortSettings {
                        clock,
                        ..PortSettings::default()
                    },
                ],
            );
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            run_until(&mut engine, origin + ms(15));
            assert!(engine.output.messages().iter().any(|message| {
                message.port == PortId(1)
                    && message.tick == Ticks::ZERO
                    && matches!(message.message, MidiMessage::NoteOn { .. })
                    && message.at > engine.clock.now()
            }));
            let panic_at = apply(&mut handle, &mut engine, TransportCommand::Panic) - origin;
            run_until(&mut engine, origin + ms(200));
            let sent = engine.output.messages();
            for port in [PortId(0), PortId(1)] {
                let on_port: Vec<_> = sent
                    .iter()
                    .filter(|message| message.port == port)
                    .copied()
                    .collect();
                let notes: Vec<_> = on_port
                    .iter()
                    .filter(|message| {
                        matches!(
                            message.message,
                            MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. }
                        )
                    })
                    .map(|message| (message.at - origin, message.tick, what(message)))
                    .collect();
                let setup = u64::from(clock);
                let expected = if port == PortId(0) {
                    vec![
                        (ms(setup), Ticks::ZERO, "on 36".to_owned()),
                        (panic_at, Ticks::ZERO, "off 36".to_owned()),
                    ]
                } else {
                    vec![
                        (ms(30 + setup), Ticks::ZERO, "on 36".to_owned()),
                        (ms(155 + setup), Ticks(240), "off 36".to_owned()),
                    ]
                };
                assert_eq!(notes, expected, "{port:?}, clock {clock}");
                let ticks: Vec<_> = pulses(&on_port).iter().map(|pulse| pulse.0).collect();
                if clock {
                    assert!(!ticks.is_empty());
                    let every_40: Vec<_> = (0..ticks.len() as u64).map(|n| n * 40).collect();
                    assert_eq!(ticks, every_40, "{port:?}");
                } else {
                    assert!(ticks.is_empty());
                }
            }
        }
    }

    #[test]
    fn panic_leaves_no_gap_and_repeats_nothing_on_any_port() {
        for scheduled in [false, true] {
            // A 16th of hi-hat every 8th: heard at 31 ms, 156 ms, 281 ms...
            let hats = |id| track(id, 10, 240, &[(0, 42, 120)]);
            let slow_clock = PortSettings {
                latency: ms(30),
                clock: true,
            };
            let fast_clock = PortSettings {
                clock: true,
                ..PortSettings::default()
            };
            let output = if scheduled {
                RecordingOutput::scheduled(ms(50))
            } else {
                RecordingOutput::new()
            };
            let (mut handle, mut engine) = engine_with_ports(
                project(vec![hats(1), on_port_1(hats(2))]),
                output,
                [slow_clock, fast_clock],
            );
            let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
            // The slower port has sent the hat heard at 156 ms, the faster
            // one hasn't.
            run_until(&mut engine, origin + ms(130));
            apply(&mut handle, &mut engine, TransportCommand::Panic);
            run_until(&mut engine, origin + ms(400));
            let sent = engine.output.messages();
            let only = |what: &str, heard: Vec<(Duration, String)>| -> Vec<Duration> {
                heard
                    .into_iter()
                    .filter(|(_, line)| line == what)
                    .map(|(heard, _)| heard)
                    .collect()
            };
            for what in ["on 42", "clock"] {
                let slower = only(what, heard(sent, 0, ms(30), origin, ms(400)));
                let faster = only(what, heard(sent, 1, Duration::ZERO, origin, ms(400)));
                assert_eq!(slower, faster, "{what}, scheduled {scheduled}");
            }
            let hats = only("on 42", heard(sent, 1, Duration::ZERO, origin, ms(400)));
            assert_eq!(hats, [31, 156, 281].map(ms), "scheduled {scheduled}");
            for port in [PortId(0), PortId(1)] {
                let on_port: Vec<TimedMessage> = sent
                    .iter()
                    .filter(|sent| sent.port == port)
                    .copied()
                    .collect();
                let ticks: Vec<u64> = pulses(&on_port).iter().map(|pulse| pulse.0).collect();
                let every_40: Vec<u64> = (0..ticks.len() as u64).map(|n| n * 40).collect();
                assert_eq!(ticks, every_40, "{port:?}, scheduled {scheduled}");
            }
        }
    }

    #[test]
    fn an_edit_during_a_switch_waits_until_every_port_has_played_the_line() {
        // Kicks on beats 1 and 3, so the rest of each bar shows which edit
        // plays it too.
        let kick = |id, note| track(id, id as u8, 3840, &[(0, note, 240), (1920, note, 240)]);
        let both = |note| project(vec![kick(1, note), on_port_1(kick(2, note))]);
        let (mut handle, mut engine) = engine_with_ports(
            both(36),
            RecordingOutput::new(),
            [latency(30), PortSettings::default()],
        );
        let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
        run_until(&mut engine, origin + ms(1000));
        handle.publish(both(38));
        // The line is heard at 2.03 s, and the slower port played it at 2 s.
        // An edit now can't change that bar on the faster port alone.
        run_until(&mut engine, origin + ms(2015));
        handle.publish(both(40));
        run_until(&mut engine, origin + ms(4100));
        assert_eq!(
            sent_per_port(&mut engine, origin),
            [
                [
                    "0ns on 36",
                    "125ms off 36",
                    "1s on 36",
                    "1.125s off 36",
                    "2s on 38",
                    "2.125s off 38",
                    "3s on 38",
                    "3.125s off 38",
                    "4s on 40"
                ],
                [
                    "30ms on 36",
                    "155ms off 36",
                    "1.03s on 36",
                    "1.155s off 36",
                    "2.03s on 38",
                    "2.155s off 38",
                    "3.03s on 38",
                    "3.155s off 38",
                    "4.03s on 40"
                ],
            ]
        );
    }

    /// Notes when each message actually goes out, by the engine's clock.
    struct PunctualOutput {
        clock: SharedClock,
        sent: Vec<(Instant, TimedMessage)>,
    }

    impl MidiOutput for PunctualOutput {
        type Error = Infallible;

        fn delivery(&self) -> Delivery {
            Delivery::Immediate
        }

        fn send(&mut self, message: &TimedMessage) -> Result<(), Infallible> {
            self.sent.push((self.clock.now(), *message));
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
    fn every_port_sends_each_message_right_when_it_is_due() {
        let kicks = |note| track(1, 10, 960, &[(0, note, 240), (480, 38, 100)]);
        // A bass note held over the first bar line, where the edit mutes it.
        let bass = on_port_1(track(2, 1, 3840, &[(0, 40, 700), (3000, 43, 1000)]));
        let muted = Arc::new(Track {
            muted: true,
            ..(*bass).clone()
        });
        let (mut handle, link) = link(project(vec![kicks(36), bass.clone()]));
        let origin = Instant::now();
        let clock = SharedClock {
            origin,
            nanos: Arc::new(AtomicU64::new(0)),
        };
        let output = PunctualOutput {
            clock: clock.clone(),
            sent: Vec::new(),
        };
        let slow_clock = PortSettings {
            latency: ms(30),
            clock: true,
        };
        let config = EngineConfig {
            lookahead: ms(20),
            ports: vec![slow_clock, latency(7)],
            realtime: false,
        };
        let mut engine = Engine::new(link, output, clock, config);
        handle.command(TransportCommand::Play).unwrap();
        // A tempo change, an edit and a panic, each in the first cycle at or
        // after its time.
        let mut changes = 0;
        while engine.clock.now() < origin + ms(3000) {
            let elapsed = engine.clock.now() - origin;
            if changes == 0 && elapsed >= ms(700) {
                handle.publish(Arc::new(Project {
                    tempo: Bpm::new(97.3).unwrap(),
                    tracks: vec![kicks(36), bass.clone()],
                    ..Project::default()
                }));
                changes += 1;
            } else if changes == 1 && elapsed >= ms(1500) {
                handle.publish(Arc::new(Project {
                    tempo: Bpm::new(97.3).unwrap(),
                    tracks: vec![kicks(37), muted.clone()],
                    ..Project::default()
                }));
                changes += 1;
            } else if changes == 2 && elapsed >= ms(2600) {
                handle.command(TransportCommand::Panic).unwrap();
                changes += 1;
            }
            assert!(engine.cycle().unwrap());
        }
        let sent = &engine.output.sent;
        assert!(sent.len() > 150, "{}", sent.len());
        for (when, message) in sent {
            assert_eq!(*when, message.at, "{message:?}");
        }
        let played =
            |wanted: fn(&MidiMessage) -> bool| sent.iter().any(|(_, sent)| wanted(&sent.message));
        assert!(played(
            |message| matches!(message, MidiMessage::NoteOn { note, .. } if note.get() == 37)
        ));
        assert!(played(
            |message| matches!(message, MidiMessage::ControlChange { controller, .. } if *controller == ALL_NOTES_OFF)
        ));
        // The faster port ends the bass note as it reaches the line, though it
        // has nothing else to send then.
        let line = engine.timebase.time_of(Ticks(3840));
        let bass_end = sent
            .iter()
            .rfind(|(_, sent)| {
                sent.port == PortId(1) && matches!(sent.message, MidiMessage::NoteOff { .. })
            })
            .map(|(when, sent)| (*when, what(sent)));
        assert_eq!(bass_end, Some((line - ms(7), "off 43".to_owned())));
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
                let render_capacity = engine.streams[0].rendered.capacity();
                let origin = apply(&mut handle, &mut engine, TransportCommand::Play);
                run_until(&mut engine, origin + ms(115));
                if scheduled {
                    assert_eq!(engine.scheduled.len(), RENDER_CAPACITY);
                } else {
                    assert_eq!(engine.streams[0].rendered.len(), RENDER_CAPACITY);
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
                assert_eq!(engine.streams[0].rendered.capacity(), render_capacity);
                assert!(engine.streams[0].rendered.is_empty());
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
