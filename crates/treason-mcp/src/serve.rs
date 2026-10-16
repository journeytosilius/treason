//! Transports: streamable HTTP and stdio.

use std::future::IntoFuture;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::server::TreasonServer;

/// Let in-flight requests finish briefly before the caller tears down its
/// runtime. A request body may never finish, even after sessions are cancelled.
const HTTP_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Serves MCP over streamable HTTP at `http://{addr}/mcp` until `shutdown`
/// completes.
///
/// Allows up to one second for HTTP requests to drain after shutdown. The
/// caller must then shut down the runtime to drop any remaining connection
/// tasks and their copies of the server.
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
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_http_on(server, listener, shutdown).await
}

/// Serves HTTP on an already bound listener, with the same checks and shutdown
/// behavior as [`serve_http`]. Useful when the caller needs the chosen port.
pub async fn serve_http_on(
    server: TreasonServer,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let cancel = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .enforce_origin_validation()
        .with_cancellation_token(cancel.child_token());
    let service: StreamableHttpService<TreasonServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Default::default(), config);
    let router = axum::Router::new().nest_service("/mcp", service);
    tracing::info!("serving MCP at http://{}/mcp", listener.local_addr()?);
    let shutdown_cancel = cancel.clone();
    let serving = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown.await;
            // Ends open event streams, which would otherwise hold the server up.
            shutdown_cancel.cancel();
        })
        .into_future();
    tokio::pin!(serving);
    tokio::select! {
        result = &mut serving => result,
        () = cancel.cancelled() => {
            match tokio::time::timeout(HTTP_SHUTDOWN_GRACE, serving).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!("HTTP shutdown grace expired; dropping remaining connections with the runtime");
                    Ok(())
                }
            }
        }
    }
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
