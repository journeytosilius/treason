//! The transport: whether the engine plays, and from where.

use treason_core::midi::MidiMessage;
use treason_core::playback::{PlaybackStatus, TransportCommand, TransportState};
use treason_core::time::Ticks;

/// The transport state machine.
///
/// Pure state: the runtime feeds it commands and acts on what comes back, so
/// MIDI transport semantics can be tested without a clock or an output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transport {
    state: TransportState,
    position: Ticks,
}

/// What a command asks of the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportEffect {
    /// Nothing to do, such as Play while already playing from the top.
    None,
    /// Start playing at `at`: anchor the timebase there. Clock ports get
    /// `message`, Start from the top or Continue from elsewhere.
    Start { at: Ticks, message: MidiMessage },
    /// Stop: end the sounding notes, and clock ports get Stop.
    Stop,
    /// Jump to a position. Clock ports get a Song Position Pointer.
    Locate(Ticks),
    /// End every sounding note, then send All Notes Off on every channel.
    Panic,
}

impl Transport {
    /// Stopped at the top.
    pub const fn new() -> Self {
        Self {
            state: TransportState::Stopped,
            position: Ticks::ZERO,
        }
    }

    /// Running or not, and where.
    pub const fn status(&self) -> PlaybackStatus {
        PlaybackStatus {
            state: self.state,
            position: self.position,
        }
    }

    /// Records how far playback got. The runtime calls this as it renders.
    pub fn advance_to(&mut self, position: Ticks) {
        self.position = position;
    }

    /// Applies a command and says what the engine must do about it.
    #[expect(unused_variables, reason = "not implemented yet")]
    pub fn apply(&mut self, command: TransportCommand) -> TransportEffect {
        todo!("MIDI transport semantics; see ARCHITECTURE.md, Timing")
    }
}
