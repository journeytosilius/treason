//! Transports: streamable HTTP and stdio.

use std::io;
use std::net::SocketAddr;

use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::server::TreasonServer;

/// Serves MCP over streamable HTTP at `http://{addr}/mcp` until `shutdown`
/// completes.
///
/// Agents can come and go while the server, and the music, keep running.
/// Only loopback `Host` headers are accepted and requests carrying an
/// `Origin` header are refused, so web pages can't drive the sequencer
/// through the browser.
pub async fn serve_http(
    server: TreasonServer,
    addr: SocketAddr,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let cancel = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .enforce_origin_validation()
        .with_cancellation_token(cancel.child_token());
    let service: StreamableHttpService<TreasonServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Default::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("serving MCP at http://{}/mcp", listener.local_addr()?);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown.await;
            // Ends open event streams, which would otherwise hold the server up.
            cancel.cancel();
        })
        .await
}

/// Serves MCP to one agent over stdin and stdout, until the agent
/// disconnects or `shutdown` completes. Logs must go to stderr.
pub async fn serve_stdio(
    server: TreasonServer,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| ServeError::Initialize(Box::new(error)))?;
    let cancel = running.cancellation_token();
    let watcher = tokio::spawn(async move {
        shutdown.await;
        cancel.cancel();
    });
    let finished = running.waiting().await;
    watcher.abort();
    finished?;
    Ok(())
}

/// Why serving over stdio failed.
#[derive(Debug, Error)]
pub enum ServeError {
    /// The client didn't complete the MCP handshake.
    #[error("the MCP client did not initialize")]
    Initialize(#[source] Box<rmcp::service::ServerInitializeError>),
    /// The MCP service task failed.
    #[error("the MCP service failed")]
    Service(#[from] tokio::task::JoinError),
}
