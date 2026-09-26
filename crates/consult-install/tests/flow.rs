//! The install, upgrade and uninstall flows, end to end, with a scripted UI and fakes
//! for the scheduled task, the `claude` CLI, the network and the host.
//!
//! Every run works in a temp dir: nothing here runs schtasks or claude, fetches
//! anything, or touches the real Claude dir or install dir.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use consult_core::display::{ProgressStyle, SummaryStyle};
use consult_core::generate::Display;
use consult_core::key::KeyCheck;
use consult_core::paths::binary_path;
use consult_core::settings::{CONSULT_TOOLS, HookKind, hook_command, legacy_hook_command};
use consult_install::{
    ClaudeCli, Host, InstallError, InstallOptions, InstallOutcome, McpOutcome, Network,
    PanelAnswer, PanelRequest, PlainUi, Removal, ServiceOps, ServiceOutcome, System, Transport, Ui,
    UnattendedUi, UninstallOptions, UninstallOutcome, delete_install_dir_with, foreign_status_line,
    install_with, parse_panel_answer, uninstall_with,
};
use consult_service::{Registration, ServiceError, ServiceSpec};
use consult_tui::StepKind;
use consult_tui::widgets::format_step;
use serde_json::{Value, json};

/// An obviously fake key.
const KEY: &str = "sk-or-v1-fake-test-key-00000000";
const OTHER_KEY: &str = "sk-or-v1-other-fake-key-1111111";

// ---- fakes -------------------------------------------------------------------------------

#[derive(Default, Debug)]
struct Log {
    service: Vec<String>,
    claude: Vec<Vec<String>>,
    key_checks: Vec<String>,
    removed_after_exit: Vec<PathBuf>,
}

type Shared = Arc<Mutex<Log>>;

fn log(shared: &Shared) -> std::sync::MutexGuard<'_, Log> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

struct FakeService {
    log: Shared,
    registered: Arc<Mutex<Option<PathBuf>>>,
    registration: Registration,
    listening: bool,
}

impl ServiceOps for FakeService {
    fn supported(&self) -> bool {
        true
    }
    fn registered_dir(&self) -> Option<PathBuf> {
        self.registered.lock().expect("lock").clone()
    }
    fn stop(&self, dirs: &[PathBuf]) -> Result<Vec<u32>, ServiceError> {
        let dirs: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
        log(&self.log)
            .service
            .push(format!("stop {}", dirs.join("|")));
        Ok(Vec::new())
    }
    fn install(&self, spec: &ServiceSpec) -> Result<Registration, ServiceError> {
        log(&self.log).service.push(format!(
            "install {} {} {} {}",
            spec.exe.display(),
            spec.working_dir.display(),
            spec.port,
            spec.host
        ));
        *self.registered.lock().expect("lock") = Some(spec.working_dir.clone());
        Ok(self.registration.clone())
    }
    fn start(&self, port: u16) -> Result<bool, ServiceError> {
        log(&self.log).service.push(format!("start {port}"));
        Ok(self.listening)
    }
    fn uninstall(&self) -> Result<(), ServiceError> {
        log(&self.log).service.push("uninstall".into());
        *self.registered.lock().expect("lock") = None;
        Ok(())
    }
}

struct FakeClaude {
    log: Shared,
    available: bool,
    exit: i32,
}

impl ClaudeCli for FakeClaude {
    fn available(&self) -> bool {
        self.available
    }
    fn run(&self, args: &[String]) -> std::io::Result<i32> {
        log(&self.log).claude.push(args.to_vec());
        Ok(self.exit)
    }
}

struct FakeNetwork {
    log: Shared,
    listing: Option<String>,
    keys: HashMap<String, KeyCheck>,
}

fn fetched() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 25, 14, 30, 0)
        .single()
        .expect("time")
}

impl Network for FakeNetwork {
    fn fetch_listing(&self) -> Option<(String, DateTime<Utc>)> {
        self.listing.clone().map(|b| (b, fetched()))
    }
    fn check_key(&self, key: &str) -> KeyCheck {
        log(&self.log).key_checks.push(key.to_string());
        self.keys
            .get(key)
            .cloned()
            .unwrap_or(KeyCheck::Rejected { status: 401 })
    }
}

struct FakeHost {
    log: Shared,
    real_claude: PathBuf,
    env_key: Option<String>,
    current_exe: Option<PathBuf>,
}

impl Host for FakeHost {
    fn git_found(&self) -> bool {
        true
    }
    fn real_claude_dir(&self) -> PathBuf {
        self.real_claude.clone()
    }
    fn env_key(&self) -> Option<String> {
        self.env_key.clone()
    }
    fn current_exe(&self) -> Option<PathBuf> {
        self.current_exe.clone()
    }
    fn remove_after_exit(&self, dir: &Path) -> std::io::Result<()> {
        log(&self.log).removed_after_exit.push(dir.to_path_buf());
        Ok(())
    }
}

/// A writer the test can read back after the UI took ownership of it.
#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("lock").extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("lock").clone()).expect("utf8")
    }
}

// ---- a scripted UI ---------------------------------------------------------------------------

#[derive(Debug)]
enum A {
    Yes,
    No,
    Key(String),
    Panel(PanelAnswer),
    Display(Display),
}

#[derive(Default)]
struct Script {
    answers: VecDeque<A>,
    lines: Vec<String>,
    questions: Vec<String>,
    offline_seen: Option<bool>,
}

