//! The Windows implementation: everything goes through `schtasks.exe`.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::process::live;
use crate::xml::{self, LogonKind, RegisteredTask};
use crate::{
    DEFAULT_PORT, ProcessInfo, Registration, ServiceError, ServiceSpec, ServiceStatus, Task,
};

/// Keeps a console window from flashing up when a windowless process (the
/// TUI, the service) runs schtasks.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The one place schtasks is run. By absolute path, so a `schtasks.exe` in the
/// working directory or on PATH is never picked up instead.
fn run_schtasks(args: &[&OsStr]) -> Result<Output, ServiceError> {
    use std::os::windows::process::CommandExt;
    let exe = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("schtasks.exe"))
        .unwrap_or_else(|| PathBuf::from("schtasks.exe"));
    Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(ServiceError::Spawn)
}

fn failure(action: &str, out: &Output) -> ServiceError {
    let mut message = xml::decode_output(&out.stderr).trim().to_owned();
    if message.is_empty() {
        message = xml::decode_output(&out.stdout).trim().to_owned();
    }
    if message.is_empty() {
        message = format!("exit status {}", out.status);
    }
    ServiceError::Schtasks {
        action: action.to_owned(),
        message,
    }
}

fn checked(action: &str, args: &[&OsStr]) -> Result<Output, ServiceError> {
    let out = run_schtasks(args)?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(failure(action, &out))
    }
}

/// `DOMAIN\user` of the current user.
pub(crate) fn current_account() -> Result<String, ServiceError> {
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .ok_or(ServiceError::NoAccount)?;
    Ok(match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.trim().is_empty() => format!("{domain}\\{user}"),
        _ => user,
    })
}

fn create(name: &str, xml_text: &str) -> Result<(), ServiceError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let file = std::env::temp_dir().join(format!(
        "claude-consult-task-{}-{nanos}.xml",
        std::process::id()
    ));
    std::fs::write(&file, xml::encode_utf16le(xml_text))?;
    let result = checked(
        "/Create",
        &[
            OsStr::new("/Create"),
            OsStr::new("/TN"),
            OsStr::new(name),
            OsStr::new("/XML"),
            file.as_os_str(),
            OsStr::new("/F"),
        ],
    );
    let _ = std::fs::remove_file(&file);
    result.map(|_| ())
}

impl Task {
    fn name_os(&self) -> &OsStr {
        OsStr::new(&self.name)
    }

    pub(crate) fn install_impl(&self, spec: &ServiceSpec) -> Result<Registration, ServiceError> {
        if !spec.exe.is_file() {
            return Err(ServiceError::ExeNotFound(spec.exe.clone()));
        }
        let account = current_account()?;
        let s4u = xml::task_xml(spec, &account, LogonKind::S4U);
        match create(&self.name, &s4u) {
            Ok(()) => Ok(Registration::S4U),
            Err(ServiceError::Schtasks { message, .. }) => {
                // Fall back to an interactive-only task so an unelevated
                // install still succeeds; it then starts at logon rather than
                // at boot.
                let interactive = xml::task_xml(spec, &account, LogonKind::InteractiveToken);
                match create(&self.name, &interactive) {
                    Ok(()) => Ok(Registration::InteractiveLogonOnly { reason: message }),
                    Err(ServiceError::Schtasks {
                        message: second, ..
                    }) => Err(ServiceError::Schtasks {
                        action: "/Create".to_owned(),
                        message: format!(
                            "S4U registration failed ({message}); logon-only registration failed too ({second})"
                        ),
                    }),
                    Err(other) => Err(other),
                }
            }
            Err(other) => Err(other),
        }
    }

    pub(crate) fn start_impl(&self, port: u16) -> Result<bool, ServiceError> {
        checked(
            "/Run",
            &[OsStr::new("/Run"), OsStr::new("/TN"), self.name_os()],
        )?;
        for _ in 0..24 {
            std::thread::sleep(Duration::from_millis(250));
            if crate::is_listening(port) {
                return Ok(true);
            }
        }
        Ok(crate::is_listening(port))
    }

    pub(crate) fn stop_impl(&self, dirs: &[PathBuf]) -> Result<Vec<u32>, ServiceError> {
        // Failing to end is not an error: the task may be absent or idle.
        let _ = run_schtasks(&[OsStr::new("/End"), OsStr::new("/TN"), self.name_os()])?;
        Ok(live::kill(dirs))
    }

    pub(crate) fn uninstall_impl(&self) -> Result<(), ServiceError> {
        let Some(task) = self.query()? else {
            return Ok(());
        };
        let dirs: Vec<PathBuf> = task.working_dir.into_iter().collect();
        self.stop_impl(&dirs)?;
        checked(
            "/Delete",
            &[
                OsStr::new("/Delete"),
                OsStr::new("/TN"),
                self.name_os(),
                OsStr::new("/F"),
            ],
        )?;
        Ok(())
    }

    /// The registered task, or `None` when it is not registered (or schtasks
    /// cannot read it, which callers treat the same way).
    pub(crate) fn query(&self) -> Result<Option<RegisteredTask>, ServiceError> {
        let out = run_schtasks(&[
            OsStr::new("/Query"),
            OsStr::new("/TN"),
            self.name_os(),
            OsStr::new("/XML"),
        ])?;
        if !out.status.success() {
            return Ok(None);
        }
        xml::parse_task_xml(&xml::decode_output(&out.stdout)).map(Some)
    }

    fn state(&self) -> Option<String> {
        let out = run_schtasks(&[
            OsStr::new("/Query"),
            OsStr::new("/TN"),
            self.name_os(),
            OsStr::new("/FO"),
            OsStr::new("CSV"),
            OsStr::new("/NH"),
        ])
        .ok()?;
        if !out.status.success() {
            return None;
        }
        xml::parse_csv_state(&xml::decode_output(&out.stdout))
    }

    pub(crate) fn status_impl(
        &self,
        port: Option<u16>,
        dirs: Option<&[PathBuf]>,
    ) -> Result<ServiceStatus, ServiceError> {
        let task = self.query()?;
        let port = port
            .or_else(|| task.as_ref().and_then(RegisteredTask::port))
            .unwrap_or(DEFAULT_PORT);
        let working_dir = task.as_ref().and_then(|t| t.working_dir.clone());
        let registered_dirs: Vec<PathBuf> = working_dir.iter().cloned().collect();
        let processes: Vec<ProcessInfo> = live::find(dirs.unwrap_or(&registered_dirs));
        Ok(ServiceStatus {
            supported: true,
            registered: task.is_some(),
            state: task.as_ref().and_then(|_| self.state()),
            runs_as: task.as_ref().and_then(RegisteredTask::runs_as),
            logon_type: task.as_ref().and_then(|t| t.logon_type.clone()),
            triggers: task.map(|t| t.triggers).unwrap_or_default(),
            port,
            listening: crate::is_listening(port),
            processes,
            working_dir,
        })
    }

    pub(crate) fn processes_impl(dirs: &[PathBuf]) -> Vec<ProcessInfo> {
        live::find(dirs)
    }
}
