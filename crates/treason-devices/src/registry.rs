//! A set of device profiles, loaded from a directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use thiserror::Error;

use crate::profile::{DeviceProfile, ProfileError};

/// Every known device profile, by id.
#[derive(Debug, Clone, Default)]
pub struct DeviceRegistry {
    profiles: BTreeMap<String, DeviceProfile>,
}

/// Why a directory of profiles couldn't be loaded.
#[derive(Debug, Error)]
pub enum LoadError {
    /// A file or the directory couldn't be read.
    #[error("could not read {}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A file isn't a valid profile.
    #[error("could not parse {}", .path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    /// A file parses but doesn't hold together.
    #[error("invalid profile in {}", .path.display())]
    Invalid {
        path: PathBuf,
        #[source]
        source: ProfileError,
    },
    /// Two files use the same device id.
    #[error("{} reuses the device id `{id}`", .path.display())]
    DuplicateId { path: PathBuf, id: String },
}

impl DeviceRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads and validates every `.toml` file in `dir`, not recursively.
    pub fn load_dir(dir: &Path) -> Result<Self, LoadError> {
        let read_error = |source| LoadError::Read {
            path: dir.to_path_buf(),
            source,
        };
        let mut paths = Vec::new();
        for entry in fs::read_dir(dir).map_err(read_error)? {
            let path = entry.map_err(read_error)?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                paths.push(path);
            }
        }
        // Sorted, so which file counts as the duplicate is reproducible.
        paths.sort();

        let mut registry = Self::new();
        for path in paths {
            let text = fs::read_to_string(&path).map_err(|source| LoadError::Read {
                path: path.clone(),
                source,
            })?;
            let profile: DeviceProfile =
                toml::from_str(&text).map_err(|source| LoadError::Parse {
                    path: path.clone(),
                    source,
                })?;
            profile.validate().map_err(|source| LoadError::Invalid {
                path: path.clone(),
                source,
            })?;
            if registry.profiles.contains_key(&profile.id) {
                return Err(LoadError::DuplicateId {
                    path,
                    id: profile.id,
                });
            }
            registry.insert(profile);
        }
        Ok(registry)
    }

    /// Adds a profile, returning any it replaces with the same id. Doesn't
    /// validate; call [`DeviceProfile::validate`] first.
    pub fn insert(&mut self, profile: DeviceProfile) -> Option<DeviceProfile> {
        self.profiles.insert(profile.id.clone(), profile)
    }

    /// The profile with this id.
    pub fn get(&self, id: &str) -> Option<&DeviceProfile> {
        self.profiles.get(id)
    }

    /// All profiles, ordered by id.
    pub fn iter(&self) -> impl Iterator<Item = &DeviceProfile> {
        self.profiles.values()
    }

    /// How many profiles there are.
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use treason_core::midi::{Channel, U7};

    use super::*;

    #[test]
    fn loads_the_bundled_profiles() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../devices");
        let registry = DeviceRegistry::load_dir(&dir).unwrap();

        let gm = registry.get("gm-drums").unwrap();
        assert_eq!(gm.default_channel, Channel::DRUMS);
        assert_eq!(gm.voice("kick"), U7::new(36));
        assert_eq!(gm.voice("bd"), U7::new(36));
        assert_eq!(gm.voice_name(U7::new(42).unwrap()), Some("closed_hat"));
        assert_eq!(gm.param("volume").map(|param| param.cc), U7::new(7));
    }
}
