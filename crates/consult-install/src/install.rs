//! Install and upgrade: a port of the PowerShell installer's main section, step by step.
//!
//! Re-running is the upgrade path: the binary is replaced, the files regenerated, the
//! service restarted, and a Python install in the same dir migrated.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use consult_core::catalog::{self, Catalog};
use consult_core::generate::{
    Display, InstallInputs, InstallReport, PanelChoice, Templates, check_panel_choice,
    install_files, panel_keys, pick_display,
};
use consult_core::key::{
    KeyCheck, clean_key, looks_like_key, mask_key, settings_key, validate_key_format,
};
use consult_core::listing::reviewable_models_text;
use consult_core::paths::{binary_path, display_path, settings_path};
use consult_core::settings::{
    HookKind, StatusLineMode, StatusLineOutcome, hook_command, ours_test,
};
use consult_service::{
    DEFAULT_HOST, Registration, ServiceSpec, TASK_NAME, explain_interactive_fallback,
};
use consult_tui::picker::format_price;
use consult_tui::{PickerRow, StepKind, rows_from};
use indexmap::IndexMap;
use serde_json::Value;

use crate::files::{CopyOutcome, copy_binary, has_legacy, remove_legacy};
use crate::options::{InstallOptions, Transport, same_dir};
use crate::system::System;
use crate::ui::{PanelAnswer, PanelRequest, Ui};
use crate::{InstallError, MCP_NAME};

/// What happened to the service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// `--skip-service`: left alone.
    Skipped,
    /// The stdio transport needs none; `removed` when this install's task was taken out.
    NotUsed {
        /// Whether this install's old task was deleted.
        removed: bool,
    },
    /// Registered and started.
    Started {
        /// How the task was registered.
        registration: Registration,
        /// Whether the port answered within the wait.
        listening: bool,
    },
}

/// What happened to the MCP registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpOutcome {
    /// `--skip-mcp-registration`.
    Skipped,
    /// `--claude-dir` is not Claude Code's real config dir, which `claude mcp` edits.
    NotRealClaudeDir,
    /// `claude mcp add` succeeded.
    Registered,
    /// `claude mcp add` failed; the manual command was shown.
    Failed,
    /// No `claude` on PATH; the manual command was shown.
    ClaudeMissing,
}

/// A finished install.
#[derive(Clone, Debug, PartialEq)]
pub struct Installed {
    /// Where it went.
    pub install_dir: PathBuf,
    /// The Claude dir it wrote into.
    pub claude_dir: PathBuf,
    /// The installed binary.
    pub binary: PathBuf,
    /// Whether the binary was copied, or already was the installed copy.
    pub copy: CopyOutcome,
    /// What the file generator did.
    pub report: InstallReport,
    /// The slash commands now installed, without the slash.
    pub commands: Vec<String>,
    /// Files of the old Python install that were removed, relative to the install dir.
    pub legacy_removed: Vec<String>,
    /// The service.
    pub service: ServiceOutcome,
    /// The MCP registration.
    pub mcp: McpOutcome,
}

/// How an install ended, short of an error.
#[derive(Clone, Debug, PartialEq)]
pub enum InstallOutcome {
    /// Installed.
    Installed(Box<Installed>),
    /// The user said no to a question that ends the install (nothing was changed). The
    /// PowerShell installer's exit code for that question: 0 for "Proceed?", 1 for
    /// "Continue anyway?" after a warning.
    Cancelled {
        /// The process exit code to use.
        exit_code: i32,
    },
}

pub(crate) enum Halt {
    Declined(i32),
    Error(InstallError),
}

impl From<InstallError> for Halt {
    fn from(e: InstallError) -> Self {
        Self::Error(e)
    }
}

pub(crate) struct Ctx<'a> {
    pub ui: &'a mut dyn Ui,
    pub sys: &'a System,
    pub unattended: bool,
}

