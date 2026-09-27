//! [`Backend`] on the real machine, through consult-install, consult-core and
//! consult-service. Nothing here decides anything the install flow decides: a change is
//! a re-run of that flow with answers given up front.

use std::path::{Path, PathBuf};
use std::time::Duration;

use consult_core::catalog::{self, check_catalog, check_report, fetch_full_listing};
use consult_core::display::{load_progress_style, load_summary_style};
use consult_core::generate::{Display, load_manifest};
use consult_core::key::{KEY_VAR, KeyCheck, clean_key, mask_key, settings_key};
use consult_core::listing::reviewable_models_text;
use consult_core::models::load_models;
use consult_core::openrouter::Client;
use consult_core::paths::{binary_path, state_dir};
use consult_install::{
    InstallError, InstallOptions, InstallOutcome, Network, PanelAnswer, PanelRequest, RealNetwork,
    Transport, Ui, UninstallOptions, UninstallOutcome, is_install_dir, mcp_add_command, same_dir,
    uninstall_with,
};
use consult_service::{
    DEFAULT_HOST, DEFAULT_PORT, Registration, ServiceError, ServiceSpec, ServiceStatus, Task,
    elevated_success, explain_interactive_fallback,
};
use consult_tui::StepKind;
use indexmap::IndexMap;
use serde_json::Value;

use crate::actions::{
    Backend, CatalogVerdict, KeyInfo, KeySource, PanelMember, Reinstall, Secret, ServiceAction,
    Sink, StatusReport, TransportState,
};
use crate::sessions::{self, Session};

/// The real machine, for one install dir and Claude dir.
#[derive(Clone, Debug)]
pub struct RealBackend {
    install_dir: PathBuf,
    claude_dir: PathBuf,
}

impl RealBackend {
    /// A backend acting on these dirs (already resolved and absolute).
    pub fn new(install_dir: PathBuf, claude_dir: PathBuf) -> Self {
        Self {
            install_dir,
            claude_dir,
        }
    }

    fn port(&self) -> u16 {
        consult_service::registered_port().unwrap_or(DEFAULT_PORT)
    }

    fn service_status(&self) -> Result<ServiceStatus, String> {
        // Processes are looked for in this install, not the task's dir: the screen is
        // about this install, and another install's server is not ours to show.
        Task::default()
            .status_in(None, std::slice::from_ref(&self.install_dir))
            .map_err(|e| e.to_string())
    }
}

/// The transport an install is using, read from the task: registered for this dir is
/// the service; anything else is stdio.
pub fn transport_state(
    status: &Result<ServiceStatus, String>,
    install_dir: &Path,
) -> TransportState {
    match status {
        Ok(s) if s.registered => match &s.working_dir {
            Some(dir) if !same_dir(dir, install_dir) => TransportState::OtherInstall {
                dir: dir.clone(),
                port: s.port,
            },
            _ => TransportState::Service { port: s.port },
        },
        _ => TransportState::Stdio,
    }
}

/// Where the server's key comes from, as `load_api_key` looks: the environment first.
pub fn key_info(env_value: Option<&str>, claude_dir: &Path) -> KeyInfo {
    let env = env_value.map(clean_key).filter(|k| !k.is_empty());
    let settings = settings_key(claude_dir);
    match (env, settings) {
        (Some(e), s) => KeyInfo {
            source: KeySource::EnvVar,
            masked: Some(mask_key(e)),
            shadowed: s.filter(|s| s != e).map(|s| mask_key(&s)),
        },
        (None, Some(s)) => KeyInfo {
            source: KeySource::Settings,
            masked: Some(mask_key(&s)),
            shadowed: None,
        },
        (None, None) => KeyInfo {
            source: KeySource::NotFound,
            masked: None,
            shadowed: None,
        },
    }
}