impl Script {
    fn new(answers: Vec<A>) -> Self {
        Self {
            answers: answers.into(),
            ..Self::default()
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn unexpected(&self, what: &str, got: Option<A>) -> InstallError {
        InstallError::Stopped(format!("script: {what}, but the next answer is {got:?}"))
    }
}

impl Ui for Script {
    fn report_step(&mut self, kind: StepKind, text: &str) {
        self.lines.extend(format_step(kind, text));
    }

    fn confirm(&mut self, question: &str, default: bool) -> Result<bool, InstallError> {
        self.questions.push(question.to_string());
        let _ = default;
        match self.answers.pop_front() {
            Some(A::Yes) => Ok(true),
            Some(A::No) => Ok(false),
            other => Err(self.unexpected(&format!("asked {question:?}"), other)),
        }
    }

    fn enter_key(&mut self, _prompt: &str) -> Result<String, InstallError> {
        self.questions.push("key".into());
        match self.answers.pop_front() {
            Some(A::Key(k)) => Ok(k),
            other => Err(self.unexpected("asked for the key", other)),
        }
    }

    fn choose_panel(&mut self, request: &PanelRequest<'_>) -> Result<PanelAnswer, InstallError> {
        self.questions.push("panel".into());
        self.offline_seen = Some(request.offline);
        match self.answers.pop_front() {
            Some(A::Panel(p)) => Ok(p),
            other => Err(self.unexpected("asked for the panel", other)),
        }
    }

    fn choose_display(&mut self, current: Display) -> Result<Display, InstallError> {
        if matches!(self.answers.front(), Some(A::Display(_)))
            && let Some(A::Display(d)) = self.answers.pop_front()
        {
            return Ok(d);
        }
        Ok(current)
    }

    fn done(&mut self, headline: &str, lines: &[String]) {
        self.lines.push(format!("DONE {headline}"));
        self.lines.extend(lines.iter().cloned());
    }

    fn fail(&mut self, message: &str) {
        self.lines.push(format!("FAIL {message}"));
    }
}

// ---- fixtures -------------------------------------------------------------------------------------

fn listed(id: &str, name: &str, prompt: &str, completion: &str) -> Value {
    json!({"id": id, "name": name, "context_length": 1_048_576,
           "pricing": {"prompt": prompt, "completion": completion},
           "supported_parameters": ["max_tokens", "tools"]})
}

/// The listing as OpenRouter serves it, cut down. gpt-6-sol is left out: a favourite
/// OpenRouter no longer offers.
fn listing_body() -> String {
    let data = vec![
        listed(
            "deepseek/deepseek-v4-pro",
            "DeepSeek: DeepSeek V4 Pro",
            "0.00000055071",
            "0.00000110142",
        ),
        listed(
            "z-ai/glm-5.2",
            "Z.ai: GLM 5.2",
            "0.0000006496",
            "0.0000020416",
        ),
        listed(
            "openai/gpt-6-luna-pro",
            "OpenAI: GPT-6 Luna Pro",
            "0.00000005",
            "0.00000025",
        ),
        listed(
            "z-ai/glm-5.3-flash",
            "Z.ai: GLM 5.3 Flash",
            "0.0000001",
            "0.0000004",
        ),
        listed(
            "deepseek/deepseek-v4.1-flash",
            "DeepSeek: V4.1 Flash",
            "0.0000001",
            "0.0000004",
        ),
        listed(
            "minimax/minimax-m3",
            "MiniMax: M3",
            "0.0000003",
            "0.0000012",
        ),
        listed("x-ai/grok-4.7", "xAI: Grok 4.7", "0.000003", "0.000015"),
        listed(
            "qwen/qwen3.8-max-0902",
            "Qwen: Qwen3.8 Max",
            "0.0000012",
            "0.000006",
        ),
        listed(
            "google/gemini-3.1-pro-preview",
            "Google: Gemini 3.1 Pro",
            "0.000002",
            "0.000012",
        ),
        listed(
            "moonshotai/kimi-k3",
            "MoonshotAI: Kimi K3",
            "0.000003",
            "0.000015",
        ),
        listed(
            "mistralai/devstral-2",
            "Mistral: Devstral 2",
            "0.0000004",
            "0.000002",
        ),
        listed(
            "anthropic/claude-opus-5.5",
            "Anthropic: Claude Opus 5.5",
            "0.000005",
            "0.000025",
        ),
        listed("openrouter/auto", "Auto Router", "-1", "-1"),
    ];
    json!({"data": data}).to_string()
}

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    claude: PathBuf,
    install: PathBuf,
    source: PathBuf,
    log: Shared,
    registered: Arc<Mutex<Option<PathBuf>>>,
    listing: Option<String>,
    env_key: Option<String>,
    keys: HashMap<String, KeyCheck>,
    real_claude: Option<PathBuf>,
    claude_available: bool,
    claude_exit: i32,
    registration: Registration,
    current_exe: Option<PathBuf>,
}

fn valid() -> KeyCheck {
    KeyCheck::Valid {
        usage: Some(1234.5),
        limit: None,
        data: json!({}),
    }
}

impl Rig {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        // Long names: the generator resolves paths, and a short 8.3 temp path would
        // not compare equal to what it writes.
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        let claude = root.join("claude");
        std::fs::create_dir_all(&claude).expect("mkdir");
        let source = root.join("src").join("claude-consult-build");
        std::fs::create_dir_all(source.parent().expect("parent")).expect("mkdir");
        std::fs::write(&source, b"binary v1").expect("write");
        let mut keys = HashMap::new();
        keys.insert(KEY.to_string(), valid());
        Self {
            install: root.join("install"),
            _tmp: tmp,
            claude,
            source,
            root,
            log: Shared::default(),
            registered: Arc::new(Mutex::new(None)),
            listing: Some(listing_body()),
            env_key: Some(KEY.to_string()),
            keys,
            real_claude: None,
            claude_available: true,
            claude_exit: 0,
            registration: Registration::S4U,
            current_exe: None,
        }
    }

    fn system(&self) -> System {
        System {
            service: Box::new(FakeService {
                log: self.log.clone(),
                registered: self.registered.clone(),
                registration: self.registration.clone(),
                listening: true,
            }),
            claude: Box::new(FakeClaude {
                log: self.log.clone(),
                available: self.claude_available,
                exit: self.claude_exit,
            }),
            network: Box::new(FakeNetwork {
                log: self.log.clone(),
                listing: self.listing.clone(),
                keys: self.keys.clone(),
            }),
            host: Box::new(FakeHost {
                log: self.log.clone(),
                real_claude: self
                    .real_claude
                    .clone()
                    .unwrap_or_else(|| self.claude.clone()),
                env_key: self.env_key.clone(),
                current_exe: self.current_exe.clone(),
            }),
        }
    }

