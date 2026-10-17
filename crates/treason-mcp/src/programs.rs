//! Programs by number or by a device's name for them, and back.

use thiserror::Error;
use treason_core::midi::U7;
use treason_core::model::Program;
use treason_devices::DeviceProfile;

use crate::dto::ProgramRef;

/// How many similar names an unknown program name suggests.
const SUGGESTIONS: usize = 5;

/// What's wrong with a program in a call.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProgramError {
    /// A bank with no program to select in it.
    #[error("bank_msb and bank_lsb select the bank of a program: give the program too")]
    BankWithoutProgram,
    /// A program number past MIDI's range.
    #[error(
        "program must be 0-127, the number sent, not {0}; a manual that numbers programs 1-128 means one more"
    )]
    NumberOutOfRange(u8),
    /// A bank select byte past MIDI's range.
    #[error("{field} must be 0-127, not {value}")]
    BankOutOfRange {
        /// `bank_msb` or `bank_lsb`.
        field: &'static str,
        /// The value given.
        value: u8,
    },
    /// A bank along with a name, which brings its own.
    #[error(
        "`{0}` brings its own bank: leave out bank_msb and bank_lsb, or give the program by number"
    )]
    NameWithBank(String),
    /// A name on a track without a device.
    #[error(
        "`{0}` is not a program number, and the track has no device to name programs: give a number 0-127, or give the track a device with edit_track (see list_devices)"
    )]
    NoDevice(String),
    /// A name the device doesn't have.
    #[error("{device} has no program `{name}`; {hint}")]
    UnknownName {
        /// The device's id.
        device: String,
        /// The name given.
        name: String,
        /// Where to find the right one.
        hint: String,
    },
}

/// The program a call asks for: `program`, in the bank that `bank_msb` and
/// `bank_lsb` select, on a track whose device is `device`. `None` when the
/// call gives none. A name is one of the device's programs and brings its
/// own bank.
pub fn requested(
    program: Option<&ProgramRef>,
    bank_msb: Option<u8>,
    bank_lsb: Option<u8>,
    device: Option<&DeviceProfile>,
) -> Result<Option<Program>, ProgramError> {
    let banked = bank_msb.is_some() || bank_lsb.is_some();
    match program {
        None if banked => Err(ProgramError::BankWithoutProgram),
        None => Ok(None),
        Some(&ProgramRef::Number(number)) => Ok(Some(Program {
            number: U7::new(number).ok_or(ProgramError::NumberOutOfRange(number))?,
            bank_msb: bank("bank_msb", bank_msb)?,
            bank_lsb: bank("bank_lsb", bank_lsb)?,
        })),
        Some(ProgramRef::Name(name)) if banked => Err(ProgramError::NameWithBank(name.clone())),
        Some(ProgramRef::Name(name)) => {
            let device = device.ok_or_else(|| ProgramError::NoDevice(name.clone()))?;
            device
                .program(name)
                .map(Some)
                .ok_or_else(|| ProgramError::UnknownName {
                    device: device.id.clone(),
                    name: name.clone(),
                    hint: hint(device, name),
                })
        }
    }
}

/// How a track's program reads back: by its device's name for it, or by
/// number with its bank.
pub fn view(
    program: Program,
    device: Option<&DeviceProfile>,
) -> (ProgramRef, Option<u8>, Option<u8>) {
    match device.and_then(|device| device.program_name(program)) {
        Some(name) => (ProgramRef::Name(name.to_owned()), None, None),
        None => (
            ProgramRef::Number(program.number.get()),
            program.bank_msb.map(U7::get),
            program.bank_lsb.map(U7::get),
        ),
    }
}

/// A bank select byte, 0-127.
fn bank(field: &'static str, value: Option<u8>) -> Result<Option<U7>, ProgramError> {
    value
        .map(|value| U7::new(value).ok_or(ProgramError::BankOutOfRange { field, value }))
        .transpose()
}

