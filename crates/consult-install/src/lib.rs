//! The claude-consult installer, upgrader and uninstaller.
//!
//! A port of the PowerShell `install.ps1`: the same questions, checks, steps and
//! messages, with the Python venv replaced by a copy of this binary in
//! `<install_dir>/bin`, which the hooks and the service run.
//!
//! # Use
//!
//! ```no_run
//! use consult_install::{InstallOptions, InstallOutcome, choose_ui, install};
//!
//! let opts = InstallOptions::default();
//! let mut ui = choose_ui(opts.unattended);
//! match install(&opts, ui.as_mut()) {
//!     Ok(InstallOutcome::Installed(_)) => {}
//!     Ok(InstallOutcome::Cancelled { exit_code }) => std::process::exit(exit_code),
//!     // Already shown to the user through the Ui.
//!     Err(e) => std::process::exit(e.exit_code()),
//! }
//! ```
//!
//! - [`install`] / [`install_with`]: install or upgrade ([`InstallOptions`] ->
//!   [`InstallOutcome`]).
//! - [`uninstall`] / [`uninstall_with`]: remove it ([`UninstallOptions`] ->
//!   [`UninstallOutcome`]).
//! - [`Ui`]: what the flows ask and tell; [`TuiUi`], [`PlainUi`], [`UnattendedUi`]
//!   implement it, [`choose_ui`] picks one.
//! - [`System`]: the machine, behind [`ServiceOps`], [`ClaudeCli`], [`Network`] and
//!   [`Host`], so everything outside the install and Claude dirs can be faked.
//!
//! The flows are synchronous; they run the few network calls on a runtime of their own,
//! so call them from outside any async runtime.
//!
//! The API key is never put on a command line, in a log or in output: the flows hand
//! the [`Ui`] only its masked form.

mod files;
mod install;
mod options;
mod plain_ui;
mod system;
mod tui_ui;
mod ui;
mod uninstall;

pub use files::{
    CopyOutcome, LEGACY_DIRS, LEGACY_FILES, LEGACY_HOOKS, Removal, base64, copy_binary,
    delete_install_dir, delete_install_dir_with, has_legacy, removal_script, remove_after_exit,
    remove_legacy,
};
pub use install::{
    InstallOutcome, Installed, McpOutcome, ServiceOutcome, foreign_status_line, install,
    install_with, mcp_add_args, mcp_add_command, mcp_remove_args,
};
pub use options::{InstallOptions, Transport, UninstallOptions, absolute, same_dir};
pub use plain_ui::{BANNER, PlainUi, UnattendedUi};
pub use system::{
    ClaudeCli, Host, Network, RealClaude, RealHost, RealNetwork, RealService, ServiceOps, System,
    find_program,
};
pub use tui_ui::TuiUi;
pub use ui::{PanelAnswer, PanelRequest, Ui, parse_panel_answer};
pub use uninstall::{UninstallOutcome, Uninstalled, is_install_dir, uninstall, uninstall_with};

/// The MCP server's name in Claude Code.
pub const MCP_NAME: &str = "openrouter";

/// Why an install or uninstall stopped.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// A refusal, as the PowerShell installer's `Stop-Install`. Already shown through
    /// [`Ui::fail`] when a flow returns it.
    #[error("{0}")]
    Stopped(String),
    /// The terminal or a prompt failed (stdin closed, say). Not shown by the flow: the
    /// caller prints it.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl InstallError {
    /// The process exit code: 1, as `Stop-Install` exited.
    pub fn exit_code(&self) -> i32 {
        1
    }
}

/// The UI for this run: [`UnattendedUi`] when asked to be unattended, the full-screen
/// [`TuiUi`] when stdin and stdout are a terminal, else [`PlainUi`] on stdin/stdout.
pub fn choose_ui(unattended: bool) -> Box<dyn Ui> {
    if unattended {
        Box::new(UnattendedUi::stdio())
    } else if consult_tui::plain::is_interactive() {
        Box::new(TuiUi::new())
    } else {
        Box::new(PlainUi::stdio())
    }
}