    fn opts(&self) -> InstallOptions {
        InstallOptions {
            install_dir: Some(self.install.clone()),
            claude_dir: Some(self.claude.clone()),
            port: 8766,
            transport: Transport::Service,
            source_exe: self.source.clone(),
            ..InstallOptions::default()
        }
    }

    fn unattended(&self) -> InstallOptions {
        InstallOptions {
            unattended: true,
            ..self.opts()
        }
    }

    /// Runs an install with the unattended UI; returns the outcome and the output.
    fn run_unattended(
        &self,
        opts: &InstallOptions,
    ) -> (Result<InstallOutcome, InstallError>, String) {
        let out = Buf::default();
        let mut ui = UnattendedUi::new(out.clone());
        let res = install_with(opts, &mut ui, &self.system());
        (res, out.text())
    }

    fn installed(&self, opts: &InstallOptions) -> (consult_install::Installed, String) {
        let (res, out) = self.run_unattended(opts);
        match res {
            Ok(InstallOutcome::Installed(done)) => (*done, out),
            other => panic!("install failed: {other:?}\n{out}"),
        }
    }

    fn uninstall_opts(&self) -> UninstallOptions {
        UninstallOptions {
            install_dir: Some(self.install.clone()),
            claude_dir: Some(self.claude.clone()),
            yes: true,
            ..UninstallOptions::default()
        }
    }

    fn settings(&self) -> Value {
        let text = std::fs::read_to_string(self.claude.join("settings.json")).expect("settings");
        serde_json::from_str(&text).expect("json")
    }

    fn write_settings(&self, v: &Value) {
        std::fs::write(
            self.claude.join("settings.json"),
            serde_json::to_string_pretty(v).expect("json"),
        )
        .expect("write");
    }

    fn cmd(&self, kind: HookKind) -> String {
        hook_command(&self.install, kind)
    }

    fn manifest(&self) -> Value {
        let text = std::fs::read_to_string(self.install.join("manifest.json")).expect("manifest");
        serde_json::from_str(&text).expect("json")
    }

    fn service_log(&self) -> Vec<String> {
        log(&self.log).service.clone()
    }

    fn claude_log(&self) -> Vec<String> {
        log(&self.log).claude.iter().map(|a| a.join(" ")).collect()
    }
}

fn assert_no_key(rig: &Rig, text: &str) {
    for key in [KEY, OTHER_KEY] {
        assert!(!text.contains(key), "the key leaked into output:\n{text}");
        for args in &log(&rig.log).claude {
            assert!(
                !args.iter().any(|a| a.contains(key)),
                "the key reached argv"
            );
        }
    }
}

// ---- install ---------------------------------------------------------------------------------------

#[test]
fn fresh_unattended_install_writes_everything() {
    let rig = Rig::new();
    let (done, out) = rig.installed(&rig.unattended());

    // The binary is a copy of the running one.
    assert_eq!(done.binary, binary_path(&rig.install));
    assert_eq!(std::fs::read(&done.binary).expect("bin"), b"binary v1");

    // Files core writes.
    for f in [
        "consult.md",
        "cleanroom.md",
        "deepseek.md",
        "glm.md",
        "luna.md",
    ] {
        assert!(rig.claude.join("commands").join(f).is_file(), "{f}");
    }
    assert!(
        rig.claude
            .join("skills/openrouter-workflow/SKILL.md")
            .is_file()
    );
    assert!(rig.claude.join("workflows/verify-claims.js").is_file());
    let models: Value = serde_json::from_str(
        &std::fs::read_to_string(rig.install.join("models.json")).expect("models"),
    )
    .expect("json");
    assert_eq!(
        models["default_panel"],
        json!(["deepseek-v4-pro", "glm-5.2", "gpt-6-luna-pro"])
    );
    assert_eq!(models["priced_at"], "2026-09-25T14:30:00Z");
    assert!(
        (models["models"]["glm-5.2"]["price_in"]
            .as_f64()
            .expect("price")
            - 0.6496)
            .abs()
            < 1e-9
    );
    let display: Value = serde_json::from_str(
        &std::fs::read_to_string(rig.install.join("display.json")).expect("display"),
    )
    .expect("json");
    assert_eq!(
        (display["progress"].as_str(), display["summary"].as_str()),
        (Some("full"), Some("dim"))
    );

    // The settings merge.
    let s = rig.settings();
    assert_eq!(s["env"]["OPENROUTER_API_KEY"], KEY);
    assert_eq!(s["env"]["MCP_TOOL_TIMEOUT"], "2400000");
    assert_eq!(
        s["hooks"]["PostToolUse"],
        json!([{"matcher": CONSULT_TOOLS, "hooks": [{"type": "command", "command": rig.cmd(HookKind::Summary)}]}])
    );
    assert_eq!(
        s["hooks"]["MessageDisplay"],
        json!([{"hooks": [{"type": "command", "command": rig.cmd(HookKind::Display)}]}])
    );
    assert_eq!(
        s["statusLine"],
        json!({"type": "command", "command": rig.cmd(HookKind::Statusline)})
    );
    assert!(rig.cmd(HookKind::Summary).contains("/bin/claude-consult"));

    // The manifest.
    let m = rig.manifest();
    assert_eq!(
        m["panel"],
        json!(["deepseek-v4-pro", "glm-5.2", "gpt-6-luna-pro"])
    );
    assert_eq!(m["settings"]["statusLine"], rig.cmd(HookKind::Statusline));
    assert!(m["files"].as_array().expect("files").len() >= 7);

    // The service and the registration.
    let inst = rig.install.display().to_string();
    assert_eq!(
        rig.service_log(),
        [
            format!("stop {inst}"),
            format!("install {} {inst} 8766 127.0.0.1", done.binary.display()),
            "start 8766".to_string(),
        ]
    );
    assert_eq!(
        rig.claude_log(),
        [
            "mcp remove openrouter -s user",
            "mcp add --transport http --scope user openrouter http://127.0.0.1:8766/mcp",
        ]
    );
    assert_eq!(done.mcp, McpOutcome::Registered);
    assert_eq!(
        done.service,
        ServiceOutcome::Started {
            registration: Registration::S4U,
            listening: true
        }
    );
    assert_eq!(
        done.commands,
        ["consult", "cleanroom", "deepseek", "glm", "luna"]
    );

    // What was said.
    for line in [
        "==> Checking prerequisites",
        "  [ok] Claude Code CLI found",
        "  [ok] Key accepted by OpenRouter (spent so far: $1,234.50, credit limit: none set)",
        "  [!!] No longer offered on OpenRouter with tool calling: gpt-6-sol",
        "  [ok] Panel: deepseek-v4-pro, glm-5.2, gpt-6-luna-pro",
        "==> Ready to install",
        "       Prices      : USD per million tokens in / out, live from OpenRouter at 14:30 UTC",
        "       Key         : sk-or-v1-...0000 -> settings.json env.OPENROUTER_API_KEY",
        "       Status line : consult session total",
        "       Service     : scheduled task 'OpenRouterMCP' on http://127.0.0.1:8766/mcp",
        "       Commands    : /consult  /cleanroom  /deepseek  /glm  /luna   (+ openrouter-workflow skill)",
        "  [ok] Listening on 127.0.0.1:8766",
        "  [ok] MCP server 'openrouter' -> http://127.0.0.1:8766/mcp (user scope)",
        "  claude-consult is installed.",
        "       Commands : /consult  /cleanroom  /deepseek  /glm  /luna",
    ] {
        assert!(out.contains(line), "missing {line:?} in:\n{out}");
    }
    assert!(out.contains(&format!(
        "              {:<36} {:>7} / {}",
        "glm-5.2", "0.65", "2.042"
    )));
    assert!(out.contains("  [ok] Updated settings.json: OPENROUTER_API_KEY, MCP_TOOL_TIMEOUT"));
    assert_no_key(&rig, &out);
    assert_eq!(log(&rig.log).key_checks, [KEY]);
}

