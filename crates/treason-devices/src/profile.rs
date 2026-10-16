//! The device profile format.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use treason_core::midi::{Channel, U7};

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
    /// A voice or alias named with digits alone.
    #[error("`{0}` is a number, which tools read as a MIDI note: give it a name with a letter")]
    NumericName(String),
}

impl DeviceProfile {
    /// Checks that every alias points to a voice, that no alias hides one,
    /// and that no name is a number, since tools read numbers as MIDI notes.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let numeric = |name: &&String| !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit());
        if let Some(name) = self.voices.keys().chain(self.aliases.keys()).find(numeric) {
            return Err(ProfileError::NumericName(name.clone()));
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

    /// A parameter, by name.
    pub fn param(&self, name: &str) -> Option<Param> {
        self.params.get(name).copied()
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
        for (extra, name) in [("16 = 40", "16"), ("[aliases]\n1 = \"kick\"", "1")] {
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
    fn rejects_channels_outside_1_to_16() {
        let text = MINIMAL.replace("default_channel = 1", "default_channel = 17");
        assert!(toml::from_str::<DeviceProfile>(&text).is_err());
    }
}