impl Ctx<'_> {
    pub fn step(&mut self, text: &str) {
        self.ui.report_step(StepKind::Step, text);
    }

    pub fn ok(&mut self, text: &str) {
        self.ui.report_step(StepKind::Ok, text);
    }

    pub fn note(&mut self, text: &str) {
        self.ui.report_step(StepKind::Note, text);
    }

    pub fn info(&mut self, text: &str) {
        self.ui.report_step(StepKind::Info, text);
    }

    /// `Confirm-Choice`: unattended takes the default without asking.
    pub fn confirm(&mut self, question: &str, default: bool) -> Result<bool, Halt> {
        if self.unattended {
            return Ok(default);
        }
        Ok(self.ui.confirm(question, default)?)
    }

    /// `Stop-Install`.
    pub fn stop(&mut self, message: impl Into<String>) -> Halt {
        let message = message.into();
        self.ui.fail(&message);
        Halt::Error(InstallError::Stopped(message))
    }
}

/// Installs, or upgrades an install, on the real machine. See [`install_with`].
pub fn install(opts: &InstallOptions, ui: &mut dyn Ui) -> Result<InstallOutcome, InstallError> {
    install_with(opts, ui, &System::real())
}

/// Installs, or upgrades an install, with the machine given (the tests' fakes).
///
/// Asks for the key and the panel, shows the plan, then: stops the service, copies the
/// binary to `<install_dir>/bin`, writes the commands, skill, workflow, registry,
/// display styles and settings entries, removes an old Python install's files,
/// registers and starts the service, and registers the MCP server with Claude Code.
///
/// Every refusal before the plan is confirmed leaves the machine as it was. A
/// [`InstallError::Stopped`] has already been shown through [`Ui::fail`].
pub fn install_with(
    opts: &InstallOptions,
    ui: &mut dyn Ui,
    sys: &System,
) -> Result<InstallOutcome, InstallError> {
    let unattended = opts.unattended || ui.unattended();
    let mut cx = Ctx {
        ui,
        sys,
        unattended,
    };
    match run(opts, &mut cx) {
        Ok(done) => Ok(InstallOutcome::Installed(Box::new(done))),
        Err(Halt::Declined(exit_code)) => Ok(InstallOutcome::Cancelled { exit_code }),
        Err(Halt::Error(e)) => Err(e),
    }
}

/// The listing as fetched: the raw body for the generator, when it arrived, and its
/// reviewable models by id.
struct Listing {
    body: String,
    at: DateTime<Utc>,
    models: IndexMap<String, Value>,
}

