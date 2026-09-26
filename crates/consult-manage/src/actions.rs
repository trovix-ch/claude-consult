//! What the TUI asks of the machine, behind [`Backend`], so the state machine is tested
//! against a fake. Every method may block (schtasks, the network, the install flow): the
//! app calls them on a background thread only.

use std::fmt;
use std::path::PathBuf;

use consult_core::generate::Display;
use consult_core::key::{KeyCheck, mask_key};
use consult_service::ServiceStatus;
use consult_tui::StepKind;
use indexmap::IndexMap;
use serde_json::Value;

use crate::sessions::Session;

/// Where a job's output lines go.
pub type Sink<'a> = &'a mut dyn FnMut(StepKind, &str);

/// A key held in memory. Its [`Debug`] shows it masked, so neither a log nor a panic
/// message can carry it in clear.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key itself, for the one place that must send or store it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The key as it may be shown.
    pub fn masked(&self) -> String {
        mask_key(&self.0)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({})", self.masked())
    }
}

/// Where the key the server would use comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// `OPENROUTER_API_KEY` in the environment, which wins over settings.json.
    EnvVar,
    /// The `env` block of the Claude dir's settings.json.
    Settings,
    /// Neither.
    NotFound,
}

impl KeySource {
    /// As the status screen names it.
    pub fn label(self) -> &'static str {
        match self {
            Self::EnvVar => "env var",
            Self::Settings => "settings.json",
            Self::NotFound => "NOT FOUND",
        }
    }
}

/// The key as the status screen shows it: never in clear.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// Where it comes from.
    pub source: KeySource,
    /// The key in use, masked.
    pub masked: Option<String>,
    /// A different key in settings.json that the environment's shadows, masked.
    pub shadowed: Option<String>,
}

/// One panel seat from `models.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelMember {
    /// Its alias in the registry (a favourite's alias, or an outside model's command).
    pub alias: String,
    /// Its OpenRouter id.
    pub id: Option<String>,
}

/// How Claude Code reaches this install's server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportState {
    /// The scheduled task serves this install on this port.
    Service {
        /// The port.
        port: u16,
    },
    /// The task serves another install; this one is not served by it.
    OtherInstall {
        /// The dir the task serves from.
        dir: PathBuf,
        /// Its port.
        port: u16,
    },
    /// No task for this install: Claude Code starts `claude-consult serve` itself.
    Stdio,
}

/// Everything the status screen shows.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusReport {
    /// This binary's version.
    pub version: String,
    /// The install dir.
    pub install_dir: PathBuf,
    /// The Claude dir.
    pub claude_dir: PathBuf,
    /// Whether the install dir holds an install.
    pub installed: bool,
    /// manifest.json's `generated_on`.
    pub manifest_date: Option<String>,
    /// The key.
    pub key: KeyInfo,
    /// The panel from models.json, or why it could not be read.
    pub panel: Result<Vec<PanelMember>, String>,
    /// When models.json's prices held.
    pub priced_at: Option<String>,
    /// display.json's styles.
    pub display: Display,
    /// The scheduled task, or why it could not be queried.
    pub service: Result<ServiceStatus, String>,
    /// The transport in use.
    pub transport: TransportState,
    /// The command that registers the stdio server by hand.
    pub stdio_hint: String,
}

impl StatusReport {
    /// The panel's aliases, or none when models.json could not be read.
    pub fn panel_aliases(&self) -> Vec<String> {
        self.panel
            .as_ref()
            .map(|p| p.iter().map(|m| m.alias.clone()).collect())
            .unwrap_or_default()
    }
}

/// A re-run of the install flow, keeping everything not named here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reinstall {
    /// The new panel (favourites' aliases or OpenRouter ids); `None` keeps the installed one.
    pub panel: Option<Vec<String>>,
    /// A new key; `None` keeps the one in settings.json.
    pub key: Option<Secret>,
    /// Use the new key although it does not start with `sk-or-` (the user said so).
    pub allow_unusual_key: bool,
    /// A new progress style; `None` keeps the installed one.
    pub progress: Option<consult_core::display::ProgressStyle>,
    /// A new summary style; `None` keeps the installed one.
    pub summary: Option<consult_core::display::SummaryStyle>,
}

/// What the service screen can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceAction {
    /// Run the task and wait for the port.
    Start,
    /// End the task and this install's serve processes.
    Stop,
    /// Stop, then start.
    Restart,
    /// Register (or replace) the task for this install, then start it.
    Register,
    /// Stop and delete the task.
    Unregister,
}

impl ServiceAction {
    /// In screen order.
    pub const ALL: [ServiceAction; 5] = [
        Self::Start,
        Self::Stop,
        Self::Restart,
        Self::Register,
        Self::Unregister,
    ];

    /// The menu entry.
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Stop => "Stop",
            Self::Restart => "Restart",
            Self::Register => "Re-register task",
            Self::Unregister => "Uninstall task",
        }
    }

    /// What it does, beside the entry.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Start => "run the task and wait for the port",
            Self::Stop => "end the task and this install's server processes",
            Self::Restart => "stop, then start: sessions reach the current binary",
            Self::Register => "register the task again for this install, then start it",
            Self::Unregister => "stop and delete the task (Claude Code loses the server)",
        }
    }
}

/// The catalog check's lines and exit-code-style verdict: 0 every favourite checks
/// out, 1 at least one problem, 2 nothing could be checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogVerdict {
    /// The report, one line each (`ok  ...`, `!!  ...`, the count).
    pub lines: Vec<String>,
    /// The verdict.
    pub code: i32,
}

/// The machine, as the management TUI acts on it.
///
/// Implementations must be shareable across threads: the app runs each call on a
/// thread of its own. The real one is [`crate::real::RealBackend`].
pub trait Backend: Send + Sync {
    /// A status snapshot.
    fn status(&self) -> StatusReport;
    /// OpenRouter's reviewable models by id (the picker's live rows), or `None` offline.
    fn fetch_listing(&self) -> Option<IndexMap<String, Value>>;
    /// What OpenRouter's `/key` says about a key.
    fn check_key(&self, key: &Secret) -> KeyCheck;
    /// Re-runs the install flow with `request`, reporting its lines; the headline on
    /// success, the reason on failure.
    fn reinstall(&self, request: &Reinstall, log: Sink<'_>) -> Result<String, String>;
    /// Whether this platform has the shared service.
    fn service_supported(&self) -> bool;
    /// A service action; the headline on success, the reason on failure.
    fn service_action(&self, action: ServiceAction, log: Sink<'_>) -> Result<String, String>;
    /// The catalog check.
    fn catalog_check(&self) -> CatalogVerdict;
    /// The sessions in the state dir.
    fn sessions(&self) -> Result<Vec<Session>, String>;
    /// Deletes one session's files; how many there were.
    fn delete_session(&self, id: &str) -> Result<usize, String>;
    /// Uninstalls; the headline on success, the reason on failure.
    fn uninstall(&self, remove_key: bool, log: Sink<'_>) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_is_masked() {
        let s = Secret::new("sk-or-v1-0123456789abcdef");
        let shown = format!("{s:?}");
        assert!(!shown.contains("0123456789"));
        assert!(shown.contains("cdef"));
        let r = Reinstall {
            key: Some(s),
            ..Reinstall::default()
        };
        assert!(!format!("{r:?}").contains("0123456789"));
    }
}
