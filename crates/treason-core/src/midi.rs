//! MIDI 1.0 values and messages.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// A value outside the range a MIDI field allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{value} is out of range (0-{max})")]
pub struct OutOfRange {
    /// The rejected value.
    pub value: u32,
    /// The largest allowed value.
    pub max: u32,
}

/// A 7-bit MIDI value (0-127): a note, velocity, controller, value or program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct U7(u8);

impl U7 {
    /// 0.
    pub const MIN: Self = Self(0);
    /// 127.
    pub const MAX: Self = Self(127);

    /// Returns `None` if `value` is above 127.
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 127 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Clamps `value` to 0-127.
    pub const fn saturating(value: u8) -> Self {
        if value <= 127 { Self(value) } else { Self::MAX }
    }

    /// The raw value.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for U7 {
    type Error = OutOfRange;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(OutOfRange {
            value: value.into(),
            max: 127,
        })
    }
}

impl From<U7> for u8 {
    fn from(value: U7) -> Self {
        value.0
    }
}

impl fmt::Display for U7 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A 14-bit MIDI value (0-16383), used by pitch bend and Song Position Pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct U14(u16);

impl U14 {
    /// 0.
    pub const MIN: Self = Self(0);
    /// 8192, pitch bend at rest.
    pub const CENTER: Self = Self(8192);
    /// 16383.
    pub const MAX: Self = Self(16383);

    /// Returns `None` if `value` is above 16383.
    pub const fn new(value: u16) -> Option<Self> {
        if value <= 16383 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The raw value.
    pub const fn get(self) -> u16 {
        self.0
    }

    const fn lsb(self) -> u8 {
        (self.0 & 0x7f) as u8
    }

    const fn msb(self) -> u8 {
        (self.0 >> 7) as u8
    }
}

impl TryFrom<u16> for U14 {
    type Error = OutOfRange;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(OutOfRange {
            value: value.into(),
            max: 16383,
        })
    }
}

impl From<U14> for u16 {
    fn from(value: U14) -> Self {
        value.0
    }
}

/// A MIDI channel.
///
/// Stored as 0-15, the wire format, but shown, parsed and serialized as 1-16,
/// the way devices and manuals number channels. Files and tool calls never
/// deal with off-by-one channel numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Channel(u8);

impl Channel {
    /// Channel 10, where General MIDI puts drums.
    pub const DRUMS: Self = Self(9);

    /// From a 0-based index (0-15).
    pub const fn from_index(index: u8) -> Option<Self> {
        if index < 16 { Some(Self(index)) } else { None }
    }

    /// From a 1-based channel number (1-16).
    pub const fn from_number(number: u8) -> Option<Self> {
        if matches!(number, 1..=16) {
            Some(Self(number - 1))
        } else {
            None
        }
    }

    /// The 0-based index (0-15), as sent on the wire.
    pub const fn index(self) -> u8 {
        self.0
    }

    /// The 1-based number (1-16), as shown to people.
    pub const fn number(self) -> u8 {
        self.0 + 1
    }
}

impl Default for Channel {
    /// Channel 1, where most devices listen out of the box.
    fn default() -> Self {
        Self(0)
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.number().fmt(f)
    }
}

impl Serialize for Channel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.number())
    }
}

impl<'de> Deserialize<'de> for Channel {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = u8::deserialize(deserializer)?;
        Self::from_number(number).ok_or_else(|| {
            serde::de::Error::custom(format_args!("MIDI channel must be 1-16, got {number}"))
        })
    }
}

/// A MIDI message the engine sends.
///
/// `Copy` and at most three bytes on the wire, so messages move through
/// real-time queues without allocating. System exclusive is left out on
/// purpose: it is variable-length and will get a separate path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiMessage {
    /// Starts a note.
    NoteOn {
        channel: Channel,
        note: U7,
        velocity: U7,
    },
    /// Ends a note.
    NoteOff {
        channel: Channel,
        note: U7,
        velocity: U7,
    },
    /// Aftertouch on one note.
    PolyPressure {
        channel: Channel,
        note: U7,
        pressure: U7,
    },
    /// Control change (CC).
    ControlChange {
        channel: Channel,
        controller: U7,
        value: U7,
    },
    /// Selects a program: a patch, or a pattern on many drum machines.
    ProgramChange { channel: Channel, program: U7 },
    /// Aftertouch on the whole channel.
    ChannelPressure { channel: Channel, pressure: U7 },
    /// Pitch bend. [`U14::CENTER`] is no bend.
    PitchBend { channel: Channel, value: U14 },
    /// Song Position Pointer, in MIDI beats (16th notes) from the start.
    SongPosition(U14),
    /// MIDI clock pulse, 24 per quarter note.
    TimingClock,
    /// Start playing from the beginning.
    Start,
    /// Resume playing from the current position.
    Continue,
    /// Stop playing.
    Stop,
}

