//! `claude-consult`: the one binary. A thin clap front end over the library crates;
//! every decision lives in them.
//!
//! Only `serve`, `run`, `reviewers` and `check-catalog` get a tokio runtime. The
//! installer runs one of its own inside, and a runtime cannot be started from within
//! another, so `install` and the other synchronous commands must stay outside one.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context;
use clap::builder::PossibleValue;
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use consult_core::display::{ProgressStyle, SummaryStyle};
use consult_core::panel::{DEFAULT_MAX_STEPS, MAX_REVIEWER_COST_USD};
use consult_core::paths;
use consult_hooks::HookKind;
use consult_install::{InstallError, InstallOptions, InstallOutcome, UninstallOptions};
use consult_service::{DEFAULT_PORT, Registration, ServiceSpec, TASK_NAME};

/// Ask models from other labs, through OpenRouter, for an outside opinion from inside
/// Claude Code.
#[derive(Debug, Parser)]
#[command(name = "claude-consult", bin_name = "claude-consult", version, about)]
struct Cli {
    /// The install dir [default: $CLAUDE_CONSULT_DIR, else the dir this binary is
    /// installed in, else the per-user default].
    // Shown after each subcommand's own options, not interleaved with them.
    #[arg(long, global = true, value_name = "DIR", display_order = 900)]
    install_dir: Option<PathBuf>,

    /// Claude Code's config dir [default: $CLAUDE_CONFIG_DIR, else ~/.claude].
    #[arg(long, global = true, value_name = "DIR", display_order = 901)]
    claude_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install or upgrade: the binary, the commands, the hooks and the MCP server.
    Install(InstallArgs),
    /// Remove everything the install wrote.
    Uninstall(UninstallArgs),
    /// The management TUI: status, panel, key, display styles, service.
    Manage,
    /// Serve the MCP server (stdio unless --http).
    Serve(ServeArgs),
    /// Consult once and print the result, instead of serving MCP.
    Run(RunCli),
    /// Print the registered reviewers, as the list_reviewers tool shows them.
    Reviewers,
    /// Manage the shared service (a scheduled task; Windows only).
    Service(ServiceArgs),
    /// Run a Claude Code hook; the event JSON arrives on stdin.
    Hook {
        /// Which hook.
        #[arg(value_enum)]
        kind: HookArg,
    },
    /// Check the curated favourites against OpenRouter's live listing (no key, no cost).
    ///
    /// Exit codes: 0 every favourite checks out; 1 at least one problem; 2 the listing
    /// could not be fetched or read, so nothing was checked.
    CheckCatalog {
        /// Print problems only.
        #[arg(long)]
        quiet: bool,
    },
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// No prompts: the key from OPENROUTER_API_KEY or settings.json, the recommended
    /// panel unless --panel, a foreign status line kept.
    #[arg(long)]
    unattended: bool,
    /// The panel: favourites' aliases or OpenRouter ids, comma-separated.
    #[arg(long, value_name = "A,B,C")]
    panel: Vec<String>,
    /// The shared service's port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// Do not check the key against OpenRouter.
    #[arg(long)]
    skip_key_check: bool,
    /// Leave the scheduled task alone.
    #[arg(long)]
    skip_service: bool,
    /// When start-at-boot needs administrator rights, keep the logon-only task
    /// instead of bringing up the Windows administrator prompt.
    #[arg(long)]
    no_elevate: bool,
    /// Do not run `claude mcp add`.
    #[arg(long)]
    skip_mcp_registration: bool,
    /// How a running consult reports progress [default: keep the installed style].
    #[arg(long, value_enum, value_name = "STYLE")]
    progress_style: Option<ProgressArg>,
    /// How the summary under Claude's reply is drawn [default: keep the installed
    /// style].
    #[arg(long, value_enum, value_name = "STYLE")]
    summary_style: Option<SummaryArg>,
    /// How Claude Code reaches the server [default: service on Windows, stdio elsewhere].
    #[arg(long, value_enum)]
    transport: Option<TransportArg>,
}