/// The installed panel as the flow's `--panel` takes it: a favourite by its alias, an
/// outside model by its id (its registry alias is only its command).
pub fn installed_panel(install_dir: &Path) -> Result<Vec<String>, String> {
    let registry = load_models(install_dir).map_err(|e| e.to_string())?;
    let catalog = catalog::embedded().map_err(|e| e.to_string())?;
    let panel: Vec<String> = registry
        .default_panel
        .iter()
        .map(|alias| {
            if catalog.models.contains_key(alias) {
                alias.clone()
            } else {
                registry
                    .models
                    .get(alias)
                    .and_then(|m| m.id.clone())
                    .unwrap_or_else(|| alias.clone())
            }
        })
        .collect();
    if panel.is_empty() {
        return Err("models.json names no panel".to_string());
    }
    Ok(panel)
}

impl Backend for RealBackend {
    fn status(&self) -> StatusReport {
        let service = self.service_status();
        let transport = transport_state(&service, &self.install_dir);
        let (panel, priced_at) = match load_models(&self.install_dir) {
            Ok(r) => (
                Ok(r.default_panel
                    .iter()
                    .map(|a| PanelMember {
                        alias: a.clone(),
                        id: r.models.get(a).and_then(|m| m.id.clone()),
                    })
                    .collect()),
                r.priced_at,
            ),
            Err(e) => (Err(e.to_string()), None),
        };
        let manifest = load_manifest(&self.install_dir);
        StatusReport {
            version: env!("CARGO_PKG_VERSION").to_string(),
            install_dir: self.install_dir.clone(),
            claude_dir: self.claude_dir.clone(),
            installed: is_install_dir(&self.install_dir),
            manifest_date: manifest
                .get("generated_on")
                .and_then(Value::as_str)
                .map(str::to_string),
            key: key_info(std::env::var(KEY_VAR).ok().as_deref(), &self.claude_dir),
            panel,
            priced_at,
            display: Display {
                progress: load_progress_style(&self.install_dir),
                summary: load_summary_style(&self.install_dir),
            },
            stdio_hint: mcp_add_command(
                Transport::Stdio,
                DEFAULT_PORT,
                &binary_path(&self.install_dir),
            ),
            service,
            transport,
        }
    }

    fn fetch_listing(&self) -> Option<IndexMap<String, Value>> {
        let (body, _) = RealNetwork::new().fetch_listing()?;
        let models = reviewable_models_text(&body);
        (!models.is_empty()).then_some(models)
    }

    fn check_key(&self, key: &Secret) -> KeyCheck {
        RealNetwork::new().check_key(key.expose())
    }

