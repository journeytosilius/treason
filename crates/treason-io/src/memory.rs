//! An in-memory output.

use std::convert::Infallible;

use treason_core::output::{Delivery, MidiOutput, TimedMessage};

/// Keeps every message it is given, in order.
///
/// Used in tests, and as the first stage of offline rendering: run the engine
/// with `treason-engine`'s `ManualClock` into a recording, then write the
/// recording out.
#[derive(Debug, Clone, Default)]
pub struct RecordingOutput {
    messages: Vec<TimedMessage>,
}

impl RecordingOutput {
    /// An empty recording.
    pub fn new() -> Self {
        Self::default()
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

impl MidiOutput for RecordingOutput {
    type Error = Infallible;

    fn delivery(&self) -> Delivery {
        // With a manual clock, "waiting" until each message is due costs nothing.
        Delivery::Immediate
    }

    fn send(&mut self, message: &TimedMessage) -> Result<(), Self::Error> {
        self.messages.push(*message);
        Ok(())
    }
}
