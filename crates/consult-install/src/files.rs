//! The install dir's own files: the binary's copy, the old Python layout, and deleting
//! the dir on uninstall.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use consult_core::paths::{binary_path, exe_name};

use crate::system::Host;

/// What [`copy_binary`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The binary was copied into place.
    Copied,
    /// The running binary already is the installed copy; nothing to copy.
    AlreadyInPlace,
}

const ATTEMPTS: u32 = 5;

// A virus scanner or the indexer may hold a fresh file for a moment; a sharing
// violation then clears on its own.
fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(e),
            Err(e) => last = Some(e),
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(Duration::from_millis(300));
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("copy failed")))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Copies `source` to `<install_dir>/bin/claude-consult[.exe]`, the copy the hooks and
/// the service run.
///
/// The copy lands next to the target first and is then renamed over it, so the target
/// is never half-written. Windows will not replace or delete a running binary, but it
/// will rename one: a copy still running (a session's stdio server, say) is moved aside
/// as `claude-consult.exe.old-<n>` and removed on a later install once nothing runs it.
pub fn copy_binary(source: &Path, install_dir: &Path) -> io::Result<CopyOutcome> {
    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("the binary to install is missing: {}", source.display()),
        ));
    }
    let dest = binary_path(install_dir);
    let bin = install_dir.join("bin");
    fs::create_dir_all(&bin)?;
    if dest.exists() && same_file(source, &dest) {
        return Ok(CopyOutcome::AlreadyInPlace);
    }
    remove_stale_copies(&bin);
    let tmp = sibling(&dest, ".new");
    retry(|| fs::copy(source, &tmp).map(|_| ()))?;
    retry(|| {
        match fs::rename(&tmp, &dest) {
            Ok(()) => return Ok(()),
            Err(e) if !dest.exists() => return Err(e),
            Err(_) => {}
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        fs::rename(&dest, sibling(&dest, &format!(".old-{stamp}")))?;
        fs::rename(&tmp, &dest)
    })?;
    remove_stale_copies(&bin);
    Ok(CopyOutcome::Copied)
}

/// Removes the moved-aside copies [`copy_binary`] left behind, as far as nothing runs
/// them any more.
fn remove_stale_copies(bin: &Path) {
    let prefix = format!("{}.old", exe_name());
    let Ok(entries) = fs::read_dir(bin) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The Python install's server files, at the top of its install dir.
pub const LEGACY_FILES: [&str; 5] = [
    "server.py",
    "panel.py",
    "sandbox.py",
    "service.ps1",
    "requirements.txt",
];
/// Its hook scripts, under `hooks/`.
pub const LEGACY_HOOKS: [&str; 3] = ["summary_hook.py", "display_hook.py", "statusline.py"];
/// Its directories: the venv and Python's byte-code cache.
pub const LEGACY_DIRS: [&str; 2] = [".venv", "__pycache__"];

/// Whether `install_dir` still holds any of the Python install's files.
pub fn has_legacy(install_dir: &Path) -> bool {
    LEGACY_FILES.iter().any(|f| install_dir.join(f).is_file())
        || LEGACY_DIRS.iter().any(|d| install_dir.join(d).is_dir())
        || LEGACY_HOOKS
            .iter()
            .any(|h| install_dir.join("hooks").join(h).is_file())
}

/// Removes the Python install's files from `install_dir`. Returns what went, as paths
/// relative to it, and a line for each that could not.
pub fn remove_legacy(install_dir: &Path) -> (Vec<String>, Vec<String>) {
    let (mut removed, mut failed) = (Vec::new(), Vec::new());
    let mut take = |rel: String, path: PathBuf, dir: bool| {
        let result = if dir {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed.push(rel),
            Err(e) => failed.push(format!("{}: {e}", path.display())),
        }
    };
    for f in LEGACY_FILES {
        let p = install_dir.join(f);
        if p.is_file() {
            take(f.to_string(), p, false);
        }
    }
    let hooks = install_dir.join("hooks");
    for h in LEGACY_HOOKS {
        let p = hooks.join(h);
        if p.is_file() {
            take(format!("hooks/{h}"), p, false);
        }
    }
    let cache = hooks.join("__pycache__");
    if cache.is_dir() {
        take("hooks/__pycache__".to_string(), cache, true);
    }
    for d in LEGACY_DIRS {
        let p = install_dir.join(d);
        if p.is_dir() {
            take(d.to_string(), p, true);
        }
    }
    // Only when nothing of the user's is left in it.
    if hooks.is_dir() && fs::read_dir(&hooks).is_ok_and(|mut d| d.next().is_none()) {
        let _ = fs::remove_dir(&hooks);
    }
    (removed, failed)
}

/// How the install dir went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// Deleted.
    Deleted,
    /// Everything but the running binary is gone; the rest goes once this process exits.
    AfterExit,
}

/// Deletes the install dir. When the running binary is the installed copy on Windows,
/// which cannot delete a running binary, everything else goes now and the dir is handed
/// to [`Host::remove_after_exit`].
pub fn delete_install_dir(dir: &Path, host: &dyn Host) -> io::Result<Removal> {
    delete_install_dir_with(dir, host.current_exe().as_deref(), host, cfg!(windows))
}

