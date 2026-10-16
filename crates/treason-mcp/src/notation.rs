//! The text notations agents write: step strings and pitch names.
//!
//! Not implemented yet.

use thiserror::Error;
use treason_core::midi::U7;

/// One step of a step string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `.`: nothing.
    Rest,
    /// `x`: a hit at normal velocity.
    Hit,
    /// `X`: an accented hit.
    Accent,
}

/// Notation that doesn't parse. The messages tell the agent how to fix it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NotationError {
    /// A character that isn't a step.
    #[error(
        "unexpected `{found}` at step {position}: use `x` for a hit, `X` for an accent and `.` for a rest"
    )]
    UnexpectedStep {
        /// The character.
        found: char,
        /// Which step, counting from 1.
        position: usize,
    },
    /// Not a pitch name.
    #[error(
        "`{0}` is not a pitch: write a letter, an optional `#` or `b`, then an octave, such as C4 or F#2"
    )]
    BadPitch(String),
}

/// Parses a step string such as `"x...x...x..x.x.."`. Spaces and `|` are
/// ignored, so steps can be grouped: `"x... x... | x..x .x.."`.
#[expect(unused_variables, reason = "not implemented yet")]
pub fn parse_steps(steps: &str) -> Result<Vec<Step>, NotationError> {
    todo!("parse step characters")
}

/// Writes steps as a step string, in groups of four.
#[expect(unused_variables, reason = "not implemented yet")]
pub fn format_steps(steps: &[Step]) -> String {
    todo!("format step characters")
}

/// The MIDI note for a pitch name such as `C4` (middle C, 60), `F#2` or
/// `Bb3`.
#[expect(unused_variables, reason = "not implemented yet")]
pub fn parse_pitch(name: &str) -> Result<U7, NotationError> {
    todo!("parse pitch names")
}