#[test]
fn rerun_is_the_upgrade_path() {
    let rig = Rig::new();
    rig.installed(&rig.unattended());
    std::fs::write(&rig.source, b"binary v2").expect("write");
    log(&rig.log).service.clear();
    log(&rig.log).claude.clear();

    let opts = InstallOptions {
        panel: Some(vec![
            "deepseek-v4-pro,glm-5.2".into(),
            "mistralai/devstral-2".into(),
        ]),
        ..rig.unattended()
    };
    let (done, out) = rig.installed(&opts);
    assert_eq!(std::fs::read(&done.binary).expect("bin"), b"binary v2");
    // The task served this install, so it is stopped (once) and re-registered.
    let inst = rig.install.display().to_string();
    assert_eq!(rig.service_log()[0], format!("stop {inst}|{inst}"));
    // luna left the panel: its command goes, the outsider gets one.
    assert!(!rig.claude.join("commands/luna.md").exists());
    assert!(rig.claude.join("commands/devstral-2.md").is_file());
    assert!(out.contains("Removed stale "), "{out}");
    assert!(
        out.contains("and one each for mistralai/devstral-2"),
        "{out}"
    );
    assert_eq!(
        done.commands,
        ["consult", "cleanroom", "deepseek", "glm", "devstral-2"]
    );
    // Nothing duplicated in settings, and nothing to change there.
    let s = rig.settings();
    assert_eq!(
        s["hooks"]["PostToolUse"].as_array().expect("hooks").len(),
        1
    );
    assert_eq!(
        s["hooks"]["MessageDisplay"]
            .as_array()
            .expect("hooks")
            .len(),
        1
    );
    assert!(
        out.contains("  [ok] settings.json already up to date"),
        "{out}"
    );
    assert_eq!(done.report.status_line.as_str(), "ours");
}

#[test]
fn a_foreign_status_line_is_kept_unattended() {
    let rig = Rig::new();
    let theirs = json!({"type": "command", "command": "bash ~/.claude/my-status.sh"});
    rig.write_settings(&json!({"statusLine": theirs, "model": "opus"}));
    let (done, out) = rig.installed(&rig.unattended());
    assert_eq!(done.report.status_line.as_str(), "kept");
    let s = rig.settings();
    assert_eq!(s["statusLine"], theirs);
    assert_eq!(s["model"], "opus");
    assert!(
        out.contains("       Status line : yours, unchanged"),
        "{out}"
    );
    let hint = format!(
        "  [!!] Kept your status line. To add the consult total, have your script pipe its stdin to {}.",
        rig.cmd(HookKind::Statusline)
    );
    assert!(out.contains(&hint), "{out}");
}

#[test]
fn a_bad_panel_entry_aborts_before_anything_is_written() {
    for (panel, why) in [
        (
            "deepseek-v4-pro,anthropic/claude-opus-5.5",
            "'anthropic/claude-opus-5.5': consult asks models other than Claude.",
        ),
        (
            "deepseek-v4-pro,nope",
            "'nope' is neither a favourite nor an OpenRouter id (provider/model).",
        ),
        (
            "gpt-6-sol",
            "'gpt-6-sol' (openai/gpt-6-sol) is not offered on OpenRouter with tool calling.",
        ),
        (
            "glm-5.2,glm-5.3-flash",
            "Two panel seats from one lab (z-ai): their agreement says less than two labs agreeing.",
        ),
    ] {
        let rig = Rig::new();
        let opts = InstallOptions {
            panel: Some(vec![panel.into()]),
            ..rig.unattended()
        };
        let (res, out) = rig.run_unattended(&opts);
        let err = res.expect_err("refused");
        let want = format!("Invalid --panel: {}", panel.replace(',', ", "));
        assert_eq!(err.to_string(), want);
        assert!(out.contains(&format!("  [!!] {why}")), "{out}");
        assert!(out.contains(&format!("  [xx] {want}")), "{out}");
        assert!(!rig.install.exists());
        assert_eq!(std::fs::read_dir(&rig.claude).expect("claude").count(), 0);
        assert!(rig.service_log().is_empty());
        assert!(rig.claude_log().is_empty());
    }
}

