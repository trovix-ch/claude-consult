//! Serving the server: stdio for a per-session child, stateless streamable HTTP for the
//! shared service.

use std::path::PathBuf;

use consult_core::paths;
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::server::ConsultServer;
use crate::{Error, init_logging};

/// The shared service's port when none is given.
pub const DEFAULT_HTTP_PORT: u16 = 8765;
/// The shared service's host when none is given.
pub const DEFAULT_HTTP_HOST: &str = "127.0.0.1";
/// The path the HTTP transport serves MCP on.
pub const MCP_PATH: &str = "/mcp";

/// Which transport to serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// MCP over stdin/stdout: one server per Claude Code session.
    Stdio,
    /// Stateless streamable HTTP at `http://host:port/mcp`: one server for every session.
    Http {
        /// Address to bind.
        host: String,
        /// Port to bind.
        port: u16,
    },
}

/// What `claude-consult serve` asks for.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    /// The transport.
    pub transport: Transport,
    /// `--install-dir`; `None` resolves through [`paths::install_dir`].
    pub install_dir: Option<PathBuf>,
    /// `--claude-dir`; `None` resolves through [`paths::claude_dir`].
    pub claude_dir: Option<PathBuf>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            transport: Transport::Stdio,
            install_dir: None,
            claude_dir: None,
        }
    }
}

/// Serves until the client goes away (stdio) or Ctrl+C (HTTP).
///
/// The dirs are resolved once; the files in them are read on every call.
pub async fn serve(opts: ServeOptions) -> Result<(), Error> {
    init_logging();
    let server = ConsultServer::new(
        paths::install_dir(opts.install_dir.as_deref()),
        paths::claude_dir(opts.claude_dir.as_deref()),
    );
    match opts.transport {
        Transport::Stdio => serve_stdio(server).await,
        Transport::Http { host, port } => serve_http(server, &host, port).await,
    }
}

/// Serves MCP on stdin/stdout until stdin closes. Nothing else may write to stdout.
pub async fn serve_stdio(server: ConsultServer) -> Result<(), Error> {
    let running = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    running
        .waiting()
        .await
        .map_err(|e| Error::Transport(e.to_string()))?;
    Ok(())
}

/// Serves stateless streamable HTTP at `http://host:port/mcp` until Ctrl+C (or SIGTERM).
///
/// Stateless keeps concurrent sessions from sharing transport state: each request
/// stands alone, which is what a multi-client server needs, and a client still holding
/// a session id from before a restart is served rather than refused.
pub async fn serve_http(server: ConsultServer, host: &str, port: u16) -> Result<(), Error> {
    let shutdown = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(|e| Error::Bind(format!("{host}:{port}"), e))?;
    tracing::info!("serving MCP on http://{host}:{port}{MCP_PATH}");
    tokio::spawn(shutdown_on_signal(shutdown.clone()));
    serve_http_on(server, listener, shutdown).await
}

/// [`serve_http`] on a bound listener, until `shutdown` is cancelled.
pub async fn serve_http_on(
    server: ConsultServer,
    listener: tokio::net::TcpListener,
    shutdown: CancellationToken,
) -> Result<(), Error> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_sse_keep_alive(None)
        .with_cancellation_token(shutdown.child_token());
    let service: StreamableHttpService<ConsultServer, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(server.clone()), Default::default(), config);
    let router = axum::Router::new().nest_service(MCP_PATH, service);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
        .await
        .map_err(Error::Io)
}

async fn shutdown_on_signal(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => ctrl_c().await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c().await;
    shutdown.cancel();
}

/// Resolves on Ctrl+C. When the handler cannot be installed it never resolves: a
/// shared service must not shut itself down because a console is missing.
async fn ctrl_c() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::warn!("no Ctrl+C handler: {e}");
        std::future::pending::<()>().await;
    }
}
