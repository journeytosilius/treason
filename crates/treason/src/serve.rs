//! `treason serve`: the long-running sequencer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use treason_control::Session;
use treason_core::model::Project;
use treason_core::playback::Playback;
use treason_engine::{Engine, SystemClock};
use treason_io::MidirOutput;
use treason_mcp::TreasonServer;

use crate::CLIENT_NAME;
use crate::config::Config;

/// How long shutting down waits for work that can't be cancelled, such as a
/// pending read from stdin, before leaving it behind.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

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

/// Opens the ports, starts the engine and serves MCP until Ctrl-C or
/// SIGTERM, or with `--stdio` until the agent disconnects. Then stops the
/// engine, which ends every sounding note, and waits for it.
pub fn run(config: &Config, args: &ServeArgs) -> anyhow::Result<()> {
    let devices = config.load_devices()?;
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    let output =
        MidirOutput::open(CLIENT_NAME, &config.port_specs()).context("opening MIDI ports")?;
    let project = Arc::new(Project::default());
    let (playback, link) = treason_engine::link(Arc::clone(&project));
    let engine = Engine::new(link, output, SystemClock, config.engine_config())
        .spawn()
        .context("starting the engine thread")?;

    let playback: Box<dyn Playback + Send> = Box::new(playback);
    let session = Session::new(project, devices, playback);
    let server = TreasonServer::new(session, config.port_names());

    let served = runtime.block_on(async {
        if args.stdio {
            treason_mcp::serve_stdio(server, shutdown_signal())
                .await
                .context("serving MCP over stdio")
        } else {
            let addr = args.http.unwrap_or(config.mcp.http);
            treason_mcp::serve_http(server, addr, shutdown_signal())
                .await
                .with_context(|| format!("serving MCP on {addr}"))
        }
    });
    // Shutting the runtime down drops every copy of the server, and with them
    // the engine's handle, so the engine ends its notes and returns. Waiting
    // for it means no note is left sounding when the process exits.
    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    let played = engine.join().context("running the engine");
    served.and(played)
}

/// Completes on Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "can't listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = terminate() => {}
    }
    tracing::info!("shutting down");
}

/// Completes on SIGTERM, which `kill`, service managers and MCP clients send
/// to stop a program.
#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            terminate.recv().await;
        }
        Err(error) => {
            tracing::error!(%error, "can't listen for SIGTERM");
            std::future::pending::<()>().await;
        }
    }
}

/// Never completes: SIGTERM is Unix only.
#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}