    fn reinstall(&self, request: &Reinstall, log: Sink<'_>) -> Result<String, String> {
        let panel = match &request.panel {
            Some(p) => p.clone(),
            None => installed_panel(&self.install_dir)
                .map_err(|e| format!("Cannot read the installed panel: {e}"))?,
        };
        let service = self.service_status();
        let transport = match transport_state(&service, &self.install_dir) {
            TransportState::Service { .. } => Transport::Service,
            // Re-pointing another install's task is a decision for `install`, not a
            // side effect of changing the panel here.
            TransportState::OtherInstall { .. } | TransportState::Stdio => Transport::Stdio,
        };
        let opts = InstallOptions {
            unattended: false,
            panel: Some(panel),
            install_dir: Some(self.install_dir.clone()),
            claude_dir: Some(self.claude_dir.clone()),
            port: self.port(),
            // The key screen checks a new key itself, before this runs.
            skip_key_check: true,
            skip_service: false,
            // A panel or key change must not raise an administrator prompt nobody
            // asked for; the service screen's Re-register is where that happens.
            no_elevate: true,
            skip_mcp_registration: false,
            progress_style: request.progress,
            summary_style: request.summary,
            transport,
            ..InstallOptions::default()
        };
        let mut ui = JobUi::new(log, request.key.clone(), request.allow_unusual_key);
        match consult_install::install(&opts, &mut ui) {
            Ok(InstallOutcome::Installed(done)) => Ok(format!(
                "Re-installed: panel {}, progress {}, summary {}",
                done.report.panel.join(", "),
                done.report.display.progress,
                done.report.display.summary
            )),
            Ok(InstallOutcome::Cancelled { .. }) => Err("The install was declined.".to_string()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn service_supported(&self) -> bool {
        cfg!(windows)
    }

    fn needs_elevation(&self) -> bool {
        cfg!(windows) && !consult_service::is_elevated()
    }

    fn service_action(&self, action: ServiceAction, log: Sink<'_>) -> Result<String, String> {
        let port = self.port();
        let dirs = std::slice::from_ref(&self.install_dir);
        let start = |log: &mut dyn FnMut(StepKind, &str)| -> Result<String, String> {
            log(StepKind::Info, "Starting the service ...");
            match consult_service::start(port) {
                Ok(true) => Ok(format!("Listening on 127.0.0.1:{port}")),
                Ok(false) => Err(format!("Nothing is listening on port {port} yet.")),
                Err(e) => Err(format!("Could not start the task: {e}")),
            }
        };
        match action {
            ServiceAction::Start => start(log),
            ServiceAction::Stop => match consult_service::stop(dirs) {
                Ok(pids) if pids.is_empty() => Ok("Stopped; no server process was running.".into()),
                Ok(pids) => Ok(format!(
                    "Stopped; ended pid {}",
                    pids.iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Err(e) => Err(format!("Could not stop the service: {e}")),
            },
            ServiceAction::Restart => {
                if let Err(e) = consult_service::stop(dirs) {
                    log(StepKind::Note, &format!("Could not stop the service: {e}"));
                } else {
                    log(StepKind::Ok, "Stopped");
                }
                std::thread::sleep(Duration::from_millis(500));
                start(log)
            }
            ServiceAction::Register | ServiceAction::RegisterElevated => {
                let spec = ServiceSpec {
                    exe: binary_path(&self.install_dir),
                    working_dir: self.install_dir.clone(),
                    port,
                    host: DEFAULT_HOST.to_string(),
                    account: None,
                };
                if action == ServiceAction::RegisterElevated {
                    log(StepKind::Info, consult_service::ELEVATE_NOTICE);
                    // The running binary, never the copy in bin/: an older copy would
                    // not know the flags the elevated run is given.
                    let exe = std::env::current_exe().unwrap_or_else(|_| spec.exe.clone());
                    match consult_service::install_elevated(&spec, &exe) {
                        Ok(()) => match consult_service::registered() {
                            Some(task) if task.is_s4u() => {
                                let account = task
                                    .runs_as()
                                    .or_else(|| consult_service::current_account().ok())
                                    .unwrap_or_else(|| "you".to_string());
                                log(StepKind::Ok, &elevated_success(&account));
                                return start(log);
                            }
                            _ => log(
                                StepKind::Note,
                                "The elevated run finished, but the task is not registered for boot; registering it for logon instead.",
                            ),
                        },
                        Err(ServiceError::ElevationCancelled) => log(
                            StepKind::Note,
                            "The administrator prompt was cancelled; registering the task for logon only.",
                        ),
                        Err(e) => log(
                            StepKind::Note,
                            &format!("{e}; registering the task for logon only."),
                        ),
                    }
                }
                match consult_service::install(&spec) {
                    Ok(Registration::S4U) => log(
                        StepKind::Ok,
                        "Registered the task (starts at boot and at logon, as you)",
                    ),
                    Ok(Registration::InteractiveLogonOnly { reason }) => log(
                        StepKind::Note,
                        &explain_interactive_fallback(&reason, &self.install_dir),
                    ),
                    Err(e) => return Err(format!("Registering the task failed: {e}")),
                }
                start(log)
            }
            ServiceAction::Unregister => consult_service::uninstall()
                .map(|()| "Removed the scheduled task.".to_string())
                .map_err(|e| format!("Could not remove the task: {e}")),
        }
    }

    fn catalog_check(&self) -> CatalogVerdict {
        let catalog = match catalog::embedded() {
            Ok(c) => c,
            Err(e) => {
                return CatalogVerdict {
                    lines: vec![format!("!!  the built-in catalog is invalid: {e}")],
                    code: 2,
                };
            }
        };
        // A runtime of its own, on this job's thread: nothing else here is async.
        let outcome = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt
                .block_on(fetch_full_listing(&Client::new()))
                .map_err(|e| e.to_string())
                .and_then(|body| check_catalog(&catalog, &body)),
            Err(e) => Err(e.to_string()),
        };
        let (text, code) = check_report(&catalog, outcome, false);
        CatalogVerdict {
            lines: text.lines().map(str::to_string).collect(),
            code,
        }
    }

    fn sessions(&self) -> Result<Vec<Session>, String> {
        sessions::list_sessions(&state_dir(&self.install_dir)).map_err(|e| e.to_string())
    }

    fn delete_session(&self, id: &str) -> Result<usize, String> {
        sessions::delete_session(&state_dir(&self.install_dir), id).map_err(|e| e.to_string())
    }

    fn uninstall(&self, remove_key: bool, log: Sink<'_>) -> Result<String, String> {
        let opts = UninstallOptions {
            install_dir: Some(self.install_dir.clone()),
            claude_dir: Some(self.claude_dir.clone()),
            remove_key,
            // Both confirmations and the key question were asked on the screen.
            yes: true,
            skip_mcp_registration: false,
        };
        let mut ui = JobUi::new(log, None, false);
        match uninstall_with(&opts, &mut ui, &consult_install::System::real()) {
            Ok(UninstallOutcome::Uninstalled(_)) => {
                Ok("claude-consult is uninstalled.".to_string())
            }
            Ok(UninstallOutcome::Cancelled { .. }) => {
                Err("The uninstall was declined.".to_string())
            }
            Err(e) => Err(e.to_string()),
        }
    }
}

// ---- the Ui the flows run with ----------------------------------------------------------

/// How [`JobUi`] answers a question of the flow. Every answer was settled on screen
/// before the flow started, so nothing is asked twice.
pub fn answer(question: &str, rotating_key: bool, allow_unusual_key: bool) -> bool {
    if question.starts_with("Use the key from") {
        // Rotating: the offered existing keys are what is being replaced.
        return !rotating_key;
    }
    if question.starts_with("Use it anyway?") {
        // The installed key is kept as it is; a new one only if the user said so.
        return !rotating_key || allow_unusual_key;
    }
    if question.starts_with("Save it without checking?") {
        // Never reached with skip_key_check; checking was the key screen's business.
        return true;
    }
    // "Proceed?", "Continue?", "Continue anyway?" (the install already runs this way),
    // "Keep this panel anyway?" (the picker showed the same-lab warning).
    true
}

/// A [`Ui`] that sends every line to the job's log and answers from what was settled
/// on screen. The key, when rotating, is handed over once through
/// [`Ui::enter_key`], the flow's own way in; it is never reported.
struct JobUi<'a> {
    log: Sink<'a>,
    key: Option<Secret>,
    rotating: bool,
    allow_unusual_key: bool,
}

impl<'a> JobUi<'a> {
    fn new(log: Sink<'a>, key: Option<Secret>, allow_unusual_key: bool) -> Self {
        Self {
            log,
            rotating: key.is_some(),
            key,
            allow_unusual_key,
        }
    }
}

impl Ui for JobUi<'_> {
    fn report_step(&mut self, kind: StepKind, text: &str) {
        (self.log)(kind, text);
    }

    fn busy(&mut self, what: &str) {
        (self.log)(StepKind::Info, &format!("{what} ..."));
    }

    fn confirm(&mut self, question: &str, _default: bool) -> Result<bool, InstallError> {
        let yes = answer(question, self.rotating, self.allow_unusual_key);
        (self.log)(
            StepKind::Info,
            &format!("{question} {}", if yes { "yes" } else { "no" }),
        );
        Ok(yes)
    }

    fn enter_key(&mut self, _prompt: &str) -> Result<String, InstallError> {
        match self.key.take() {
            Some(k) => Ok(k.expose().to_string()),
            None => {
                let why = if self.rotating {
                    "The new key was not accepted; nothing was changed."
                } else {
                    "No usable key in settings.json: rotate the key on the Key screen first."
                };
                (self.log)(StepKind::Fail, why);
                Err(InstallError::Stopped(why.to_string()))
            }
        }
    }

    fn choose_panel(&mut self, _request: &PanelRequest<'_>) -> Result<PanelAnswer, InstallError> {
        // The panel is always passed as --panel; reaching the picker means it was empty.
        Ok(PanelAnswer::Cancelled)
    }

    fn status_line_question(&mut self, theirs: &str) -> Result<bool, InstallError> {
        // A foreign status line was kept at install time (else it would be ours now).
        (self.log)(
            StepKind::Info,
            &format!("Keeping your status line: {theirs}"),
        );
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_keep_the_install_as_it_is() {
        assert!(answer(
            "Use the key from your Claude Code settings (sk-...)?",
            false,
            false
        ));
        assert!(!answer(
            "Use the key from your Claude Code settings (sk-...)?",
            true,
            false
        ));
        assert!(!answer(
            "Use the key from the OPENROUTER_API_KEY environment variable (x)?",
            true,
            true
        ));
        assert!(answer("Use it anyway?", false, false));
        assert!(!answer("Use it anyway?", true, false));
        assert!(answer("Use it anyway?", true, true));
        assert!(answer("Proceed?", true, false));
        assert!(answer("Keep this panel anyway?", false, false));
    }

    #[test]
    fn key_info_prefers_the_environment() {
        let dir = tempfile::tempdir().expect("tmp");
        let none = key_info(None, dir.path());
        assert_eq!(none.source, KeySource::NotFound);
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"env": {"OPENROUTER_API_KEY": "sk-or-v1-aaaaaaaaaaaaaaaaaaaa1111"}}"#,
        )
        .expect("write");
        let s = key_info(None, dir.path());
        assert_eq!(s.source, KeySource::Settings);
        assert_eq!(s.masked.as_deref(), Some("sk-or-v1-...1111"));
        let e = key_info(Some(" sk-or-v1-bbbbbbbbbbbbbbbbbbbb2222 "), dir.path());
        assert_eq!(e.source, KeySource::EnvVar);
        assert_eq!(e.masked.as_deref(), Some("sk-or-v1-...2222"));
        assert_eq!(e.shadowed.as_deref(), Some("sk-or-v1-...1111"));
    }

    #[test]
    fn transport_follows_the_task() {
        let dir = tempfile::tempdir().expect("tmp");
        let mut s = ServiceStatus {
            supported: true,
            registered: true,
            state: None,
            runs_as: None,
            logon_type: None,
            triggers: Vec::new(),
            port: 8766,
            listening: false,
            processes: Vec::new(),
            working_dir: Some(dir.path().to_path_buf()),
        };
        assert_eq!(
            transport_state(&Ok(s.clone()), dir.path()),
            TransportState::Service { port: 8766 }
        );
        s.working_dir = Some(dir.path().join("other"));
        assert!(matches!(
            transport_state(&Ok(s.clone()), dir.path()),
            TransportState::OtherInstall { port: 8766, .. }
        ));
        s.registered = false;
        assert_eq!(transport_state(&Ok(s), dir.path()), TransportState::Stdio);
        assert_eq!(
            transport_state(&Err("x".into()), dir.path()),
            TransportState::Stdio
        );
    }

    #[test]
    fn installed_panel_names_outsiders_by_id() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(installed_panel(dir.path()).is_err());
        std::fs::write(
            dir.path().join("models.json"),
            r#"{"default_panel": ["glm-5.2", "mistral-medium-3-1"],
                "models": {"glm-5.2": {"id": "z-ai/glm-5.2"},
                           "mistral-medium-3-1": {"id": "mistralai/mistral-medium-3.1"}}}"#,
        )
        .expect("write");
        assert_eq!(
            installed_panel(dir.path()).expect("panel"),
            ["glm-5.2", "mistralai/mistral-medium-3.1"]
        );
    }
}
