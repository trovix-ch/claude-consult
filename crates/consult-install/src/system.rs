//! Everything the flows do to the machine outside the install and Claude dirs, behind
//! small traits so the tests can put fakes in: the scheduled task, the `claude` CLI,
//! the network and the host itself.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use consult_core::key::{KeyCheck, check_key, clean_key};
use consult_core::listing::fetch_listing;
use consult_core::openrouter::Client;
use consult_service::{RegisteredTask, Registration, ServiceError, ServiceSpec};

/// The shared service, as [`consult_service`] manages it.
pub trait ServiceOps {
    /// Whether this platform has the service at all (Windows).
    fn supported(&self) -> bool;
    /// The dir the registered task serves from, if one is registered.
    fn registered_dir(&self) -> Option<PathBuf>;
    /// Ends the task and kills the serve processes running from `dirs`.
    fn stop(&self, dirs: &[PathBuf]) -> Result<Vec<u32>, ServiceError>;
    /// Registers (or replaces) the task.
    fn install(&self, spec: &ServiceSpec) -> Result<Registration, ServiceError>;
    /// Whether this process is elevated, so a refused S4U registration would be
    /// refused through the administrator prompt too.
    fn is_elevated(&self) -> bool;
    /// Registers the task with S4U from an elevated copy of `exe`, through the
    /// administrator prompt, and waits for it.
    fn install_elevated(&self, spec: &ServiceSpec, exe: &Path) -> Result<(), ServiceError>;
    /// The task as registered now, if it is.
    fn registered(&self) -> Option<RegisteredTask>;
    /// Runs the task and waits for the port; `true` when it listens.
    fn start(&self, port: u16) -> Result<bool, ServiceError>;
    /// Stops and deletes the task.
    fn uninstall(&self) -> Result<(), ServiceError>;
}

/// The `claude` CLI.
pub trait ClaudeCli {
    /// Whether `claude` is on PATH.
    fn available(&self) -> bool;
    /// Runs `claude <args>` with its output discarded; the exit code, or an error when
    /// it could not be started. Never given the API key.
    fn run(&self, args: &[String]) -> io::Result<i32>;
}

/// OpenRouter, as far as the installer talks to it.
pub trait Network {
    /// The installer's tool-capable listing (most popular first), raw, and when it
    /// arrived; `None` when it could not be fetched. Asked once per run.
    fn fetch_listing(&self) -> Option<(String, DateTime<Utc>)>;
    /// What OpenRouter's free `/key` endpoint says about the key.
    fn check_key(&self, key: &str) -> KeyCheck;
}

/// The host the installer runs on.
pub trait Host {
    /// Whether git is on PATH.
    fn git_found(&self) -> bool;
    /// Claude Code's real config dir (`CLAUDE_CONFIG_DIR`, else `~/.claude`): the one
    /// `claude mcp` edits whatever `--claude-dir` says.
    fn real_claude_dir(&self) -> PathBuf;
    /// `OPENROUTER_API_KEY` from the environment, cleaned, if set.
    fn env_key(&self) -> Option<String>;
    /// The running binary.
    fn current_exe(&self) -> Option<PathBuf>;
    /// Deletes `dir` once this process has exited (it holds a file there open).
    fn remove_after_exit(&self, dir: &Path) -> io::Result<()>;
}

/// The machine the flows act on.
pub struct System {
    /// The scheduled task.
    pub service: Box<dyn ServiceOps>,
    /// The `claude` CLI.
    pub claude: Box<dyn ClaudeCli>,
    /// OpenRouter.
    pub network: Box<dyn Network>,
    /// The host.
    pub host: Box<dyn Host>,
}

impl System {
    /// The real machine.
    pub fn real() -> Self {
        Self {
            service: Box::new(RealService),
            claude: Box::new(RealClaude::find()),
            network: Box::new(RealNetwork::new()),
            host: Box::new(RealHost),
        }
    }
}

// ---- real implementations ------------------------------------------------------------

/// [`ServiceOps`] through [`consult_service`].
#[derive(Clone, Copy, Debug, Default)]
pub struct RealService;

