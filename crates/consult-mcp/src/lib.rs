//! The claude-consult MCP server, named `openrouter`, with the tools `consult`,
//! `consult_clean` and `list_reviewers`; its progress heartbeat; and the one-shot
//! runner behind `claude-consult run`.
//!
//! The binary calls [`serve`] for `serve`, [`run`] for `run` and [`reviewers`] for
//! `reviewers`. Nothing here prints to stdout except the runner's result and the stdio
//! transport's protocol: logs go to stderr.

pub mod heartbeat;
pub mod run;
pub mod server;
pub mod transport;

use std::path::PathBuf;

pub use heartbeat::{PROGRESS_INTERVAL, PeerSink, ProgressSink, Ticker};
pub use run::{RunArgs, RunIo, reviewers, run, run_with};
pub use server::{ConsultServer, INSTRUCTIONS, SERVER_NAME};
pub use transport::{
    DEFAULT_HTTP_HOST, DEFAULT_HTTP_PORT, MCP_PATH, ServeOptions, Transport, serve, serve_http,
    serve_http_on, serve_stdio,
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

/// Sends `tracing` output to stderr at WARN, or as `RUST_LOG` says. Never stdout: on
/// the stdio transport that is the protocol channel. Does nothing when a subscriber is
/// already installed.
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
