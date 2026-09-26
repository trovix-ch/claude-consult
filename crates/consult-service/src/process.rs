//! Finding the service's own processes, and only those.

use std::path::{Path, PathBuf};

/// Normalises a path or command line for matching: forward slashes, lower
/// case, no `\\?\` verbatim prefix.
fn normalise(text: &str) -> String {
    let s = text.replace('\\', "/").to_lowercase();
    match s.strip_prefix("//?/") {
        Some(rest) => rest.to_owned(),
        None => s,
    }
}

/// The needle a command line must contain to belong to `dir`: the directory
/// with a trailing slash, so `C:/x/claude-consult` never matches a process
/// running from `C:/x/claude-consult-old`.
fn needle(dir: &Path) -> Option<String> {
    let s = normalise(&dir.to_string_lossy());
    let s = s.trim().trim_end_matches('/');
    (!s.is_empty()).then(|| format!("{s}/"))
}

/// Whether `command_line` is a `serve --http` process running from one of
/// `dirs`.
///
/// Only processes serving from the given directories count, never some other
/// project's server. `serve` is a substring test on purpose: the legacy Python
/// service ran `"<dir>/server.py" --http`, and an upgrade must stop it too.
/// Empty directories are ignored, so an empty list matches nothing.
pub fn is_serve_command_line(command_line: &str, dirs: &[PathBuf]) -> bool {
    let cl = normalise(command_line);
    if !(cl.contains("serve") && cl.contains("--http")) {
        return false;
    }
    dirs.iter()
        .filter_map(|d| needle(d))
        .any(|n| cl.contains(&n))
}

#[cfg(windows)]
pub(crate) mod live {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    use super::is_serve_command_line;
    use crate::ProcessInfo;

    fn snapshot() -> System {
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_memory()
                .with_cmd(UpdateKind::Always),
        );
        sys
    }

    fn command_line(process: &sysinfo::Process) -> String {
        process
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn matching(sys: &System, dirs: &[PathBuf]) -> Vec<(Pid, ProcessInfo)> {
        let me = std::process::id();
        let mut found: Vec<(Pid, ProcessInfo)> = sys
            .processes()
            .iter()
            .filter(|(pid, _)| pid.as_u32() != me)
            .filter_map(|(pid, p)| {
                let cl = command_line(p);
                is_serve_command_line(&cl, dirs).then(|| {
                    (
                        *pid,
                        ProcessInfo {
                            pid: pid.as_u32(),
                            rss_bytes: p.memory(),
                            command_line: cl,
                        },
                    )
                })
            })
            .collect();
        found.sort_by_key(|(_, info)| info.pid);
        found
    }

    /// The service processes currently running from `dirs`.
    pub(crate) fn find(dirs: &[PathBuf]) -> Vec<ProcessInfo> {
        matching(&snapshot(), dirs)
            .into_iter()
            .map(|(_, info)| info)
            .collect()
    }

    /// Kills the service processes running from `dirs` and waits briefly for
    /// them to exit, so a caller can replace the binary they were running.
    /// Returns the pids that were signalled.
    pub(crate) fn kill(dirs: &[PathBuf]) -> Vec<u32> {
        let sys = snapshot();
        let mut killed = Vec::new();
        for (pid, info) in matching(&sys, dirs) {
            if sys.process(pid).is_some_and(|p| p.kill()) {
                killed.push(info.pid);
            }
        }
        if killed.is_empty() {
            return killed;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            let mut sys = System::new();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            if killed
                .iter()
                .all(|pid| sys.process(Pid::from_u32(*pid)).is_none())
            {
                break;
            }
        }
        killed
    }
}
