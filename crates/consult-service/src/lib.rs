//! Management of claude-consult's shared MCP service.
//!
//! One process serves every Claude Code session on the host instead of each
//! session spawning its own stdio child. On Windows it is a scheduled task
//! that starts at boot and again at logon, driven through `schtasks.exe`.
//! Elsewhere every operation returns [`ServiceError::Unsupported`], except
//! [`is_listening`] and [`status`].
//!
//! Identity matters: the task runs as the interactive user via S4U (no stored
//! password) rather than as SYSTEM. The server resolves the OpenRouter key
//! from the environment or `~/.claude/settings.json`; as SYSTEM, `~` is
//! `C:\Windows\System32\config\systemprofile`, the key would not be found,
//! and every consult would fail at runtime while the port still looked
//! healthy.
//!
//! The free functions act on the task named [`TASK_NAME`]; [`Task`] does the
//! same for any name, so a manual test can use a throwaway task.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod process;
#[cfg(windows)]
mod windows;
mod xml;

pub use process::is_serve_command_line;
pub use xml::{
    LogonKind, RegisteredTask, decode_output, elevated_arguments, encode_utf16le, escape,
    parse_csv_state, parse_task_xml, port_from_arguments, quote_arg, task_arguments,
    task_description, task_xml,
};

/// The scheduled task's name. Kept from the Python version so an upgrade
/// replaces its task instead of registering a second one.
pub const TASK_NAME: &str = "OpenRouterMCP";

/// The port the service listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 8765;

/// The host the service binds unless told otherwise.
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// What the scheduled task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// The installed binary (`<install_dir>/bin/claude-consult.exe`).
    pub exe: PathBuf,
    /// The task's working directory, the install dir.
    pub working_dir: PathBuf,
    /// The port passed as `--port`.
    pub port: u16,
    /// The host; passed as `--host` only when it is not [`DEFAULT_HOST`].
    pub host: String,
    /// The account (`DOMAIN\user`) the task runs as and whose logon triggers it;
    /// `None` is the current account ([`current_account`]).
    ///
    /// Set for the elevated copy: whoever answered the administrator prompt may be a
    /// different account, and a task running as them would read their profile and
    /// never find the key.
    pub account: Option<String>,
}

/// How the task ended up registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registration {
    /// As the current user with S4U: starts at boot and at logon.
    S4U,
    /// S4U was refused (usually: the shell is not elevated), so the task was
    /// registered with an interactive token and a logon trigger only.
    /// `reason` is schtasks's own message. Show the user
    /// [`explain_interactive_fallback`].
    InteractiveLogonOnly {
        /// Why the S4U registration failed, as schtasks reported it.
        reason: String,
    },
}

/// A process serving consult over HTTP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Process id.
    pub pid: u32,
    /// Resident set size (working set) in bytes.
    pub rss_bytes: u64,
    /// The command line, arguments joined by spaces (quoting is not kept).
    pub command_line: String,
}

/// A snapshot of the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    /// False off Windows, where only `port` and `listening` mean anything.
    pub supported: bool,
    /// Whether the task is registered.
    pub registered: bool,
    /// Task Scheduler's state (`Ready`, `Running`, `Disabled`, ... in the
    /// display language), when it could be read.
    pub state: Option<String>,
    /// The account the task runs as.
    pub runs_as: Option<String>,
    /// The principal's logon type as stored (`S4U`, `InteractiveToken`).
    pub logon_type: Option<String>,
    /// Trigger kinds: `Boot`, `Logon`.
    pub triggers: Vec<String>,
    /// The port checked: the one asked for, else the registered one, else
    /// [`DEFAULT_PORT`].
    pub port: u16,
    /// Whether something accepts connections on `127.0.0.1:port`.
    pub listening: bool,
    /// Serve processes running from the task's working directory.
    pub processes: Vec<ProcessInfo>,
    /// The task's working directory (the install dir it serves from).
    pub working_dir: Option<PathBuf>,
}

/// Everything that can go wrong managing the service.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The shared service exists only on Windows.
    #[error("the shared service is only supported on Windows")]
    Unsupported,
    /// schtasks.exe could not be started at all.
    #[error("could not run schtasks.exe: {0}")]
    Spawn(#[source] std::io::Error),
    /// schtasks.exe ran and reported a failure.
    #[error("schtasks {action} failed: {message}")]
    Schtasks {
        /// The schtasks verb, such as `/Create`.
        action: String,
        /// schtasks's own message.
        message: String,
    },
    /// The binary the task would run does not exist.
    #[error("service executable not found: {}", .0.display())]
    ExeNotFound(PathBuf),
    /// `USERNAME` is not set, so there is no account to run the task as.
    #[error("cannot tell the current user: USERNAME is not set")]
    NoAccount,
    /// The registered task's XML could not be read.
    #[error("could not parse the task XML: {0}")]
    Parse(String),
    /// Writing the temporary task definition failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The administrator prompt was dismissed, so nothing ran elevated.
    #[error("the administrator prompt was cancelled")]
    ElevationCancelled,
    /// The elevated registration could not be started, or ran and failed.
    #[error("the elevated registration failed: {0}")]
    Elevation(String),
}

