//! Kernel-scheduled output through an ALSA sequencer queue (Linux).
//!
//! The engine hands each message over up to the lookahead early, stamped
//! with when it should go out, and the kernel's queue sends it then. So the
//! engine thread's own timing jitter doesn't reach the MIDI cable, as long as
//! the thread runs within the lookahead. The queue runs on the sequencer's
//! default timer and sends each message on the timer's first tick at or after
//! its time. That timer ticks every millisecond, unless the `snd-seq`
//! module's `seq_default_timer_resolution` parameter asks for more ticks a
//! second, up to 6250.
//!
//! Cancelling has to say exactly which messages it took back, and the kernel
//! doesn't tell. So a private port of the same client is connected to every
//! output port: whatever the queue sends to the gear, it also sends there,
//! in the same dispatch, as a receipt. Cancelling removes all of the
//! client's queued messages at once, then queues an echo to the private port
//! for right away. The kernel sends the echo only after finishing any
//! dispatch already under way, so when it arrives, every message sent before
//! the removal has its receipt. The rest were removed: those due at or after
//! the cut are reported, and the earlier ones are queued again at their
//! times.

use std::collections::VecDeque;
use std::ffi::{CString, NulError};
use std::io;
use std::time::{Duration, Instant};

use alsa::Direction;
use alsa::poll::{Descriptors, pollfd};
use alsa::seq::{
    Addr, ClientIter, EvCtrl, EvNote, EvQueueControl, Event, EventType, PortCap, PortIter,
    PortSubscribe, PortType, Seq,
};
use thiserror::Error;
use treason_core::midi::{Channel, MidiMessage, U7, U14};
use treason_core::model::PortId;
use treason_core::output::{Delivery, MidiOutput, TimedMessage};

use crate::port::PortSpec;

/// The most events the kernel lets one client have queued, and the most it
/// holds for one to read.
const POOL: u32 = 2000;

/// How many messages can wait in the queue: all of the pool but one cell for
/// the echo. So neither the queue nor the receipts can overflow.
const QUEUED: usize = POOL as usize - 1;

/// How long to wait on the kernel before giving up. It sends every queued
/// message within the lookahead, which is at most a second.
const STALL: Duration = Duration::from_secs(2);

/// One ALSA sequencer client with a port per [`PortSpec`] and one queue.
///
/// The queue sends each message at its time ([`Delivery::Scheduled`]), so
/// the engine can hand messages over early.
pub struct AlsaSeqOutput {
    seq: Seq,
    /// This client's number.
    client: i32,
    queue: i32,
    /// The sequencer port of each [`PortId`].
    ports: Vec<i32>,
    /// The private port that gets a receipt for each message the queue sends.
    receipts: i32,
    /// When the queue's clock read zero.
    origin: Instant,
    lookahead: Duration,
    queued: Queued,
    /// For waiting until the kernel takes more messages.
    writable: Vec<pollfd>,
    /// For waiting until receipts arrive.
    readable: Vec<pollfd>,
}

impl AlsaSeqOutput {
    /// Opens a sequencer client named `client_name` with `ports`, and a queue
    /// that accepts messages up to `lookahead` before they are due.
    pub fn open(
        client_name: &str,
        ports: &[PortSpec],
        lookahead: Duration,
    ) -> Result<Self, AlsaSeqError> {
        let seq = Seq::open(None, None, true)?;
        let name = CString::new(client_name)?;
        seq.set_client_name(&name)?;
        seq.set_client_pool_output(POOL)?;
        seq.set_client_pool_input(POOL)?;
        let client = seq.client_id()?;
        // The output ports come first, so `treason:0` is the first one, as
        // with midir.
        let opened = ports
            .iter()
            .map(|spec| {
                Ok(seq.create_simple_port(
                    &CString::new(spec.name.as_str())?,
                    PortCap::READ | PortCap::SUBS_READ,
                    PortType::MIDI_GENERIC | PortType::APPLICATION,
                )?)
            })
            .collect::<Result<Vec<_>, AlsaSeqError>>()?;
        let receipts = seq.create_simple_port(
            c"receipts",
            PortCap::WRITE | PortCap::NO_EXPORT,
            PortType::APPLICATION,
        )?;
        for (spec, &port) in ports.iter().zip(&opened) {
            let ours = Addr { client, port };
            subscribe(
                &seq,
                ours,
                Addr {
                    client,
                    port: receipts,
                },
            )?;
            if let Some(pattern) = &spec.connect {
                let target = find_port(&seq, client, pattern)
                    .ok_or_else(|| AlsaSeqError::NoSuchPort(pattern.clone()))?;
                subscribe(&seq, ours, target)?;
            }
        }
        let mut writable = descriptors(&seq, Direction::Playback)?;
        let readable = descriptors(&seq, Direction::Capture)?;
        let queue = seq.alloc_named_queue(&name)?;
        seq.control_queue(queue, EventType::Start, 0, None)?;
        drain(&seq, &mut writable)?;
        // Reading the queue's clock takes a moment. Taking the start of that
        // moment for the time it read can only put the origin early, and so
        // each message late, never early.
        let before = Instant::now();
        let elapsed = seq.get_queue_status(queue)?.get_real_time();
        Ok(Self {
            seq,
            client,
            queue,
            ports: opened,
            receipts,
            origin: before.checked_sub(elapsed).unwrap_or(before),
            lookahead,
            queued: Queued::new(QUEUED),
            writable,
            readable,
        })
    }

