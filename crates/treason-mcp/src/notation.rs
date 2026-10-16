//! The text notations agents write: step strings and pitch names.
//!
//! Pitch names are not implemented yet.

use thiserror::Error;
use treason_core::midi::U7;

/// One step of a step string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `.` (or `-`): nothing.
    Rest,
    /// `x`: a hit at normal velocity.
    Hit,
    /// `X`: an accented hit.
    Accent,
}

impl Step {
    /// The character that writes this step.
    pub const fn symbol(self) -> char {
        match self {
            Self::Rest => '.',
            Self::Hit => 'x',
            Self::Accent => 'X',
        }
    }
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

/// Parses a step string such as `"x...x...x..x.x.."`: `x` is a hit, `X` an
/// accent, and `.` or `-` a rest. Whitespace and `|` are ignored, so steps
/// can be grouped: `"x... x... | x..x .x.."`.
pub fn parse_steps(steps: &str) -> Result<Vec<Step>, NotationError> {
    let mut parsed = Vec::with_capacity(steps.len());
    for found in steps.chars() {
        let step = match found {
            'x' => Step::Hit,
            'X' => Step::Accent,
            '.' | '-' => Step::Rest,
            '|' => continue,
            _ if found.is_whitespace() => continue,
            _ => {
                return Err(NotationError::UnexpectedStep {
                    found,
                    position: parsed.len() + 1,
                });
            }
        };
        parsed.push(step);
    }
    Ok(parsed)
}

/// Writes steps as a step string with a space after every `group` steps,
/// such as one group per beat. A `group` of 0 writes them unbroken.
pub fn format_steps(steps: &[Step], group: usize) -> String {
    let mut text = String::with_capacity(steps.len() * 2);
    for (index, step) in steps.iter().enumerate() {
        if group > 0 && index > 0 && index.is_multiple_of(group) {
            text.push(' ');
        }
        text.push(step.symbol());
    }
    text
}

/// The MIDI note for a pitch name such as `C4` (middle C, 60), `F#2` or
/// `Bb3`.
#[expect(unused_variables, reason = "not implemented yet")]
pub fn parse_pitch(name: &str) -> Result<U7, NotationError> {
    todo!("parse pitch names")
}

#[cfg(test)]
mod tests {
    use super::Step::{Accent, Hit, Rest};
    use super::*;

    #[test]
    fn steps_parse_and_grouping_is_ignored() {
        assert_eq!(parse_steps("x.X-").unwrap(), [Hit, Rest, Accent, Rest]);
        assert_eq!(
            parse_steps("x... | X...\n").unwrap(),
            parse_steps("x...X...").unwrap()
        );
        assert_eq!(parse_steps(" | ").unwrap(), []);
    }

    #[test]
    fn bad_steps_say_which_step() {
        assert_eq!(
            parse_steps("x.. o"),
            Err(NotationError::UnexpectedStep {
                found: 'o',
                position: 4
            })
        );
        let message = parse_steps("x1").unwrap_err().to_string();
        assert!(message.contains("`1` at step 2"), "{message}");
    }

    #[test]
    fn steps_format_in_groups_and_parse_back() {
        let steps = parse_steps("x...x..Xx...x...").unwrap();
        let text = format_steps(&steps, 4);
        assert_eq!(text, "x... x..X x... x...");
        assert_eq!(parse_steps(&text).unwrap(), steps);
        assert_eq!(format_steps(&steps[..6], 3), "x.. .x.");
        assert_eq!(format_steps(&steps[..3], 0), "x..");
        assert_eq!(format_steps(&[], 4), "");
    }
}
