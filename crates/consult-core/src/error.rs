//! The error types shared across modules.

use std::path::PathBuf;

use crate::sandbox::SandboxError;

/// Why a consult could not run, or why one reviewer's request failed.
///
/// `Display` already carries the Python exception name where the Python server added
/// it (`"SandboxError: ..."`), so a tool reports `format!("Consult failed: {e}")` and
/// gets the Python text: a plain `ConsultError` shows its message alone.
#[derive(Debug, thiserror::Error)]
pub enum ConsultError {
    /// What the Python code raised as `ConsultError`.
    #[error("{0}")]
    Message(String),
    /// The review root was refused.
    #[error("SandboxError: {0}")]
    Sandbox(#[from] SandboxError),
    /// Anything else, named the way Python would name it.
    #[error("{kind}: {message}")]
    Other {
        /// The exception name Python would show, e.g. `FileNotFoundError`.
        kind: &'static str,
        /// The message.
        message: String,
    },
}

impl ConsultError {
    /// A plain message, as Python's `ConsultError(message)`.
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    /// The error as Python's `f"{type(e).__name__}: {e}"`, used where a reviewer's
    /// result records what stopped it.
    pub fn typed(&self) -> String {
        match self {
            Self::Message(m) => format!("ConsultError: {m}"),
            other => other.to_string(),
        }
    }
}

/// Why the install plan (catalog, panel, templates, settings) cannot go ahead.
///
/// Every check runs before anything is written, so this error means nothing changed
/// unless it is an [`GenerateError::Io`] raised while writing.
#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    /// A refusal with a message for the user.
    #[error("{0}")]
    Invalid(String),
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: std::io::Error,
    },
}

impl GenerateError {
    /// A refusal with this message.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// An I/O failure on this path.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