fn run(opts: &InstallOptions, cx: &mut Ctx<'_>) -> Result<Installed, Halt> {
    let install_dir = opts.resolved_install_dir();
    let claude_dir = opts.resolved_claude_dir();
    let real_claude = crate::options::absolute(&cx.sys.host.real_claude_dir());
    // `claude mcp add/remove` always edits the real Claude Code config, whatever
    // --claude-dir says. A --claude-dir test install must never reach it: a sandboxed
    // uninstall once deregistered a live server this way.
    let may_touch_cli = same_dir(&claude_dir, &real_claude) && !opts.skip_mcp_registration;
    let use_service = opts.transport == Transport::Service && !opts.skip_service;

    if !cx.ui.welcome()? {
        return Err(Halt::Declined(0));
    }

    // ---- 1. preflight ----
    cx.step("Checking prerequisites");
    if use_service && !cx.sys.service.supported() {
        return Err(cx.stop(
            "The shared service runs on Windows only. Use --transport stdio, or --skip-service.",
        ));
    }
    cx.ok(&format!("claude-consult {}", env!("CARGO_PKG_VERSION")));
    let has_claude = cx.sys.claude.available();
    if has_claude {
        cx.ok("Claude Code CLI found");
    } else if !opts.skip_mcp_registration {
        cx.note(
            "Claude Code CLI (claude) not found; the MCP server will need registering by hand.",
        );
    }
    if cx.sys.host.git_found() {
        cx.ok("git found (reviewers use read-only git)");
    } else {
        cx.note("git not found: reviewers will work without git history.");
    }
    if use_service && !same_dir(&claude_dir, &real_claude) {
        cx.note(&format!(
            "Claude dir {} is not the one Claude Code and the service read ({}).",
            claude_dir.display(),
            real_claude.display()
        ));
        cx.info("The service would not find the key. Set CLAUDE_CONFIG_DIR, or use --skip-service for a test install.");
        if !cx.confirm("Continue anyway?", false)? {
            return Err(Halt::Declined(1));
        }
    }
    let previous_dir = if opts.skip_service || !cx.sys.service.supported() {
        None
    } else {
        cx.sys.service.registered_dir()
    };
    if use_service
        && let Some(prev) = previous_dir.as_deref()
        && !same_dir(prev, &install_dir)
    {
        cx.note(&format!(
            "Task '{TASK_NAME}' currently serves consult from {}.",
            prev.display()
        ));
        cx.info(&format!(
            "This install re-points it to {}. The old folder is left untouched.",
            install_dir.display()
        ));
        if !cx.confirm("Continue?", true)? {
            return Err(Halt::Declined(0));
        }
    }

    // ---- 2. key ----
    cx.step("OpenRouter API key");
    let api_key = read_api_key(cx, opts, &claude_dir)?;

    // ---- 3. panel ----
    cx.step("Choose the review panel");
    let catalog = match catalog::embedded() {
        Ok(c) => c,
        Err(e) => return Err(cx.stop(format!("The built-in catalog is invalid: {e}"))),
    };
    cx.ui.busy("Fetching OpenRouter's model listing");
    let listing = cx.sys.network.fetch_listing().and_then(|(body, at)| {
        let models = reviewable_models_text(&body);
        (!models.is_empty()).then_some(Listing { body, at, models })
    });
    let live = listing.as_ref().map(|l| &l.models);
    let rows = rows_from(&catalog, live);
    let chosen = select_panel(cx, opts, &catalog, live, &rows)?;
    let outsiders: Vec<&String> = chosen
        .iter()
        .filter(|a| !catalog.models.contains_key(*a))
        .collect();
    let mut commands: Vec<String> = vec!["consult".into(), "cleanroom".into()];
    commands.extend(
        chosen
            .iter()
            .filter_map(|a| catalog.models.get(a).map(|m| m.command.clone())),
    );
    cx.ok(&format!("Panel: {}", chosen.join(", ")));

    let status_line = select_status_line_mode(cx, &claude_dir, &install_dir)?;

    let current = pick_display(&install_dir, opts.progress_style, opts.summary_style);
    let display = if cx.unattended {
        current
    } else {
        cx.ui.choose_display(current)?
    };

    // ---- 4. confirm ----
    let plan = plan_lines(&PlanInput {
        install_dir: &install_dir,
        claude_dir: &claude_dir,
        chosen: &chosen,
        rows: &rows,
        listed_at: listing.as_ref().map(|l| l.at),
        commands: &commands,
        outsiders: &outsiders,
        masked_key: &mask_key(&api_key),
        status_line,
        display,
        opts,
    });
    let proceed = if cx.unattended {
        cx.step("Ready to install");
        for line in &plan {
            cx.info(line);
        }
        true
    } else {
        cx.ui.confirm_plan(&plan)?
    };
    if !proceed {
        return Err(Halt::Declined(0));
    }

    // ---- 5. the binary ----
    cx.step("Installing the binary");
    let task_is_ours = previous_dir
        .as_deref()
        .is_some_and(|p| same_dir(p, &install_dir));
    if use_service || (task_is_ours && !opts.skip_service) {
        let mut dirs = vec![install_dir.clone()];
        dirs.extend(previous_dir.iter().cloned());
        cx.ui.busy("Stopping the service");
        if let Err(e) = cx.sys.service.stop(&dirs) {
            cx.note(&format!("Could not stop the service: {e}"));
        }
    }
    let binary = binary_path(&install_dir);
    let copy = match copy_binary(&opts.source_exe, &install_dir) {
        Ok(c) => c,
        Err(e) => {
            return Err(cx.stop(format!(
                "Could not copy the binary to {}: {e}",
                binary.display()
            )));
        }
    };
    match copy {
        CopyOutcome::Copied => cx.ok(&format!("Copied claude-consult to {}", binary.display())),
        CopyOutcome::AlreadyInPlace => cx.ok(&format!("{} is already in place", binary.display())),
    }

    // ---- 6. generated files ----
    cx.step("Generating Claude Code commands, skill and settings");
    let templates = Templates::embedded();
    let inputs = InstallInputs {
        catalog: &catalog,
        listing: listing.as_ref().map(|l| (l.body.as_str(), l.at)),
        panel: chosen.clone(),
        install_dir: &install_dir,
        claude_dir: &claude_dir,
        key: Some(api_key),
        progress_style: Some(display.progress),
        summary_style: Some(display.summary),
        status_line,
        templates: &templates,
        now: None,
    };
    let report = match install_files(&inputs) {
        Ok(r) => r,
        Err(e) => return Err(cx.stop(format!("Generating files failed: {e}"))),
    };
    // The panel as written: an outside model's alias is its command.
    let commands: Vec<String> = ["consult".to_string(), "cleanroom".to_string()]
        .into_iter()
        .chain(report.panel.iter().map(|a| {
            catalog
                .models
                .get(a)
                .map_or_else(|| a.clone(), |m| m.command.clone())
        }))
        .collect();
    report_files(cx, &report, &install_dir);

    let mut legacy_removed = Vec::new();
    if has_legacy(&install_dir) {
        let (removed, failed) = remove_legacy(&install_dir);
        if !removed.is_empty() {
            cx.ok(&format!(
                "Removed the Python install's files: {}",
                removed.join(", ")
            ));
        }
        for f in failed {
            cx.note(&format!("Could not remove {f}"));
        }
        legacy_removed = removed;
    }

    // ---- 7. service ----
    let service = if opts.skip_service {
        ServiceOutcome::Skipped
    } else if !use_service {
        let mut removed = false;
        if task_is_ours {
            // Claude Code now starts the server itself; a task left running this
            // install's binary would only hold it locked.
            match cx.sys.service.uninstall() {
                Ok(()) => {
                    cx.ok(&format!(
                        "Removed scheduled task '{TASK_NAME}': Claude Code starts the server over stdio now"
                    ));
                    removed = true;
                }
                Err(e) => cx.note(&format!(
                    "Could not remove scheduled task '{TASK_NAME}': {e}"
                )),
            }
        }
        ServiceOutcome::NotUsed { removed }
    } else {
        start_service(cx, opts, &install_dir, &binary)?
    };

    // ---- 8. MCP registration ----
    let mcp = register_mcp(cx, opts, has_claude, may_touch_cli, &binary);

    // ---- done ----
    let done = done_lines(&commands);
    cx.ui.done("claude-consult is installed.", &done);

    Ok(Installed {
        install_dir,
        claude_dir,
        binary,
        copy,
        report,
        commands,
        legacy_removed,
        service,
        mcp,
    })
}