/// Where to find the program meant by `name`, which `device` doesn't have:
/// up to [`SUGGESTIONS`] of its names that share the most words with it,
/// and list_devices for the rest.
fn hint(device: &DeviceProfile, name: &str) -> String {
    if device.programs.is_empty() {
        return "it names no programs, so give a number 0-127".into();
    }
    let name = name.to_ascii_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() > 2)
        .collect();
    let mut similar: Vec<(usize, &str)> = device
        .programs
        .keys()
        .map(|program| {
            let shared = words.iter().filter(|word| program.contains(**word)).count();
            (shared, program.as_str())
        })
        .filter(|&(shared, _)| shared > 0)
        .collect();
    // Stable, so names that share as many words stay in order.
    similar.sort_by_key(|&(shared, _)| std::cmp::Reverse(shared));
    let similar: Vec<&str> = similar
        .into_iter()
        .take(SUGGESTIONS)
        .map(|(_, program)| program)
        .collect();
    if similar.is_empty() {
        "list_devices shows its programs".into()
    } else {
        let similar = similar.join(", ");
        format!("similar ones are {similar} (list_devices shows them all)")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use treason_core::midi::Channel;

    use super::*;

    fn u7(value: u8) -> U7 {
        U7::new(value).unwrap()
    }

    fn synth() -> DeviceProfile {
        let warm_pad = Program {
            bank_msb: Some(u7(1)),
            ..Program::new(u7(89))
        };
        DeviceProfile {
            id: "synth".into(),
            name: "Synth".into(),
            manufacturer: None,
            default_channel: Channel::default(),
            voices: BTreeMap::new(),
            aliases: BTreeMap::new(),
            programs: BTreeMap::from([
                ("acoustic_bass".into(), Program::new(u7(32))),
                ("electric_bass_finger".into(), Program::new(u7(33))),
                ("electric_bass_pick".into(), Program::new(u7(34))),
                ("electric_piano_1".into(), Program::new(u7(4))),
                ("warm_pad".into(), warm_pad),
            ]),
            params: BTreeMap::new(),
        }
    }

    fn number(number: u8) -> Option<ProgramRef> {
        Some(ProgramRef::Number(number))
    }

    fn name(name: &str) -> Option<ProgramRef> {
        Some(ProgramRef::Name(name.into()))
    }

    #[test]
    fn programs_are_numbers_with_a_bank_or_the_devices_names() {
        let synth = synth();
        let five = |bank_msb: Option<u8>, bank_lsb: Option<u8>| Program {
            number: u7(5),
            bank_msb: bank_msb.map(u7),
            bank_lsb: bank_lsb.map(u7),
        };
        let cases = [
            (None, None, None, Some(&synth), None),
            (number(5), None, None, None, Some(five(None, None))),
            (number(5), Some(1), None, None, Some(five(Some(1), None))),
            (number(5), None, Some(2), None, Some(five(None, Some(2)))),
            (
                number(5),
                Some(0),
                Some(127),
                None,
                Some(five(Some(0), Some(127))),
            ),
            // A number needs no name, even on a device with names.
            (
                number(127),
                None,
                None,
                Some(&synth),
                Some(Program::new(u7(127))),
            ),
            (
                name("electric_bass_finger"),
                None,
                None,
                Some(&synth),
                Some(Program::new(u7(33))),
            ),
            // A name brings its bank.
            (
                name("warm_pad"),
                None,
                None,
                Some(&synth),
                synth.program("warm_pad"),
            ),
        ];
        for (program, bank_msb, bank_lsb, device, expected) in cases {
            let requested = requested(program.as_ref(), bank_msb, bank_lsb, device);
            assert_eq!(requested, Ok(expected), "{program:?}");
        }
    }

    #[test]
    fn bad_programs_say_how_to_fix_the_call() {
        let synth = synth();
        let bare = DeviceProfile {
            programs: BTreeMap::new(),
            ..synth.clone()
        };
        let cases = [
            (None, Some(1), None, Some(&synth), "give the program too"),
            (None, None, Some(1), None, "give the program too"),
            (
                number(128),
                None,
                None,
                None,
                "0-127, the number sent, not 128; a manual",
            ),
            (
                number(5),
                Some(128),
                None,
                None,
                "bank_msb must be 0-127, not 128",
            ),
            (
                number(5),
                None,
                Some(200),
                None,
                "bank_lsb must be 0-127, not 200",
            ),
            (
                name("warm_pad"),
                Some(1),
                None,
                Some(&synth),
                "`warm_pad` brings its own bank",
            ),
            (
                name("piano"),
                None,
                None,
                None,
                "the track has no device to name programs",
            ),
            (
                name("piano"),
                None,
                None,
                Some(&bare),
                "it names no programs, so give a number",
            ),
            (
                name("Electric-Bass"),
                None,
                None,
                Some(&synth),
                "synth has no program `Electric-Bass`; similar ones are electric_bass_finger, \
                 electric_bass_pick, acoustic_bass, electric_piano_1 (list_devices shows them \
                 all)",
            ),
            (
                name("organ"),
                None,
                None,
                Some(&synth),
                "`organ`; list_devices shows its programs",
            ),
        ];
        for (program, bank_msb, bank_lsb, device, expected) in cases {
            let error = requested(program.as_ref(), bank_msb, bank_lsb, device)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{program:?}: {error}");
        }
    }

    #[test]
    fn programs_read_back_by_the_devices_name_for_them() {
        let synth = synth();
        let warm_pad = synth.program("warm_pad").unwrap();
        let named = |name: &str| (ProgramRef::Name(name.into()), None, None);
        let numbered =
            |number, bank_msb, bank_lsb| (ProgramRef::Number(number), bank_msb, bank_lsb);
        let lsb_only = Program {
            bank_lsb: Some(u7(9)),
            ..Program::new(u7(7))
        };
        let cases = [
            (
                Program::new(u7(33)),
                Some(&synth),
                named("electric_bass_finger"),
            ),
            (warm_pad, Some(&synth), named("warm_pad")),
            // In another bank, it's another sound, with no name.
            (Program::new(u7(89)), Some(&synth), numbered(89, None, None)),
            (Program::new(u7(33)), None, numbered(33, None, None)),
            (warm_pad, None, numbered(89, Some(1), None)),
            (lsb_only, Some(&synth), numbered(7, None, Some(9))),
        ];
        for (program, device, expected) in cases {
            assert_eq!(view(program, device), expected, "{program:?}");
        }
    }
}
