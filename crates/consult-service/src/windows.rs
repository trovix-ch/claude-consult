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

/// Whether the process token is elevated. Any failure to ask reads as "no",
/// which only means the caller offers the administrator prompt.
#[allow(unsafe_code)]
pub(crate) fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing;
    // `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    // SAFETY: `token` was opened above with TOKEN_QUERY; the buffer is a
    // TOKEN_ELEVATION of exactly the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    // SAFETY: `token` is a handle this function owns.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// Runs `exe arguments` through the administrator prompt, hidden, and waits
/// for it. See [`crate::install_elevated`].
#[allow(unsafe_code)]
pub(crate) fn run_elevated(exe: &std::path::Path, arguments: &str) -> Result<(), ServiceError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    /// `SW_HIDE`: the child is a console program and gets a console of its
    /// own; hidden, nothing flashes up while it registers the task.
    const SW_HIDE: i32 = 0;

    if !exe.is_file() {
        return Err(ServiceError::ExeNotFound(exe.to_path_buf()));
    }
    let wide = |s: &OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let params = wide(OsStr::new(arguments));

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: params.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    // SAFETY: every pointer in `info` points into a buffer that outlives the
    // call, each NUL-terminated; `info` is initialised with its own size.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: no other call in between that could reset it.
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_CANCELLED {
            ServiceError::ElevationCancelled
        } else {
            ServiceError::Elevation(std::io::Error::from_raw_os_error(code as i32).to_string())
        });
    }
    let process = info.hProcess;
    if process.is_null() {
        // No process handle means ShellExecute handed the request to
        // something else; there is nothing to wait on or read back.
        return Err(ServiceError::Elevation(
            "no process handle came back from the administrator prompt".to_owned(),
        ));
    }
    let mut code = 1u32;
    // SAFETY: `process` is the handle SEE_MASK_NOCLOSEPROCESS asked for; it
    // is waited on, read and closed exactly once, here.
    let read = unsafe {
        WaitForSingleObject(process, INFINITE);
        let read = GetExitCodeProcess(process, &mut code);
        CloseHandle(process);
        read
    };
    if read == 0 {
        return Err(ServiceError::Elevation(
            "could not read the elevated process's exit code".to_owned(),
        ));
    }
    if code != 0 {
        return Err(ServiceError::Elevation(format!(
            "the elevated 'service install' exited with code {code}"
        )));
    }
    Ok(())
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

    pub(crate) fn install_impl(
        &self,
        spec: &ServiceSpec,
        fallback: bool,
    ) -> Result<Registration, ServiceError> {
        if !spec.exe.is_file() {
            return Err(ServiceError::ExeNotFound(spec.exe.clone()));
        }
        let account = crate::task_account(spec)?;
        let s4u = xml::task_xml(spec, &account, LogonKind::S4U);
        match create(&self.name, &s4u) {
            Ok(()) => Ok(Registration::S4U),
            Err(ServiceError::Schtasks { message, .. }) if fallback => {
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