#[test]
fn offline_install_uses_favourites_without_prices() {
    let mut rig = Rig::new();
    rig.listing = None;
    let (done, out) = rig.installed(&rig.unattended());
    assert!(out.contains("  [!!] Could not reach OpenRouter's model listing: offering the favourites only, without prices."));
    assert!(out.contains("       Prices      : unknown, OpenRouter is unreachable"));
    assert_eq!(done.report.priced_at, None);
    // An outsider cannot be checked offline.
    let opts = InstallOptions {
        panel: Some(vec!["mistralai/devstral-2".into()]),
        ..rig.unattended()
    };
    let (res, out) = rig.run_unattended(&opts);
    assert!(res.is_err());
    assert!(out.contains("cannot be checked while OpenRouter's listing is unreachable"));
}

#[test]
fn unattended_without_a_usable_key_stops() {
    let mut rig = Rig::new();
    rig.env_key = Some(OTHER_KEY.into());
    let (res, out) = rig.run_unattended(&rig.unattended());
    assert_eq!(
        res.expect_err("no key").to_string(),
        "No usable OpenRouter key. For --unattended, set OPENROUTER_API_KEY first."
    );
    assert!(
        out.contains("  [!!] That key was rejected by OpenRouter (HTTP 401)."),
        "{out}"
    );
    assert!(!rig.install.exists());
    assert_no_key(&rig, &out);

    // --skip-key-check takes it unchecked.
    let opts = InstallOptions {
        skip_key_check: true,
        ..rig.unattended()
    };
    let (res, out) = rig.run_unattended(&opts);
    assert!(matches!(res, Ok(InstallOutcome::Installed(_))), "{out}");
    assert!(out.contains("  [!!] Not validating the key (--skip-key-check)."));
    assert_eq!(rig.settings()["env"]["OPENROUTER_API_KEY"], OTHER_KEY);
}

#[test]
fn a_claude_dir_that_is_not_the_real_one() {
    let mut rig = Rig::new();
    rig.real_claude = Some(rig.root.join("real-claude"));
    // With the service, it asks, and unattended takes the default: no.
    let (res, out) = rig.run_unattended(&rig.unattended());
    assert!(
        matches!(res, Ok(InstallOutcome::Cancelled { exit_code: 1 })),
        "{out}"
    );
    assert!(out.contains("is not the one Claude Code and the service read"));
    assert!(!rig.install.exists());
    // A test install: no service, and `claude mcp` is never run.
    let opts = InstallOptions {
        skip_service: true,
        ..rig.unattended()
    };
    let (done, out) = rig.installed(&opts);
    assert_eq!(done.mcp, McpOutcome::NotRealClaudeDir);
    assert_eq!(done.service, ServiceOutcome::Skipped);
    assert!(out.contains("Skipped: --claude-dir is not Claude Code's config dir, and 'claude mcp' only edits the real one."));
    assert!(out.contains("       Service     : skipped (--skip-service)"));
    assert!(rig.claude_log().is_empty());
    assert!(rig.service_log().is_empty());
}

#[test]
fn missing_or_failing_claude_prints_the_manual_command() {
    let mut rig = Rig::new();
    rig.claude_available = false;
    let (done, out) = rig.installed(&rig.unattended());
    assert_eq!(done.mcp, McpOutcome::ClaudeMissing);
    assert!(out.contains(
        "  [!!] Claude Code CLI (claude) not found; the MCP server will need registering by hand."
    ));
    assert!(out.contains("Run once Claude Code is installed: claude mcp add --transport http --scope user openrouter http://127.0.0.1:8766/mcp"));
    assert!(rig.claude_log().is_empty());
    rig.claude_available = true;
    rig.claude_exit = 1;
    let (done, out) = rig.installed(&rig.unattended());
    assert_eq!(done.mcp, McpOutcome::Failed);
    assert!(out.contains("claude mcp add failed. Run: claude mcp add --transport http"));
}

#[test]
fn the_logon_only_fallback_is_explained() {
    let mut rig = Rig::new();
    rig.registration = Registration::InteractiveLogonOnly {
        reason: "Access is denied.".into(),
    };
    let (_, out) = rig.installed(&rig.unattended());
    assert!(out.contains("LOGON ONLY"), "{out}");
    assert!(out.contains("Access is denied."));
}

#[test]
fn stdio_transport_registers_the_binary_and_retires_this_installs_task() {
    let rig = Rig::new();
    *rig.registered.lock().expect("lock") = Some(rig.install.clone());
    let opts = InstallOptions {
        transport: Transport::Stdio,
        ..rig.unattended()
    };
    let (done, out) = rig.installed(&opts);
    assert_eq!(done.service, ServiceOutcome::NotUsed { removed: true });
    let bin = binary_path(&rig.install).display().to_string();
    assert_eq!(
        rig.claude_log()[1],
        format!("mcp add --transport stdio --scope user openrouter -- {bin} serve")
    );
    let inst = rig.install.display().to_string();
    assert_eq!(
        rig.service_log(),
        [format!("stop {inst}|{inst}"), "uninstall".to_string()]
    );
    assert!(out.contains("Server      : started by Claude Code for each session (stdio)"));
}

#[test]
fn a_task_serving_another_install_is_confirmed_before_repointing() {
    let rig = Rig::new();
    let other = rig.root.join("other-install");
    *rig.registered.lock().expect("lock") = Some(other.clone());
    let mut ui = Script::new(vec![A::No]);
    let res = install_with(&rig.opts(), &mut ui, &rig.system()).expect("ran");
    assert_eq!(res, InstallOutcome::Cancelled { exit_code: 0 });
    assert_eq!(ui.questions, ["Continue?"]);
    assert!(ui.text().contains(&format!(
        "  [!!] Task 'OpenRouterMCP' currently serves consult from {}.",
        other.display()
    )));
    assert!(!rig.install.exists());
}