// ---- key ------------------------------------------------------------------------------

fn read_api_key(
    cx: &mut Ctx<'_>,
    opts: &InstallOptions,
    claude_dir: &Path,
) -> Result<String, Halt> {
    let mut offers: Vec<(&str, String)> = Vec::new();
    let existing = settings_key(claude_dir);
    if let Some(k) = &existing {
        offers.push(("your Claude Code settings", k.clone()));
    }
    if let Some(env) = cx.sys.host.env_key()
        && existing.as_deref() != Some(env.as_str())
    {
        offers.push(("the OPENROUTER_API_KEY environment variable", env));
    }
    for (source, key) in offers {
        let question = format!("Use the key from {source} ({})?", mask_key(&key));
        if cx.confirm(&question, true)? && approve_key(cx, opts, &key)? {
            return Ok(key);
        }
    }
    if cx.unattended {
        return Err(
            cx.stop("No usable OpenRouter key. For --unattended, set OPENROUTER_API_KEY first.")
        );
    }
    cx.info("Create one at https://openrouter.ai/settings/keys - giving the key a credit");
    cx.info("limit there caps what a runaway review could ever spend.");
    loop {
        let raw = cx.ui.enter_key("OpenRouter API key (input hidden)")?;
        let key = clean_key(&raw).to_string();
        if key.is_empty() {
            cx.note("A key is required.");
            continue;
        }
        if approve_key(cx, opts, &key)? {
            return Ok(key);
        }
    }
}

