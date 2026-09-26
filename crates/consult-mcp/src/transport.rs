//! Serving the server: stdio for a per-session child, stateless streamable HTTP for the
//! shared service.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use consult_core::paths;
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::server::ConsultServer;
use crate::{DETACHED_LOG_FILTER, Error, init_logging, log_filter};

/// The detached service's log, in the install's state dir.
pub const LOG_FILE: &str = "service.log";

/// Past this size the log is moved to `service.log.1` (replacing the one before) and
/// started afresh, so it never holds more than about twice this.
pub const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// `<install_dir>/state/service.log`.
pub fn log_path(install_dir: &Path) -> PathBuf {
    paths::state_dir(install_dir).join(LOG_FILE)
}

/// A log file that rotates itself once past a size: the file becomes `<name>.1` and a
/// new one is started. One generation is kept, so a crash's lines survive the restart
/// that follows it (the task restarts the service on failure) without the file growing
/// for ever.
#[derive(Debug)]
pub struct RotatingLog {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    max: u64,
}

impl RotatingLog {
    /// Opens `path` for appending, creating its directory; rotates first if it is
    /// already past `max` bytes.
    pub fn open(path: &Path, max: u64) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut log = Self {
            path: path.to_path_buf(),
            file: None,
            written: 0,
            max,
        };
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size >= max {
            log.rotate()?;
        } else {
            log.file = Some(OpenOptions::new().create(true).append(true).open(path)?);
            log.written = size;
        }
        Ok(log)
    }

    /// The previous generation: `<name>.1`.
    pub fn rotated_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Closed first: Windows will not rename a file this process holds open.
        self.file = None;
        let _ = std::fs::rename(&self.path, self.rotated_path());
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?,
        );
        self.written = 0;
        Ok(())
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written > 0 && self.written + buf.len() as u64 > self.max {
            self.rotate()?;
        }
        let file = match self.file.as_mut() {
            Some(f) => f,
            None => {
                self.rotate()?;
                self.file
                    .as_mut()
                    .ok_or_else(|| io::Error::other("log closed"))?
            }
        };
        let n = file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// A subscriber writing plain lines to `path` through a [`RotatingLog`] capped at
/// [`LOG_MAX_BYTES`].
pub fn file_subscriber(
    path: &Path,
    filter: tracing_subscriber::EnvFilter,
) -> io::Result<impl tracing::Subscriber + Send + Sync + 'static> {
    let log = RotatingLog::open(path, LOG_MAX_BYTES)?;
    Ok(tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(log))
        .with_ansi(false)
        .finish())
}

/// Detaches the service from its console, for `serve --http --detached`: gives the
/// console up (Windows; the window Task Scheduler opened for it closes) and sends all
/// logging to [`log_path`], filtered by [`DETACHED_LOG_FILTER`] or `RUST_LOG`.
/// Panics are logged there too. Returns the log's path, or `None` when it could not
/// be opened, in which case nothing is logged at all.
///
/// Call it first, before anything is written: from here on stdout and stderr lead
/// nowhere, and every error has to be reported through `tracing`.
pub fn detach(install_dir: &Path) -> Option<PathBuf> {
    free_console();
    let path = log_path(install_dir);
    let subscriber = file_subscriber(&path, log_filter(DETACHED_LOG_FILTER)).ok()?;
    tracing::subscriber::set_global_default(subscriber).ok()?;
    std::panic::set_hook(Box::new(|info| tracing::error!("panic: {info}")));
    tracing::info!(
        "claude-consult {} starting detached, pid {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
    Some(path)
}

/// Gives up the console and points the standard handles at nothing.
///
/// Nulled rather than left alone: after `FreeConsole` the old handle values are stale
/// and may be reused by some unrelated object, so a stray write could land in it.
/// With a null handle Rust's stdio treats the stream as absent and discards writes,
/// so an `eprintln!` somewhere deep cannot panic or misfire either.
#[cfg(windows)]
#[allow(unsafe_code)]
fn free_console() {
    use windows_sys::Win32::System::Console::{
        FreeConsole, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    // SAFETY: plain Win32 calls without pointers; failure (no console attached) is
    // harmless and leaves nothing to undo.
    unsafe {
        FreeConsole();
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            SetStdHandle(id, std::ptr::null_mut());
        }
    }
}

#[cfg(not(windows))]
fn free_console() {}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_logging_writes_the_file_under_state() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = log_path(dir.path());
        assert_eq!(path, dir.path().join("state").join("service.log"));
        assert!(!path.parent().expect("state").exists());
        let subscriber = file_subscriber(
            &path,
            tracing_subscriber::EnvFilter::new(DETACHED_LOG_FILTER),
        )
        .expect("open");
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("serving MCP on http://127.0.0.1:1/mcp");
            tracing::warn!(target: "rmcp::service", "response error id=listen:1");
            tracing::error!("cannot listen on 127.0.0.1:1");
        });
        let text = std::fs::read_to_string(&path).expect("log");
        assert!(text.contains("INFO"), "{text}");
        assert!(
            text.contains("serving MCP on http://127.0.0.1:1/mcp"),
            "{text}"
        );
        assert!(text.contains("cannot listen on 127.0.0.1:1"), "{text}");
        assert!(!text.contains("listen:1"), "{text}");
        // Plain text: no colour escapes in a file.
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[test]
    fn the_log_rotates_past_its_cap_keeping_one_generation() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("state").join("service.log");
        let mut log = RotatingLog::open(&path, 100).expect("open");
        let rotated = log.rotated_path();
        assert_eq!(rotated, dir.path().join("state").join("service.log.1"));
        log.write_all(&[b'a'; 60]).expect("write");
        log.write_all(&[b'b'; 60]).expect("write");
        assert_eq!(std::fs::read(&rotated).expect("old"), [b'a'; 60]);
        assert_eq!(std::fs::read(&path).expect("new"), [b'b'; 60]);
        log.write_all(&[b'c'; 60]).expect("write");
        assert_eq!(std::fs::read(&rotated).expect("old"), [b'b'; 60]);
        drop(log);

        // Reopened under the cap it appends; over the cap it rotates first.
        let mut log = RotatingLog::open(&path, 100).expect("open");
        log.write_all(b"d").expect("write");
        drop(log);
        let mut want = vec![b'c'; 60];
        want.push(b'd');
        assert_eq!(std::fs::read(&path).expect("appended"), want);
        let log = RotatingLog::open(&path, 10).expect("open");
        drop(log);
        assert_eq!(std::fs::read(&rotated).expect("old"), want);
        assert!(std::fs::read(&path).expect("new").is_empty());
    }
}
