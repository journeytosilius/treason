//! Kernel-scheduled output through an ALSA sequencer queue (Linux).
//!
//! Not implemented yet. The engine will hand over each message up to the
//! lookahead early, stamped with its time, and the kernel's queue will send
//! it on time. The engine thread's own scheduling jitter then never reaches
//! the MIDI cable.

use std::time::Duration;

use thiserror::Error;
use treason_core::output::{Delivery, MidiOutput, TimedMessage};

use crate::port::PortSpec;

/// One ALSA sequencer client with a port per [`PortSpec`] and one queue.
#[expect(dead_code, reason = "used by the queue, which is not written yet")]
pub struct AlsaSeqOutput {
    seq: alsa::Seq,
    queue: i32,
    ports: Vec<i32>,
    lookahead: Duration,
}

impl AlsaSeqOutput {
    /// Opens a sequencer client named `client_name` with `ports`, and a queue
    /// that accepts messages up to `lookahead` before they are due.
    #[expect(unused_variables, reason = "not implemented yet")]
    pub fn open(
        client_name: &str,
        ports: &[PortSpec],
        lookahead: Duration,
    ) -> Result<Self, AlsaSeqError> {
        todo!("create the client, its ports and a queue")
    }
}

impl MidiOutput for AlsaSeqOutput {
    type Error = AlsaSeqError;

    fn delivery(&self) -> Delivery {
        Delivery::Scheduled {
            lookahead: self.lookahead,
        }
    }

    #[expect(unused_variables, reason = "not implemented yet")]
    fn send(&mut self, message: &TimedMessage) -> Result<(), Self::Error> {
        todo!("schedule the message on the queue at its time")
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        todo!("drain the output buffer to the kernel")
    }
}

/// An ALSA sequencer failure.
#[derive(Debug, Error)]
#[error("ALSA sequencer error")]
pub struct AlsaSeqError(#[from] alsa::Error);
