//! `treason serve`: the long-running sequencer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use treason_control::Session;
use treason_core::model::Project;
use treason_core::output::MidiOutput;
use treason_core::playback::Playback;
use treason_devices::DeviceRegistry;
use treason_engine::{Engine, SystemClock};
#[cfg(target_os = "linux")]
use treason_io::AlsaSeqOutput;
use treason_io::MidirOutput;
use treason_mcp::TreasonServer;

use crate::CLIENT_NAME;
use crate::config::{Backend, Config};

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
    let ports = config.port_specs();
    match config.engine.backend {
        Backend::Midir => {
            let output = MidirOutput::open(CLIENT_NAME, &ports).context("opening MIDI ports")?;
            serve(config, args, devices, output)
        }
        #[cfg(target_os = "linux")]
        Backend::AlsaSeq => {
            let lookahead = config.engine_config().lookahead;
            let output = AlsaSeqOutput::open(CLIENT_NAME, &ports, lookahead)
                .context("opening MIDI ports on the ALSA sequencer")?;
            serve(config, args, devices, output)
        }
        #[cfg(not(target_os = "linux"))]
        Backend::AlsaSeq => anyhow::bail!("engine.backend = \"alsa-seq\" needs Linux"),
    }
}

/// Plays through `output` and serves MCP until shutdown.
fn serve<O: MidiOutput + 'static>(
    config: &Config,
    args: &ServeArgs,
    devices: DeviceRegistry,
    output: O,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
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

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::mpsc;
    use std::time::Instant;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;
    use treason_core::midi::{Channel, MidiMessage, U7};
    use treason_core::model::{Clip, ClipEvent, EventKind, OutputTarget, PortId, Track, TrackId};
    use treason_core::output::{Delivery, MidiOutput, TimedMessage};
    use treason_core::playback::TransportCommand;
    use treason_core::time::Ticks;
    use treason_devices::DeviceRegistry;
    use treason_engine::{EngineConfig, ManualClock, PortSettings};

    use super::*;

    /// Records bounded evidence across the engine thread without MIDI ports.
    struct NoteOutput {
        first_note: Option<oneshot::Sender<()>>,
        finished: mpsc::Sender<(usize, usize, bool)>,
        ons: usize,
        offs: usize,
        sounding: bool,
    }

    impl MidiOutput for NoteOutput {
        type Error = Infallible;

        fn delivery(&self) -> Delivery {
            Delivery::Immediate
        }

        fn send(&mut self, message: &TimedMessage) -> Result<(), Infallible> {
            match message.message {
                MidiMessage::NoteOn { .. } => {
                    assert!(!self.sounding);
                    self.sounding = true;
                    self.ons += 1;
                    if let Some(first_note) = self.first_note.take() {
                        let _ = first_note.send(());
                    }
                }
                MidiMessage::NoteOff { .. } => {
                    assert!(self.sounding);
                    self.sounding = false;
                    self.offs += 1;
                }
                _ => unreachable!(),
            }
            Ok(())
        }

        fn cancel_from(
            &mut self,
            _from: Instant,
            _cancelled: &mut dyn FnMut(&TimedMessage),
        ) -> Result<(), Infallible> {
            Ok(())
        }
    }

    impl Drop for NoteOutput {
        fn drop(&mut self) {
            let _ = self.finished.send((self.ons, self.offs, self.sounding));
        }
    }

    fn shutdown_during_playback(unfinished_request: bool) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // A note held throughout each beat, so playback keeps a note sounding.
        let project = Arc::new(Project {
            tracks: vec![Arc::new(Track {
                id: TrackId(1),
                name: "held note".into(),
                device: None,
                target: OutputTarget {
                    port: PortId(0),
                    channel: Channel::DRUMS,
                },
                muted: false,
                clip: Clip::new(
                    Ticks::quarters(1),
                    vec![ClipEvent {
                        start: Ticks::ZERO,
                        kind: EventKind::Note {
                            note: U7::new(60).unwrap(),
                            velocity: U7::MAX,
                            length: Ticks::quarters(1),
                        },
                    }],
                )
                .unwrap(),
            })],
            ..Project::default()
        });
        let (mut playback, link) = treason_engine::link(Arc::clone(&project));
        playback.command(TransportCommand::Play).unwrap();
        let (first_note, playing) = oneshot::channel();
        let (finished, notes) = mpsc::channel();
        let output = NoteOutput {
            first_note: Some(first_note),
            finished,
            ons: 0,
            offs: 0,
            sounding: false,
        };
        let engine = Engine::new(
            link,
            output,
            ManualClock::new(),
            EngineConfig {
                ports: vec![PortSettings::default()],
                realtime: false,
                ..EngineConfig::default()
            },
        )
        .spawn()
        .unwrap();
        let playback: Box<dyn Playback + Send> = Box::new(playback);
        let server = TreasonServer::new(
            Session::new(project, DeviceRegistry::new(), playback),
            vec!["test".into()],
        );
        playing.blocking_recv().unwrap();

        let (served, request, elapsed) = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (stop, shutdown) = oneshot::channel();
            let serving = tokio::spawn(treason_mcp::serve_http_on(server, listener, async {
                shutdown.await.unwrap();
            }));
            let request = if unfinished_request {
                let mut request = tokio::net::TcpStream::connect(addr).await.unwrap();
                request
                    .write_all(
                        b"POST /mcp HTTP/1.1\r\nHost: localhost\r\n\
                          Accept: application/json, text/event-stream\r\n\
                          Content-Type: application/json\r\nContent-Length: 1024\r\n\
                          Expect: 100-continue\r\n\r\n",
                    )
                    .await
                    .unwrap();
                // Hyper sends this only once the service polls the body. This
                // proves shutdown races an in-flight read, not just a socket.
                let mut response = [0; b"HTTP/1.1 100 Continue\r\n\r\n".len()];
                tokio::time::timeout(Duration::from_secs(5), request.read_exact(&mut response))
                    .await
                    .expect("the HTTP service did not start reading the request body")
                    .unwrap();
                assert_eq!(&response, b"HTTP/1.1 100 Continue\r\n\r\n");
                Some(request)
            } else {
                None
            };
            tokio::time::pause();
            let start = tokio::time::Instant::now();
            stop.send(()).unwrap();
            let served = tokio::time::timeout(Duration::from_secs(5), serving).await;
            (served, request, start.elapsed())
        });
        // Keep the client's unfinished body open across the production shutdown
        // sequence: runtime tasks must release the handle before engine.join().
        runtime.shutdown_timeout(SHUTDOWN_GRACE);
        let (ons, offs, sounding) = notes.recv_timeout(Duration::from_secs(5)).unwrap();
        engine.join().unwrap();
        drop(request);

        served
            .expect("HTTP drain exceeded its grace period")
            .unwrap()
            .unwrap();
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        if !unfinished_request {
            assert_eq!(elapsed, Duration::ZERO);
        }
        assert!(ons > 0);
        assert_eq!(ons, offs);
        assert!(!sounding, "shutdown left a note sounding");
    }

    #[test]
    fn http_shutdown_ends_playback_with_an_unfinished_request_body() {
        shutdown_during_playback(true);
    }

    #[test]
    fn http_shutdown_without_requests_ends_playback_without_waiting_for_the_grace() {
        shutdown_during_playback(false);
    }
}