#[derive(Debug, Args)]
struct UninstallArgs {
    /// Also delete OPENROUTER_API_KEY from Claude Code's settings.json.
    #[arg(long)]
    remove_key: bool,
    /// Ask nothing; every question takes its default (the key stays unless
    /// --remove-key).
    #[arg(long)]
    yes: bool,
    /// Leave the MCP registration alone.
    #[arg(long)]
    skip_mcp_registration: bool,
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// Serve stateless streamable HTTP at http://HOST:PORT/mcp instead of stdio.
    #[arg(long)]
    http: bool,
    /// The address to bind.
    #[arg(long, requires = "http", default_value = consult_mcp::DEFAULT_HTTP_HOST)]
    host: String,
    /// The port to bind.
    #[arg(long, requires = "http", default_value_t = consult_mcp::DEFAULT_HTTP_PORT)]
    port: u16,
    /// Run as the background service: give up the console (Windows) and log to
    /// state/service.log in the install dir instead of stderr. The scheduled task
    /// passes it.
    #[arg(long, requires = "http")]
    detached: bool,
}

#[derive(Debug, Args)]
struct RunCli {
    /// The project the reviewers may read.
    #[arg(long, default_value = ".")]
    root: String,
    /// The plan or question.
    #[arg(long)]
    question: Option<String>,
    /// Read the question from a file instead.
    #[arg(long, value_name = "FILE")]
    question_file: Option<PathBuf>,
    /// Comma-separated aliases, command names or OpenRouter ids.
    #[arg(long)]
    models: Option<String>,
    /// Comma-separated project-relative files to include up front.
    #[arg(long)]
    attach: Option<String>,
    /// Tool-calling steps per reviewer.
    #[arg(long, default_value_t = DEFAULT_MAX_STEPS)]
    max_steps: usize,
    /// Per-reviewer spend ceiling in USD.
    #[arg(long, default_value_t = MAX_REVIEWER_COST_USD)]
    max_cost: f64,
    /// Clean-room: no project context, no tools, no repo access.
    #[arg(long)]
    clean: bool,
    /// Review a plan/idea, or diagnose an unexplained problem.
    #[arg(long, value_enum, default_value_t = ModeArg::Review)]
    mode: ModeArg,
    /// Emit raw JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ServiceArgs {
    /// What to do.
    #[arg(value_enum)]
    action: ServiceAction,
    /// The port [default: the registered task's port, else 8765].
    #[arg(long)]
    port: Option<u16>,
    /// install: when start-at-boot is refused for want of administrator rights, go to
    /// the Windows administrator prompt even without a terminal (interactive runs do
    /// so anyway).
    #[arg(long, conflicts_with = "no_elevate")]
    elevate: bool,
    /// install: never bring up the administrator prompt; keep the logon-only task.
    #[arg(long)]
    no_elevate: bool,
    /// install: register for start-at-boot (S4U) or fail; never fall back to the
    /// logon-only task. What the elevated copy runs.
    #[arg(long)]
    no_fallback: bool,
    /// install: register the task for this account (DOMAIN\user) instead of the
    /// current one. The elevated copy is given the caller's, since its own
    /// environment names whoever answered the administrator prompt.
    #[arg(long, value_name = "DOMAIN\\USER", hide = true)]
    run_as: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ServiceAction {
    /// Register the task for this install's binary (replacing any other).
    Install,
    /// Run the task and wait for the port.
    Start,
    /// End the task and its serve processes.
    Stop,
    /// Stop, then start: sessions reach the current binary.
    Restart,
    /// Show the task, the port, the key and the processes.
    Status,
    /// Stop and delete the task.
    Uninstall,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HookArg {
    /// PostToolUse on consult: tallies the call and parks its summary.
    Summary,
    /// MessageDisplay: draws the parked summary under the reply.
    Display,
    /// The status line: this session's consult total.
    Statusline,
}

impl From<HookArg> for HookKind {
    fn from(h: HookArg) -> Self {
        match h {
            HookArg::Summary => Self::Summary,
            HookArg::Display => Self::Display,
            HookArg::Statusline => Self::Statusline,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    Review,
    Diagnose,
}

impl ModeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Diagnose => "diagnose",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TransportArg {
    /// One shared service (a scheduled task) for every session. Windows only.
    Service,
    /// Claude Code starts `claude-consult serve` per session.
    Stdio,
}

impl From<TransportArg> for consult_install::Transport {
    fn from(t: TransportArg) -> Self {
        match t {
            TransportArg::Service => Self::Service,
            TransportArg::Stdio => Self::Stdio,
        }
    }
}

// The style enums live in core, which has no clap; the names and their order come from
// there so the flags can never offer a style core does not know.

#[derive(Clone, Copy, Debug)]
struct ProgressArg(ProgressStyle);

static PROGRESS_ARGS: LazyLock<Vec<ProgressArg>> =
    LazyLock::new(|| ProgressStyle::ALL.into_iter().map(ProgressArg).collect());

impl ValueEnum for ProgressArg {
    fn value_variants<'a>() -> &'a [Self] {
        &PROGRESS_ARGS
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.0.as_str()))
    }
}

#[derive(Clone, Copy, Debug)]
struct SummaryArg(SummaryStyle);

static SUMMARY_ARGS: LazyLock<Vec<SummaryArg>> =
    LazyLock::new(|| SummaryStyle::ALL.into_iter().map(SummaryArg).collect());

impl ValueEnum for SummaryArg {
    fn value_variants<'a>() -> &'a [Self] {
        &SUMMARY_ARGS
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.0.as_str()))
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    let install_dir = cli.install_dir;
    let claude_dir = cli.claude_dir;
    match cli.command {
        Command::Install(a) => Ok(install(a, install_dir, claude_dir)),
        Command::Uninstall(a) => Ok(uninstall(a, install_dir, claude_dir)),
        Command::Manage => manage(install_dir, claude_dir),
        Command::Hook { kind } => Ok(consult_hooks::run(kind.into(), install_dir.as_deref())),
        Command::Service(a) => service(a, install_dir.as_deref(), claude_dir.as_deref()),
        Command::Serve(a) => serve(a, install_dir, claude_dir),
        Command::Run(a) => run(a, install_dir, claude_dir),
        Command::Reviewers => {
            let table = block_on(consult_mcp::reviewers(
                install_dir.as_deref(),
                claude_dir.as_deref(),
            ))?;
            println!("{table}");
            Ok(0)
        }
        Command::CheckCatalog { quiet } => check_catalog(quiet),
    }
}

fn serve(
    a: ServeArgs,
    install_dir: Option<PathBuf>,
    claude_dir: Option<PathBuf>,
) -> anyhow::Result<i32> {
    // Detaching comes before anything else is written: from here on stderr leads
    // nowhere, so every failure below is logged and returned as an exit code, never
    // handed back to main to print.
    let detached = a.detached;
    if detached {
        // Without a log file there is nowhere left to report to; serve regardless.
        let _ = consult_mcp::detach(&paths::install_dir(install_dir.as_deref()));
    }
    let transport = if a.http {
        consult_mcp::Transport::Http {
            host: a.host,
            port: a.port,
        }
    } else {
        consult_mcp::Transport::Stdio
    };
    let opts = consult_mcp::ServeOptions {
        transport,
        install_dir,
        claude_dir,
    };
    let result = block_on(consult_mcp::serve(opts)).and_then(|r| r.context("serve"));
    match result {
        Ok(()) => Ok(0),
        Err(e) if detached => {
            tracing::error!("{e:#}");
            Ok(1)
        }
        Err(e) => Err(e),
    }
}

/// Runs `fut` on a multi-thread runtime made for it, and leaves without waiting on
/// blocking work still parked in the runtime (a stdin read that will never return).
fn block_on<F: std::future::Future>(fut: F) -> anyhow::Result<F::Output> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start the async runtime")?;
    let out = rt.block_on(fut);
    rt.shutdown_timeout(Duration::from_millis(250));
    Ok(out)
}

