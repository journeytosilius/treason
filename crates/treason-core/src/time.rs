//! Musical time.
//!
//! Positions and durations are whole [`Ticks`] at [`PPQN`] ticks per quarter
//! note, so musical arithmetic is exact. Only the engine converts ticks to
//! wall-clock time.

use std::fmt;
use std::ops::{Add, Sub};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Ticks per quarter note.
///
/// 960 divides evenly into MIDI clock pulses (24 per quarter note), 16ths,
/// triplets and 64th notes.
pub const PPQN: u64 = 960;

/// Ticks between two MIDI clock pulses.
pub const TICKS_PER_CLOCK_PULSE: u64 = PPQN / 24;

/// Ticks per MIDI beat (a 16th note), the unit of Song Position Pointer.
pub const TICKS_PER_MIDI_BEAT: u64 = PPQN / 4;

/// A position or a duration, in ticks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Ticks(pub u64);

impl Ticks {
    /// No time.
    pub const ZERO: Self = Self(0);

    /// `n` quarter notes.
    pub const fn quarters(n: u64) -> Self {
        Self(n * PPQN)
    }

    /// `n` 16th notes.
    pub const fn sixteenths(n: u64) -> Self {
        Self(n * PPQN / 4)
    }
}

impl Add for Ticks {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub for Ticks {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl fmt::Display for Ticks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ticks", self.0)
    }
}

/// Tempo, in quarter notes per minute.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Bpm(f64);

impl Bpm {
    /// The slowest supported tempo.
    pub const MIN: f64 = 20.0;
    /// The fastest supported tempo.
    pub const MAX: f64 = 400.0;

    /// Returns `None` unless `bpm` is between [`Self::MIN`] and [`Self::MAX`].
    pub fn new(bpm: f64) -> Option<Self> {
        (Self::MIN..=Self::MAX).contains(&bpm).then_some(Self(bpm))
    }

    /// Beats per minute.
    pub const fn get(self) -> f64 {
        self.0
    }

    /// The length of one tick at this tempo, in nanoseconds.
    pub fn nanos_per_tick(self) -> f64 {
        60e9 / (self.0 * PPQN as f64)
    }
}

impl Default for Bpm {
    fn default() -> Self {
        Self(120.0)
    }
}

impl TryFrom<f64> for Bpm {
    type Error = InvalidTempo;

    fn try_from(bpm: f64) -> Result<Self, Self::Error> {
        Self::new(bpm).ok_or(InvalidTempo(bpm))
    }
}

impl From<Bpm> for f64 {
    fn from(bpm: Bpm) -> Self {
        bpm.0
    }
}

/// A tempo outside [`Bpm::MIN`] to [`Bpm::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Error)]
#[error("tempo must be between {min} and {max} BPM, got {0}", min = Bpm::MIN, max = Bpm::MAX)]
pub struct InvalidTempo(pub f64);

/// A time signature such as 4/4 or 7/8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawTimeSignature")]
pub struct TimeSignature {
    numerator: u8,
    denominator: u8,
}

impl TimeSignature {
    /// 4/4.
    pub const COMMON: Self = Self {
        numerator: 4,
        denominator: 4,
    };

    /// Returns `None` unless `numerator` is 1-32 and `denominator` is a power
    /// of two from 1 to 32.
    pub const fn new(numerator: u8, denominator: u8) -> Option<Self> {
        if matches!(numerator, 1..=32) && matches!(denominator, 1 | 2 | 4 | 8 | 16 | 32) {
            Some(Self {
                numerator,
                denominator,
            })
        } else {
            None
        }
    }

    /// Beats per bar.
    pub const fn numerator(self) -> u8 {
        self.numerator
    }

    /// The note value of one beat.
    pub const fn denominator(self) -> u8 {
        self.denominator
    }

    /// The length of one bar.
    pub const fn bar_length(self) -> Ticks {
        Ticks(PPQN * 4 * self.numerator as u64 / self.denominator as u64)
    }
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self::COMMON
    }
}

impl fmt::Display for TimeSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

impl FromStr for TimeSignature {
    type Err = ParseTimeSignatureError;

    /// Reads a time signature as [`Display`](fmt::Display) writes it, such as
    /// "7/8".
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let error = || ParseTimeSignatureError(text.to_owned());
        let (numerator, denominator) = text.split_once('/').ok_or_else(error)?;
        let numerator = numerator.parse().map_err(|_| error())?;
        let denominator = denominator.parse().map_err(|_| error())?;
        Self::new(numerator, denominator).ok_or_else(error)
    }
}

#[derive(Deserialize)]
struct RawTimeSignature {
    numerator: u8,
    denominator: u8,
}

impl TryFrom<RawTimeSignature> for TimeSignature {
    type Error = InvalidTimeSignature;

    fn try_from(raw: RawTimeSignature) -> Result<Self, Self::Error> {
        Self::new(raw.numerator, raw.denominator).ok_or(InvalidTimeSignature {
            numerator: raw.numerator,
            denominator: raw.denominator,
        })
    }
}

/// Text that isn't a time signature [`TimeSignature::new`] accepts.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "`{0}` is not a time signature such as 4/4 or 7/8: beats per bar must be 1-32 and the \
     beat 1, 2, 4, 8, 16 or 32"
)]
pub struct ParseTimeSignatureError(pub String);

/// A time signature [`TimeSignature::new`] rejects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invalid time signature {numerator}/{denominator}")]
pub struct InvalidTimeSignature {
    /// The rejected numerator.
    pub numerator: u8,
    /// The rejected denominator.
    pub denominator: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midi_units_divide_evenly() {
        assert_eq!(TICKS_PER_CLOCK_PULSE * 24, PPQN);
        assert_eq!(TICKS_PER_MIDI_BEAT, 240);
        assert_eq!(Ticks::sixteenths(4), Ticks::quarters(1));
    }

    #[test]
    fn bar_lengths() {
        assert_eq!(TimeSignature::COMMON.bar_length(), Ticks::quarters(4));
        assert_eq!(TimeSignature::new(7, 8).unwrap().bar_length(), Ticks(3360));
        assert_eq!(TimeSignature::new(4, 3), None);
    }

    #[test]
    fn time_signatures_read_back_what_they_write() {
        for time_signature in [TimeSignature::COMMON, TimeSignature::new(7, 8).unwrap()] {
            let text = time_signature.to_string();
            assert_eq!(text.parse(), Ok(time_signature));
        }
        assert_eq!("32/32".parse(), Ok(TimeSignature::new(32, 32).unwrap()));
        for text in [
            "", "4", "4/", "/4", "4/3", "0/4", "33/4", "4/4/4", " 4/4", "4 / 4", "-4/4",
        ] {
            assert_eq!(
                text.parse::<TimeSignature>(),
                Err(ParseTimeSignatureError(text.to_owned())),
                "{text:?}"
            );
        }
    }

    #[test]
    fn tempo_range() {
        assert!(Bpm::new(120.0).is_some());
        assert!(Bpm::new(f64::NAN).is_none());
        assert!(Bpm::new(1000.0).is_none());
    }

    #[test]
    fn tick_length_at_120_bpm() {
        let nanos = Bpm::new(120.0).unwrap().nanos_per_tick();
        assert!((nanos - 520_833.333).abs() < 0.001);
    }
}