#[test]
fn interactive_install_then_uninstall() {
    let mut rig = Rig::new();
    rig.env_key = None;
    let theirs = json!({"type": "command", "command": "~/.claude/statusline.sh", "padding": 0});
    rig.write_settings(&json!({"statusLine": theirs}));
    std::fs::create_dir_all(rig.claude.join("commands")).expect("mkdir");
    std::fs::write(
        rig.claude.join("commands/minimax.md"),
        "my own minimax command\n",
    )
    .expect("write");

    let mut ui = Script::new(vec![
        // The typed key: first one OpenRouter rejects, then the good one, padded.
        A::Key(OTHER_KEY.into()),
        A::Key(format!("  {KEY} \r\n")),
        A::Panel(PanelAnswer::Picked(vec![
            "gpt-6-luna-pro".into(),
            "minimax-m3".into(),
            "mistralai/devstral-2".into(),
        ])),
        A::Yes, // replace your status line
        A::Display(Display {
            progress: ProgressStyle::Marks,
            summary: SummaryStyle::Off,
        }),
        A::Yes, // Proceed?
    ]);
    let res = install_with(&rig.opts(), &mut ui, &rig.system()).expect("installed");
    let InstallOutcome::Installed(done) = res else {
        panic!("{}", ui.text());
    };
    assert_eq!(
        ui.questions,
        [
            "key",
            "key",
            "panel",
            "Replace your status line with it?",
            "Proceed?"
        ]
    );
    assert_eq!(ui.offline_seen, Some(false));
    let text = ui.text();
    assert!(text.contains("  [!!] That key was rejected by OpenRouter (HTTP 401)."));
    assert!(text.contains("       You already have a status line: ~/.claude/statusline.sh"));
    assert!(
        text.contains(
            "Status line : consult session total, replacing yours (put back on uninstall)"
        )
    );
    assert!(text.contains("Display     : progress marks, summary off"));
    assert!(text.contains("  [!!] Replaced files it had not generated; originals saved in "));
    assert_no_key(&rig, &text);

    let s = rig.settings();
    assert_eq!(s["env"]["OPENROUTER_API_KEY"], KEY);
    assert_eq!(s["statusLine"]["command"], rig.cmd(HookKind::Statusline));
    assert!(s["hooks"].get("MessageDisplay").is_none());
    assert_eq!(rig.manifest()["status_line_displaced"], theirs);
    assert_eq!(done.report.display.summary, SummaryStyle::Off);
    assert_eq!(
        done.commands,
        ["consult", "cleanroom", "luna", "minimax", "devstral-2"]
    );

    // Uninstall, removing the key.
    let mut ui = Script::new(vec![A::Yes, A::Yes]);
    let opts = UninstallOptions {
        yes: false,
        ..rig.uninstall_opts()
    };
    let res = uninstall_with(&opts, &mut ui, &rig.system()).expect("uninstalled");
    let UninstallOutcome::Uninstalled(gone) = res else {
        panic!("{}", ui.text());
    };
    assert!(gone.task_removed);
    assert!(gone.mcp_removed);
    assert!(gone.report.key_removed);
    assert!(gone.report.status_line_restored);
    assert_eq!(gone.removal, Removal::Deleted);
    assert!(!rig.install.exists());
    let s = rig.settings();
    assert_eq!(s["statusLine"], theirs);
    assert!(s["env"].get("OPENROUTER_API_KEY").is_none());
    assert!(s.get("hooks").is_none());
    assert_eq!(
        std::fs::read_to_string(rig.claude.join("commands/minimax.md")).expect("restored"),
        "my own minimax command\n"
    );
    assert!(!rig.claude.join("commands/consult.md").exists());
    let text = ui.text();
    for line in [
        "  [ok] Removed scheduled task 'OpenRouterMCP'",
        "  [ok] Removed MCP server 'openrouter' from Claude Code",
        "  [ok] Restored your original status line",
        "  [ok] Removed OPENROUTER_API_KEY from settings.json",
        "DONE claude-consult is uninstalled. Restart open Claude Code sessions.",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert!(rig.service_log().contains(&"uninstall".to_string()));
    assert_eq!(
        rig.claude_log().last().map(String::as_str),
        Some("mcp remove openrouter -s user")
    );
}

#[test]
fn the_picker_can_be_cancelled_and_typed_panels_are_checked() {
    let rig = Rig::new();
    let mut ui = Script::new(vec![A::Yes, A::Panel(PanelAnswer::Cancelled)]);
    let err = install_with(&rig.opts(), &mut ui, &rig.system()).expect_err("cancelled");
    assert_eq!(err.to_string(), "Cancelled.");
    assert!(ui.text().contains("FAIL Cancelled."));
    assert!(!rig.install.exists());

    // A typed panel that fails the check is asked for again.
    let mut ui = Script::new(vec![
        A::Yes,
        A::Panel(PanelAnswer::Typed(vec!["anthropic/claude-opus-5.5".into()])),
        A::Panel(PanelAnswer::Typed(vec![
            "glm-5.2".into(),
            "glm-5.3-flash".into(),
        ])),
        A::No, // keep this panel anyway?
        A::Panel(PanelAnswer::Typed(vec!["glm-5.2".into(), "kimi-k3".into()])),
        A::Yes, // Proceed?
    ]);
    let res = install_with(&rig.opts(), &mut ui, &rig.system()).expect("installed");
    let InstallOutcome::Installed(done) = res else {
        panic!("{}", ui.text());
    };
    assert_eq!(done.report.panel, ["glm-5.2", "kimi-k3"]);
    assert_eq!(
        ui.questions,
        [
            "Use the key from the OPENROUTER_API_KEY environment variable (sk-or-v1-...0000)?",
            "panel",
            "panel",
            "Keep this panel anyway?",
            "panel",
            "Proceed?"
        ]
    );
}

#[test]
fn plain_ui_numbered_list_and_hidden_key() {
    let mut rig = Rig::new();
    rig.env_key = None;
    let input = format!("{OTHER_KEY}\n{KEY}\n12\n1,3 minimax-m3\n\n");
    let out = Buf::default();
    let mut ui = PlainUi::new(std::io::Cursor::new(input.into_bytes()), out.clone());
    let res = install_with(&rig.opts(), &mut ui, &rig.system()).expect("installed");
    let text = out.text();
    let InstallOutcome::Installed(done) = res else {
        panic!("{text}");
    };
    assert_eq!(
        done.report.panel,
        ["deepseek-v4-pro", "gpt-6-luna-pro", "minimax-m3"]
    );
    let header = format!(
        "    #  alias{}lab{}tier{}$/M in  $/M out context",
        " ".repeat(17),
        " ".repeat(10),
        " ".repeat(8)
    );
    let first = format!(
        "    1  deepseek-v4-pro{}DeepSeek{}panel{}0.551{}1.101{}1024K",
        " ".repeat(7),
        " ".repeat(5),
        " ".repeat(8),
        " ".repeat(4),
        " ".repeat(3)
    );
    for line in [
        "  claude-consult - outside reviews from non-Claude models, in Claude Code",
        "  OpenRouter API key (input hidden): ",
        "  [!!] That key was rejected by OpenRouter (HTTP 401).",
        &header,
        &first,
        "       Recommended : deepseek-v4-pro, glm-5.2, gpt-6-luna-pro",
        "  [!!] '12' is not a number from 1 to 10.",
        "  Proceed? [Y/n]: ",
    ] {
        assert!(text.contains(line), "missing {line:?} in:\n{text}");
    }
    assert_no_key(&rig, &text);
}

#[test]
fn a_python_install_is_migrated() {
    let mut rig = Rig::new();
    // Settings hold the key; the environment offers none.
    rig.env_key = None;
    let d = &rig.install.clone();
    for f in [
        "server.py",
        "panel.py",
        "sandbox.py",
        "service.ps1",
        "requirements.txt",
    ] {
        std::fs::create_dir_all(d).expect("mkdir");
        std::fs::write(d.join(f), "# old\n").expect("write");
    }
    std::fs::create_dir_all(d.join(".venv/Scripts")).expect("mkdir");
    std::fs::write(d.join(".venv/Scripts/python.exe"), "").expect("write");
    std::fs::create_dir_all(d.join("hooks")).expect("mkdir");
    for h in ["summary_hook.py", "display_hook.py", "statusline.py"] {
        std::fs::write(d.join("hooks").join(h), "# old\n").expect("write");
    }
    let legacy = |k| legacy_hook_command(d, k);
    rig.write_settings(&json!({
        "env": {"OPENROUTER_API_KEY": KEY},
        "hooks": {
            "PostToolUse": [{"matcher": CONSULT_TOOLS, "hooks": [{"type": "command", "command": legacy(HookKind::Summary)}]}],
            "MessageDisplay": [{"hooks": [{"type": "command", "command": legacy(HookKind::Display)}]}],
        },
        "statusLine": {"type": "command", "command": legacy(HookKind::Statusline)},
    }));
    *rig.registered.lock().expect("lock") = Some(d.clone());
    let (done, out) = rig.installed(&rig.unattended());
    assert!(
        out.contains("  [ok] Removed the Python install's files: server.py"),
        "{out}"
    );
    assert!(done.legacy_removed.contains(&".venv".to_string()));
    for f in ["server.py", "panel.py", ".venv", "hooks"] {
        assert!(!rig.install.join(f).exists(), "{f}");
    }
    let s = rig.settings();
    assert_eq!(
        s["hooks"]["PostToolUse"],
        json!([{"matcher": CONSULT_TOOLS, "hooks": [{"type": "command", "command": rig.cmd(HookKind::Summary)}]}])
    );
    assert_eq!(s["statusLine"]["command"], rig.cmd(HookKind::Statusline));
    assert_eq!(done.report.status_line.as_str(), "ours");
    // The old service ran from this dir: stopped, then replaced.
    assert!(rig.service_log()[0].starts_with("stop "));
    assert_eq!(rig.settings()["env"]["OPENROUTER_API_KEY"], KEY);
}

// ---- uninstall -----------------------------------------------------------------------------------

#[test]
fn uninstall_restores_a_displaced_file_and_leaves_another_installs_task_alone() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.claude.join("commands")).expect("mkdir");
    std::fs::write(rig.claude.join("commands/glm.md"), "mine\n").expect("write");
    rig.installed(&rig.unattended());
    assert_ne!(
        std::fs::read_to_string(rig.claude.join("commands/glm.md")).expect("glm"),
        "mine\n"
    );
    let other = rig.root.join("elsewhere");
    *rig.registered.lock().expect("lock") = Some(other.clone());
    log(&rig.log).service.clear();
    log(&rig.log).claude.clear();

    let out = Buf::default();
    let mut ui = UnattendedUi::new(out.clone());
    let res = uninstall_with(&rig.uninstall_opts(), &mut ui, &rig.system()).expect("ran");
    let text = out.text();
    let UninstallOutcome::Uninstalled(gone) = res else {
        panic!("{text}");
    };
    assert!(!gone.task_removed);
    assert_eq!(gone.other_task.as_deref(), Some(other.as_path()));
    assert!(!gone.mcp_removed);
    assert!(rig.service_log().is_empty());
    assert!(rig.claude_log().is_empty());
    assert!(text.contains(&format!(
        "  [!!] Task 'OpenRouterMCP' serves from {}, not this install; leaving it alone.",
        other.display()
    )));
    assert!(text.contains(
        "  [!!] Leaving MCP server 'openrouter' registered: the other install still uses it."
    ));
    assert!(text.contains("Restored your original "));
    assert_eq!(
        std::fs::read_to_string(rig.claude.join("commands/glm.md")).expect("glm"),
        "mine\n"
    );
    // The key stays unless asked.
    assert_eq!(rig.settings()["env"]["OPENROUTER_API_KEY"], KEY);
    assert!(!rig.install.exists());
}

