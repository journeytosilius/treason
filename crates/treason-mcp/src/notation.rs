//! The text notations agents write: step strings and pitch names.

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
    /// A pitch name past either end of MIDI's range.
    #[error("`{0}` is outside MIDI's range: pitches go from C-1 (note 0) to G9 (note 127)")]
    PitchOutOfRange(String),
}

/// The pitch classes from C, written with sharps.
const PITCH_CLASSES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

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
/// `Bb3`: a letter from A to G, an optional sharp (`#` or `♯`) or flat (`b`
/// or `♭`), then an octave from -1 to 9. C-1 is note 0 and G9 note 127.
/// The letter can be lower case.
pub fn parse_pitch(name: &str) -> Result<U7, NotationError> {
    let bad = || NotationError::BadPitch(name.to_owned());
    let mut chars = name.chars();
    let class = match chars.next().map(|letter| letter.to_ascii_uppercase()) {
        Some('C') => 0,
        Some('D') => 2,
        Some('E') => 4,
        Some('F') => 5,
        Some('G') => 7,
        Some('A') => 9,
        Some('B') => 11,
        _ => return Err(bad()),
    };
    let rest = chars.as_str();
    let (shift, octave) = if let Some(octave) = rest.strip_prefix(['#', '♯']) {
        (1, octave)
    } else if let Some(octave) = rest.strip_prefix(['b', '♭']) {
        (-1, octave)
    } else {
        (0, rest)
    };
    let digits = octave.strip_prefix('-').unwrap_or(octave);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad());
    }
    // Every octave with more than one digit is out of range, which also
    // keeps the arithmetic small.
    let note = if digits.len() > 1 {
        None
    } else {
        let octave: i32 = octave.parse().map_err(|_| bad())?;
        u8::try_from((octave + 1) * 12 + class + shift)
            .ok()
            .and_then(U7::new)
    };
    note.ok_or_else(|| NotationError::PitchOutOfRange(name.to_owned()))
}

/// The pitch name of a MIDI note, with sharps: 60 is `C4`, 61 `C#4` and 0
/// `C-1`. [`parse_pitch`] reads it back.
pub fn format_pitch(note: U7) -> String {
    let note = note.get();
    let octave = i32::from(note / 12) - 1;
    format!("{}{octave}", PITCH_CLASSES[usize::from(note % 12)])
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

    #[test]
    fn pitch_names_read_as_midi_notes() {
        for (name, note) in [
            ("C4", 60),
            ("c4", 60),
            ("A4", 69),
            ("C-1", 0),
            ("G9", 127),
            ("F#2", 42),
            ("F♯2", 42),
            ("Bb3", 58),
            ("bb3", 58),
            ("B♭3", 58),
            ("Cb4", 59),
            ("B#3", 60),
            ("E#4", 65),
            ("Fb4", 64),
            ("Db-1", 1),
        ] {
            assert_eq!(parse_pitch(name), Ok(U7::new(note).unwrap()), "{name}");
        }
    }

    #[test]
    fn bad_and_out_of_range_pitches_say_which() {
        for name in [
            "", "H4", "C", "C#", "4", "C4x", "C##4", "Cbb4", "C 4", "C+4", "C-", "#C4",
        ] {
            assert_eq!(
                parse_pitch(name),
                Err(NotationError::BadPitch(name.into())),
                "{name}"
            );
        }
        for name in ["C10", "G#9", "B#9", "Cb-1", "C-2", "A99", "C-10"] {
            assert_eq!(
                parse_pitch(name),
                Err(NotationError::PitchOutOfRange(name.into())),
                "{name}"
            );
        }
    }

    #[test]
    fn every_note_has_a_name_that_reads_back() {
        for note in (0..=127).filter_map(U7::new) {
            assert_eq!(parse_pitch(&format_pitch(note)), Ok(note));
        }
        assert_eq!(format_pitch(U7::new(61).unwrap()), "C#4");
        assert_eq!(format_pitch(U7::MIN), "C-1");
        assert_eq!(format_pitch(U7::MAX), "G9");
    }
}