fn install(a: InstallArgs, install_dir: Option<PathBuf>, claude_dir: Option<PathBuf>) -> i32 {
    let defaults = InstallOptions::default();
    let opts = InstallOptions {
        unattended: a.unattended,
        panel: (!a.panel.is_empty()).then_some(a.panel),
        install_dir,
        claude_dir,
        port: a.port,
        skip_key_check: a.skip_key_check,
        skip_service: a.skip_service,
        no_elevate: a.no_elevate,
        skip_mcp_registration: a.skip_mcp_registration,
        progress_style: a.progress_style.map(|p| p.0),
        summary_style: a.summary_style.map(|s| s.0),
        transport: a.transport.map_or(defaults.transport, Into::into),
        source_exe: defaults.source_exe,
    };
    let mut ui = consult_install::choose_ui(opts.unattended);
    let outcome = consult_install::install(&opts, ui.as_mut());
    // The UI goes first: a full-screen one gives the terminal back when dropped, and
    // anything printed before that would be lost with the alternate screen.
    drop(ui);
    match outcome {
        Ok(InstallOutcome::Installed(_)) => 0,
        Ok(InstallOutcome::Cancelled { exit_code }) => exit_code,
        Err(e) => install_failed(e),
    }
}

fn uninstall(a: UninstallArgs, install_dir: Option<PathBuf>, claude_dir: Option<PathBuf>) -> i32 {
    let opts = UninstallOptions {
        install_dir,
        claude_dir,
        remove_key: a.remove_key,
        yes: a.yes,
        skip_mcp_registration: a.skip_mcp_registration,
    };
    let mut ui = consult_install::choose_ui(a.yes);
    let outcome = consult_install::uninstall(&opts, ui.as_mut());
    drop(ui);
    match outcome {
        Ok(consult_install::UninstallOutcome::Uninstalled(_)) => 0,
        Ok(consult_install::UninstallOutcome::Cancelled { exit_code }) => exit_code,
        Err(e) => install_failed(e),
    }
}