    /// The queue's time for `at`. Times before the queue started are due at
    /// once.
    fn queue_time(&self, at: Instant) -> Duration {
        at.saturating_duration_since(self.origin)
    }

    /// Reads the receipts that have arrived, without waiting. Returns whether
    /// the echo was among them.
    fn take_receipts(&mut self) -> Result<bool, AlsaSeqError> {
        let mut input = self.seq.input();
        let mut echo = false;
        loop {
            match input.event_input_pending(true) {
                Ok(0) => return Ok(echo),
                Ok(_) => {}
                Err(error) if would_block(&error) => return Ok(echo),
                Err(error) => return Err(error.into()),
            }
            let event = input.event_input()?;
            let source = event.get_source();
            // Anything else was sent to the port directly, not by the queue.
            if source.client != self.client {
                continue;
            }
            if source.port == self.receipts && event.get_type() == EventType::Echo {
                echo = true;
            } else if let Some(port) = self.ports.iter().position(|&port| port == source.port) {
                let time = event.get_time().ok_or(AlsaSeqError::Receipt)?;
                self.queued.sent(PortId(port as u16), time)?;
            }
        }
    }

    /// Waits until the queue has room for another message, as it sends what
    /// it holds.
    fn make_room(&mut self) -> Result<(), AlsaSeqError> {
        if !self.queued.is_full() {
            return Ok(());
        }
        // Only messages the kernel has can be sent.
        drain(&self.seq, &mut self.writable)?;
        let deadline = Instant::now() + STALL;
        loop {
            self.take_receipts()?;
            if !self.queued.is_full() {
                return Ok(());
            }
            wait(&mut self.readable, deadline)?;
        }
    }

    /// Waits until every message handed over has its receipt.
    fn send_everything(&mut self) -> Result<(), AlsaSeqError> {
        drain(&self.seq, &mut self.writable)?;
        let deadline = Instant::now() + STALL;
        loop {
            self.take_receipts()?;
            if self.queued.messages.is_empty() {
                return Ok(());
            }
            wait(&mut self.readable, deadline)?;
        }
    }

    /// Queues an echo to the private port, due at once, and reads receipts
    /// until it arrives. The kernel sends it only after finishing any
    /// dispatch already under way, and that dispatch's receipts arrive before
    /// it, so afterwards every message the queue has started to send has its
    /// receipt.
    fn echo(&mut self) -> Result<(), AlsaSeqError> {
        let mut echo = Event::new(EventType::Echo, &[0_u8; 12]);
        echo.set_source(self.receipts);
        echo.set_dest(Addr {
            client: self.client,
            port: self.receipts,
        });
        echo.schedule_real(self.queue, false, Duration::ZERO);
        self.seq.event_output_buffer(&mut echo)?;
        drain(&self.seq, &mut self.writable)?;
        let deadline = Instant::now() + STALL;
        while !self.take_receipts()? {
            wait(&mut self.readable, deadline)?;
        }
        Ok(())
    }
}

impl MidiOutput for AlsaSeqOutput {
    type Error = AlsaSeqError;

    fn delivery(&self) -> Delivery {
        Delivery::Scheduled {
            lookahead: self.lookahead,
        }
    }

    fn send(&mut self, message: &TimedMessage) -> Result<(), Self::Error> {
        let port = *self
            .ports
            .get(usize::from(message.port.0))
            .ok_or(AlsaSeqError::UnknownPort(message.port))?;
        self.make_room()?;
        let time = self.queue_time(message.at);
        self.queued.push(time, *message)?;
        output(
            &self.seq,
            &mut self.writable,
            self.queue,
            port,
            time,
            &message.message,
        )
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        drain(&self.seq, &mut self.writable)?;
        self.take_receipts()?;
        Ok(())
    }