/// A scheduled task by name. [`Task::default`] is [`TASK_NAME`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    name: String,
}

impl Default for Task {
    fn default() -> Self {
        Task::named(TASK_NAME)
    }
}

#[cfg_attr(not(windows), allow(unused_variables))]
impl Task {
    /// A handle on the task called `name`.
    pub fn named(name: impl Into<String>) -> Self {
        Task { name: name.into() }
    }

    /// The task's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Registers (or replaces) the task: S4U first, the interactive logon-only
    /// variant when S4U is refused.
    pub fn install(&self, spec: &ServiceSpec) -> Result<Registration, ServiceError> {
        #[cfg(windows)]
        return self.install_impl(spec, true);
        #[cfg(not(windows))]
        Err(ServiceError::Unsupported)
    }

    /// Registers (or replaces) the task with S4U only: a refusal is an error,
    /// never the logon-only fallback. For the elevated child, whose whole job
    /// is the S4U registration.
    pub fn install_s4u(&self, spec: &ServiceSpec) -> Result<(), ServiceError> {
        #[cfg(windows)]
        return self.install_impl(spec, false).map(|_| ());
        #[cfg(not(windows))]
        Err(ServiceError::Unsupported)
    }

    /// Runs the task, then waits up to 6 seconds for `port` to listen.
    /// Returns whether it is listening.
    pub fn start(&self, port: u16) -> Result<bool, ServiceError> {
        #[cfg(windows)]
        return self.start_impl(port);
        #[cfg(not(windows))]
        Err(ServiceError::Unsupported)
    }

    /// Ends the task and kills the serve processes running from `dirs`, and
    /// only those, never some other install's server. Returns the killed pids.
    pub fn stop(&self, dirs: &[PathBuf]) -> Result<Vec<u32>, ServiceError> {
        #[cfg(windows)]
        return self.stop_impl(dirs);
        #[cfg(not(windows))]
        Err(ServiceError::Unsupported)
    }

    /// Stops the task (and the serve processes in its working directory) and
    /// deletes it. Succeeds when the task is not registered.
    pub fn uninstall(&self) -> Result<(), ServiceError> {
        #[cfg(windows)]
        return self.uninstall_impl();
        #[cfg(not(windows))]
        Err(ServiceError::Unsupported)
    }

    /// The task as registered, or `None` when it is not.
    pub fn registered(&self) -> Result<Option<RegisteredTask>, ServiceError> {
        #[cfg(windows)]
        return self.query();
        #[cfg(not(windows))]
        Err(ServiceError::Unsupported)
    }

    /// A status snapshot; processes are looked for in the task's working
    /// directory. See [`status`].
    pub fn status(&self, port: Option<u16>) -> Result<ServiceStatus, ServiceError> {
        #[cfg(windows)]
        return self.status_impl(port, None);
        #[cfg(not(windows))]
        Ok(unsupported_status(port))
    }

    /// Like [`Task::status`], but looks for serve processes in `dirs` instead
    /// of the task's working directory.
    pub fn status_in(
        &self,
        port: Option<u16>,
        dirs: &[PathBuf],
    ) -> Result<ServiceStatus, ServiceError> {
        #[cfg(windows)]
        return self.status_impl(port, Some(dirs));
        #[cfg(not(windows))]
        Ok(unsupported_status(port))
    }

    /// The port in the registered task's arguments.
    pub fn registered_port(&self) -> Option<u16> {
        self.registered().ok().flatten().and_then(|t| t.port())
    }

    /// The registered task's working directory.
    pub fn registered_dir(&self) -> Option<PathBuf> {
        self.registered().ok().flatten().and_then(|t| t.working_dir)
    }
}

#[cfg(not(windows))]
fn unsupported_status(port: Option<u16>) -> ServiceStatus {
    let port = port.unwrap_or(DEFAULT_PORT);
    ServiceStatus {
        supported: false,
        registered: false,
        state: None,
        runs_as: None,
        logon_type: None,
        triggers: Vec::new(),
        port,
        listening: is_listening(port),
        processes: Vec::new(),
        working_dir: None,
    }
}

/// Registers (or replaces) the [`TASK_NAME`] task. See [`Task::install`].
pub fn install(spec: &ServiceSpec) -> Result<Registration, ServiceError> {
    Task::default().install(spec)
}

/// Registers the [`TASK_NAME`] task with S4U or fails. See [`Task::install_s4u`].
pub fn install_s4u(spec: &ServiceSpec) -> Result<(), ServiceError> {
    Task::default().install_s4u(spec)
}

