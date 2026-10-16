//! `treason serve`: the long-running sequencer.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use treason_control::Session;
use treason_core::model::Project;
use treason_core::playback::Playback;
use treason_engine::{Engine, SystemClock};
use treason_io::MidirOutput;
use treason_mcp::TreasonServer;

use crate::CLIENT_NAME;
use crate::config::Config;

/// Options for `treason serve`.
#[derive(Debug, clap::Args)]
pub struct ServeArgs {
    /// Serve MCP on stdin and stdout, for a single agent, instead of HTTP.
    #[arg(long)]
    stdio: bool,
    /// Serve MCP over HTTP at this address instead of the configured one.
    #[arg(long, value_name = "ADDR", conflicts_with = "stdio")]
    http: Option<SocketAddr>,
}

/// Opens the ports, starts the engine and serves MCP until Ctrl-C.
pub fn run(config: &Config, args: &ServeArgs) -> anyhow::Result<()> {
    let devices = config.load_devices()?;
    let output =
        MidirOutput::open(CLIENT_NAME, &config.port_specs()).context("opening MIDI ports")?;
    let project = Arc::new(Project::default());
    let (playback, link) = treason_engine::link(Arc::clone(&project));
    let engine = Engine::new(link, output, SystemClock, config.engine_config());

    // The engine loop is the next step (ARCHITECTURE.md, "Next steps"), so the
    // engine isn't started yet. Opening the ports has checked the
    // configuration; dropping the engine closes them again, and the transport
    // tools report that the engine isn't running.
    drop(engine);
    tracing::warn!("the engine is not implemented yet: edits work, but nothing plays");

    let playback: Box<dyn Playback + Send> = Box::new(playback);
    let session = Session::new(project, devices, playback);
    let server = TreasonServer::new(session, config.port_names());

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async {
        if args.stdio {
            treason_mcp::serve_stdio(server, ctrl_c())
                .await
                .context("serving MCP over stdio")
        } else {
            let addr = args.http.unwrap_or(config.mcp.http);
            treason_mcp::serve_http(server, addr, ctrl_c())
                .await
                .with_context(|| format!("serving MCP on {addr}"))
        }
    })
}

/// Completes on Ctrl-C.
async fn ctrl_c() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "can't listen for Ctrl-C");
        std::future::pending::<()>().await;
    }
    tracing::info!("shutting down");
}
