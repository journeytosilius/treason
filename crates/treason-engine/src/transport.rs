//! The transport: whether the engine plays, and from where.

use treason_core::midi::{MidiMessage, U14};
use treason_core::playback::{PlaybackStatus, TransportCommand, TransportState};
use treason_core::time::{TICKS_PER_MIDI_BEAT, Ticks};

/// Song Position Pointer has 14 bits, so it counts 16384 MIDI beats (16ths)
/// and then wraps: 1024 bars of 4/4.
const SONG_POSITIONS: u64 = 1 << 14;

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
    /// Start playing at `at` and anchor the timebase there. With `restart`,
    /// playback was running and Play or Locate jumps: end the sounding notes
    /// first.
    Start { at: Ticks, restart: bool },
    /// Stop: end the sounding notes.
    Stop,
    /// Move to a position while stopped.
    Locate(Ticks),
    /// End every sounding note, then send All Notes Off on every channel.
    /// Playback carries on.
    Panic,
}

impl TransportEffect {
    /// The MIDI transport messages for the ports that get MIDI clock, in
    /// order.
    ///
    /// Starting from the top sends Start. Starting anywhere else sends a
    /// Song Position Pointer and Continue: the receiver resumes at the
    /// pointer's 16th on the next clock pulse, which comes at
    /// [`first_pulse`]. A running clock is stopped first, since receivers
    /// ignore Song Position Pointer while playing. Stop sends Stop, and
    /// Locate while stopped a Song Position Pointer. Panic sends nothing:
    /// playback carries on.
    pub fn clock_messages(self) -> impl Iterator<Item = MidiMessage> {
        let messages = match self {
            Self::None | Self::Panic => [None; 3],
            Self::Start { at, restart } => [
                restart.then_some(MidiMessage::Stop),
                (at != Ticks::ZERO).then(|| song_position(at)),
                Some(if at == Ticks::ZERO {
                    MidiMessage::Start
                } else {
                    MidiMessage::Continue
                }),
            ],
            Self::Stop => [Some(MidiMessage::Stop), None, None],
            Self::Locate(to) => [Some(song_position(to)), None, None],
        };
        messages.into_iter().flatten()
    }
}

/// Where MIDI clock pulses start when playback starts at `at`: the first
/// 16th at or after it, where a Song Position Pointer has put the
/// receivers. Before then they get no pulses, so they wait.
pub fn first_pulse(at: Ticks) -> Ticks {
    Ticks(at.0.div_ceil(TICKS_PER_MIDI_BEAT) * TICKS_PER_MIDI_BEAT)
}

/// A Song Position Pointer to [`first_pulse`] for `at`, wrapped to its 14
/// bits.
fn song_position(at: Ticks) -> MidiMessage {
    let beats = first_pulse(at).0 / TICKS_PER_MIDI_BEAT % SONG_POSITIONS;
    MidiMessage::SongPosition(U14::new(beats as u16).expect("wrapped to 14 bits"))
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
    /// Stop keeps the position. Locate moves, and while playing carries on
    /// from there.
    pub fn apply(&mut self, command: TransportCommand) -> TransportEffect {
        let playing = self.state == TransportState::Playing;
        match command {
            TransportCommand::Play => {
                self.position = Ticks::ZERO;
                self.start(playing)
            }
            TransportCommand::Continue if playing => TransportEffect::None,
            TransportCommand::Continue => self.start(false),
            TransportCommand::Stop if playing => {
                self.state = TransportState::Stopped;
                TransportEffect::Stop
            }
            TransportCommand::Stop => TransportEffect::None,
            TransportCommand::Locate(position) => {
                self.position = position;
                if playing {
                    self.start(true)
                } else {
                    TransportEffect::Locate(position)
                }
            }
            TransportCommand::Panic => TransportEffect::Panic,
        }
    }

    /// Plays from the current position.
    fn start(&mut self, restart: bool) -> TransportEffect {
        self.state = TransportState::Playing;
        TransportEffect::Start {
            at: self.position,
            restart,
        }
    }
}

#[cfg(test)]
mod tests {
    use TransportState::{Playing, Stopped};

    use super::*;

    fn start(at: u64, restart: bool) -> TransportEffect {
        TransportEffect::Start {
            at: Ticks(at),
            restart,
        }
    }

    #[test]
    fn commands_follow_midi_transport_semantics() {
        use TransportCommand::{Continue, Locate, Panic, Play, Stop};

        let bar_3 = Ticks::quarters(8);
        let cases = [
            // From the top, and back to the top while playing.
            (Stopped, 0, Play, start(0, false), Playing, 0),
            (Stopped, 960, Play, start(0, false), Playing, 0),
            (Playing, 1920, Play, start(0, true), Playing, 0),
            // From where it stopped.
            (Stopped, 960, Continue, start(960, false), Playing, 960),
            (Stopped, 0, Continue, start(0, false), Playing, 0),
            (Playing, 960, Continue, TransportEffect::None, Playing, 960),
            (Playing, 960, Stop, TransportEffect::Stop, Stopped, 960),
            (Stopped, 960, Stop, TransportEffect::None, Stopped, 960),
            // A jump, which plays on from there while playing.
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
                start(bar_3.0, true),
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
            start(1234, false)
        );
    }

    /// The clock messages for `effect`, such as "Stop, SPP 2, Continue".
    fn clock(effect: TransportEffect) -> String {
        effect
            .clock_messages()
            .map(|message| match message {
                MidiMessage::SongPosition(beats) => format!("SPP {}", beats.get()),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[test]
    fn clock_ports_start_at_the_top_or_resume_at_the_next_16th() {
        let cases = [
            (start(0, false), "Start", 0),
            (start(0, true), "Stop, Start", 0),
            (start(240, false), "SPP 1, Continue", 240),
            (start(241, false), "SPP 2, Continue", 480),
            (start(479, true), "Stop, SPP 2, Continue", 480),
        ];
        for (effect, messages, pulse) in cases {
            assert_eq!(clock(effect), messages, "{effect:?}");
            let TransportEffect::Start { at, .. } = effect else {
                unreachable!()
            };
            assert_eq!(first_pulse(at), Ticks(pulse), "{effect:?}");
        }
        assert_eq!(clock(TransportEffect::Stop), "Stop");
        assert_eq!(clock(TransportEffect::Locate(Ticks(960))), "SPP 4");
        assert_eq!(clock(TransportEffect::Panic), "");
        assert_eq!(clock(TransportEffect::None), "");
    }

    #[test]
    fn song_position_wraps_after_16384_sixteenths() {
        let sixteenth = TICKS_PER_MIDI_BEAT;
        let last = Ticks(16_383 * sixteenth);
        assert_eq!(clock(TransportEffect::Locate(last)), "SPP 16383");
        assert_eq!(clock(TransportEffect::Locate(Ticks(last.0 + 1))), "SPP 0");
        assert_eq!(clock(start(16_385 * sixteenth, false)), "SPP 1, Continue");
        // The clock itself doesn't wrap.
        assert_eq!(first_pulse(Ticks(last.0 + 1)), Ticks(16_384 * sixteenth));
    }
}