fn install_failed(e: InstallError) -> i32 {
    // A Stopped error was already shown through the UI; an I/O one was not.
    if let InstallError::Io(io) = &e {
        eprintln!("error: {io}");
    }
    e.exit_code()
}

fn manage(install_dir: Option<PathBuf>, claude_dir: Option<PathBuf>) -> anyhow::Result<i32> {
    Ok(consult_manage::run(consult_manage::ManageOptions {
        install_dir,
        claude_dir,
    })?)
}

fn run(
    a: RunCli,
    install_dir: Option<PathBuf>,
    claude_dir: Option<PathBuf>,
) -> anyhow::Result<i32> {
    let args = consult_mcp::RunArgs {
        root: a.root,
        question: a.question,
        question_file: a.question_file,
        models: a.models,
        attach: a.attach,
        max_steps: a.max_steps,
        max_cost: a.max_cost,
        clean: a.clean,
        mode: a.mode.as_str().to_string(),
        json: a.json,
        install_dir,
        claude_dir,
    };
    match block_on(consult_mcp::run(args))? {
        Ok(code) => Ok(code),
        // A usage error, as argparse reported it: the usage line and exit code 2.
        Err(e @ consult_mcp::Error::NoQuestion) => {
            let mut cmd = Cli::command();
            cmd.build();
            let err = match cmd.find_subcommand_mut("run") {
                Some(sub) => sub.error(ErrorKind::MissingRequiredArgument, e),
                None => Cli::command().error(ErrorKind::MissingRequiredArgument, e),
            };
            let _ = err.print();
            Ok(err.exit_code())
        }
        Err(e) => Err(e.into()),
    }
}

fn check_catalog(quiet: bool) -> anyhow::Result<i32> {
    let catalog = consult_core::catalog::embedded().context("the embedded catalog")?;
    let body = block_on(async {
        let client = consult_core::openrouter::Client::new();
        consult_core::catalog::fetch_full_listing(&client).await
    })?;
    let outcome = body
        .map_err(|e| e.to_string())
        .and_then(|b| consult_core::catalog::check_catalog(&catalog, &b));
    let (report, code) = consult_core::catalog::check_report(&catalog, outcome, quiet);
    if !report.is_empty() {
        println!("{report}");
    }
    Ok(code)
}