/// Registers the [`TASK_NAME`] task with S4U from an elevated copy of `exe`:
/// runs `exe` with [`elevated_arguments`] through the administrator prompt
/// (UAC), hidden, and waits for it to exit.
///
/// The child is told to register for `spec.account`, else this process's account.
/// Otherwise only the port and the working directory reach it; it registers
/// `<working_dir>/bin/claude-consult.exe` on the default host; `spec.exe` is
/// expected to be that binary. A dismissed prompt is
/// [`ServiceError::ElevationCancelled`]; a child that exits non-zero is
/// [`ServiceError::Elevation`] with its exit code (its own message went to a
/// hidden console). Read the task back with [`status`] to see what it left.
pub fn install_elevated(spec: &ServiceSpec, exe: &Path) -> Result<(), ServiceError> {
    #[cfg(windows)]
    {
        // The caller's account, fixed here, before the prompt: the elevated copy's own
        // environment is whoever answered it.
        let spec = ServiceSpec {
            account: Some(task_account(spec)?),
            ..spec.clone()
        };
        windows::run_elevated(exe, &elevated_arguments(&spec))
    }
    #[cfg(not(windows))]
    {
        let _ = (spec, exe);
        Err(ServiceError::Unsupported)
    }
}

/// The account the task is registered for: `spec.account` when set, else
/// [`current_account`].
pub fn task_account(spec: &ServiceSpec) -> Result<String, ServiceError> {
    match spec.account.as_deref().map(str::trim) {
        Some(a) if !a.is_empty() => Ok(a.to_owned()),
        _ => current_account(),
    }
}

/// Whether this process runs elevated (an administrator token with UAC's
/// filter lifted). Always `false` off Windows, where there is no service.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    return windows::is_elevated();
    #[cfg(not(windows))]
    false
}

/// Runs the task and waits up to 6 seconds for `port`. See [`Task::start`].
pub fn start(port: u16) -> Result<bool, ServiceError> {
    Task::default().start(port)
}

/// Ends the task and kills only serve processes from `dirs`. See [`Task::stop`].
pub fn stop(dirs: &[PathBuf]) -> Result<Vec<u32>, ServiceError> {
    Task::default().stop(dirs)
}

/// Stops and deletes the task. See [`Task::uninstall`].
pub fn uninstall() -> Result<(), ServiceError> {
    Task::default().uninstall()
}

/// A status snapshot. `port` defaults to the registered port, because a
/// non-default install would otherwise report "not listening" while running
/// fine. Off Windows it reports `supported: false` plus the listening check.
pub fn status(port: Option<u16>) -> Result<ServiceStatus, ServiceError> {
    Task::default().status(port)
}

/// The [`TASK_NAME`] task as registered, or `None` when it is not (or cannot be read).
pub fn registered() -> Option<RegisteredTask> {
    Task::default().registered().ok().flatten()
}

/// The port the registered task was given.
pub fn registered_port() -> Option<u16> {
    Task::default().registered_port()
}

/// The registered task's working directory, i.e. the install it serves from.
pub fn registered_dir() -> Option<PathBuf> {
    Task::default().registered_dir()
}

/// The serve processes currently running from `dirs`.
pub fn service_processes(dirs: &[PathBuf]) -> Result<Vec<ProcessInfo>, ServiceError> {
    #[cfg(windows)]
    return Ok(Task::processes_impl(dirs));
    #[cfg(not(windows))]
    {
        let _ = dirs;
        Err(ServiceError::Unsupported)
    }
}

/// The account the task is registered for: `DOMAIN\user`.
pub fn current_account() -> Result<String, ServiceError> {
    #[cfg(windows)]
    return windows::current_account();
    #[cfg(not(windows))]
    Err(ServiceError::Unsupported)
}

/// Whether something accepts TCP connections on `127.0.0.1:port`.
pub fn is_listening(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// The text to show after [`Registration::InteractiveLogonOnly`]: what
/// happened, why, and the command that gets start-at-boot.
pub fn explain_interactive_fallback(reason: &str, install_dir: &Path) -> String {
    let exe = install_dir
        .join("bin")
        .join(format!("claude-consult{}", std::env::consts::EXE_SUFFIX));
    // PowerShell single quotes: a quote inside is written twice.
    let exe = exe.to_string_lossy().replace('\'', "''");
    format!(
        "Registered '{TASK_NAME}' as an interactive task - LOGON ONLY.\n\
         \x20 Reason: {reason}\n\
         \x20 Boot-start requires the S4U logon type, which only an elevated\n\
         \x20 process may register. To get start-at-boot, run this from\n\
         \x20 PowerShell and accept the administrator prompt:\n\
         \x20   & '{exe}' service install --elevate",
        reason = reason.trim()
    )
}

/// The line that reports an S4U registration made through the administrator
/// prompt.
pub fn elevated_success(account: &str) -> String {
    format!("Registered '{TASK_NAME}' as {account} (S4U) - starts at boot and at logon")
}

/// The question asked before the administrator prompt.
pub const ELEVATE_QUESTION: &str =
    "Register the task with administrator rights so it starts at boot?";

#[cfg(test)]
mod tests;
