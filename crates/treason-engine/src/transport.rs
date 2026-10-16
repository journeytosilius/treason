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
    /// Nothing to do, such as Stop while stopped.
    None,
    /// Start playing at `at`: end any sounding notes, since Play restarts
    /// while playing, and anchor the timebase there. Clock ports get
    /// `message`, Start from the top or Continue from elsewhere.
    Start { at: Ticks, message: MidiMessage },
    /// Stop: end the sounding notes, and clock ports get Stop.
    Stop,
    /// Jump to a position. While playing, end the sounding notes and carry
    /// on from there. Clock ports get a Song Position Pointer.
    Locate(Ticks),
    /// End every sounding note, then send All Notes Off on every channel.
    /// Playback carries on.
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
    ///
    /// Play starts from the top, and restarts there while playing. Continue
    /// starts from the current position and does nothing while playing.
    /// Stop keeps the position. Locate moves, playing or not.
    pub fn apply(&mut self, command: TransportCommand) -> TransportEffect {
        let playing = self.state == TransportState::Playing;
        match command {
            TransportCommand::Play => {
                self.position = Ticks::ZERO;
                self.start()
            }
            TransportCommand::Continue if playing => TransportEffect::None,
            TransportCommand::Continue => self.start(),
            TransportCommand::Stop if playing => {
                self.state = TransportState::Stopped;
                TransportEffect::Stop
            }
            TransportCommand::Stop => TransportEffect::None,
            TransportCommand::Locate(position) => {
                self.position = position;
                TransportEffect::Locate(position)
            }
            TransportCommand::Panic => TransportEffect::Panic,
        }
    }

    /// Plays from the current position.
    fn start(&mut self) -> TransportEffect {
        self.state = TransportState::Playing;
        let message = if self.position == Ticks::ZERO {
            MidiMessage::Start
        } else {
            MidiMessage::Continue
        };
        TransportEffect::Start {
            at: self.position,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use TransportState::{Playing, Stopped};

    use super::*;

    fn start(at: u64, message: MidiMessage) -> TransportEffect {
        TransportEffect::Start {
            at: Ticks(at),
            message,
        }
    }

    #[test]
    fn commands_follow_midi_transport_semantics() {
        use TransportCommand::{Continue, Locate, Panic, Play, Stop};

        let bar_3 = Ticks::quarters(8);
        let cases = [
            // From the top, and back to the top while playing.
            (Stopped, 0, Play, start(0, MidiMessage::Start), Playing, 0),
            (Stopped, 960, Play, start(0, MidiMessage::Start), Playing, 0),
            (
                Playing,
                1920,
                Play,
                start(0, MidiMessage::Start),
                Playing,
                0,
            ),
            // From where it stopped; at the top that is a Start.
            (
                Stopped,
                960,
                Continue,
                start(960, MidiMessage::Continue),
                Playing,
                960,
            ),
            (
                Stopped,
                0,
                Continue,
                start(0, MidiMessage::Start),
                Playing,
                0,
            ),
            (Playing, 960, Continue, TransportEffect::None, Playing, 960),
            (Playing, 960, Stop, TransportEffect::Stop, Stopped, 960),
            (Stopped, 960, Stop, TransportEffect::None, Stopped, 960),
            (
                Stopped,
                0,
                Locate(bar_3),
                TransportEffect::Locate(bar_3),
                Stopped,
                bar_3.0,
            ),
            (
                Playing,
                960,
                Locate(bar_3),
                TransportEffect::Locate(bar_3),
                Playing,
                bar_3.0,
            ),
            (Playing, 960, Panic, TransportEffect::Panic, Playing, 960),
            (Stopped, 960, Panic, TransportEffect::Panic, Stopped, 960),
        ];
        for (state, position, command, effect, state_after, position_after) in cases {
            let mut transport = Transport {
                state,
                position: Ticks(position),
            };
            assert_eq!(
                (transport.apply(command), transport.status()),
                (
                    effect,
                    PlaybackStatus {
                        state: state_after,
                        position: Ticks(position_after),
                    }
                ),
                "{command:?} while {state:?} at {position}"
            );
        }
    }

    #[test]
    fn continue_resumes_where_playback_stopped() {
        let mut transport = Transport::new();
        transport.apply(TransportCommand::Play);
        transport.advance_to(Ticks(1234));
        assert_eq!(
            transport.apply(TransportCommand::Stop),
            TransportEffect::Stop
        );
        assert_eq!(
            transport.apply(TransportCommand::Continue),
            start(1234, MidiMessage::Continue)
        );
    }
}
