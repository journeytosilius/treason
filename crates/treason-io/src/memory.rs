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

    fn cancel_from(
        &mut self,
        from: Instant,
        cancelled: &mut dyn FnMut(&TimedMessage),
    ) -> Result<(), Self::Error> {
        if matches!(self.delivery, Delivery::Scheduled { .. }) {
            self.messages.retain(|message| {
                if message.at < from {
                    true
                } else {
                    cancelled(message);
                    false
                }
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use treason_core::{midi::MidiMessage, model::PortId, time::Ticks};

    #[test]
    fn cancellation_reports_only_messages_actually_withdrawn() {
        for scheduled in [false, true] {
            let mut output = if scheduled {
                RecordingOutput::scheduled(Duration::from_millis(50))
            } else {
                RecordingOutput::new()
            };
            let origin = Instant::now();
            let messages = [0, 1, 2].map(|n| TimedMessage {
                at: origin + Duration::from_millis(n),
                tick: Ticks(n * 40),
                port: PortId(0),
                message: MidiMessage::TimingClock,
            });
            for message in &messages {
                output.send(message).unwrap();
            }
            let mut withdrawn = Vec::new();
            output
                .cancel_from(messages[1].at, &mut |message| withdrawn.push(*message))
                .unwrap();
            if scheduled {
                assert_eq!(withdrawn, messages[1..]);
                assert_eq!(output.messages(), &messages[..1]);
            } else {
                assert!(withdrawn.is_empty());
                assert_eq!(output.messages(), messages);
            }
        }
    }
}
