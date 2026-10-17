//! The device profile format.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use treason_core::midi::{Channel, U7};
use treason_core::model::Program;

use crate::controllers;

/// What one synth or drum machine calls its notes and controls.
///
/// Written in TOML; `devices/gm-drums.toml` is an example.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfile {
    /// A short id used in projects and tool calls, such as `gm-drums`.
    pub id: String,
    /// A name for people.
    pub name: String,
    /// Who makes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    /// The channel the device listens on out of the box (1-16 in the file).
    pub default_channel: Channel,
    /// Named notes. On a drum machine, one per sound.
    #[serde(default)]
    pub voices: BTreeMap<String, U7>,
    /// Other names for voices, such as `bd = "kick"`.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Named sounds, each a program number such as `piano = 0`, or for a
    /// sound in a bank, `warm_pad = { program = 89, bank_msb = 1 }`.
    #[serde(default, with = "programs")]
    pub programs: BTreeMap<String, Program>,
    /// Named parameters reachable over MIDI.
    #[serde(default)]
    pub params: BTreeMap<String, Param>,
}

/// How to reach one parameter. NRPN support is planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    /// The controller number.
    pub cc: U7,
}

/// A profile that parses but doesn't hold together.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProfileError {
    /// An alias names a voice the profile doesn't have.
    #[error("alias `{alias}` points to unknown voice `{voice}`")]
    UnknownAliasTarget { alias: String, voice: String },
    /// A name is both a voice and an alias.
    #[error("`{0}` is both a voice and an alias")]
    AliasShadowsVoice(String),
    /// A voice, alias, program or parameter named with digits alone.
    #[error(
        "`{0}` is a number, which tools read as a MIDI note, controller or program: give it a name with a letter"
    )]
    NumericName(String),
    /// A parameter named like pitch bend or channel pressure, which tools
    /// send as messages of their own.
    #[error(
        "parameter `{0}` has the name set_controls gives pitch bend or channel pressure, which aren't controllers: give it another name"
    )]
    ReservedParam(String),
}

impl DeviceProfile {
    /// Checks that every alias points to a voice, that no alias hides one,
    /// that no name is a number, since tools read numbers as MIDI notes,
    /// controllers and programs, and that no parameter takes the name of
    /// pitch bend or channel pressure.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let numeric = |name: &&String| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit());
        let names = self.voices.keys().chain(self.aliases.keys());
        let names = names.chain(self.programs.keys());
        if let Some(name) = names.chain(self.params.keys()).find(numeric) {
            return Err(ProfileError::NumericName(name.clone()));
        }
        let reserved = [controllers::PITCH_BEND, controllers::PRESSURE];
        if let Some(name) = self
            .params
            .keys()
            .find(|name| reserved.contains(&name.as_str()))
        {
            return Err(ProfileError::ReservedParam(name.clone()));
        }
        for (alias, voice) in &self.aliases {
            if self.voices.contains_key(alias) {
                return Err(ProfileError::AliasShadowsVoice(alias.clone()));
            }
            if !self.voices.contains_key(voice) {
                return Err(ProfileError::UnknownAliasTarget {
                    alias: alias.clone(),
                    voice: voice.clone(),
                });
            }
        }
        Ok(())
    }

    /// The note for a voice name or alias.
    pub fn voice(&self, name: &str) -> Option<U7> {
        let name = self.aliases.get(name).map_or(name, String::as_str);
        self.voices.get(name).copied()
    }

    /// The voice name for a note, to describe patterns back to the agent.
    pub fn voice_name(&self, note: U7) -> Option<&str> {
        self.voices
            .iter()
            .find_map(|(name, &voice)| (voice == note).then_some(name.as_str()))
    }

    /// A program, by name.
    pub fn program(&self, name: &str) -> Option<Program> {
        self.programs.get(name).copied()
    }

    /// The name of a program, bank included, to describe tracks back to the
    /// agent.
    pub fn program_name(&self, program: Program) -> Option<&str> {
        self.programs
            .iter()
            .find_map(|(name, &named)| (named == program).then_some(name.as_str()))
    }

    /// A parameter, by name.
    pub fn param(&self, name: &str) -> Option<Param> {
        self.params.get(name).copied()
    }

    /// The parameter name for a controller, to describe controls back to
    /// the agent.
    pub fn param_name(&self, controller: U7) -> Option<&str> {
        self.params
            .iter()
            .find_map(|(name, param)| (param.cc == controller).then_some(name.as_str()))
    }
}

