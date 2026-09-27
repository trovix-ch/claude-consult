//! What an install or an uninstall is asked to do.

use std::path::{Path, PathBuf};

use consult_core::display::{ProgressStyle, SummaryStyle};
use consult_core::paths;
use consult_service::DEFAULT_PORT;

/// How Claude Code reaches the MCP server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transport {
    /// One shared service (a scheduled task) on `http://127.0.0.1:<port>/mcp`. Windows
    /// only; the default there.
    Service,
    /// Claude Code starts `claude-consult serve` per session over stdio. The default
    /// everywhere else.
    Stdio,
}

impl Default for Transport {
    fn default() -> Self {
        if cfg!(windows) {
            Self::Service
        } else {
            Self::Stdio
        }
    }
}

impl Transport {
    /// The name on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Stdio => "stdio",
        }
    }

    /// The transport with this name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "service" => Some(Self::Service),
            "stdio" => Some(Self::Stdio),
            _ => None,
        }
    }
}

/// Everything `claude-consult install` takes. [`Default`] is a plain interactive
/// install of the running binary into the default places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOptions {
    /// No prompts: the key from `OPENROUTER_API_KEY` (or the settings), the recommended
    /// panel unless `panel`, a foreign status line kept, every question its default.
    pub unattended: bool,
    /// Favourites' aliases or OpenRouter ids; entries may themselves hold commas. Skips
    /// the picker.
    pub panel: Option<Vec<String>>,
    /// The install dir; `None` resolves as [`paths::install_dir`] does.
    pub install_dir: Option<PathBuf>,
    /// Claude Code's config dir; `None` resolves as [`paths::claude_dir`] does.
    pub claude_dir: Option<PathBuf>,
    /// The service's port.
    pub port: u16,
    /// Do not check the key against OpenRouter (offline installs).
    pub skip_key_check: bool,
    /// Leave the scheduled task alone.
    pub skip_service: bool,
    /// When start-at-boot is refused for want of administrator rights, keep the
    /// logon-only task instead of going to the administrator prompt.
    pub no_elevate: bool,
    /// Do not run `claude mcp add`.
    pub skip_mcp_registration: bool,
    /// A progress style to set; `None` keeps the installed one.
    pub progress_style: Option<ProgressStyle>,
    /// A summary style to set; `None` keeps the installed one.
    pub summary_style: Option<SummaryStyle>,
    /// How Claude Code reaches the server.
    pub transport: Transport,
    /// The binary copied to `<install_dir>/bin`: by default the one running.
    pub source_exe: PathBuf,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            unattended: false,
            panel: None,
            install_dir: None,
            claude_dir: None,
            port: DEFAULT_PORT,
            skip_key_check: false,
            skip_service: false,
            no_elevate: false,
            skip_mcp_registration: false,
            progress_style: None,
            summary_style: None,
            transport: Transport::default(),
            source_exe: std::env::current_exe().unwrap_or_default(),
        }
    }
}

impl InstallOptions {
    /// The panel entries, commas split and blanks dropped, as `-Panel a,b -Panel c` was.
    pub fn panel_entries(&self) -> Option<Vec<String>> {
        let entries: Vec<String> = self
            .panel
            .as_ref()?
            .iter()
            .flat_map(|p| p.split(','))
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        (!entries.is_empty()).then_some(entries)
    }

    /// The install dir, absolute.
    pub fn resolved_install_dir(&self) -> PathBuf {
        absolute(&paths::install_dir(self.install_dir.as_deref()))
    }

    /// The Claude dir, absolute.
    pub fn resolved_claude_dir(&self) -> PathBuf {
        absolute(&paths::claude_dir(self.claude_dir.as_deref()))
    }
}

/// Everything `claude-consult uninstall` takes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UninstallOptions {
    /// The install dir; `None` resolves as [`paths::install_dir`] does.
    pub install_dir: Option<PathBuf>,
    /// Claude Code's config dir; `None` resolves as [`paths::claude_dir`] does.
    pub claude_dir: Option<PathBuf>,
    /// Also delete `OPENROUTER_API_KEY` from settings.json, without asking.
    pub remove_key: bool,
    /// Ask nothing: every question takes its default (the key stays unless
    /// `remove_key`).
    pub yes: bool,
    /// Leave the MCP registration alone.
    pub skip_mcp_registration: bool,
}

impl UninstallOptions {
    /// The install dir, absolute.
    pub fn resolved_install_dir(&self) -> PathBuf {
        absolute(&paths::install_dir(self.install_dir.as_deref()))
    }

    /// The Claude dir, absolute.
    pub fn resolved_claude_dir(&self) -> PathBuf {
        absolute(&paths::claude_dir(self.claude_dir.as_deref()))
    }
}

/// `path` made absolute and `.`/`..` collapsed, without touching the disk (the dir may
/// not exist yet), as .NET's `GetFullPath`.
pub fn absolute(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    consult_core::util::normalize_lexically(&abs)
}

/// Whether two dirs are the same one: compared resolved as far as they exist, without a
/// trailing separator, and ignoring case on Windows.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = consult_core::util::resolve_lenient(p)
            .to_string_lossy()
            .replace('\\', "/");
        let s = s.trim_end_matches('/').to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_entries_split_commas_and_drop_blanks() {
        let o = InstallOptions {
            panel: Some(vec!["a, b".into(), " ".into(), "c,,".into()]),
            ..InstallOptions::default()
        };
        assert_eq!(
            o.panel_entries(),
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        let o = InstallOptions {
            panel: Some(vec![" , ".into()]),
            ..InstallOptions::default()
        };
        assert_eq!(o.panel_entries(), None);
    }

    #[test]
    fn same_dir_ignores_trailing_separators() {
        let tmp = tempfile::tempdir().expect("tmp");
        let a = tmp.path().join("x");
        let mut b = a.clone().into_os_string();
        b.push(std::path::MAIN_SEPARATOR_STR);
        assert!(same_dir(&a, Path::new(&b)));
        assert!(!same_dir(&a, &tmp.path().join("y")));
        assert!(same_dir(&a, &tmp.path().join("z").join("..").join("x")));
        assert_eq!(Transport::parse("stdio"), Some(Transport::Stdio));
        assert_eq!(Transport::Service.as_str(), "service");
    }
}
