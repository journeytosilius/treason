//! Device profiles: what a synth or drum machine calls its notes and controls.
//!
//! A profile maps names the agent can use ("kick", "cutoff") to MIDI (note 36,
//! CC 74) and back, so the agent never has to know note numbers. Profiles are
//! TOML files; see `devices/` at the root of the repository. [`controllers`]
//! names the controllers MIDI itself defines, such as `modulation`, for any
//! track.

pub mod controllers;
pub mod profile;
pub mod registry;

pub use profile::{DeviceProfile, Param, ProfileError};
pub use registry::{DeviceRegistry, LoadError};