impl ServiceOps for RealService {
    fn supported(&self) -> bool {
        cfg!(windows)
    }

    fn registered_dir(&self) -> Option<PathBuf> {
        consult_service::registered_dir()
    }

    fn stop(&self, dirs: &[PathBuf]) -> Result<Vec<u32>, ServiceError> {
        consult_service::stop(dirs)
    }

    fn install(&self, spec: &ServiceSpec) -> Result<Registration, ServiceError> {
        consult_service::install(spec)
    }

    fn is_elevated(&self) -> bool {
        consult_service::is_elevated()
    }

    fn install_elevated(&self, spec: &ServiceSpec, exe: &Path) -> Result<(), ServiceError> {
        consult_service::install_elevated(spec, exe)
    }

    fn registered(&self) -> Option<RegisteredTask> {
        consult_service::registered()
    }

    fn start(&self, port: u16) -> Result<bool, ServiceError> {
        consult_service::start(port)
    }

    fn uninstall(&self) -> Result<(), ServiceError> {
        consult_service::uninstall()
    }
}

/// A program on PATH, trying each of `PATHEXT`'s extensions on Windows (`claude` is
/// often an npm `.cmd` shim there).
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|p| p.is_file())
    })
}

/// [`ClaudeCli`] running the `claude` found on PATH.
#[derive(Clone, Debug, Default)]
pub struct RealClaude {
    program: Option<PathBuf>,
}

impl RealClaude {
    /// Looks `claude` up on PATH.
    pub fn find() -> Self {
        Self {
            program: find_program("claude"),
        }
    }
}

impl ClaudeCli for RealClaude {
    fn available(&self) -> bool {
        self.program.is_some()
    }

    fn run(&self, args: &[String]) -> io::Result<i32> {
        let Some(program) = &self.program else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "claude not found"));
        };
        // Quiet, as the PowerShell installer ran it: its chatter would tear the
        // full-screen UI, and only the exit code decides anything.
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.code().unwrap_or(1))
    }
}

/// [`Network`] against the real OpenRouter, on a runtime of its own.
pub struct RealNetwork {
    client: Client,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Default for RealNetwork {
    fn default() -> Self {
        Self::new()
    }
}

impl RealNetwork {
    /// How long the listing may take. It is only for the picker's prices and the
    /// non-favourites, so an install never waits longer than this for it.
    pub const LISTING_TIMEOUT: Duration = Duration::from_secs(30);

    /// A client for the real OpenRouter.
    pub fn new() -> Self {
        Self::with_client(Client::new())
    }

    /// A client for another API root (a mock server).
    pub fn with_client(client: Client) -> Self {
        // Current-thread and owned here: the installer is synchronous, and a failure
        // to build one only means "offline".
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok();
        Self { client, runtime }
    }
}

impl Network for RealNetwork {
    fn fetch_listing(&self) -> Option<(String, DateTime<Utc>)> {
        let rt = self.runtime.as_ref()?;
        let body = rt
            .block_on(fetch_listing(&self.client, Self::LISTING_TIMEOUT, true))
            .ok()?;
        Some((body, Utc::now()))
    }

    fn check_key(&self, key: &str) -> KeyCheck {
        match self.runtime.as_ref() {
            Some(rt) => rt.block_on(check_key(&self.client, key)),
            None => KeyCheck::Unreachable {
                reason: "no async runtime".to_string(),
            },
        }
    }
}

/// [`Host`] for this machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct RealHost;

impl Host for RealHost {
    fn git_found(&self) -> bool {
        find_program("git").is_some()
    }

    fn real_claude_dir(&self) -> PathBuf {
        consult_core::paths::claude_dir(None)
    }

    fn env_key(&self) -> Option<String> {
        let raw = std::env::var(consult_core::key::KEY_VAR).ok()?;
        let key = clean_key(&raw);
        (!key.is_empty()).then(|| key.to_string())
    }

    fn current_exe(&self) -> Option<PathBuf> {
        std::env::current_exe().ok()
    }

    fn remove_after_exit(&self, dir: &Path) -> io::Result<()> {
        crate::files::remove_after_exit(dir)
    }
}