fn service(
    a: ServiceArgs,
    install_dir: Option<&Path>,
    claude_dir: Option<&Path>,
) -> anyhow::Result<i32> {
    let dir = consult_install::absolute(&paths::install_dir(install_dir));
    // Everything talks to the port the task was registered with, or a non-default
    // install reports "not listening" while running fine.
    let port = a
        .port
        .or_else(consult_service::registered_port)
        .unwrap_or(DEFAULT_PORT);
    match a.action {
        ServiceAction::Install => {
            // Interactive runs go to the administrator prompt by themselves; --elevate
            // does so without a terminal too, --no-elevate never.
            let may_elevate = a.elevate || (!a.no_elevate && is_interactive());
            service_install(&dir, port, a.no_fallback, may_elevate, a.run_as)
        }
        ServiceAction::Start => {
            service_start(port)?;
            Ok(0)
        }
        ServiceAction::Stop => {
            service_stop(&dir, true)?;
            println!("service stopped");
            Ok(0)
        }
        ServiceAction::Restart => {
            service_stop(&dir, false)?;
            std::thread::sleep(Duration::from_millis(500));
            service_start(port)?;
            println!("restarted - sessions now reach the current binary");
            Ok(0)
        }
        ServiceAction::Status => {
            service_status(&dir, a.port, claude_dir)?;
            Ok(0)
        }
        ServiceAction::Uninstall => {
            service_stop(&dir, false)?;
            consult_service::uninstall()?;
            println!("unregistered '{TASK_NAME}'");
            Ok(0)
        }
    }
}

fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn service_install(
    dir: &Path,
    port: u16,
    no_fallback: bool,
    may_elevate: bool,
    run_as: Option<String>,
) -> anyhow::Result<i32> {
    let spec = ServiceSpec {
        exe: paths::binary_path(dir),
        working_dir: dir.to_path_buf(),
        port,
        host: consult_service::DEFAULT_HOST.to_string(),
        account: run_as,
    };
    let account = consult_service::task_account(&spec)?;
    let installed = || {
        println!(
            "installed '{TASK_NAME}' as {account} (S4U) - starts at boot and at logon, port {port}"
        );
    };
    if no_fallback {
        // The elevated copy: S4U or an error, and nothing to ask.
        consult_service::install_s4u(&spec)?;
        installed();
        return Ok(0);
    }
    let reason = match consult_service::install(&spec)? {
        Registration::S4U => {
            installed();
            return Ok(0);
        }
        Registration::InteractiveLogonOnly { reason } => reason,
    };
    let explain = || {
        println!(
            "{}",
            consult_service::explain_interactive_fallback(&reason, dir)
        );
    };
    // Already elevated and still refused: another prompt would change nothing.
    if !may_elevate || consult_service::is_elevated() {
        explain();
        return Ok(0);
    }
    // The running binary, never the copy in bin/: that may be an older build that does
    // not know the flags the elevated run is given. The task still runs the copy.
    let exe = std::env::current_exe().context("cannot tell which binary is running")?;
    println!("{}", consult_service::ELEVATE_NOTICE);
    match consult_service::install_elevated(&spec, &exe) {
        Ok(()) => {}
        Err(consult_service::ServiceError::ElevationCancelled) => {
            println!("The administrator prompt was cancelled; keeping the logon-only task.");
            explain();
            return Ok(0);
        }
        Err(e) => {
            println!("{e}");
            explain();
            return Ok(0);
        }
    }
    match consult_service::registered() {
        Some(task) if task.is_s4u() => {
            let who = task.runs_as().unwrap_or_else(|| account.clone());
            println!("[ok] {}", consult_service::elevated_success(&who));
        }
        _ => explain(),
    }
    Ok(0)
}

fn service_start(port: u16) -> anyhow::Result<()> {
    if consult_service::start(port)? {
        println!("listening on 127.0.0.1:{port}");
    } else {
        println!("task started but port {port} is not listening - check 'status'");
    }
    Ok(())
}

