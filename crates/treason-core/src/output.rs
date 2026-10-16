//! The contract between the engine and the MIDI backends.
//!
//! The engine produces [`TimedMessage`]s and a backend in `treason-io`
//! delivers them by implementing [`MidiOutput`]. Neither crate depends on the
//! other.

use std::time::{Duration, Instant};

use crate::midi::MidiMessage;
use crate::model::PortId;
use crate::time::Ticks;

/// A message and when it is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedMessage {
    /// When the message should leave the computer, with the port's latency
    /// offset already applied.
    pub at: Instant,
    /// The musical position it belongs to, for backends that work in ticks,
    /// such as a MIDI file writer.
    pub tick: Ticks,
    /// The output port.
    pub port: PortId,
    /// The message.
    pub message: MidiMessage,
}

/// How a backend handles timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Sends as soon as [`MidiOutput::send`] is called, so the engine waits
    /// until each message is due before handing it over (midir works this
    /// way).
    Immediate,
    /// Queues messages and sends each at its `at` time itself (as an ALSA
    /// sequencer queue does), so the engine may hand them over early.
    Scheduled {
        /// How far ahead of time the backend accepts messages.
        lookahead: Duration,
    },
}

/// A destination for MIDI messages. The backends in `treason-io` implement it.
///
/// Called on the engine's real-time thread: [`send`](Self::send) should not
/// allocate or block beyond writing to the device.
pub trait MidiOutput: Send {
    /// What can go wrong while sending.
    type Error: std::error::Error + Send + Sync + 'static;

    /// How this output handles timing.
    fn delivery(&self) -> Delivery;

    /// Delivers one message.
    fn send(&mut self, message: &TimedMessage) -> Result<(), Self::Error>;

    /// Cancels undelivered messages whose timestamp is at or after `from`,
    /// including messages already flushed to a scheduling queue. On return,
    /// none of that work may be delivered. Immediate outputs do nothing.
    fn cancel_from(&mut self, from: Instant) -> Result<(), Self::Error>;

    /// Pushes out anything buffered. Called after each batch of messages.
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