/// `$1,234.50`, as PowerShell's `{0:N2}` with a dollar sign.
pub(crate) fn dollars(x: f64) -> String {
    let s = format!("{:.2}", x.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    format!("{sign}${grouped}.{frac}")
}

fn approve_key(cx: &mut Ctx<'_>, opts: &InstallOptions, key: &str) -> Result<bool, Halt> {
    // Checked first: such a key fails every request with a 401 while the service looks
    // healthy, and the generator would refuse it only after the plan was confirmed.
    if let Err(why) = validate_key_format(key) {
        cx.note(&format!("That key cannot be used: {why}."));
        return Ok(false);
    }
    if !looks_like_key(key) {
        cx.note("That does not look like an OpenRouter key (they start with 'sk-or-').");
        if !cx.confirm("Use it anyway?", false)? {
            return Ok(false);
        }
    }
    if opts.skip_key_check {
        cx.note("Not validating the key (--skip-key-check).");
        return Ok(true);
    }
    cx.ui.busy("Checking the key with OpenRouter");
    match cx.sys.network.check_key(key) {
        KeyCheck::Valid { usage, limit, .. } => {
            let limit = limit.map_or_else(|| "none set".to_string(), dollars);
            cx.ok(&format!(
                "Key accepted by OpenRouter (spent so far: {}, credit limit: {limit})",
                dollars(usage.unwrap_or(0.0))
            ));
            Ok(true)
        }
        rejected @ KeyCheck::Rejected { .. } => {
            cx.note(&format!(
                "That key {}.",
                rejected.reason().unwrap_or_default()
            ));
            Ok(false)
        }
        KeyCheck::Unreachable { reason } => {
            cx.note(&format!(
                "Could not reach OpenRouter to check the key: {reason}"
            ));
            cx.confirm("Save it without checking?", false)
        }
    }
}

// ---- panel ------------------------------------------------------------------------------

/// `Test-PanelChoice`: a refusal is noted, two seats from one lab need a yes.
pub(crate) fn check_panel(
    cx: &mut Ctx<'_>,
    catalog: &Catalog,
    chosen: &[String],
    listing: Option<&IndexMap<String, Value>>,
) -> Result<bool, Halt> {
    match check_panel_choice(catalog, chosen, listing) {
        PanelChoice::Ok => Ok(true),
        PanelChoice::SameLab { message, .. } => {
            cx.note(&message);
            cx.confirm("Keep this panel anyway?", false)
        }
        PanelChoice::Refused(message) => {
            cx.note(&message);
            Ok(false)
        }
    }
}

fn select_panel(
    cx: &mut Ctx<'_>,
    opts: &InstallOptions,
    catalog: &Catalog,
    listing: Option<&IndexMap<String, Value>>,
    rows: &[PickerRow],
) -> Result<Vec<String>, Halt> {
    let recommended = catalog.default_panel.clone();
    let budget = catalog.budget_panel.clone();
    if listing.is_none() {
        cx.note("Could not reach OpenRouter's model listing: offering the favourites only, without prices.");
    } else {
        let gone: Vec<&str> = catalog
            .models
            .keys()
            .filter(|a| !rows.iter().any(|r| &r.key == *a))
            .map(String::as_str)
            .collect();
        if !gone.is_empty() {
            cx.note(&format!(
                "No longer offered on OpenRouter with tool calling: {}",
                gone.join(", ")
            ));
        }
    }

    if let Some(entries) = opts.panel_entries() {
        let panel = panel_keys(catalog, &entries);
        if !check_panel(cx, catalog, &panel, listing)? {
            return Err(cx.stop(format!("Invalid --panel: {}", entries.join(", "))));
        }
        return Ok(panel);
    }
    if cx.unattended {
        if !check_panel(cx, catalog, &recommended, listing)? {
            return Err(cx.stop("The recommended panel is not available; pass --panel."));
        }
        return Ok(recommended);
    }

    cx.info("Three models from three different labs is the point: agreement across");
    cx.info("independent lineages is evidence, agreement within one lab much less so.");
    loop {
        let request = PanelRequest {
            catalog,
            rows,
            recommended: &recommended,
            budget: &budget,
            offline: listing.is_none(),
        };
        match cx.ui.choose_panel(&request)? {
            PanelAnswer::Picked(chosen) if !chosen.is_empty() => return Ok(chosen),
            PanelAnswer::Typed(chosen) => {
                if !chosen.is_empty() && check_panel(cx, catalog, &chosen, listing)? {
                    return Ok(chosen);
                }
            }
            PanelAnswer::Picked(_) | PanelAnswer::Cancelled => return Err(cx.stop("Cancelled.")),
        }
    }
}

// ---- status line ----------------------------------------------------------------------------

/// The user's own status line command, or `None` when there is none or it is ours (this
/// install's, another install's, or one whose program is gone: the same test the
/// generator applies, which replaces such a line in place without asking). A status
/// line without a command is `(not a command)`.
pub fn foreign_status_line(claude_dir: &Path, install_dir: &Path) -> Option<String> {
    let text = std::fs::read(settings_path(claude_dir)).ok()?;
    let text = String::from_utf8_lossy(&text);
    let data: Value = serde_json::from_str(consult_core::util::strip_bom(&text)).ok()?;
    let line = data.get("statusLine").filter(|v| !v.is_null())?;
    let cmd = line.get("command").unwrap_or(&Value::Null);
    if ours_test(install_dir, true).is_ours(cmd) {
        return None;
    }
    match cmd {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => Some("(not a command)".to_string()),
    }
}

fn select_status_line_mode(
    cx: &mut Ctx<'_>,
    claude_dir: &Path,
    install_dir: &Path,
) -> Result<StatusLineMode, Halt> {
    let Some(theirs) = foreign_status_line(claude_dir, install_dir) else {
        return Ok(StatusLineMode::Auto);
    };
    // Unattended never replaces something the user set up.
    if cx.unattended {
        return Ok(StatusLineMode::Keep);
    }
    Ok(if cx.ui.status_line_question(&theirs)? {
        StatusLineMode::Replace
    } else {
        StatusLineMode::Keep
    })
}

// ---- the plan ---------------------------------------------------------------------------------

struct PlanInput<'a> {
    install_dir: &'a Path,
    claude_dir: &'a Path,
    chosen: &'a [String],
    rows: &'a [PickerRow],
    listed_at: Option<DateTime<Utc>>,
    commands: &'a [String],
    outsiders: &'a [&'a String],
    masked_key: &'a str,
    status_line: StatusLineMode,
    display: Display,
    opts: &'a InstallOptions,
}