#[test]
fn uninstall_refuses_a_dir_that_is_no_install() {
    let rig = Rig::new();
    std::fs::create_dir_all(&rig.install).expect("mkdir");
    std::fs::write(rig.install.join("precious.txt"), "x").expect("write");
    let out = Buf::default();
    let mut ui = UnattendedUi::new(out.clone());
    let err = uninstall_with(&rig.uninstall_opts(), &mut ui, &rig.system()).expect_err("refused");
    assert!(
        err.to_string()
            .contains("does not look like a claude-consult install")
    );
    assert!(rig.install.join("precious.txt").is_file());
}

#[test]
fn uninstall_declined_changes_nothing() {
    let rig = Rig::new();
    rig.installed(&rig.unattended());
    let mut ui = Script::new(vec![A::No]);
    let opts = UninstallOptions {
        yes: false,
        ..rig.uninstall_opts()
    };
    let res = uninstall_with(&opts, &mut ui, &rig.system()).expect("ran");
    assert_eq!(res, UninstallOutcome::Cancelled { exit_code: 0 });
    assert!(rig.install.join("manifest.json").is_file());
}

#[test]
fn a_running_installed_copy_is_removed_after_exit() {
    let rig = Rig::new();
    rig.installed(&rig.unattended());
    let exe = binary_path(&rig.install);
    let host = FakeHost {
        log: rig.log.clone(),
        real_claude: rig.claude.clone(),
        env_key: None,
        current_exe: Some(exe.clone()),
    };
    let removal = delete_install_dir_with(&rig.install, Some(&exe), &host, true).expect("removed");
    assert_eq!(removal, Removal::AfterExit);
    assert!(exe.is_file());
    assert!(!rig.install.join("manifest.json").exists());
    assert_eq!(std::fs::read_dir(&rig.install).expect("dir").count(), 1);
    assert_eq!(
        log(&rig.log).removed_after_exit,
        std::slice::from_ref(&rig.install)
    );
    // Where a running binary can be deleted, it simply goes.
    let removal = delete_install_dir_with(&rig.install, Some(&exe), &host, false).expect("removed");
    assert_eq!(removal, Removal::Deleted);
    assert!(!rig.install.exists());
}

