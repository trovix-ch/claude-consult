//! The claude-consult management TUI, run by `claude-consult manage`.
//!
//! Eight screens over one install: Status, Panel, Key, Display, Sessions, Catalog,
//! Service and Uninstall. The TUI owns no logic of its own: a change re-runs the install
//! flow of [`consult_install`] with the answers settled on screen, service actions go
//! through [`consult_service`], reads through [`consult_core`].
//!
//! - [`app`]: the state machine ([`App`], [`Screen`]), driven by keys and job results.
//! - [`ui`]: drawing, a pure function of the state.
//! - [`actions`]: the [`Backend`] trait every job goes through, and its types.
//! - [`real`]: the [`Backend`] on the real machine.
//! - [`sessions`]: the per-session totals in `state/`.
//!
//! ```no_run
//! let code = consult_manage::run(consult_manage::ManageOptions::default())?;
//! std::process::exit(code);
//! # Ok::<(), consult_manage::ManageError>(())
//! ```

pub mod actions;
pub mod app;
pub mod real;
pub mod sessions;
pub mod ui;

#[cfg(test)]
mod tests;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use consult_core::paths;
use consult_install::absolute;
use consult_tui::{Terminal, map_key};
use ratatui::crossterm::event::{self, Event};

pub use actions::Backend;
pub use app::{App, Screen};
pub use real::RealBackend;

/// What `claude-consult manage` takes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManageOptions {
    /// The install dir; `None` resolves as [`paths::install_dir`] does.
    pub install_dir: Option<PathBuf>,
    /// Claude Code's config dir; `None` resolves as [`paths::claude_dir`] does.
    pub claude_dir: Option<PathBuf>,
}

/// Why the TUI could not run.
#[derive(Debug, thiserror::Error)]
pub enum ManageError {
    /// stdin or stdout is not a terminal: a full-screen UI cannot run there.
    #[error("claude-consult manage needs a terminal (stdin and stdout must not be redirected)")]
    NotATerminal,
    /// The terminal failed.
    #[error("terminal error: {0}")]
    Io(#[from] io::Error),
}

/// Runs the management TUI until the user quits (or uninstalls); the process exit code.
///
/// Must not be called inside an async runtime: the install flow it runs builds its own.
pub fn run(opts: ManageOptions) -> Result<i32, ManageError> {
    if !consult_tui::plain::is_interactive() {
        return Err(ManageError::NotATerminal);
    }
    let install_dir = absolute(&paths::install_dir(opts.install_dir.as_deref()));
    let claude_dir = absolute(&paths::claude_dir(opts.claude_dir.as_deref()));
    let backend = Arc::new(RealBackend::new(install_dir, claude_dir));
    let mut app = App::new(backend);
    let farewell = {
        let mut term = Terminal::enter()?;
        loop {
            let size = term.inner().size()?;
            app.resize(size.height);
            term.draw(|f| ui::draw(f, &app))?;
            if app.should_quit() {
                break;
            }
            // Polled, not blocking: job results and the spinner move between keys.
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
                && let Some(key) = map_key(k)
            {
                app.handle_key(key);
            }
            app.tick();
        }
        app.uninstall.done.then(|| app.uninstall.log.clone())
    };
    // The uninstall's report outlives the alternate screen, as the installer's does.
    if let Some(log) = farewell {
        for (kind, line) in log.entries() {
            consult_tui::plain::print_step(*kind, line);
        }
    }
    Ok(app.exit_code())
}