fn plan_lines(p: &PlanInput<'_>) -> Vec<String> {
    let mut lines = vec![
        format!("Install dir : {}", p.install_dir.display()),
        format!("Claude dir  : {}", p.claude_dir.display()),
        format!("Panel       : {}", p.chosen.join(", ")),
    ];
    match p.listed_at {
        Some(at) => {
            lines.push(format!(
                "Prices      : USD per million tokens in / out, live from OpenRouter at {} UTC",
                at.format("%H:%M")
            ));
            for a in p.chosen {
                let row = p.rows.iter().find(|r| &r.key == a);
                lines.push(format!(
                    "              {a:<36} {:>7} / {}",
                    format_price(row.and_then(|r| r.price_in)),
                    format_price(row.and_then(|r| r.price_out))
                ));
            }
        }
        None => lines.push("Prices      : unknown, OpenRouter is unreachable".to_string()),
    }
    let slashed: Vec<String> = p.commands.iter().map(|c| format!("/{c}")).collect();
    lines.push(format!(
        "Commands    : {}   (+ openrouter-workflow skill)",
        slashed.join("  ")
    ));
    if !p.outsiders.is_empty() {
        let names: Vec<&str> = p.outsiders.iter().map(|s| s.as_str()).collect();
        lines.push(format!(
            "              and one each for {}",
            names.join(", ")
        ));
    }
    lines.push("Workflow    : verify-claims, with this panel as its outside voices".to_string());
    lines.push(format!(
        "Key         : {} -> settings.json env.OPENROUTER_API_KEY",
        p.masked_key
    ));
    let status = match p.status_line {
        StatusLineMode::Replace => "consult session total, replacing yours (put back on uninstall)",
        StatusLineMode::Keep => "yours, unchanged",
        StatusLineMode::Auto => "consult session total",
    };
    lines.push(format!("Status line : {status}"));
    lines.push(format!(
        "Display     : progress {}, summary {}",
        p.display.progress, p.display.summary
    ));
    lines.push(if p.opts.skip_service {
        "Service     : skipped (--skip-service)".to_string()
    } else if p.opts.transport == Transport::Stdio {
        "Server      : started by Claude Code for each session (stdio)".to_string()
    } else {
        format!(
            "Service     : scheduled task '{TASK_NAME}' on http://127.0.0.1:{}/mcp",
            p.opts.port
        )
    });
    lines
}