/// Ends the task and kills the serve processes of this install and of the one the
/// task serves from, and no others.
fn service_stop(dir: &Path, report: bool) -> anyhow::Result<()> {
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(task_dir) = consult_service::registered_dir()
        && !consult_install::same_dir(&task_dir, dir)
    {
        dirs.push(task_dir);
    }
    let stopped = consult_service::stop(&dirs)?;
    if report {
        for pid in stopped {
            println!("stopped pid {pid}");
        }
    }
    Ok(())
}

fn service_status(dir: &Path, port: Option<u16>, claude_dir: Option<&Path>) -> anyhow::Result<()> {
    let s = consult_service::status(port)?;
    let yes_no = |b: bool| if b { "True" } else { "False" };
    println!("root            : {}", dir.display());
    if !s.supported {
        println!("task registered : False (the shared service runs on Windows only)");
    } else {
        println!("task registered : {}", yes_no(s.registered));
    }
    if s.registered {
        println!(
            "task state      : {}",
            s.state.as_deref().unwrap_or("unknown")
        );
        println!(
            "runs as         : {} ({})",
            s.runs_as.as_deref().unwrap_or("unknown"),
            s.logon_type.as_deref().unwrap_or("unknown")
        );
        println!("triggers        : {}", s.triggers.join(", "));
        if let Some(wd) = &s.working_dir
            && !consult_install::same_dir(wd, dir)
        {
            println!("serves from     : {}", wd.display());
        }
    }
    println!("listening {} : {}", s.port, yes_no(s.listening));
    println!("api key via     : {}", key_source(claude_dir));
    println!("processes       : {}", s.processes.len());
    for p in &s.processes {
        println!(
            "  pid {}  RSS {:.1} MB",
            p.pid,
            p.rss_bytes as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

/// Where the key would be found, as the server looks for it. The failure this catches:
/// service up, port listening, every consult failing because the key is not reachable.
fn key_source(claude_dir: Option<&Path>) -> &'static str {
    let env = std::env::var(consult_core::key::KEY_VAR).unwrap_or_default();
    if !consult_core::key::clean_key(&env).is_empty() {
        "env var"
    } else if consult_core::key::settings_key(&paths::claude_dir(claude_dir)).is_some() {
        "settings.json"
    } else {
        "NOT FOUND"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn style_flags_offer_every_core_style() {
        let names: Vec<String> = ProgressArg::value_variants()
            .iter()
            .filter_map(ValueEnum::to_possible_value)
            .map(|p| p.get_name().to_string())
            .collect();
        let core: Vec<&str> = ProgressStyle::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, core);
        let names: Vec<String> = SummaryArg::value_variants()
            .iter()
            .filter_map(ValueEnum::to_possible_value)
            .map(|p| p.get_name().to_string())
            .collect();
        let core: Vec<&str> = SummaryStyle::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, core);
    }

    #[test]
    fn host_and_port_need_http() {
        assert!(Cli::try_parse_from(["claude-consult", "serve", "--port", "1"]).is_err());
        let cli = Cli::try_parse_from(["claude-consult", "serve", "--http", "--port", "1"])
            .expect("parses");
        assert!(matches!(
            cli.command,
            Command::Serve(ServeArgs {
                http: true,
                port: 1,
                ..
            })
        ));
        assert!(Cli::try_parse_from(["claude-consult", "serve"]).is_ok());
    }

    #[test]
    fn detached_is_for_the_http_service_only() {
        assert!(Cli::try_parse_from(["claude-consult", "serve", "--detached"]).is_err());
        let cli = Cli::try_parse_from([
            "claude-consult",
            "serve",
            "--http",
            "--port",
            "8765",
            "--detached",
        ])
        .expect("parses");
        assert!(matches!(
            cli.command,
            Command::Serve(ServeArgs {
                http: true,
                detached: true,
                port: 8765,
                ..
            })
        ));
        // Exactly what the scheduled task runs.
        let spec = ServiceSpec {
            exe: PathBuf::from("x"),
            working_dir: PathBuf::from("y"),
            port: 8765,
            host: consult_service::DEFAULT_HOST.into(),
            account: None,
        };
        let task_args = consult_service::task_arguments(&spec);
        let argv = std::iter::once("claude-consult").chain(task_args.split_whitespace());
        assert!(matches!(
            Cli::try_parse_from(argv)
                .expect("the task's arguments parse")
                .command,
            Command::Serve(ServeArgs { detached: true, .. })
        ));
    }

    /// Splits a command line the way `CommandLineToArgvW` (and Rust's runtime on
    /// Windows) does for arguments after the program name: whitespace separates
    /// unless quoted, `2n` backslashes before a quote are `n` backslashes and the
    /// quote toggles, `2n+1` are `n` and a literal quote, other backslashes are
    /// literal.
    fn split_windows(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut in_arg = false;
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    let mut n = 1;
                    while chars.peek() == Some(&'\\') {
                        chars.next();
                        n += 1;
                    }
                    in_arg = true;
                    if chars.peek() == Some(&'"') {
                        cur.extend(std::iter::repeat_n('\\', n / 2));
                        if n % 2 == 1 {
                            chars.next();
                            cur.push('"');
                        }
                    } else {
                        cur.extend(std::iter::repeat_n('\\', n));
                    }
                }
                '"' => {
                    in_arg = true;
                    quoted = !quoted;
                }
                ' ' | '\t' if !quoted => {
                    if in_arg {
                        args.push(std::mem::take(&mut cur));
                        in_arg = false;
                    }
                }
                _ => {
                    in_arg = true;
                    cur.push(c);
                }
            }
        }
        if in_arg {
            args.push(cur);
        }
        args
    }

    /// The launcher (consult_service::elevated_arguments) and this parser must agree,
    /// or the elevated run exits 2 behind a hidden console and nobody sees why.
    #[test]
    fn the_elevated_childs_arguments_parse() {
        let dir = r"C:\Users\A B\AppData\Local\claude-consult\";
        let spec = ServiceSpec {
            exe: PathBuf::from("x"),
            working_dir: PathBuf::from(dir),
            port: 9001,
            host: consult_service::DEFAULT_HOST.into(),
            account: Some(r"EXAMPLE\some caller".into()),
        };
        let line = consult_service::elevated_arguments(&spec);
        let argv = std::iter::once("claude-consult".to_string()).chain(split_windows(&line));
        let cli = Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{line}\n{e}"));
        assert_eq!(cli.install_dir.as_deref(), Some(Path::new(dir)));
        let Command::Service(s) = cli.command else {
            panic!("not service: {line}");
        };
        assert_eq!(s.action, ServiceAction::Install);
        assert_eq!(s.port, Some(9001));
        assert!(s.no_fallback && s.no_elevate && !s.elevate);
        assert_eq!(s.run_as.as_deref(), Some(r"EXAMPLE\some caller"));
    }

    #[test]
    fn the_windows_splitter_follows_the_rules() {
        assert_eq!(
            split_windows(r#"a "b c" d\e "f\\" "g\"h" """#),
            ["a", "b c", r"d\e", r"f\", r#"g"h"#, ""]
        );
    }

    #[test]
    fn elevate_and_no_elevate_exclude_each_other() {
        assert!(
            Cli::try_parse_from([
                "claude-consult",
                "service",
                "install",
                "--elevate",
                "--no-elevate"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["claude-consult", "service", "install", "--elevate"]).is_ok());
    }

    #[test]
    fn globals_go_before_or_after_the_subcommand() {
        let cli =
            Cli::try_parse_from(["claude-consult", "hook", "statusline", "--install-dir", "x"])
                .expect("parses");
        assert_eq!(cli.install_dir.as_deref(), Some(Path::new("x")));
        let cli = Cli::try_parse_from(["claude-consult", "--claude-dir", "c", "reviewers"])
            .expect("parses");
        assert_eq!(cli.claude_dir.as_deref(), Some(Path::new("c")));
    }

    #[test]
    fn the_mode_is_review_or_diagnose() {
        assert!(Cli::try_parse_from(["claude-consult", "run", "--mode", "other"]).is_err());
        let cli =
            Cli::try_parse_from(["claude-consult", "run", "--mode", "diagnose"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Run(RunCli {
                mode: ModeArg::Diagnose,
                ..
            })
        ));
    }
}
