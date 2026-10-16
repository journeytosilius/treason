//! An in-memory output.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use treason_core::output::{Delivery, MidiOutput, TimedMessage};

/// Keeps every message it is given, in order.
///
/// Used in tests, and as the first stage of offline rendering: run the engine
/// with `treason-engine`'s `ManualClock` into a recording, then write the
/// recording out.
#[derive(Debug, Clone)]
pub struct RecordingOutput {
    messages: Vec<TimedMessage>,
    delivery: Delivery,
}

impl RecordingOutput {
    /// An empty recording that takes each message when it is due, like a
    /// port. With a manual clock, waiting until then costs nothing.
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            delivery: Delivery::Immediate,
        }
    }

    /// An empty recording that takes messages up to `lookahead` early, like
    /// a backend that schedules them itself.
    pub fn scheduled(lookahead: Duration) -> Self {
        Self {
            messages: Vec::new(),
            delivery: Delivery::Scheduled { lookahead },
        }
    }

    /// The messages so far.
    pub fn messages(&self) -> &[TimedMessage] {
        &self.messages
    }

    /// Takes the messages so far, leaving the recording empty.
    pub fn take(&mut self) -> Vec<TimedMessage> {
        std::mem::take(&mut self.messages)
    }
}

impl Default for RecordingOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiOutput for RecordingOutput {
    type Error = Infallible;

    fn delivery(&self) -> Delivery {
        self.delivery
    }

    fn send(&mut self, message: &TimedMessage) -> Result<(), Self::Error> {
        self.messages.push(*message);
        Ok(())
    }

    fn cancel_from(&mut self, from: Instant) -> Result<(), Self::Error> {
        if matches!(self.delivery, Delivery::Scheduled { .. }) {
            self.messages.retain(|message| message.at < from);
        }
        Ok(())
    }
}