// ---- after the generator ---------------------------------------------------------------------------

fn report_files(cx: &mut Ctx<'_>, report: &InstallReport, install_dir: &Path) {
    for f in &report.written {
        cx.ok(&format!("Wrote {f}"));
    }
    for f in &report.removed {
        cx.ok(&format!("Removed stale {f}"));
    }
    for f in &report.restored {
        cx.ok(&format!("Restored your original {f}"));
    }
    if !report.backed_up.is_empty() {
        cx.note(&format!(
            "Replaced files it had not generated; originals saved in {}",
            report.backup_dir.as_deref().unwrap_or_default()
        ));
    }
    if report.settings_changed.is_empty() {
        cx.ok("settings.json already up to date");
    } else {
        cx.ok(&format!(
            "Updated settings.json: {}",
            report.settings_changed.join(", ")
        ));
    }
    cx.ok(&format!(
        "Display: progress {}, summary {} ({})",
        report.display.progress,
        report.display.summary,
        display_path(install_dir).display()
    ));
    if report.status_line == StatusLineOutcome::Kept {
        cx.note(&format!(
            "Kept your status line. To add the consult total, have your script pipe its stdin to {}.",
            hook_command(install_dir, HookKind::Statusline)
        ));
    }
}

fn start_service(
    cx: &mut Ctx<'_>,
    opts: &InstallOptions,
    install_dir: &Path,
    binary: &Path,
) -> Result<ServiceOutcome, Halt> {
    cx.step("Starting the shared MCP service");
    let spec = ServiceSpec {
        exe: binary.to_path_buf(),
        working_dir: install_dir.to_path_buf(),
        port: opts.port,
        host: DEFAULT_HOST.to_string(),
    };
    let registration = match cx.sys.service.install(&spec) {
        Ok(r) => r,
        Err(e) => {
            return Err(cx.stop(format!(
                "Registering scheduled task '{TASK_NAME}' failed: {e}"
            )));
        }
    };
    match &registration {
        Registration::S4U => cx.ok(&format!(
            "Registered scheduled task '{TASK_NAME}' (starts at boot and at logon, as you)"
        )),
        Registration::InteractiveLogonOnly { reason } => {
            let text = explain_interactive_fallback(reason, install_dir);
            cx.note(&text);
        }
    }
    cx.ui.busy("Starting the service");
    let listening = match cx.sys.service.start(opts.port) {
        Ok(listening) => listening,
        Err(e) => {
            cx.note(&format!(
                "Could not start scheduled task '{TASK_NAME}': {e}"
            ));
            false
        }
    };
    if listening {
        cx.ok(&format!("Listening on 127.0.0.1:{}", opts.port));
    } else {
        cx.note(&format!(
            "Nothing is listening on port {} yet. Check: claude-consult service status",
            opts.port
        ));
    }
    Ok(ServiceOutcome::Started {
        registration,
        listening,
    })
}

/// The `claude mcp add` arguments for this transport.
pub fn mcp_add_args(transport: Transport, port: u16, binary: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", "--transport"]
        .into_iter()
        .map(String::from)
        .collect();
    match transport {
        Transport::Service => {
            args.extend(["http", "--scope", "user", MCP_NAME].map(String::from));
            args.push(format!("http://127.0.0.1:{port}/mcp"));
        }
        Transport::Stdio => {
            args.extend(["stdio", "--scope", "user", MCP_NAME, "--"].map(String::from));
            args.push(binary.display().to_string());
            args.push("serve".to_string());
        }
    }
    args
}