/// [`delete_install_dir`] with the running binary given, and whether a running binary
/// is locked (Windows).
pub fn delete_install_dir_with(
    dir: &Path,
    running: Option<&Path>,
    host: &dyn Host,
    running_is_locked: bool,
) -> io::Result<Removal> {
    let inside = match (running.map(dunce::canonicalize), dunce::canonicalize(dir)) {
        (Some(Ok(exe)), Ok(dir)) if exe.starts_with(&dir) => Some(exe),
        _ => None,
    };
    if running_is_locked && let Some(exe) = inside {
        remove_all_except(&dunce::canonicalize(dir)?, &exe)?;
        host.remove_after_exit(dir)?;
        return Ok(Removal::AfterExit);
    }
    fs::remove_dir_all(dir)?;
    Ok(Removal::Deleted)
}

fn remove_all_except(dir: &Path, keep: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path == keep {
            continue;
        }
        if keep.starts_with(&path) {
            remove_all_except(&path, keep)?;
        } else if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Starts a hidden, detached PowerShell that waits for this process to exit and then
/// deletes `dir`. Chosen over `cmd /c` with a fixed delay: it waits for the actual exit
/// however long the last screen stays up, and the path travels as a single-quoted
/// literal inside `-EncodedCommand`, so no character in it can break the quoting.
#[cfg(windows)]
pub fn remove_after_exit(dir: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let script = removal_script(std::process::id(), dir);
    let encoded = base64(
        &script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<u8>>(),
    );
    let powershell = std::env::var_os("SystemRoot")
        .map(|root| {
            PathBuf::from(root)
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("powershell.exe"));
    let spawn = |flags: u32| {
        Command::new(&powershell)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                &encoded,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    // A terminal that runs us in a job object would kill the helper with us; breaking
    // away is refused by some jobs, and then it is tried without.
    let base = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    spawn(base | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| spawn(base))
        .map(|_| ())
}

/// Elsewhere a running binary can be deleted, so the dir goes at once.
#[cfg(not(windows))]
pub fn remove_after_exit(dir: &Path) -> io::Result<()> {
    fs::remove_dir_all(dir)
}

/// The PowerShell that [`remove_after_exit`] runs.
pub fn removal_script(pid: u32, dir: &Path) -> String {
    let lit = dir.to_string_lossy().replace('\'', "''");
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n\
         Wait-Process -Id {pid} -Timeout 3600\n\
         for ($i = 0; $i -lt 20 -and (Test-Path -LiteralPath '{lit}'); $i++) {{\n\
         \x20   Start-Sleep -Milliseconds 500\n\
         \x20   Remove-Item -LiteralPath '{lit}' -Recurse -Force\n\
         }}\n"
    )
}

/// Standard base64 with padding.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn removal_script_quotes_the_path() {
        let s = removal_script(42, Path::new("C:/it's here"));
        assert!(s.contains("Wait-Process -Id 42"));
        assert!(s.contains("-LiteralPath 'C:/it''s here'"));
    }

    #[test]
    fn copy_replaces_the_installed_binary_and_skips_itself() {
        let tmp = tempfile::tempdir().expect("tmp");
        let src = tmp.path().join("src.exe");
        fs::write(&src, b"one").expect("write");
        let install = tmp.path().join("install");
        assert_eq!(
            copy_binary(&src, &install).expect("copy"),
            CopyOutcome::Copied
        );
        fs::write(&src, b"two").expect("write");
        assert_eq!(
            copy_binary(&src, &install).expect("copy"),
            CopyOutcome::Copied
        );
        let dest = binary_path(&install);
        assert_eq!(fs::read(&dest).expect("read"), b"two");
        assert_eq!(
            copy_binary(&dest, &install).expect("copy"),
            CopyOutcome::AlreadyInPlace
        );
        let names: Vec<String> = fs::read_dir(install.join("bin"))
            .expect("bin")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [exe_name()]);
        let missing = copy_binary(&tmp.path().join("nope"), &install).expect_err("missing");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn legacy_files_go_and_the_users_stay() {
        let tmp = tempfile::tempdir().expect("tmp");
        let d = tmp.path();
        for f in LEGACY_FILES {
            fs::write(d.join(f), "#").expect("write");
        }
        fs::create_dir_all(d.join(".venv").join("Scripts")).expect("mkdir");
        fs::write(d.join(".venv").join("Scripts").join("python.exe"), "").expect("write");
        fs::create_dir_all(d.join("hooks")).expect("mkdir");
        for h in LEGACY_HOOKS {
            fs::write(d.join("hooks").join(h), "#").expect("write");
        }
        fs::write(d.join("hooks").join("mine.sh"), "#").expect("write");
        fs::write(d.join("models.json"), "{}").expect("write");
        assert!(has_legacy(d));
        let (removed, failed) = remove_legacy(d);
        assert!(failed.is_empty(), "{failed:?}");
        assert!(removed.contains(&"server.py".to_string()));
        assert!(removed.contains(&".venv".to_string()));
        assert!(removed.contains(&"hooks/statusline.py".to_string()));
        assert!(!has_legacy(d));
        assert!(d.join("hooks").join("mine.sh").is_file());
        assert!(d.join("models.json").is_file());
        fs::remove_file(d.join("hooks").join("mine.sh")).expect("rm");
        fs::write(d.join("server.py"), "#").expect("write");
        remove_legacy(d);
        assert!(!d.join("hooks").exists());
    }
}