    fn cancel_from(
        &mut self,
        from: Instant,
        cancelled: &mut dyn FnMut(&TimedMessage),
    ) -> Result<(), Self::Error> {
        let from = self.queue_time(from);
        if !self.queued.any_from(from) {
            return Ok(());
        }
        // Every message handed over reaches the kernel, so each is either
        // sent or queued. Then removing them all leaves each either sent,
        // with a receipt to come, or removed. Removing only the ones due from
        // `from` on would take an `alsa::seq::RemoveEvents`, which allocates
        // and is used up by each call.
        drain(&self.seq, &mut self.writable)?;
        self.seq.drop_output()?;
        self.echo()?;
        // The ones still without a receipt were removed. Those due before
        // `from` go out as planned.
        self.queued.withdraw_from(from, cancelled);
        for &(time, message) in &self.queued.messages {
            output(
                &self.seq,
                &mut self.writable,
                self.queue,
                self.ports[usize::from(message.port.0)],
                time,
                &message.message,
            )?;
        }
        drain(&self.seq, &mut self.writable)
    }
}

impl Drop for AlsaSeqOutput {
    /// Waits for the queue to send what it holds. Closing the client would
    /// drop it, and with it the note-offs the engine sends on its way out.
    fn drop(&mut self) {
        let _ = self.send_everything();
    }
}

/// The messages handed to the queue whose receipts haven't been read, in the
/// order the queue sends them: by time, and those due at the same time in
/// the order they came.
struct Queued {
    messages: VecDeque<(Duration, TimedMessage)>,
    limit: usize,
}

impl Queued {
    fn new(limit: usize) -> Self {
        Self {
            messages: VecDeque::with_capacity(limit),
            limit,
        }
    }

    fn is_full(&self) -> bool {
        self.messages.len() >= self.limit
    }

    /// Whether any message is due at or after `from`.
    fn any_from(&self, from: Duration) -> bool {
        self.messages.back().is_some_and(|&(time, _)| time >= from)
    }

    /// Adds a message the queue sends at `time`. Fails rather than allocate.
    fn push(&mut self, time: Duration, message: TimedMessage) -> Result<(), AlsaSeqError> {
        if self.is_full() {
            return Err(AlsaSeqError::Full);
        }
        let index = self.messages.partition_point(|&(queued, _)| queued <= time);
        self.messages.insert(index, (time, message));
        Ok(())
    }

    /// Takes the message a receipt says the queue sent from `port` at `time`.
    ///
    /// That is the next one, unless a message handed over late, after the
    /// queue sent later ones, went before them. Of the messages with the same
    /// port and time, the queue sends the first first.
    fn sent(&mut self, port: PortId, time: Duration) -> Result<(), AlsaSeqError> {
        let index = self
            .messages
            .iter()
            .position(|&(queued, message)| queued == time && message.port == port)
            .ok_or(AlsaSeqError::Receipt)?;
        self.messages.remove(index);
        Ok(())
    }

    /// Drops the messages due at or after `from`, reporting each.
    fn withdraw_from(&mut self, from: Duration, cancelled: &mut dyn FnMut(&TimedMessage)) {
        let first = self.messages.partition_point(|&(time, _)| time < from);
        for (_, message) in self.messages.drain(first..) {
            cancelled(&message);
        }
    }
}

