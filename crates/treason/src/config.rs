//! The configuration file, `treason.toml`.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;
use treason_control::{ExportStore, ProjectStore};
use treason_devices::DeviceRegistry;
use treason_engine::{EngineConfig, PortSettings};
use treason_io::PortSpec;

/// Read from the current directory when no file is named.
const DEFAULT_FILE: &str = "treason.toml";

/// Everything in `treason.toml`. Every field has a default.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The device profile directory. Relative to the config file.
    pub devices: PathBuf,
    /// Where projects are saved. Relative to the config file.
    pub projects: PathBuf,
    /// Where exported MIDI files go. Relative to the config file.
    pub exports: PathBuf,
    /// The MCP server.
    pub mcp: McpOptions,
    /// The engine.
    pub engine: EngineOptions,
    /// The output ports. The first is the default for new tracks.
    pub ports: Vec<PortOptions>,
}

/// `[mcp]`
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpOptions {
    /// Where to serve MCP over HTTP.
    pub http: SocketAddr,
}

/// `[engine]`
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineOptions {
    /// How far ahead the engine renders, in milliseconds.
    pub lookahead_ms: u64,
    /// Whether to ask for real-time scheduling.
    pub realtime: bool,
    /// What sends the MIDI.
    pub backend: Backend,
}

/// What sends the engine's MIDI, as `engine.backend` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// midir, which sends each message when it is due.
    Midir,
    /// An ALSA sequencer queue (Linux only), which takes messages up to the
    /// lookahead early and sends each at its time.
    AlsaSeq,
}

/// `[[ports]]`
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortOptions {
    /// How tracks and agents refer to the port, and its name in the system.
    pub name: String,
    /// Connect to the first port whose name contains this. Without it, the
    /// port is virtual: other software connects to it.
    #[serde(default)]
    pub connect: Option<String>,
    /// The device's delay in milliseconds. Its messages go out this much
    /// earlier.
    #[serde(default)]
    pub latency_ms: f64,
    /// Send MIDI clock and transport messages to this port.
    #[serde(default)]
    pub clock: bool,
}

impl Config {
    /// Reads `path`. Without one, reads `treason.toml` if it exists, or
    /// uses the defaults.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let path = match path {
            Some(path) => path,
            None if Path::new(DEFAULT_FILE).exists() => Path::new(DEFAULT_FILE),
            None => return Ok(Self::default()),
        };
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if let Some(dir) = path.parent() {
            config.devices = dir.join(&config.devices);
            config.projects = dir.join(&config.projects);
            config.exports = dir.join(&config.exports);
        }
        config
            .validate()
            .with_context(|| format!("checking {}", path.display()))?;
        Ok(config)
    }

    /// Loads the device profiles.
    pub fn load_devices(&self) -> anyhow::Result<DeviceRegistry> {
        DeviceRegistry::load_dir(&self.devices)
            .with_context(|| format!("loading device profiles from {}", self.devices.display()))
    }

    /// The saved projects.
    pub fn project_store(&self) -> ProjectStore {
        ProjectStore::new(&self.projects)
    }

    /// The exported MIDI files.
    pub fn export_store(&self) -> ExportStore {
        ExportStore::new(&self.exports)
    }

    /// The ports' names, in `PortId` order.
    pub fn port_names(&self) -> Vec<String> {
        self.ports.iter().map(|port| port.name.clone()).collect()
    }

    /// What the MIDI backend opens, in `PortId` order.
    pub fn port_specs(&self) -> Vec<PortSpec> {
        self.ports
            .iter()
            .map(|port| PortSpec {
                name: port.name.clone(),
                connect: port.connect.clone(),
            })
            .collect()
    }

    /// The engine's settings.
    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig {
            lookahead: Duration::from_millis(self.engine.lookahead_ms),
            ports: self
                .ports
                .iter()
                .map(|port| PortSettings {
                    // `validate` has checked that this converts.
                    latency: Duration::from_secs_f64(port.latency_ms / 1000.0),
                    clock: port.clock,
                })
                .collect(),
            realtime: self.engine.realtime,
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        if !(1..=1000).contains(&self.engine.lookahead_ms) {
            bail!("engine.lookahead_ms must be 1 to 1000");
        }
        if self.ports.is_empty() {
            bail!("add at least one [[ports]] entry");
        }
        let mut names = BTreeSet::new();
        for port in &self.ports {
            if port.name.is_empty() {
                bail!("every port needs a name");
            }
            if !names.insert(&port.name) {
                bail!("two ports are named `{}`", port.name);
            }
            if !(0.0..=1000.0).contains(&port.latency_ms) {
                bail!("port `{}`: latency_ms must be 0 to 1000", port.name);
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            devices: PathBuf::from("devices"),
            projects: PathBuf::from("projects"),
            exports: PathBuf::from("exports"),
            mcp: McpOptions::default(),
            engine: EngineOptions::default(),
            ports: vec![PortOptions {
                name: "out".to_owned(),
                connect: None,
                latency_ms: 0.0,
                clock: false,
            }],
        }
    }
}

impl Default for McpOptions {
    fn default() -> Self {
        Self {
            http: SocketAddr::from((Ipv4Addr::LOCALHOST, 6434)),
        }
    }
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            lookahead_ms: 20,
            realtime: true,
            backend: Backend::Midir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_config_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../treason.toml");
        let config = Config::load(Some(&path)).unwrap();
        assert!(config.load_devices().unwrap().get("gm-drums").is_some());
        assert_eq!(
            config.project_store().dir(),
            path.parent().unwrap().join("projects")
        );
        assert_eq!(
            config.export_store().dir(),
            path.parent().unwrap().join("exports")
        );
        assert_eq!(config.port_names(), ["out"]);
        assert_eq!(config.engine_config().lookahead, Duration::from_millis(20));
        assert_eq!(config.engine.backend, Backend::Midir);
    }

    #[test]
    fn the_backend_is_midir_unless_the_alsa_queue_is_named() {
        let backend =
            |toml: &str| toml::from_str::<Config>(toml).map(|config| config.engine.backend);
        assert_eq!(backend("").unwrap(), Backend::Midir);
        assert_eq!(
            backend("[engine]\nlookahead_ms = 30").unwrap(),
            Backend::Midir
        );
        assert_eq!(
            backend("[engine]\nbackend = \"alsa-seq\"").unwrap(),
            Backend::AlsaSeq
        );
        let error = backend("[engine]\nbackend = \"jack\"").unwrap_err();
        assert!(error.to_string().contains("alsa-seq"), "{error}");
    }

    #[test]
    fn rejects_duplicate_ports() {
        let config: Config = toml::from_str(
            r#"
            [[ports]]
            name = "out"
            [[ports]]
            name = "out"
            "#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }
}
