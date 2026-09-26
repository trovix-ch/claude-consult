//! The claude-consult MCP server, named `openrouter`, with the tools `consult`,
//! `consult_clean` and `list_reviewers`; its progress heartbeat; and the one-shot
//! runner behind `claude-consult run`.
//!
//! The binary calls [`serve`] for `serve`, [`run`] for `run` and [`reviewers`] for
//! `reviewers`. Nothing here prints to stdout except the runner's result and the stdio
//! transport's protocol: logs go to stderr, or for a detached service ([`detach`]) to
//! `state/service.log` in the install dir.

pub mod heartbeat;
pub mod run;
pub mod server;
pub mod transport;

use std::path::PathBuf;

pub use heartbeat::{PROGRESS_INTERVAL, PeerSink, ProgressSink, Ticker};
pub use run::{RunArgs, RunIo, reviewers, run, run_with};
pub use server::{ConsultServer, INSTRUCTIONS, SERVER_NAME};
pub use transport::{
    DEFAULT_HTTP_HOST, DEFAULT_HTTP_PORT, LOG_FILE, LOG_MAX_BYTES, MCP_PATH, RotatingLog,
    ServeOptions, Transport, detach, file_subscriber, log_path, serve, serve_http, serve_http_on,
    serve_stdio,
};

/// Why serving or a one-shot run could not go on. A consult that fails is not an error
/// here: the tools return its failure as text and the runner as exit code 1.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No question on the command line or in a file, and stdin is a terminal.
    #[error("provide --question, --question-file, or pipe the question on stdin")]
    NoQuestion,
    /// The question file could not be read.
    #[error("{}: {source}", path.display())]
    QuestionFile {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP address could not be bound.
    #[error("cannot listen on {0}: {1}")]
    Bind(String, #[source] std::io::Error),
    /// The MCP transport failed.
    #[error("MCP transport failed: {0}")]
    Transport(String),
    /// Reading or writing a stream failed.
    #[error(transparent)]
    Io(std::io::Error),
    /// The result could not be serialised.
    #[error(transparent)]
    Json(serde_json::Error),
}

/// The filter when `RUST_LOG` is unset: WARN, except rmcp's service loop at ERROR.
///
/// rmcp logs every error response it sends at WARN, and Claude Code sends requests
/// this server does not implement (`subscriptions/listen`) many times a second. The
/// method-not-found answer is correct; logging each one is noise. A server-side hook
/// cannot help: rmcp logs whatever error a handler returns before sending it.
pub const LOG_FILTER: &str = "warn,rmcp::service=error";

/// [`LOG_FILTER`] plus this crate's INFO lines, for the detached service's log file:
/// the startup line is the one sign in the file that the service came up.
pub const DETACHED_LOG_FILTER: &str = "warn,rmcp::service=error,consult_mcp=info";

/// `RUST_LOG` when it is set and parses, else `default`.
pub fn log_filter(default: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default))
}

/// Sends `tracing` output to stderr filtered by [`LOG_FILTER`], or as `RUST_LOG` says.
/// Never stdout: on the stdio transport that is the protocol channel. Does nothing when
/// a subscriber is already installed (the detached service's file logger).
pub fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(log_filter(LOG_FILTER))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn logged(filter: &str) -> String {
        let buf = Buf::default();
        let out = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_writer(move || out.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(target: "rmcp::service", "rmcp-service-warn");
            tracing::error!(target: "rmcp::service", "rmcp-service-error");
            tracing::warn!(target: "rmcp::transport", "rmcp-transport-warn");
            tracing::warn!(target: "consult_mcp::server", "ours-warn");
            tracing::info!(target: "consult_mcp::transport", "ours-info");
            tracing::info!(target: "rmcp::service", "rmcp-service-info");
        });
        String::from_utf8(buf.0.lock().expect("lock").clone()).expect("utf8")
    }

    #[test]
    fn the_default_filter_quiets_only_rmcps_service_loop() {
        assert_eq!(LOG_FILTER, "warn,rmcp::service=error");
        assert!(
            tracing_subscriber::EnvFilter::builder()
                .parse(LOG_FILTER)
                .is_ok()
        );
        let text = logged(LOG_FILTER);
        assert!(!text.contains("rmcp-service-warn"), "{text}");
        assert!(text.contains("rmcp-service-error"), "{text}");
        assert!(text.contains("rmcp-transport-warn"), "{text}");
        assert!(text.contains("ours-warn"), "{text}");
        assert!(!text.contains("ours-info"), "{text}");
    }

    #[test]
    fn the_detached_filter_adds_our_info_lines_only() {
        assert!(
            tracing_subscriber::EnvFilter::builder()
                .parse(DETACHED_LOG_FILTER)
                .is_ok()
        );
        let text = logged(DETACHED_LOG_FILTER);
        assert!(text.contains("ours-info"), "{text}");
        assert!(!text.contains("rmcp-service-info"), "{text}");
        assert!(!text.contains("rmcp-service-warn"), "{text}");
        assert!(text.contains("rmcp-service-error"), "{text}");
    }
}