// ---- pieces ------------------------------------------------------------------------------------------

/// Ported from test_generate.py's test_installer_prompt_draws_the_same_line: the
/// installer's question and the generator must agree on which status lines are ours.
#[test]
fn the_status_line_question_draws_the_same_line_as_the_generator() {
    let rig = Rig::new();
    let make_install = |name: &str| {
        let d = rig.root.join(name);
        std::fs::create_dir_all(d.join("bin")).expect("mkdir");
        std::fs::write(binary_path(&d), "").expect("write");
        d
    };
    let here = make_install("install");
    let other = make_install("other");
    let gone = rig.root.join("gone");
    let mine = rig.root.join("mine");
    std::fs::create_dir_all(mine.join("bin")).expect("mkdir");
    let cmd = |d: &Path, k| hook_command(d, k);
    let piped = |d: &Path| {
        format!(
            "bash -c 'input=$(cat); echo \"$(bash ~/.claude/my-status.sh) $(echo \"$input\" | {})\"'",
            hook_command(d, HookKind::Statusline)
        )
    };
    let ours = [
        cmd(&here, HookKind::Statusline),
        format!(
            "  {}\n",
            cmd(&here, HookKind::Statusline)
                .replace('/', "\\")
                .to_uppercase()
        ),
        cmd(&other, HookKind::Statusline),
        cmd(&gone, HookKind::Display),
        legacy_hook_command(&here, HookKind::Statusline),
    ];
    let theirs = [
        piped(&here),
        piped(&gone),
        format!("{} --old", cmd(&here, HookKind::Statusline)),
        format!(
            "\"{}\" hook statusline",
            mine.join("bin").join("claude-consult-mine").display()
        ),
        "\"%LOCALAPPDATA%/gone/bin/claude-consult.exe\" hook statusline".to_string(),
        "\"./bin/claude-consult.exe\" hook statusline".to_string(),
        "~/.claude/statusline.sh".to_string(),
    ];
    for c in &ours {
        rig.write_settings(&json!({"statusLine": {"type": "command", "command": c}}));
        assert_eq!(foreign_status_line(&rig.claude, &here), None, "{c}");
    }
    for c in &theirs {
        rig.write_settings(&json!({"statusLine": {"type": "command", "command": c}}));
        assert_eq!(
            foreign_status_line(&rig.claude, &here).as_deref(),
            Some(c.as_str()),
            "{c}"
        );
    }
    rig.write_settings(&json!({"statusLine": {"type": "static"}}));
    assert_eq!(
        foreign_status_line(&rig.claude, &here).as_deref(),
        Some("(not a command)")
    );
    rig.write_settings(&json!({"statusLine": null}));
    assert_eq!(foreign_status_line(&rig.claude, &here), None);
    std::fs::write(rig.claude.join("settings.json"), "{not json").expect("write");
    assert_eq!(foreign_status_line(&rig.claude, &here), None);
}

#[test]
fn typed_panel_answers() {
    let cat = consult_core::catalog::embedded().expect("catalog");
    let rows = consult_tui::rows_from(&cat, None);
    let favs: Vec<&consult_tui::PickerRow> = rows.iter().filter(|r| r.fav).collect();
    let rec = cat.default_panel.clone();
    let bud = cat.budget_panel.clone();
    let parse = |a: &str| parse_panel_answer(a, &cat, &favs, &rec, &bud);
    assert_eq!(parse("  "), Ok(rec.clone()));
    assert_eq!(parse("B"), Ok(bud.clone()));
    assert_eq!(
        parse("1, 3;minimax-m3  z-ai/glm-5.2 qwen/other"),
        Ok(vec![
            "deepseek-v4-pro".to_string(),
            "gpt-6-luna-pro".into(),
            "minimax-m3".into(),
            "glm-5.2".into(),
            "qwen/other".into()
        ])
    );
    assert_eq!(
        parse("0"),
        Err(Some(format!(
            "'0' is not a number from 1 to {}.",
            favs.len()
        )))
    );
    assert_eq!(parse(",;"), Err(None));
}