/// The sequencer event for `message`.
fn sequencer_event(message: &MidiMessage) -> Event<'static> {
    let note = |kind, channel: Channel, note: U7, velocity: U7| {
        let note = EvNote {
            channel: channel.index(),
            note: note.get(),
            velocity: velocity.get(),
            off_velocity: 0,
            duration: 0,
        };
        Event::new(kind, &note)
    };
    let control = |kind, channel: u8, param: u8, value: i32| {
        let control = EvCtrl {
            channel,
            param: param.into(),
            value,
        };
        Event::new(kind, &control)
    };
    let realtime = |kind| {
        Event::new(
            kind,
            &EvQueueControl {
                queue: 0,
                value: (),
            },
        )
    };
    match *message {
        MidiMessage::NoteOn {
            channel,
            note: key,
            velocity,
        } => note(EventType::Noteon, channel, key, velocity),
        MidiMessage::NoteOff {
            channel,
            note: key,
            velocity,
        } => note(EventType::Noteoff, channel, key, velocity),
        MidiMessage::PolyPressure {
            channel,
            note: key,
            pressure,
        } => note(EventType::Keypress, channel, key, pressure),
        MidiMessage::ControlChange {
            channel,
            controller,
            value,
        } => control(
            EventType::Controller,
            channel.index(),
            controller.get(),
            value.get().into(),
        ),
        MidiMessage::ProgramChange { channel, program } => control(
            EventType::Pgmchange,
            channel.index(),
            0,
            program.get().into(),
        ),
        MidiMessage::ChannelPressure { channel, pressure } => control(
            EventType::Chanpress,
            channel.index(),
            0,
            pressure.get().into(),
        ),
        // The sequencer counts bend from the center.
        MidiMessage::PitchBend { channel, value } => control(
            EventType::Pitchbend,
            channel.index(),
            0,
            i32::from(value.get()) - i32::from(U14::CENTER.get()),
        ),
        MidiMessage::SongPosition(position) => {
            control(EventType::Songpos, 0, 0, position.get().into())
        }
        MidiMessage::TimingClock => realtime(EventType::Clock),
        MidiMessage::Start => realtime(EventType::Start),
        MidiMessage::Continue => realtime(EventType::Continue),
        MidiMessage::Stop => realtime(EventType::Stop),
    }
}

/// Buffers `message` to go out of `port` at the queue's `time`.
fn output(
    seq: &Seq,
    writable: &mut [pollfd],
    queue: i32,
    port: i32,
    time: Duration,
    message: &MidiMessage,
) -> Result<(), AlsaSeqError> {
    let mut event = sequencer_event(message);
    event.set_source(port);
    event.set_subs();
    event.schedule_real(queue, false, time);
    loop {
        match seq.event_output_buffer(&mut event) {
            Ok(_) => return Ok(()),
            // The buffer is full.
            Err(error) if would_block(&error) => drain(seq, writable)?,
            Err(error) => return Err(error.into()),
        }
    }
}

/// Hands every buffered event to the kernel, waiting for room in the
/// client's pool if need be.
fn drain(seq: &Seq, writable: &mut [pollfd]) -> Result<(), AlsaSeqError> {
    let mut deadline = None;
    loop {
        match seq.drain_output() {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if would_block(&error) => {}
            Err(error) => return Err(error.into()),
        }
        let deadline = *deadline.get_or_insert_with(|| Instant::now() + STALL);
        wait(writable, deadline)?;
    }
}