impl MidiMessage {
    /// Writes the wire bytes into `buf` and returns them. Never allocates.
    pub fn encode(self, buf: &mut [u8; 3]) -> &[u8] {
        let len = match self {
            Self::NoteOff {
                channel,
                note,
                velocity,
            } => put(buf, [0x80 | channel.0, note.0, velocity.0]),
            Self::NoteOn {
                channel,
                note,
                velocity,
            } => put(buf, [0x90 | channel.0, note.0, velocity.0]),
            Self::PolyPressure {
                channel,
                note,
                pressure,
            } => put(buf, [0xa0 | channel.0, note.0, pressure.0]),
            Self::ControlChange {
                channel,
                controller,
                value,
            } => put(buf, [0xb0 | channel.0, controller.0, value.0]),
            Self::ProgramChange { channel, program } => put(buf, [0xc0 | channel.0, program.0]),
            Self::ChannelPressure { channel, pressure } => put(buf, [0xd0 | channel.0, pressure.0]),
            Self::PitchBend { channel, value } => {
                put(buf, [0xe0 | channel.0, value.lsb(), value.msb()])
            }
            Self::SongPosition(position) => put(buf, [0xf2, position.lsb(), position.msb()]),
            Self::TimingClock => put(buf, [0xf8]),
            Self::Start => put(buf, [0xfa]),
            Self::Continue => put(buf, [0xfb]),
            Self::Stop => put(buf, [0xfc]),
        };
        &buf[..len]
    }

    /// The channel of a channel message, or `None` for a system message.
    pub const fn channel(self) -> Option<Channel> {
        match self {
            Self::NoteOn { channel, .. }
            | Self::NoteOff { channel, .. }
            | Self::PolyPressure { channel, .. }
            | Self::ControlChange { channel, .. }
            | Self::ProgramChange { channel, .. }
            | Self::ChannelPressure { channel, .. }
            | Self::PitchBend { channel, .. } => Some(channel),
            Self::SongPosition(_)
            | Self::TimingClock
            | Self::Start
            | Self::Continue
            | Self::Stop => None,
        }
    }

    /// Whether this is a System Real-Time message (clock and transport).
    /// Receivers accept these even in the middle of another message.
    pub const fn is_realtime(self) -> bool {
        matches!(
            self,
            Self::TimingClock | Self::Start | Self::Continue | Self::Stop
        )
    }
}

/// Copies `bytes` to the front of `buf` and returns how many were written.
fn put<const N: usize>(buf: &mut [u8; 3], bytes: [u8; N]) -> usize {
    buf[..N].copy_from_slice(&bytes);
    N
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(message: MidiMessage) -> Vec<u8> {
        let mut buf = [0; 3];
        message.encode(&mut buf).to_vec()
    }

    #[test]
    fn encodes_channel_messages() {
        let kick = MidiMessage::NoteOn {
            channel: Channel::DRUMS,
            note: U7::new(36).unwrap(),
            velocity: U7::new(100).unwrap(),
        };
        assert_eq!(bytes(kick), [0x99, 36, 100]);

        let program = MidiMessage::ProgramChange {
            channel: Channel::from_number(1).unwrap(),
            program: U7::MIN,
        };
        assert_eq!(bytes(program), [0xc0, 0]);

        let bend = MidiMessage::PitchBend {
            channel: Channel::from_number(2).unwrap(),
            value: U14::CENTER,
        };
        assert_eq!(bytes(bend), [0xe1, 0x00, 0x40]);
    }

    #[test]
    fn encodes_system_messages() {
        assert_eq!(bytes(MidiMessage::TimingClock), [0xf8]);
        assert_eq!(
            bytes(MidiMessage::SongPosition(U14::new(130).unwrap())),
            [0xf2, 2, 1]
        );
        assert!(MidiMessage::Stop.is_realtime());
        assert_eq!(MidiMessage::Start.channel(), None);
    }

    #[test]
    fn channels_are_numbered_from_one() {
        let channel = Channel::from_number(10).unwrap();
        assert_eq!(channel, Channel::DRUMS);
        assert_eq!(channel.index(), 9);
        assert_eq!(channel.to_string(), "10");
        assert_eq!(Channel::from_number(0), None);
        assert_eq!(Channel::from_number(17), None);
    }
}