/// A profile's programs in TOML: each a number, or a table with its bank.
mod programs {
    use super::*;

    /// A program in a bank, as a profile writes it.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Banked {
        program: U7,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bank_msb: Option<U7>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bank_lsb: Option<U7>,
    }

    /// One program, written as its number alone when it has no bank.
    struct Entry(Program);

    impl Serialize for Entry {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let Program {
                number,
                bank_msb,
                bank_lsb,
            } = self.0;
            if bank_msb.is_none() && bank_lsb.is_none() {
                return number.serialize(serializer);
            }
            Banked {
                program: number,
                bank_msb,
                bank_lsb,
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Entry {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(EntryVisitor).map(Entry)
        }
    }

    struct EntryVisitor;

    impl EntryVisitor {
        fn number<E: de::Error>(
            self,
            value: u64,
            unexpected: de::Unexpected,
        ) -> Result<Program, E> {
            u8::try_from(value)
                .ok()
                .and_then(U7::new)
                .map(Program::new)
                .ok_or_else(|| E::invalid_value(unexpected, &self))
        }
    }

    impl<'de> Visitor<'de> for EntryVisitor {
        type Value = Program;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(
                "a program number, 0-127, or a table such as { program = 5, bank_msb = 1 }",
            )
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> Result<Program, E> {
            let unexpected = de::Unexpected::Signed(value);
            let value = u64::try_from(value).map_err(|_| E::invalid_value(unexpected, &self))?;
            self.number(value, unexpected)
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Program, E> {
            self.number(value, de::Unexpected::Unsigned(value))
        }

        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Program, A::Error> {
            let Banked {
                program,
                bank_msb,
                bank_lsb,
            } = Banked::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(Program {
                number: program,
                bank_msb,
                bank_lsb,
            })
        }
    }

    pub fn serialize<S: Serializer>(
        programs: &BTreeMap<String, Program>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            programs
                .iter()
                .map(|(name, &program)| (name, Entry(program))),
        )
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, Program>, D::Error> {
        let entries = BTreeMap::<String, Entry>::deserialize(deserializer)?;
        Ok(entries
            .into_iter()
            .map(|(name, Entry(program))| (name, program))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        id = "test"
        name = "Test"
        default_channel = 1
        [voices]
        kick = 36
    "#;

    #[test]
    fn rejects_aliases_to_missing_voices() {
        let profile: DeviceProfile =
            toml::from_str(&format!("{MINIMAL}\n[aliases]\nsd = \"snare\"")).unwrap();
        assert_eq!(
            profile.validate(),
            Err(ProfileError::UnknownAliasTarget {
                alias: "sd".into(),
                voice: "snare".into(),
            })
        );
    }

    #[test]
    fn rejects_names_that_are_numbers() {
        for (extra, name) in [
            ("16 = 40", "16"),
            ("[aliases]\n1 = \"kick\"", "1"),
            ("[params]\n74 = { cc = 74 }", "74"),
            ("[programs]\n5 = 5", "5"),
        ] {
            let profile: DeviceProfile = toml::from_str(&format!("{MINIMAL}\n{extra}")).unwrap();
            assert_eq!(
                profile.validate(),
                Err(ProfileError::NumericName(name.into()))
            );
        }
        let named: DeviceProfile = toml::from_str(&format!("{MINIMAL}\nkick2 = 40")).unwrap();
        assert_eq!(named.validate(), Ok(()));
    }

    #[test]
    fn parameters_leave_pitch_bend_and_pressure_their_names() {
        for name in ["pitch_bend", "pressure"] {
            let text = format!("{MINIMAL}\n[params]\n{name} = {{ cc = 1 }}");
            let profile: DeviceProfile = toml::from_str(&text).unwrap();
            assert_eq!(
                profile.validate(),
                Err(ProfileError::ReservedParam(name.into()))
            );
            // Voices and programs live apart from controls.
            let text = format!("{MINIMAL}\n{name} = 40\n[programs]\n{name} = 1");
            let profile: DeviceProfile = toml::from_str(&text).unwrap();
            assert_eq!(profile.validate(), Ok(()));
        }
    }

    #[test]
    fn parameters_are_found_by_name_and_by_controller() {
        let profile: DeviceProfile = toml::from_str(&format!(
            "{MINIMAL}\n[params]\nfilter = {{ cc = 74 }}\nwheel = {{ cc = 1 }}"
        ))
        .unwrap();
        let cutoff = U7::new(74).unwrap();
        assert_eq!(profile.param("filter"), Some(Param { cc: cutoff }));
        assert_eq!(profile.param_name(cutoff), Some("filter"));
        assert_eq!(profile.param_name(U7::new(1).unwrap()), Some("wheel"));
        assert_eq!(profile.param_name(U7::new(7).unwrap()), None);
    }

    #[test]
    fn programs_are_found_by_name_and_by_sound() {
        let profile: DeviceProfile = toml::from_str(&format!(
            "{MINIMAL}\n[programs]\npiano = 0\nwarm_pad = {{ program = 89, bank_msb = 1 }}\n\
             kit = {{ program = 25, bank_lsb = 3 }}\nplain = {{ program = 7 }}"
        ))
        .unwrap();
        let u7 = |value| U7::new(value).unwrap();
        let warm_pad = Program {
            bank_msb: Some(u7(1)),
            ..Program::new(u7(89))
        };
        let kit = Program {
            bank_lsb: Some(u7(3)),
            ..Program::new(u7(25))
        };
        assert_eq!(profile.program("piano"), Some(Program::new(u7(0))));
        assert_eq!(profile.program("warm_pad"), Some(warm_pad));
        assert_eq!(profile.program("kit"), Some(kit));
        assert_eq!(profile.program("plain"), Some(Program::new(u7(7))));
        assert_eq!(profile.program("organ"), None);
        assert_eq!(profile.program_name(warm_pad), Some("warm_pad"));
        // The bank is part of the sound.
        assert_eq!(profile.program_name(Program::new(u7(89))), None);
        assert_eq!(profile.program_name(Program::new(u7(0))), Some("piano"));

        // Written back in the same forms, it reads the same.
        let text = toml::to_string(&profile).unwrap();
        assert!(text.contains("piano = 0\n"), "{text}");
        assert_eq!(toml::from_str::<DeviceProfile>(&text).unwrap(), profile);
    }

    #[test]
    fn bad_programs_say_what_is_wrong() {
        for (program, expected) in [
            ("128", "a program number, 0-127"),
            ("-1", "a program number, 0-127"),
            ("\"five\"", "a program number, 0-127"),
            ("{ program = 128 }", "128"),
            ("{ bank_msb = 1 }", "missing field `program`"),
            ("{ program = 1, bank = 2 }", "unknown field `bank`"),
            ("{ program = 1, bank_lsb = 200 }", "200"),
        ] {
            let text = format!("{MINIMAL}\n[programs]\npiano = {program}");
            let error = toml::from_str::<DeviceProfile>(&text)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{program}: {error}");
        }
    }

    #[test]
    fn rejects_channels_outside_1_to_16() {
        let text = MINIMAL.replace("default_channel = 1", "default_channel = 17");
        assert!(toml::from_str::<DeviceProfile>(&text).is_err());
    }
}
