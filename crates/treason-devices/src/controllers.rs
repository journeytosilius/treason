//! Standard names for MIDI controllers, so a control can be named on any
//! track, such as `modulation` for the mod wheel. On a track with a device,
//! the profile's own parameter names come first.

use treason_core::midi::U7;

/// Controllers whose meaning MIDI defines, by name: the MIDI 1.0
/// assignments, with General MIDI 2's meanings for the sound controllers
/// (71-78) and the effect depths (91-95). Most synths follow them; a device
/// profile's parameters say where a device differs.
pub const STANDARD: [(&str, u8); 26] = [
    ("modulation", 1),
    ("breath", 2),
    ("foot", 4),
    ("portamento_time", 5),
    ("volume", 7),
    ("balance", 8),
    ("pan", 10),
    ("expression", 11),
    ("sustain", 64),
    ("portamento", 65),
    ("sostenuto", 66),
    ("soft_pedal", 67),
    ("legato", 68),
    ("resonance", 71),
    ("release", 72),
    ("attack", 73),
    ("cutoff", 74),
    ("decay", 75),
    ("vibrato_rate", 76),
    ("vibrato_depth", 77),
    ("vibrato_delay", 78),
    ("reverb", 91),
    ("tremolo", 92),
    ("chorus", 93),
    ("detune", 94),
    ("phaser", 95),
];

/// The controller a standard name stands for, such as 1 for `modulation`.
pub fn standard_controller(name: &str) -> Option<U7> {
    STANDARD
        .iter()
        .find_map(|&(standard, number)| (standard == name).then(|| U7::saturating(number)))
}

/// A controller's standard name, if it has one.
pub fn standard_name(controller: U7) -> Option<&'static str> {
    STANDARD
        .iter()
        .find_map(|&(name, number)| (number == controller.get()).then_some(name))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn standard_names_and_numbers_each_appear_once() {
        let names: BTreeSet<_> = STANDARD.iter().map(|&(name, _)| name).collect();
        let numbers: BTreeSet<_> = STANDARD.iter().map(|&(_, number)| number).collect();
        assert_eq!(
            (names.len(), numbers.len()),
            (STANDARD.len(), STANDARD.len())
        );
        for (name, number) in STANDARD {
            // Tools read a bare number as a controller, and 120-127 are
            // channel mode messages rather than controls.
            assert!(
                name.bytes().any(|byte| byte.is_ascii_alphabetic()),
                "{name}"
            );
            assert!(number < 120, "{name} is {number}");
            let controller = U7::new(number).unwrap();
            assert_eq!(standard_controller(name), Some(controller));
            assert_eq!(standard_name(controller), Some(name));
        }
    }

    #[test]
    fn other_names_and_numbers_have_no_standard() {
        assert_eq!(standard_controller("mod_wheel"), None);
        assert_eq!(standard_controller("Modulation"), None);
        assert_eq!(standard_controller("1"), None);
        assert_eq!(standard_name(U7::MIN), None);
        assert_eq!(standard_name(U7::new(3).unwrap()), None);
    }
}
