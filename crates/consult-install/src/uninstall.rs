//! Uninstall: a port of the PowerShell installer's `Invoke-Uninstall`.

use std::path::PathBuf;

use consult_core::generate::{UninstallReport, uninstall_files};
use consult_core::paths::{MANIFEST_FILE, exe_name};
use consult_service::TASK_NAME;

use crate::files::{Removal, delete_install_dir};
use crate::install::{Ctx, Halt, mcp_remove_args};
use crate::options::{UninstallOptions, absolute, same_dir};
use crate::system::System;
use crate::ui::Ui;
use crate::{InstallError, MCP_NAME};

/// A finished uninstall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uninstalled {
    /// The install dir that was removed.
    pub install_dir: PathBuf,
    /// What came out of the Claude dir.
    pub report: UninstallReport,
    /// Whether this install's scheduled task was deleted.
    pub task_removed: bool,
    /// The dir another install's task serves from, which was left alone.
    pub other_task: Option<PathBuf>,
    /// Whether `claude mcp remove` succeeded.
    pub mcp_removed: bool,
    /// How the install dir went.
    pub removal: Removal,
}

/// How an uninstall ended, short of an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// Uninstalled.
    Uninstalled(Box<Uninstalled>),
    /// The user said no; nothing was changed.
    Cancelled {
        /// The process exit code to use.
        exit_code: i32,
    },
}

/// Whether `dir` looks like a claude-consult install: a manifest, the binary, or the
/// Python install's server.
pub fn is_install_dir(dir: &std::path::Path) -> bool {
    dir.join(MANIFEST_FILE).is_file()
        || dir.join("bin").join(exe_name()).is_file()
        || dir.join("bin").join("claude-consult").is_file()
        || dir.join("server.py").is_file()
}

/// Uninstalls from the real machine. See [`uninstall_with`].
pub fn uninstall(
    opts: &UninstallOptions,
    ui: &mut dyn Ui,
) -> Result<UninstallOutcome, InstallError> {
    uninstall_with(opts, ui, &System::real())
}

/// Uninstalls with the machine given.
///
/// Removes this install's scheduled task (never another install's), the MCP
/// registration (unless another install's service still uses it, or the Claude dir is
/// not the real one), the generated files and settings entries (putting back what they
/// displaced), and the install dir.
pub fn uninstall_with(
    opts: &UninstallOptions,
    ui: &mut dyn Ui,
    sys: &System,
) -> Result<UninstallOutcome, InstallError> {
    let unattended = opts.yes || ui.unattended();
    let mut cx = Ctx {
        ui,
        sys,
        unattended,
    };
    match run(opts, &mut cx) {
        Ok(done) => Ok(UninstallOutcome::Uninstalled(Box::new(done))),
        Err(Halt::Declined(exit_code)) => Ok(UninstallOutcome::Cancelled { exit_code }),
        Err(Halt::Error(e)) => Err(e),
    }
}

fn run(opts: &UninstallOptions, cx: &mut Ctx<'_>) -> Result<Uninstalled, Halt> {
    let install_dir = opts.resolved_install_dir();
    let claude_dir = opts.resolved_claude_dir();
    let real_claude = absolute(&cx.sys.host.real_claude_dir());
    let may_touch_cli = same_dir(&claude_dir, &real_claude) && !opts.skip_mcp_registration;

    cx.step("Uninstalling claude-consult");
    if !is_install_dir(&install_dir) {
        return Err(cx.stop(format!(
            "{} does not look like a claude-consult install (no manifest.json or bin/{}).",
            install_dir.display(),
            exe_name()
        )));
    }
    cx.info(&format!("Install dir : {}", install_dir.display()));
    cx.info(&format!("Claude dir  : {}", claude_dir.display()));
    if !cx.confirm(
        "Remove the service, the MCP registration, the generated commands, the hooks and the install dir?",
        true,
    )? {
        return Err(Halt::Declined(0));
    }
    let remove_key = opts.remove_key
        || cx.confirm(
            "Also delete OPENROUTER_API_KEY from Claude Code settings?",
            false,
        )?;

    let task_dir = if cx.sys.service.supported() {
        cx.sys.service.registered_dir()
    } else {
        None
    };
    let mut task_removed = false;
    let mut other_task = None;
    match task_dir {
        Some(dir) if same_dir(&dir, &install_dir) => {
            cx.ui.busy("Stopping the service");
            if let Err(e) = cx.sys.service.stop(std::slice::from_ref(&install_dir)) {
                cx.note(&format!("Could not stop the service: {e}"));
            }
            match cx.sys.service.uninstall() {
                Ok(()) => {
                    cx.ok(&format!("Removed scheduled task '{TASK_NAME}'"));
                    task_removed = true;
                }
                Err(e) => cx.note(&format!(
                    "Could not remove scheduled task '{TASK_NAME}': {e}"
                )),
            }
        }
        Some(dir) => {
            cx.note(&format!(
                "Task '{TASK_NAME}' serves from {}, not this install; leaving it alone.",
                dir.display()
            ));
            other_task = Some(dir);
        }
        None => {}
    }

    // The registration points at a port, not an install. If another install's service
    // is live, the registration is its, not ours.
    let mut mcp_removed = false;
    if other_task.is_some() {
        cx.note(&format!(
            "Leaving MCP server '{MCP_NAME}' registered: the other install still uses it."
        ));
    } else if !may_touch_cli {
        cx.note(&format!(
            "Leaving MCP server '{MCP_NAME}' registered (--skip-mcp-registration, or --claude-dir is not Claude Code's config dir)."
        ));
    } else if cx.sys.claude.available() && matches!(cx.sys.claude.run(&mcp_remove_args()), Ok(0)) {
        cx.ok(&format!("Removed MCP server '{MCP_NAME}' from Claude Code"));
        mcp_removed = true;
    }

    let report = match uninstall_files(&install_dir, &claude_dir, remove_key) {
        Ok(r) => r,
        Err(e) => {
            return Err(cx.stop(format!(
                "Removing generated files failed: {e}. {} was left in place.",
                install_dir.display()
            )));
        }
    };
    for f in &report.removed {
        cx.ok(&format!("Removed {f}"));
    }
    for f in &report.restored {
        cx.ok(&format!("Restored your original {f}"));
    }
    for s in &report.settings_removed {
        cx.ok(&format!("Removed our entry from settings.json {s}"));
    }
    if report.status_line_restored {
        cx.ok("Restored your original status line");
    }
    if report.key_removed {
        cx.ok("Removed OPENROUTER_API_KEY from settings.json");
    }

    let removal = match delete_install_dir(&install_dir, cx.sys.host.as_ref()) {
        Ok(r) => r,
        Err(e) => {
            return Err(cx.stop(format!("Could not delete {}: {e}", install_dir.display())));
        }
    };
    match removal {
        Removal::Deleted => cx.ok(&format!("Deleted {}", install_dir.display())),
        Removal::AfterExit => cx.ok(&format!(
            "Deleted {}, all but the running claude-consult, which goes as soon as it exits",
            install_dir.display()
        )),
    }
    cx.ui.done(
        "claude-consult is uninstalled. Restart open Claude Code sessions.",
        &[],
    );
    Ok(Uninstalled {
        install_dir,
        report,
        task_removed,
        other_task,
        mcp_removed,
        removal,
    })
}