/// Waits until `fds` are ready or a signal arrives, failing at `deadline`.
fn wait(fds: &mut [pollfd], deadline: Instant) -> Result<(), AlsaSeqError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(AlsaSeqError::Stalled);
    }
    let millis = i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1);
    match alsa::poll::poll(fds, millis) {
        Ok(_) => Ok(()),
        Err(error) if kind(&error) == io::ErrorKind::Interrupted => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// The descriptors to poll for `direction`.
fn descriptors(seq: &Seq, direction: Direction) -> Result<Vec<pollfd>, AlsaSeqError> {
    let source = (seq, Some(direction));
    let empty = pollfd {
        fd: 0,
        events: 0,
        revents: 0,
    };
    let mut fds = vec![empty; source.count()];
    source.fill(&mut fds)?;
    Ok(fds)
}

/// Connects `sender` to `dest`.
fn subscribe(seq: &Seq, sender: Addr, dest: Addr) -> Result<(), AlsaSeqError> {
    let subscription = PortSubscribe::empty()?;
    subscription.set_sender(sender);
    subscription.set_dest(dest);
    seq.subscribe_port(&subscription)?;
    Ok(())
}

/// The first port of another client that takes connections and has
/// `pattern` in its name. Names read as midir shows them,
/// "client:port c:p", so a pattern finds the same port with either backend.
fn find_port(seq: &Seq, own: i32, pattern: &str) -> Option<Addr> {
    ClientIter::new(seq)
        .filter(|client| client.get_client() != own)
        .find_map(|client| {
            let client_name = client.get_name().ok()?;
            PortIter::new(seq, client.get_client()).find_map(|port| {
                let addr = Addr {
                    client: port.get_client(),
                    port: port.get_port(),
                };
                let name = format!(
                    "{client_name}:{} {}:{}",
                    port.get_name().ok()?,
                    addr.client,
                    addr.port
                );
                (port
                    .get_capability()
                    .contains(PortCap::WRITE | PortCap::SUBS_WRITE)
                    && name.contains(pattern))
                .then_some(addr)
            })
        })
}

/// What kind of failure `error` is.
fn kind(error: &alsa::Error) -> io::ErrorKind {
    io::Error::from_raw_os_error(error.errno().abs()).kind()
}

/// Whether a call failed only because it would have had to wait.
fn would_block(error: &alsa::Error) -> bool {
    kind(error) == io::ErrorKind::WouldBlock
}

/// An ALSA sequencer failure.
#[derive(Debug, Error)]
pub enum AlsaSeqError {
    /// The sequencer refused a call.
    #[error("ALSA sequencer error")]
    Alsa(#[from] alsa::Error),
    /// A client or port name has a NUL byte in it.
    #[error("a MIDI port or client name has a NUL byte in it")]
    Name(#[from] NulError),
    /// No port matches a [`PortSpec::connect`] pattern.
    #[error("no MIDI output port has `{0}` in its name")]
    NoSuchPort(String),
    /// A message names a port that wasn't opened.
    #[error("there is no output port {0}")]
    UnknownPort(PortId),
    /// More messages are waiting than the queue holds.
    #[error("the ALSA sequencer queue is full")]
    Full,
    /// A receipt matches no queued message, so which messages were sent is
    /// no longer known.
    #[error("the ALSA sequencer sent a message treason didn't queue")]
    Receipt,
    /// The kernel took too long to send queued messages or the echo.
    #[error("the ALSA sequencer stopped sending")]
    Stalled,
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use alsa::seq::{EvCtrl, EvNote, EventType};
    use treason_core::midi::{Channel, MidiMessage, U7, U14};
    use treason_core::model::PortId;
    use treason_core::output::TimedMessage;
    use treason_core::time::Ticks;

    use super::{AlsaSeqError, Queued, sequencer_event};

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// A message on `port` whose tick tells it apart.
    fn message(port: u16, tick: u64) -> TimedMessage {
        TimedMessage {
            at: Instant::now(),
            tick: Ticks(tick),
            port: PortId(port),
            message: MidiMessage::TimingClock,
        }
    }

    /// A queue holding messages on port 0 at these times, ticked in order.
    fn queued(times: &[u64]) -> Queued {
        let mut queued = Queued::new(8);
        for (tick, &time) in times.iter().enumerate() {
            queued.push(ms(time), message(0, tick as u64)).unwrap();
        }
        queued
    }

    fn ticks(queued: &Queued) -> Vec<u64> {
        queued
            .messages
            .iter()
            .map(|(_, message)| message.tick.0)
            .collect()
    }

    #[test]
    fn queued_messages_keep_the_order_the_queue_sends_them_in() {
        // Ports send at their own pace, so times arrive out of order.
        let queued = queued(&[30, 10, 30, 20, 10]);
        // By time, and the same time in the order they came.
        assert_eq!(ticks(&queued), [1, 4, 3, 0, 2]);
    }

    #[test]
    fn a_receipt_takes_the_first_message_from_its_port_at_its_time() {
        let mut queued = Queued::new(8);
        queued.push(ms(10), message(0, 0)).unwrap();
        queued.push(ms(10), message(1, 1)).unwrap();
        queued.push(ms(10), message(1, 2)).unwrap();
        queued.push(ms(20), message(0, 3)).unwrap();
        queued.sent(PortId(1), ms(10)).unwrap();
        assert_eq!(ticks(&queued), [0, 2, 3]);
        queued.sent(PortId(0), ms(10)).unwrap();
        assert_eq!(ticks(&queued), [2, 3]);
    }

    #[test]
    fn a_late_message_sent_after_later_ones_still_finds_its_receipts() {
        // The queue has sent the message at 20 ms when one due at 10 ms
        // arrives, so the 10 ms message goes first here and second on the
        // wire.
        let mut queued = queued(&[20]);
        queued.push(ms(10), message(0, 1)).unwrap();
        assert_eq!(ticks(&queued), [1, 0]);
        queued.sent(PortId(0), ms(20)).unwrap();
        assert_eq!(ticks(&queued), [1]);
        queued.sent(PortId(0), ms(10)).unwrap();
        assert!(queued.messages.is_empty());
    }

    #[test]
    fn a_receipt_for_no_queued_message_is_an_error() {
        let mut queued = queued(&[10]);
        for (port, time) in [(0, 11), (1, 10)] {
            assert!(matches!(
                queued.sent(PortId(port), ms(time)),
                Err(AlsaSeqError::Receipt)
            ));
        }
        assert_eq!(ticks(&queued), [0]);
        assert!(matches!(
            Queued::new(8).sent(PortId(0), ms(10)),
            Err(AlsaSeqError::Receipt)
        ));
    }

    #[test]
    fn withdrawing_reports_everything_due_at_or_after_the_cut_in_order() {
        let mut queued = queued(&[10, 20, 20, 30]);
        assert!(queued.any_from(ms(30)));
        assert!(!queued.any_from(ms(31)));
        let mut reported = Vec::new();
        queued.withdraw_from(ms(20), &mut |message| reported.push(message.tick.0));
        assert_eq!(reported, [1, 2, 3]);
        assert_eq!(ticks(&queued), [0]);
        assert!(!queued.any_from(ms(11)));
        reported.clear();
        queued.withdraw_from(ms(11), &mut |message| reported.push(message.tick.0));
        assert!(reported.is_empty());
        assert_eq!(ticks(&queued), [0]);
        assert!(!Queued::new(8).any_from(Duration::ZERO));
    }

    #[test]
    fn a_full_queue_refuses_a_message_rather_than_grow() {
        let mut queued = queued(&[0, 1]);
        queued.limit = 2;
        let capacity = queued.messages.capacity();
        assert!(queued.is_full());
        assert!(matches!(
            queued.push(ms(0), message(0, 9)),
            Err(AlsaSeqError::Full)
        ));
        assert_eq!(queued.messages.capacity(), capacity);
        queued.sent(PortId(0), ms(0)).unwrap();
        queued.push(ms(0), message(0, 9)).unwrap();
        assert_eq!(ticks(&queued), [9, 1]);
    }

    #[test]
    fn every_message_becomes_the_matching_sequencer_event() {
        let channel = Channel::from_number(10).unwrap();
        let u7 = |value| U7::new(value).unwrap();
        let note = |kind, note: u8, velocity: u8| {
            let note = EvNote {
                channel: 9,
                note,
                velocity,
                off_velocity: 0,
                duration: 0,
            };
            (kind, Some(note), None)
        };
        let control = |kind, channel: u8, param: u32, value: i32| {
            let control = EvCtrl {
                channel,
                param,
                value,
            };
            (kind, None, Some(control))
        };
        let bend = |value| MidiMessage::PitchBend {
            channel,
            value: U14::new(value).unwrap(),
        };
        let cases = [
            (
                MidiMessage::NoteOn {
                    channel,
                    note: u7(36),
                    velocity: u7(100),
                },
                note(EventType::Noteon, 36, 100),
            ),
            (
                MidiMessage::NoteOff {
                    channel,
                    note: u7(36),
                    velocity: u7(64),
                },
                note(EventType::Noteoff, 36, 64),
            ),
            (
                MidiMessage::PolyPressure {
                    channel,
                    note: u7(40),
                    pressure: u7(7),
                },
                note(EventType::Keypress, 40, 7),
            ),
            (
                MidiMessage::ControlChange {
                    channel,
                    controller: u7(74),
                    value: u7(127),
                },
                control(EventType::Controller, 9, 74, 127),
            ),
            (
                MidiMessage::ProgramChange {
                    channel,
                    program: u7(5),
                },
                control(EventType::Pgmchange, 9, 0, 5),
            ),
            (
                MidiMessage::ChannelPressure {
                    channel,
                    pressure: u7(90),
                },
                control(EventType::Chanpress, 9, 0, 90),
            ),
            (bend(0), control(EventType::Pitchbend, 9, 0, -8192)),
            (bend(8192), control(EventType::Pitchbend, 9, 0, 0)),
            (bend(16383), control(EventType::Pitchbend, 9, 0, 8191)),
            (
                MidiMessage::SongPosition(U14::new(1000).unwrap()),
                control(EventType::Songpos, 0, 0, 1000),
            ),
            (MidiMessage::TimingClock, (EventType::Clock, None, None)),
            (MidiMessage::Start, (EventType::Start, None, None)),
            (MidiMessage::Continue, (EventType::Continue, None, None)),
            (MidiMessage::Stop, (EventType::Stop, None, None)),
        ];
        for (message, (kind, note, control)) in cases {
            let event = sequencer_event(&message);
            assert_eq!(event.get_type(), kind, "{message:?}");
            if let Some(note) = note {
                assert_eq!(event.get_data::<EvNote>(), Some(note), "{message:?}");
            }
            if let Some(control) = control {
                assert_eq!(event.get_data::<EvCtrl>(), Some(control), "{message:?}");
            }
        }
    }
}