/// The same registration as a command to type.
pub fn mcp_add_command(transport: Transport, port: u16, binary: &Path) -> String {
    match transport {
        Transport::Service => format!(
            "claude mcp add --transport http --scope user {MCP_NAME} http://127.0.0.1:{port}/mcp"
        ),
        Transport::Stdio => format!(
            "claude mcp add --transport stdio --scope user {MCP_NAME} -- \"{}\" serve",
            binary.display()
        ),
    }
}

/// `claude mcp remove openrouter -s user`.
pub fn mcp_remove_args() -> Vec<String> {
    ["mcp", "remove", MCP_NAME, "-s", "user"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn register_mcp(
    cx: &mut Ctx<'_>,
    opts: &InstallOptions,
    has_claude: bool,
    may_touch_cli: bool,
    binary: &Path,
) -> McpOutcome {
    if opts.skip_mcp_registration {
        return McpOutcome::Skipped;
    }
    cx.step("Registering with Claude Code");
    let manual = mcp_add_command(opts.transport, opts.port, binary);
    if !may_touch_cli {
        cx.note("Skipped: --claude-dir is not Claude Code's config dir, and 'claude mcp' only edits the real one.");
        return McpOutcome::NotRealClaudeDir;
    }
    if !has_claude {
        cx.note(&format!("Run once Claude Code is installed: {manual}"));
        return McpOutcome::ClaudeMissing;
    }
    // Whatever was registered under the name before (another transport, another port)
    // goes first; a failure here only means there was nothing to remove.
    let _ = cx.sys.claude.run(&mcp_remove_args());
    let added = cx
        .sys
        .claude
        .run(&mcp_add_args(opts.transport, opts.port, binary));
    if matches!(added, Ok(0)) {
        let target = match opts.transport {
            Transport::Service => format!("http://127.0.0.1:{}/mcp", opts.port),
            Transport::Stdio => format!("{} serve (stdio)", binary.display()),
        };
        cx.ok(&format!("MCP server '{MCP_NAME}' -> {target} (user scope)"));
        McpOutcome::Registered
    } else {
        cx.note(&format!("claude mcp add failed. Run: {manual}"));
        McpOutcome::Failed
    }
}

fn done_lines(commands: &[String]) -> Vec<String> {
    let slashed: Vec<String> = commands.iter().map(|c| format!("/{c}")).collect();
    vec![
        format!("Commands : {}", slashed.join("  ")),
        "Workflow : verify-claims, one consult per claim per panel model (args.dryRun sends none)".to_string(),
        "Manage   : claude-consult manage   (the service: claude-consult service status | restart | stop)".to_string(),
        "Restart any open Claude Code sessions to pick up the new commands and server.".to_string(),
        "Change the panel or rotate the key at any time with claude-consult manage, or by".to_string(),
        "installing again. Change the display with --progress-style / --summary-style. Editing".to_string(),
        "display.json works too, except switching the summary off or on: only the installer".to_string(),
        "adds or removes its hook.".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollars_as_n2() {
        assert_eq!(dollars(0.0), "$0.00");
        assert_eq!(dollars(1.004), "$1.00");
        assert_eq!(dollars(1234.5), "$1,234.50");
        assert_eq!(dollars(1_234_567.891), "$1,234,567.89");
        assert_eq!(dollars(-3.0), "-$3.00");
    }

    #[test]
    fn mcp_commands() {
        let bin = Path::new("C:/x/bin/claude-consult.exe");
        assert_eq!(
            mcp_add_args(Transport::Service, 8766, bin).join(" "),
            "mcp add --transport http --scope user openrouter http://127.0.0.1:8766/mcp"
        );
        assert_eq!(
            mcp_add_args(Transport::Stdio, 8765, bin).join(" "),
            "mcp add --transport stdio --scope user openrouter -- C:/x/bin/claude-consult.exe serve"
        );
        assert_eq!(
            mcp_add_command(Transport::Stdio, 8765, bin),
            "claude mcp add --transport stdio --scope user openrouter -- \"C:/x/bin/claude-consult.exe\" serve"
        );
        assert_eq!(mcp_remove_args().join(" "), "mcp remove openrouter -s user");
    }
}
