//! Where the install and Claude Code's config live.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The binary's name without extension.
pub const BIN_NAME: &str = "claude-consult";
/// Environment variable naming the install dir.
pub const INSTALL_DIR_ENV: &str = "CLAUDE_CONSULT_DIR";
/// Environment variable Claude Code honours for its config dir.
pub const CLAUDE_CONFIG_ENV: &str = "CLAUDE_CONFIG_DIR";
/// The reviewer registry the server reads.
pub const MODELS_FILE: &str = "models.json";
/// Progress and summary styles.
pub const DISPLAY_FILE: &str = "display.json";
/// What an install wrote.
pub const MANIFEST_FILE: &str = "manifest.json";

/// The binary's file name on this platform: `claude-consult.exe` on Windows.
pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "claude-consult.exe"
    } else {
        "claude-consult"
    }
}

/// `<install_dir>/bin/claude-consult[.exe]`, the copy hooks and the service run.
pub fn binary_path(install_dir: &Path) -> PathBuf {
    install_dir.join("bin").join(exe_name())
}

/// `<install_dir>/models.json`.
pub fn models_path(install_dir: &Path) -> PathBuf {
    install_dir.join(MODELS_FILE)
}

/// `<install_dir>/display.json`.
pub fn display_path(install_dir: &Path) -> PathBuf {
    install_dir.join(DISPLAY_FILE)
}

/// `<install_dir>/manifest.json`.
pub fn manifest_path(install_dir: &Path) -> PathBuf {
    install_dir.join(MANIFEST_FILE)
}

/// `<install_dir>/state`, per-session consult totals.
pub fn state_dir(install_dir: &Path) -> PathBuf {
    install_dir.join("state")
}

/// `<claude_dir>/settings.json`.
pub fn settings_path(claude_dir: &Path) -> PathBuf {
    claude_dir.join("settings.json")
}

/// The platform default install dir: `%LOCALAPPDATA%\claude-consult` on Windows,
/// `$XDG_DATA_HOME/claude-consult` (default `~/.local/share/claude-consult`) elsewhere.
pub fn default_install_dir() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::data_local_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("AppData").join("Local")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(BIN_NAME)
    }
    #[cfg(not(windows))]
    {
        let xdg = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        xdg.or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(BIN_NAME)
    }
}

/// The install dir: `explicit`, else `CLAUDE_CONSULT_DIR`, else the dir whose `bin/`
/// holds the running binary, else the platform default.
pub fn install_dir(explicit: Option<&Path>) -> PathBuf {
    install_dir_from(
        explicit,
        std::env::var_os(INSTALL_DIR_ENV),
        std::env::current_exe().ok(),
    )
}

/// [`install_dir`] with its inputs given, so the order can be tested.
///
/// The running binary only names the install when its `<X>/bin/` parent also holds an
/// install's `models.json` or `manifest.json`: `cargo install` puts the binary at
/// `~/.cargo/bin/claude-consult`, which has the same shape and is no install.
pub fn install_dir_from(
    explicit: Option<&Path>,
    env: Option<OsString>,
    current_exe: Option<PathBuf>,
) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    if let Some(dir) = env.filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = current_exe.as_deref().and_then(install_of_binary) {
        return dir;
    }
    default_install_dir()
}

fn install_of_binary(exe: &Path) -> Option<PathBuf> {
    let exe = dunce::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let name = exe.file_name()?.to_string_lossy().to_string();
    let matches = if cfg!(windows) {
        name.eq_ignore_ascii_case(exe_name()) || name.eq_ignore_ascii_case(BIN_NAME)
    } else {
        name == BIN_NAME || name == "claude-consult.exe"
    };
    let bin = exe.parent()?;
    let bin_ok = bin
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("bin"));
    let root = bin.parent()?;
    let is_install = root.join(MODELS_FILE).is_file() || root.join(MANIFEST_FILE).is_file();
    (matches && bin_ok && is_install).then(|| root.to_path_buf())
}

/// Claude Code's config dir: `explicit`, else `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir(explicit: Option<&Path>) -> PathBuf {
    claude_dir_from(
        explicit,
        std::env::var_os(CLAUDE_CONFIG_ENV),
        dirs::home_dir(),
    )
}

/// [`claude_dir`] with its inputs given.
pub fn claude_dir_from(
    explicit: Option<&Path>,
    env: Option<OsString>,
    home: Option<PathBuf>,
) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    // Claude Code itself honours CLAUDE_CONFIG_DIR; hardcoding ~/.claude would miss
    // the key and the settings on such hosts.
    if let Some(dir) = env.filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    home.unwrap_or_else(|| PathBuf::from(".")).join(".claude")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_dir_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let explicit = tmp.path().join("explicit");
        let env = tmp.path().join("env");
        let install = tmp.path().join("inst");
        std::fs::create_dir_all(install.join("bin")).expect("mkdir");
        let exe = install.join("bin").join(exe_name());
        std::fs::write(&exe, b"").expect("write");

        assert_eq!(
            install_dir_from(Some(&explicit), Some(env.clone().into()), Some(exe.clone())),
            explicit
        );
        assert_eq!(
            install_dir_from(None, Some(env.clone().into()), Some(exe.clone())),
            env
        );
        // A bare bin/ is not an install (cargo's own bin/ has this shape).
        assert_eq!(
            install_dir_from(None, None, Some(exe.clone())),
            default_install_dir()
        );
        std::fs::write(install.join(MODELS_FILE), b"{}").expect("write");
        let found = install_dir_from(None, Some(OsString::new()), Some(exe));
        assert_eq!(
            dunce::canonicalize(found).expect("canon"),
            dunce::canonicalize(&install).expect("canon")
        );
        let elsewhere = tmp.path().join("other").join(exe_name());
        assert_eq!(
            install_dir_from(None, None, Some(elsewhere)),
            default_install_dir()
        );
    }

    #[test]
    fn claude_dir_order() {
        let home = PathBuf::from("home");
        assert_eq!(
            claude_dir_from(Some(Path::new("x")), Some("y".into()), Some(home.clone())),
            PathBuf::from("x")
        );
        assert_eq!(
            claude_dir_from(None, Some("y".into()), Some(home.clone())),
            PathBuf::from("y")
        );
        assert_eq!(
            claude_dir_from(None, None, Some(home.clone())),
            home.join(".claude")
        );
        assert_eq!(
            settings_path(Path::new("c")),
            Path::new("c").join("settings.json")
        );
    }
}
