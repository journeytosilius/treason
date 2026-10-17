//! `treason`, the composition root.
//!
//! The only crate that knows all the others. It reads the configuration,
//! loads device profiles, opens the MIDI ports, starts the engine thread and
//! serves MCP, connecting the pieces through the traits in `treason-core`.

mod config;
mod serve;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use treason_io::MidirOutput;

use crate::config::Config;

/// How treason appears to other MIDI software.
const CLIENT_NAME: &str = "treason";

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// The configuration file [default: treason.toml, if it exists]
    #[arg(short, long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the sequencer and serve its MCP tools.
    Serve(serve::ServeArgs),
    /// List the MIDI output ports on this machine.
    Ports,
    /// List the device profiles.
    Devices,
    /// List the saved projects, newest first.
    Projects,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Logs go to stderr: with `serve --stdio`, stdout carries the protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::load(cli.config.as_deref())?;
    match cli.command {
        Command::Serve(args) => serve::run(&config, &args),
        Command::Ports => list_ports(),
        Command::Devices => list_devices(&config),
        Command::Projects => list_projects(&config),
    }
}

fn list_ports() -> anyhow::Result<()> {
    let names = MidirOutput::port_names(CLIENT_NAME).context("listing MIDI output ports")?;
    if names.is_empty() {
        println!("No MIDI output ports.");
    }
    for name in names {
        println!("{name}");
    }
    Ok(())
}

fn list_projects(config: &Config) -> anyhow::Result<()> {
    let projects = config.project_store();
    let list = projects.list().context("listing the saved projects")?;
    if list.names.is_empty() {
        println!("No saved projects in {}.", projects.dir().display());
    }
    for name in &list.names {
        println!("{name}");
    }
    if list.more > 0 {
        println!("...and {} older.", list.more);
    }
    Ok(())
}

fn list_devices(config: &Config) -> anyhow::Result<()> {
    for device in config.load_devices()?.iter() {
        println!(
            "{:<16} {} ({} voices, {} params)",
            device.id,
            device.name,
            device.voices.len(),
            device.params.len()
        );
    }
    Ok(())
}
