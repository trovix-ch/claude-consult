//! Offline tests for the install file plan: what an install writes and removes.
//! Ported from tests/test_generate.py, with the hook commands in the binary's form and
//! extra cases for the legacy Python form an upgrade must replace.
//!
//! Every run uses throwaway Claude and install dirs from tempfile, never the real ones.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use consult_core::GenerateError;
use consult_core::catalog::{self, Catalog};
use consult_core::display::{ProgressStyle, SummaryStyle};
use consult_core::generate::{
    FALLBACK_NOTE, FALLBACK_PITCH, InstallInputs, InstallReport, Templates, UninstallReport,
    install_files, load_listing, pick_panel, split_panel, uninstall_files,
};
use consult_core::paths::exe_name;
use consult_core::settings::{
    CONSULT_TOOLS, HookKind, StatusLineMode, StatusLineOutcome, hook_command, legacy_hook_command,
    ours_test,
};
use indexmap::IndexMap;
use serde_json::{Value, json};

fn listed(
    id: &str,
    name: &str,
    prompt: &str,
    completion: &str,
    context: Option<u64>,
    tools: bool,
) -> Value {
    json!({"id": id, "name": name, "context_length": context,
           "pricing": {"prompt": prompt, "completion": completion, "input_cache_read": "0"},
           "supported_parameters": if tools { json!(["max_tokens", "tools"]) } else { json!(["max_tokens"]) }})
}

const CTX: Option<u64> = Some(1_048_576);

/// Shaped like the body of /api/v1/models?supported_parameters=tools, cut down.
/// gpt-6-sol is left out on purpose: a favourite the listing no longer carries.
fn listing_data() -> Vec<Value> {
    vec![
        listed(
            "deepseek/deepseek-v4-pro",
            "DeepSeek: DeepSeek V4 Pro 0423",
            "0.00000055071",
            "0.00000110142",
            CTX,
            true,
        ),
        listed(
            "z-ai/glm-5.2",
            "Z.ai: GLM 5.2",
            "0.0000006496",
            "0.0000020416",
            CTX,
            true,
        ),
        listed(
            "openai/gpt-6-luna-pro",
            "OpenAI: GPT-6 Luna Pro",
            "0.00000005",
            "0.00000025",
            Some(1_050_000),
            true,
        ),
        listed(
            "google/gemini-3.1-pro-preview",
            "Google: Gemini 3.1 Pro Preview",
            "0.000001",
            "0.000006",
            CTX,
            true,
        ),
        listed(
            "moonshotai/kimi-k3",
            "MoonshotAI: Kimi K3",
            "0.000003",
            "0.000015",
            CTX,
            true,
        ),
        listed(
            "mistralai/devstral-2",
            "Mistral: Devstral 2",
            "0.0000004",
            "0.000002",
            Some(262_144),
            true,
        ),
        listed(
            "acme/consult",
            "Acme: Consult",
            "0.000001",
            "0.000002",
            CTX,
            true,
        ),
        listed(
            "acme/deepseek",
            "Acme: DeepSeek Tune",
            "0.000001",
            "0.000002",
            None,
            true,
        ),
        listed("other/DeepSeek", "Other: DeepSeek", "", "x", CTX, true),
        listed("acme/kimi-k3", "Acme: Kimi K3 Remix", "0", "0", CTX, true),
        listed(
            "acme/weird",
            "Acme: Weird: $5 {{NAME}} #1 \"model\"",
            "0.000000001",
            "0.000001",
            CTX,
            true,
        ),
        listed(
            "acme/no-tools",
            "Acme: No Tools",
            "0.000001",
            "0.000002",
            CTX,
            false,
        ),
        listed(
            "anthropic/claude-opus-5.5",
            "Anthropic: Claude Opus 5.5",
            "0.000005",
            "0.000025",
            CTX,
            true,
        ),
        json!("not a model"),
        json!({"name": "no id"}),
    ]
}

fn listing_body() -> String {
    json!({"data": listing_data(), "total_count": 15}).to_string()
}

fn fetched() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-25T12:34:56Z")
        .expect("time")
        .with_timezone(&Utc)
}

const AS_OF: &str = "as of 2026-09-25 12:34 UTC";
const K4_KEYS: [&str; 8] = [
    "id",
    "lab",
    "command",
    "note",
    "curated",
    "context",
    "price_in",
    "price_out",
];

fn user_post() -> Value {
    json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "echo user-post"}]})
}
fn user_post_2() -> Value {
    json!({"matcher": "Edit|Write", "hooks": [{"type": "command", "command": "fmt.sh"}]})
}
fn user_display() -> Value {
    json!({"hooks": [{"type": "command", "command": "user-display.sh"}]})
}
fn user_status() -> Value {
    json!({"type": "command", "command": "bash ~/.claude/my-status.sh", "padding": 1})
}

fn embedded() -> Catalog {
    catalog::embedded().expect("catalog")
}

fn default_panel() -> String {
    embedded().default_panel.join(",")
}

/// Options for one install run; the defaults are the Python test's `install_()`.
#[derive(Default, Clone)]
struct Opts {
    panel: Option<String>,
    listing: Option<String>,
    progress: Option<ProgressStyle>,
    summary: Option<SummaryStyle>,
    status_line: Option<StatusLineMode>,
    install_dir: Option<PathBuf>,
    catalog: Option<Catalog>,
    key: Option<String>,
}

fn panel(p: &str) -> Opts {
    Opts {
        panel: Some(p.to_string()),
        ..Opts::default()
    }
}

fn live() -> Opts {
    Opts {
        listing: Some(listing_body()),
        ..Opts::default()
    }
}

impl Opts {
    fn with_live(mut self) -> Self {
        self.listing = Some(listing_body());
        self
    }
    fn listing(mut self, body: &str) -> Self {
        self.listing = Some(body.to_string());
        self
    }
    fn status(mut self, mode: StatusLineMode) -> Self {
        self.status_line = Some(mode);
        self
    }
    fn summary(mut self, s: SummaryStyle) -> Self {
        self.summary = Some(s);
        self
    }
    fn progress(mut self, p: ProgressStyle) -> Self {
        self.progress = Some(p);
        self
    }
    fn at(mut self, dir: &Path) -> Self {
        self.install_dir = Some(dir.to_path_buf());
        self
    }
}

struct Gen {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    claude: PathBuf,
    install: PathBuf,
    settings_path: PathBuf,
    templates: Templates,
}

impl Gen {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("consult-gen-")
            .tempdir()
            .expect("tmp");
        let root = dunce::canonicalize(tmp.path()).expect("canon");
        let claude = root.join("claude");
        std::fs::create_dir_all(&claude).expect("mkdir");
        let mut g = Self {
            _tmp: tmp,
            settings_path: claude.join("settings.json"),
            claude,
            install: PathBuf::new(),
            root,
            templates: Templates::embedded(),
        };
        g.install = g.make_install("install");
        g
    }

    /// An install dir laid out as the installer leaves it, with a stand-in binary.
    fn make_install(&self, name: &str) -> PathBuf {
        let install = self.root.join(name);
        std::fs::create_dir_all(install.join("bin")).expect("mkdir");
        std::fs::write(install.join("bin").join(exe_name()), b"stand-in").expect("write");
        install
    }

    /// An install dir as the Python installer left it.
    fn make_legacy_install(&self, name: &str) -> PathBuf {
        let install = self.root.join(name);
        std::fs::create_dir_all(install.join("hooks")).expect("mkdir");
        for kind in [HookKind::Summary, HookKind::Display, HookKind::Statusline] {
            std::fs::write(
                install.join("hooks").join(kind.legacy_script()),
                "# stand-in\n",
            )
            .expect("write");
        }
        for f in ["panel.py", "server.py"] {
            std::fs::write(install.join(f), "# stand-in\n").expect("write");
        }
        install
    }

    fn try_install(&self, o: Opts) -> Result<InstallReport, GenerateError> {
        let cat = o.catalog.unwrap_or_else(embedded);
        let install = o.install_dir.unwrap_or_else(|| self.install.clone());
        let inputs = InstallInputs {
            catalog: &cat,
            listing: o.listing.as_deref().map(|b| (b, fetched())),
            panel: split_panel(&o.panel.unwrap_or_else(default_panel)),
            install_dir: &install,
            claude_dir: &self.claude,
            key: o.key,
            progress_style: o.progress,
            summary_style: o.summary,
            status_line: o.status_line.unwrap_or_default(),
            templates: &self.templates,
            now: None,
        };
        install_files(&inputs)
    }

    fn install_(&self, o: Opts) -> InstallReport {
        self.try_install(o).expect("installed")
    }

    fn error(&self, o: Opts) -> String {
        self.try_install(o).expect_err("refused").to_string()
    }

    fn uninstall_at(&self, dir: &Path) -> UninstallReport {
        uninstall_files(dir, &self.claude, false).expect("uninstalled")
    }

    fn uninstall(&self) -> UninstallReport {
        self.uninstall_at(&self.install)
    }

    fn models_json(&self) -> Value {
        read_json(&self.install.join("models.json"))
    }

    fn manifest(&self) -> Value {
        read_json(&self.install.join("manifest.json"))
    }

    fn rendered(&self, rel: &str) -> String {
        std::fs::read_to_string(self.claude.join(rel)).expect("rendered")
    }

    fn write_settings(&self, data: &Value) {
        std::fs::write(
            &self.settings_path,
            serde_json::to_string_pretty(data).expect("json"),
        )
        .expect("write");
    }

    fn settings(&self) -> Value {
        read_json(&self.settings_path)
    }

    fn display(&self) -> Value {
        read_json(&self.install.join("display.json"))
    }

    fn cmd(&self, kind: HookKind) -> String {
        hook_command(&self.install, kind)
    }

    fn workflow(&self) -> PathBuf {
        self.claude.join("workflows").join("verify-claims.js")
    }
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).expect("read")).expect("json")
}

fn ours_post(install: &Path) -> Value {
    json!({"matcher": CONSULT_TOOLS, "hooks": [{"type": "command", "command": hook_command(install, HookKind::Summary)}]})
}
fn ours_display(install: &Path) -> Value {
    json!({"hooks": [{"type": "command", "command": hook_command(install, HookKind::Display)}]})
}
fn ours_status(install: &Path) -> Value {
    json!({"type": "command", "command": hook_command(install, HookKind::Statusline)})
}
fn legacy_post(install: &Path) -> Value {
    json!({"matcher": CONSULT_TOOLS, "hooks": [{"type": "command", "command": legacy_hook_command(install, HookKind::Summary)}]})
}
fn legacy_display(install: &Path) -> Value {
    json!({"hooks": [{"type": "command", "command": legacy_hook_command(install, HookKind::Display)}]})
}
fn legacy_status(install: &Path) -> Value {
    json!({"type": "command", "command": legacy_hook_command(install, HookKind::Statusline)})
}

/// A user's own command that pipes its stdin to one of our hooks, the way anyone who
/// keeps their own status line might.
fn piped(install: &Path, kind: HookKind) -> String {
    let exe = install.join("bin").join(exe_name());
    let ours = format!("\"{}\" hook {}", exe.display(), kind.as_str());
    format!(
        "bash -c 'input=$(cat); echo \"$(bash ~/.claude/my-status.sh) $(echo \"$input\" | {ours})\"'"
    )
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn path_str(p: &Path) -> String {
    p.display().to_string()
}

// ---- tests ---------------------------------------------------------------

#[test]
fn fresh_install() {
    let g = Gen::new();
    let res = g.install_(Opts::default());
    let s = g.settings();
    assert_eq!(
        s["hooks"],
        json!({"PostToolUse": [ours_post(&g.install)], "MessageDisplay": [ours_display(&g.install)]})
    );
    assert_eq!(s["statusLine"], ours_status(&g.install));
    assert_eq!(res.status_line, StatusLineOutcome::Set);
    assert!(
        res.settings_changed
            .contains(&"hooks.PostToolUse".to_string())
    );
    assert!(res.settings_changed.contains(&"statusLine".to_string()));
    assert!(s["env"].get("OPENROUTER_API_KEY").is_none());
    assert_eq!(s["env"]["MCP_TOOL_TIMEOUT"], "2400000");
    assert_eq!(s["env"]["CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"], "1800000");
    assert_eq!(g.display()["progress"], "full");
    assert_eq!(g.display()["summary"], "dim");
    assert_eq!(
        serde_json::to_value(res.display).expect("json"),
        json!({"progress": "full", "summary": "dim"})
    );

    // The quoted binary of this install, forward slashes, then the hook.
    let command = s["statusLine"]["command"].as_str().expect("command");
    let exe = consult_core::util::as_posix(&g.install.join("bin").join(exe_name()));
    assert_eq!(command, format!("\"{exe}\" hook statusline"));
    assert!(!command.contains('\\'));

    let manifest = g.manifest();
    assert_eq!(
        manifest["settings"]["hooks"],
        json!({"PostToolUse": g.cmd(HookKind::Summary), "MessageDisplay": g.cmd(HookKind::Display)})
    );
    assert_eq!(
        manifest["settings"]["statusLine"],
        g.cmd(HookKind::Statusline)
    );
    assert!(manifest["status_line_displaced"].is_null());
    assert_eq!(manifest["panel"], json!(embedded().default_panel));
    assert_eq!(manifest["claude_dir"], path_str(&g.claude));
    // The report serialises with the Python keys.
    let keys: Vec<String> = serde_json::to_value(&res)
        .expect("json")
        .as_object()
        .expect("obj")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        [
            "panel",
            "priced_at",
            "written",
            "backed_up",
            "backup_dir",
            "removed",
            "restored",
            "settings_changed",
            "display",
            "status_line"
        ]
    );
}

#[test]
fn models_json_has_command() {
    let g = Gen::new();
    g.install_(Opts::default());
    let cat = embedded();
    let models = g.models_json()["models"].clone();
    let aliases: Vec<&String> = models.as_object().expect("obj").keys().collect();
    assert_eq!(aliases, cat.models.keys().collect::<Vec<_>>());
    for (alias, m) in models.as_object().expect("obj") {
        assert_eq!(m["command"], cat.models[alias].command, "{alias}");
    }
}

// ---- favourites, the live listing and what is rendered from them ------------

#[test]
fn offline_install_says_price_unknown() {
    let g = Gen::new();
    let res = g.install_(Opts::default());
    assert!(res.priced_at.is_none());
    let doc = g.models_json();
    assert!(doc["priced_at"].is_null());
    assert_eq!(doc["default_panel"], json!(embedded().default_panel));
    for (alias, m) in doc["models"].as_object().expect("obj") {
        let keys: Vec<&str> = m
            .as_object()
            .expect("obj")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, K4_KEYS, "{alias}");
        assert_eq!(m["curated"], true);
        assert!(m["context"].is_null() && m["price_in"].is_null() && m["price_out"].is_null());
    }
    assert!(
        g.rendered("commands/deepseek.md")
            .contains("(`deepseek-v4-pro`, price unknown)")
    );
    let skill = g.rendered("skills/openrouter-workflow/SKILL.md");
    assert!(skill.contains("| alias | lab | plays to | $/M in-out (price unknown) |"));
    assert!(skill.contains("| `glm-5.2` | Z.ai | whole subsystems, multi-file work, long-horizon coherence | price unknown |"));
    assert!(g.rendered("commands/cleanroom.md").contains(
        "`gemini-3.1-pro` (Google, price unknown), `kimi-k3` (Moonshot AI, price unknown), `gpt-6-sol` (OpenAI, price unknown)."
    ));
    for rel in [
        "commands/consult.md",
        "commands/cleanroom.md",
        "commands/deepseek.md",
        "skills/openrouter-workflow/SKILL.md",
    ] {
        assert!(!g.rendered(rel).contains(" as of "), "{rel}");
    }
}

#[test]
fn live_install_prices_from_the_listing() {
    let g = Gen::new();
    let res = g.install_(live());
    assert_eq!(res.priced_at.as_deref(), Some("2026-09-25T12:34:56Z"));
    let doc = g.models_json();
    assert_eq!(doc["priced_at"], "2026-09-25T12:34:56Z");
    let models = &doc["models"];
    let cat = embedded();
    assert_eq!(
        models["deepseek-v4-pro"],
        json!({
        "id": "deepseek/deepseek-v4-pro", "lab": "DeepSeek", "command": "deepseek",
        "note": cat.models["deepseek-v4-pro"].note, "curated": true,
        "context": 1_048_576, "price_in": 0.55071, "price_out": 1.10142})
    );
    assert_eq!(
        (
            &models["gpt-6-luna-pro"]["price_in"],
            &models["gpt-6-luna-pro"]["context"]
        ),
        (&json!(0.05), &json!(1_050_000))
    );
    // A favourite the listing no longer carries is still registered, unpriced.
    let sol = &models["gpt-6-sol"];
    assert_eq!(
        (
            &sol["curated"],
            &sol["context"],
            &sol["price_in"],
            &sol["price_out"]
        ),
        (&json!(true), &Value::Null, &Value::Null, &Value::Null)
    );

    assert!(g.rendered("commands/deepseek.md").contains(&format!(
        "(`deepseek-v4-pro`, 0.55/1.10 USD per M tokens in/out {AS_OF})."
    )));
    let skill = g.rendered("skills/openrouter-workflow/SKILL.md");
    assert!(skill.contains(&format!(
        "| alias | lab | plays to | $/M in-out ({AS_OF}) |"
    )));
    assert!(skill.contains("| `gpt-6-luna-pro` | OpenAI | agentic investigation; the only non-Chinese lineage at its price | 0.05 / 0.25 |"));
    let extras = format!(
        "`gemini-3.1-pro` (Google, 1.00/6.00 USD per M tokens in/out), `kimi-k3` (Moonshot AI, 3.00/15.00 USD per M tokens in/out), `gpt-6-sol` (OpenAI, price unknown); prices {AS_OF}."
    );
    assert!(g.rendered("commands/cleanroom.md").contains(&extras));
    assert!(skill.contains(&extras));
}

#[test]
fn unreadable_listing_means_offline() {
    let anthropic_only = json!({"data": [listing_data()[12].clone()]}).to_string();
    for (label, body) in [
        ("not json", "<html>rate limited</html>".to_string()),
        ("no data", json!({"error": "down"}).to_string()),
        ("empty", json!({"data": []}).to_string()),
        ("only anthropic", anthropic_only),
        ("empty body", String::new()),
    ] {
        let g = Gen::new();
        let res = g.install_(Opts::default().listing(&body));
        assert!(res.priced_at.is_none(), "{label}");
        assert!(g.models_json()["priced_at"].is_null(), "{label}");
    }
}

#[test]
fn non_favourite_panel_member() {
    let g = Gen::new();
    let res = g.install_(panel("deepseek-v4-pro,mistralai/devstral-2,glm-5.2").with_live());
    assert_eq!(
        strs(&res.panel),
        ["deepseek-v4-pro", "devstral-2", "glm-5.2"]
    );
    let doc = g.models_json();
    assert_eq!(
        doc["default_panel"],
        json!(["deepseek-v4-pro", "devstral-2", "glm-5.2"])
    );
    // Every favourite first, in catalog order, then the one from the listing.
    let mut want: Vec<String> = embedded().models.keys().cloned().collect();
    want.push("devstral-2".into());
    let got: Vec<String> = doc["models"]
        .as_object()
        .expect("obj")
        .keys()
        .cloned()
        .collect();
    assert_eq!(got, want);
    assert_eq!(
        doc["models"]["devstral-2"],
        json!({
        "id": "mistralai/devstral-2", "lab": "mistralai", "command": "devstral-2",
        "note": FALLBACK_NOTE, "curated": false,
        "context": 262_144, "price_in": 0.4, "price_out": 2.0})
    );

    let quick = g.rendered("commands/devstral-2.md");
    assert!(quick.contains("description: Quick outside view from Devstral 2 on what we're currently discussing — mistralai/devstral-2\n"));
    assert!(quick.contains(&format!(
        "from **Devstral 2**\n(`devstral-2`, 0.40/2.00 USD per M tokens in/out {AS_OF})."
    )));
    assert!(quick.contains(FALLBACK_PITCH));
    assert!(quick.contains("`models: [\"devstral-2\"]`"));
    assert!(
        g.rendered("commands/consult.md")
            .contains("**DeepSeek V4 Pro**, **Devstral 2**, **GLM 5.2**")
    );
    assert!(
        g.rendered("skills/openrouter-workflow/SKILL.md")
            .contains("| `devstral-2` | mistralai | not curated | 0.40 / 2.00 |")
    );
    assert!(
        g.rendered("workflows/verify-claims.js").contains(
            "const DEFAULT_VOICES = [\"deepseek-v4-pro\", \"devstral-2\", \"glm-5.2\"]\n"
        )
    );

    // Dropped from the panel, it leaves neither a command nor a registration.
    let res = g.install_(live());
    let dropped = g.claude.join("commands").join("devstral-2.md");
    assert!(res.removed.contains(&path_str(&dropped)));
    assert!(!dropped.exists());
    assert!(g.models_json()["models"].get("devstral-2").is_none());
}

#[test]
fn non_favourite_offline_is_taken_on_trust() {
    let g = Gen::new();
    g.install_(panel("glm-5.2,mistralai/devstral-2"));
    let m = &g.models_json()["models"]["devstral-2"];
    assert_eq!(
        (&m["id"], &m["lab"], &m["curated"], &m["price_in"]),
        (
            &json!("mistralai/devstral-2"),
            &json!("mistralai"),
            &json!(false),
            &Value::Null
        )
    );
    assert!(
        g.rendered("commands/devstral-2.md")
            .contains("from **mistralai/devstral-2**\n(`devstral-2`, price unknown).")
    );
}

#[test]
fn favourite_named_by_its_id_is_the_favourite() {
    let g = Gen::new();
    let res =
        g.install_(panel("deepseek/deepseek-v4-pro,glm-5.2,openai/gpt-6-luna-pro").with_live());
    assert_eq!(res.panel, embedded().default_panel);
    assert!(
        g.models_json()["models"]
            .as_object()
            .expect("obj")
            .values()
            .all(|m| m["curated"] == true)
    );
    assert!(
        g.error(panel("deepseek-v4-pro,deepseek/deepseek-v4-pro"))
            .contains("twice")
    );
    assert!(
        g.error(panel("mistralai/devstral-2,mistralai/devstral-2"))
            .contains("twice")
    );
}

#[test]
fn clashing_names_get_suffixes() {
    let g = Gen::new();
    // Against the reserved commands, a favourite's command, a favourite's alias, and one another.
    let res =
        g.install_(panel("acme/consult,acme/deepseek,other/DeepSeek,acme/kimi-k3").with_live());
    assert_eq!(
        strs(&res.panel),
        ["consult-2", "deepseek-2", "deepseek-3", "kimi-k3-2"]
    );
    let models = &g.models_json()["models"];
    let ids: Vec<&Value> = res.panel.iter().map(|a| &models[a]["id"]).collect();
    assert_eq!(
        ids,
        [
            &json!("acme/consult"),
            &json!("acme/deepseek"),
            &json!("other/DeepSeek"),
            &json!("acme/kimi-k3")
        ]
    );
    assert_eq!(models["deepseek-v4-pro"]["command"], "deepseek");
    assert_eq!(models["kimi-k3"]["id"], "moonshotai/kimi-k3");
    for command in &res.panel {
        assert!(
            g.claude
                .join("commands")
                .join(format!("{command}.md"))
                .is_file(),
            "{command}"
        );
    }
    // The reserved command stays the panel's own.
    assert!(
        g.rendered("commands/consult.md")
            .contains("The full panel:")
    );
    // Nulls and junk are no price; free is a price.
    assert_eq!(
        (
            &models["deepseek-2"]["context"],
            &models["deepseek-3"]["price_in"],
            &models["deepseek-3"]["price_out"],
            &models["kimi-k3-2"]["price_in"]
        ),
        (&Value::Null, &Value::Null, &Value::Null, &json!(0.0))
    );
}

#[test]
fn listing_names_cannot_break_a_command_file() {
    let g = Gen::new();
    // A name with ": ", "#", quotes, "$" before a digit or a placeholder would break the
    // YAML description line or trip the render checks.
    let res = g.install_(panel("acme/weird").with_live());
    let quick = g.rendered("commands/weird.md");
    assert_eq!(strs(&res.panel), ["weird"]);
    assert!(quick.contains("description: Quick outside view from Weird 5 NAME 1 model on "));
    assert!(quick.contains("(`weird`, <0.01/1.00 USD per M tokens in/out"));
}

#[test]
fn anthropic_models_are_refused() {
    let g = Gen::new();
    for id in [
        "anthropic/claude-opus-5.5",
        "Anthropic/claude-opus-5.5",
        "~anthropic/claude-opus-latest",
    ] {
        for with_live in [false, true] {
            let mut o = panel(&format!("glm-5.2,{id}"));
            if with_live {
                o = o.with_live();
            }
            assert!(g.error(o).contains("Anthropic"), "{id}");
        }
    }
    let (listing, _) = load_listing(Some(&listing_body()), Some(fetched()));
    assert!(!listing.keys().any(|i| i.starts_with("anthropic/")));
}

#[test]
fn batch_variants_are_refused() {
    let g = Gen::new();
    let mut data = listing_data();
    data.push(listed(
        "openai/gpt-6-luna-pro:batch",
        "OpenAI: GPT-6 Luna Pro (batch)",
        "0.000000025",
        "0.000000125",
        CTX,
        true,
    ));
    let body = json!({"data": data}).to_string();
    for with_live in [false, true] {
        let mut o = panel("glm-5.2,openai/gpt-6-luna-pro:batch");
        if with_live {
            o = o.listing(&body);
        }
        assert!(g.error(o).contains(":batch variant"));
    }
    let (listing, _) = load_listing(Some(&body), Some(fetched()));
    assert!(!listing.contains_key("openai/gpt-6-luna-pro:batch"));
    assert!(listing.contains_key("openai/gpt-6-luna-pro"));
}

#[test]
fn routers_and_presets_are_refused() {
    let g = Gen::new();
    let routers = [
        listed(
            "openrouter/auto",
            "Auto Router",
            "-1",
            "-1",
            Some(2_000_000),
            true,
        ),
        listed(
            "typesafe/jev-router",
            "Jev Router",
            "-1",
            "-1",
            Some(2_000_000),
            true,
        ),
        listed(
            "acme/half-router",
            "Acme: Half",
            "0.000001",
            "-1",
            CTX,
            true,
        ),
    ];
    let mut data = listing_data();
    data.extend(routers.iter().cloned());
    let body = json!({"data": data}).to_string();
    let cat = embedded();
    for (id, error) in [
        ("openrouter/auto", "lets OpenRouter choose"),
        ("OpenRouter/Auto", "lets OpenRouter choose"),
        ("@preset/x", "preset"),
        ("openai/gpt-4o@preset/x", "preset"),
        ("deepseek/deepseek-v4-pro@preset/x", "preset"),
    ] {
        for with_live in [false, true] {
            let mut o = panel(&format!("glm-5.2,{id}"));
            if with_live {
                o = o.listing(&body);
            }
            assert!(g.error(o).contains(error), "{id}");
        }
        let e = pick_panel(&cat, &["glm-5.2".into(), id.into()], &IndexMap::new())
            .expect_err("refused");
        assert!(e.to_string().contains(error), "{id}");
    }
    // A router with an ordinary id is known only by its per-request price.
    let (listing, _) = load_listing(Some(&body), Some(fetched()));
    for r in &routers {
        assert!(!listing.contains_key(r["id"].as_str().expect("id")));
    }
    assert!(listing.contains_key("z-ai/glm-5.2"));
    for id in ["typesafe/jev-router", "acme/half-router"] {
        assert!(
            g.error(panel(&format!("glm-5.2,{id}")).listing(&body))
                .contains("tool-capable"),
            "{id}"
        );
    }
    assert!(!g.install.join("models.json").exists());
}

#[test]
fn panel_entries_that_are_refused() {
    let g = Gen::new();
    for (p, with_live, error) in [
        ("glm-5.2,acme/no-tools", true, "tool-capable"),
        ("glm-5.2,acme/unlisted", true, "tool-capable"),
        (
            "glm-5.2,acme/has space",
            false,
            "not an OpenRouter model id",
        ),
        ("glm-5.2,/nothing", false, "not an OpenRouter model id"),
        ("glm-5.2,nope", false, "not in the catalog: nope"),
        (",", false, "empty"),
    ] {
        let mut o = panel(p);
        if with_live {
            o = o.with_live();
        }
        let e = g.error(o);
        assert!(e.contains(error), "{p}: {e}");
    }
    assert!(!g.install.join("models.json").exists());
}

#[test]
fn a_dollar_amount_in_the_catalog_stops_the_install() {
    let g = Gen::new();
    // A pitch that slips a "$" amount back into the catalog stops the install.
    let mut raw: Value = serde_json::from_str(catalog::CATALOG_JSON).expect("json");
    raw["models"]["glm-5.2"]["pitch"] = json!("Cheap: $0.40 a review.");
    let cat = catalog::load_catalog(&raw.to_string()).expect("valid");
    let e = g.error(Opts {
        catalog: Some(cat),
        ..Opts::default()
    });
    assert!(e.contains("argument substitution"), "{e}");
    assert!(!g.settings_path.exists());
}

// ---- the verify-claims workflow ---------------------------------------------

#[test]
fn workflow_defaults_to_the_installed_panel() {
    let g = Gen::new();
    let res = g.install_(Opts::default());
    let text = std::fs::read_to_string(g.workflow()).expect("workflow");
    let voices = serde_json::to_string(&embedded().default_panel)
        .expect("json")
        .replace(',', ", ");
    assert!(text.contains(&format!("const DEFAULT_VOICES = {voices}\n")));
    assert!(!text.contains("{{"));
    assert!(res.written.contains(&path_str(&g.workflow())));
    assert!(
        g.manifest()["files"]
            .as_array()
            .expect("files")
            .contains(&json!(path_str(&g.workflow())))
    );

    // A new panel is the new default, with no backup of our own old output.
    let res = g.install_(panel("glm-5.2,kimi-k3"));
    assert!(res.backed_up.is_empty());
    assert!(
        std::fs::read_to_string(g.workflow())
            .expect("workflow")
            .contains("const DEFAULT_VOICES = [\"glm-5.2\", \"kimi-k3\"]\n")
    );

    let res = g.uninstall();
    assert!(res.removed.contains(&path_str(&g.workflow())));
    assert!(!g.workflow().exists());
}

#[test]
fn users_own_workflow_is_backed_up_and_restored() {
    let g = Gen::new();
    let mine = "export const meta = { name: 'verify-claims' }  // the user's own\n";
    std::fs::create_dir_all(g.workflow().parent().expect("parent")).expect("mkdir");
    std::fs::write(g.workflow(), mine).expect("write");
    let res = g.install_(Opts::default());
    assert_eq!(res.backed_up, [path_str(&g.workflow())]);
    let backup = PathBuf::from(res.backup_dir.expect("backup dir"))
        .join("workflows")
        .join("verify-claims.js");
    assert_eq!(std::fs::read_to_string(backup).expect("backup"), mine);
    assert!(
        std::fs::read_to_string(g.workflow())
            .expect("workflow")
            .contains("const DEFAULT_VOICES = ")
    );

    let res = g.uninstall();
    assert!(res.restored.contains(&path_str(&g.workflow())));
    assert_eq!(
        std::fs::read_to_string(g.workflow()).expect("workflow"),
        mine
    );
}

#[test]
fn reinstall_is_idempotent() {
    let g = Gen::new();
    g.write_settings(&json!({"env": {"MY_VAR": "1"}, "hooks": {"PostToolUse": [user_post()]}}));
    g.install_(Opts::default());
    let first = std::fs::read(&g.settings_path).expect("read");
    let res = g.install_(Opts::default());
    assert_eq!(std::fs::read(&g.settings_path).expect("read"), first);
    assert!(res.settings_changed.is_empty());
    assert_eq!(res.status_line, StatusLineOutcome::Ours);
    let s = g.settings();
    assert_eq!(
        s["hooks"]["PostToolUse"],
        json!([user_post(), ours_post(&g.install)])
    );
    assert_eq!(
        s["hooks"]["MessageDisplay"],
        json!([ours_display(&g.install)])
    );
    // The user's keys keep their order, ours follow.
    let keys: Vec<&String> = s.as_object().expect("obj").keys().collect();
    assert_eq!(keys, ["env", "hooks", "statusLine"]);
}

#[test]
fn user_hooks_preserved_and_not_reordered() {
    let g = Gen::new();
    g.write_settings(&json!({
        "model": "opus",
        "hooks": {"PostToolUse": [user_post(), user_post_2()], "MessageDisplay": [user_display()], "Stop": [user_display()]},
    }));
    g.install_(Opts::default());
    let mut s = g.settings();
    assert_eq!(s["model"], "opus");
    assert_eq!(
        s["hooks"]["PostToolUse"],
        json!([user_post(), user_post_2(), ours_post(&g.install)])
    );
    assert_eq!(
        s["hooks"]["MessageDisplay"],
        json!([user_display(), ours_display(&g.install)])
    );
    assert_eq!(s["hooks"]["Stop"], json!([user_display()]));

    // Ours moved between the user's entries, with a stale matcher, stays in that slot on
    // re-install; the user's entries around it do not move.
    let stale = json!({"matcher": "old", "hooks": [{"type": "command", "command": g.cmd(HookKind::Summary)}]});
    s["hooks"]["PostToolUse"] = json!([user_post(), stale, user_post_2()]);
    g.write_settings(&s);
    g.install_(Opts::default());
    assert_eq!(
        g.settings()["hooks"]["PostToolUse"],
        json!([user_post(), ours_post(&g.install), user_post_2()])
    );
}

#[test]
fn mixed_group_loses_only_ours_and_is_not_duplicated() {
    let g = Gen::new();
    let mixed = json!({"matcher": "mcp__.*", "hooks": [
        {"type": "command", "command": "echo mine"},
        {"type": "command", "command": g.cmd(HookKind::Summary).replace('/', "\\").to_uppercase()},
    ]});
    g.write_settings(&json!({"hooks": {"PostToolUse": [mixed]}}));
    g.install_(Opts::default());
    assert_eq!(
        g.settings()["hooks"]["PostToolUse"],
        json!([
            {"matcher": "mcp__.*", "hooks": [{"type": "command", "command": "echo mine"}]},
            ours_post(&g.install),
        ])
    );
}

#[test]
fn odd_entries_are_tolerated_and_kept() {
    let g = Gen::new();
    let odd = json!(["not a group", {"matcher": "x"}, {"hooks": "nope"}, {"hooks": ["str", {"type": "command", "command": 7}]}]);
    g.write_settings(&json!({"hooks": {"PostToolUse": odd.clone()}, "statusLine": "weird"}));
    let res = g.install_(Opts::default());
    let s = g.settings();
    let mut want = odd.as_array().expect("arr").clone();
    want.push(ours_post(&g.install));
    assert_eq!(s["hooks"]["PostToolUse"], Value::Array(want));
    assert_eq!(s["statusLine"], "weird");
    assert_eq!(res.status_line, StatusLineOutcome::Kept);
}

#[test]
fn unmergeable_settings_stop_before_any_write() {
    let g = Gen::new();
    g.write_settings(&json!({"hooks": ["not", "an", "object"]}));
    let before = std::fs::read(&g.settings_path).expect("read");
    let e = g.error(Opts::default());
    assert!(e.contains("hooks"), "{e}");
    assert_eq!(std::fs::read(&g.settings_path).expect("read"), before);
    assert!(!g.claude.join("commands").exists());
    assert!(!g.install.join("manifest.json").exists());

    std::fs::write(&g.settings_path, "{not json").expect("write");
    assert!(g.error(Opts::default()).contains("is not valid JSON"));
    g.write_settings(&json!({"env": "x"}));
    assert!(g.error(Opts::default()).contains("'env' in "));
    g.write_settings(&json!({"hooks": {"PostToolUse": {"x": 1}}}));
    assert!(g.error(Opts::default()).contains("'hooks.PostToolUse' in "));
    assert!(!g.claude.join("commands").exists());
}

#[test]
fn missing_binary_is_refused() {
    // A hook whose program is missing fails, which Claude Code takes as a blocking
    // hook error on every call.
    let g = Gen::new();
    std::fs::remove_file(g.install.join("bin").join(exe_name())).expect("rm");
    let e = g.error(Opts::default());
    assert!(e.contains("claude-consult binary is missing"), "{e}");
    assert!(!g.settings_path.exists());
}

#[test]
fn the_key_is_stored_clean_or_refused() {
    let g = Gen::new();
    let res = g.install_(Opts {
        key: Some("\u{feff} sk-or-v1-test \r\n".into()),
        ..Opts::default()
    });
    assert_eq!(g.settings()["env"]["OPENROUTER_API_KEY"], "sk-or-v1-test");
    assert!(
        res.settings_changed
            .contains(&"OPENROUTER_API_KEY".to_string())
    );
    let res = g.install_(Opts {
        key: Some("sk-or-v1-test".into()),
        ..Opts::default()
    });
    assert!(
        !res.settings_changed
            .contains(&"OPENROUTER_API_KEY".to_string())
    );
    let e = g.error(Opts {
        key: Some("has space".into()),
        ..Opts::default()
    });
    assert!(e.contains("spaces or non-ASCII"), "{e}");
    let res = uninstall_files(&g.install, &g.claude, true).expect("uninstalled");
    assert!(res.key_removed);
    assert!(g.settings()["env"].get("OPENROUTER_API_KEY").is_none());
}

#[test]
fn summary_off_removes_message_display() {
    let g = Gen::new();
    g.write_settings(&json!({"hooks": {"MessageDisplay": [user_display()]}}));
    g.install_(Opts::default());
    assert_eq!(
        g.settings()["hooks"]["MessageDisplay"],
        json!([user_display(), ours_display(&g.install)])
    );
    let res = g.install_(Opts::default().summary(SummaryStyle::Off));
    assert!(
        res.settings_changed
            .contains(&"hooks.MessageDisplay".to_string())
    );
    assert_eq!(
        g.settings()["hooks"]["MessageDisplay"],
        json!([user_display()])
    );
    assert_eq!(g.display()["summary"], "off");
    assert!(
        g.manifest()["settings"]["hooks"]
            .get("MessageDisplay")
            .is_none()
    );

    // With only ours there, the event list goes entirely.
    g.write_settings(&json!({}));
    g.install_(Opts::default().summary(SummaryStyle::Off));
    assert!(g.settings()["hooks"].get("MessageDisplay").is_none());
    assert_eq!(
        g.settings()["hooks"]["PostToolUse"],
        json!([ours_post(&g.install)])
    );

    // A later run without the flag keeps it off, and turning it back on brings the hook back.
    g.install_(Opts::default());
    assert_eq!(g.display()["summary"], "off");
    assert!(g.settings()["hooks"].get("MessageDisplay").is_none());
    g.install_(Opts::default().summary(SummaryStyle::Dim));
    assert_eq!(
        g.settings()["hooks"]["MessageDisplay"],
        json!([ours_display(&g.install)])
    );
}

#[test]
fn display_styles_kept_on_reinstall() {
    let g = Gen::new();
    g.install_(
        Opts::default()
            .progress(ProgressStyle::Marks)
            .summary(SummaryStyle::Quote),
    );
    assert_eq!(
        (
            g.display()["progress"].clone(),
            g.display()["summary"].clone()
        ),
        (json!("marks"), json!("quote"))
    );
    let res = g.install_(Opts::default());
    assert_eq!(
        serde_json::to_value(res.display).expect("json"),
        json!({"progress": "marks", "summary": "quote"})
    );
    g.install_(Opts::default().progress(ProgressStyle::Ticker));
    assert_eq!(
        (
            g.display()["progress"].clone(),
            g.display()["summary"].clone()
        ),
        (json!("ticker"), json!("quote"))
    );
    // A hand-edited value outside the known styles falls back to the default.
    let mut d = g.display();
    d["progress"] = json!("sparkles");
    std::fs::write(g.install.join("display.json"), d.to_string()).expect("write");
    let res = g.install_(Opts::default());
    assert_eq!(
        serde_json::to_value(res.display).expect("json"),
        json!({"progress": "full", "summary": "quote"})
    );
    assert!(g.display()["_comment"].as_str().expect("comment").contains("progress: full, quiet, count, marks, percent, latest, ticker. summary: dim, italic, quote, off."));
}

#[test]
fn display_json_with_a_bom_is_kept_on_reinstall() {
    let g = Gen::new();
    // Notepad and PowerShell 5.1's Set-Content -Encoding UTF8 both write a BOM.
    g.install_(
        Opts::default()
            .progress(ProgressStyle::Marks)
            .summary(SummaryStyle::Off),
    );
    let text = format!("\u{feff}{}", g.display());
    std::fs::write(g.install.join("display.json"), text).expect("write");
    let res = g.install_(Opts::default());
    assert_eq!(
        serde_json::to_value(res.display).expect("json"),
        json!({"progress": "marks", "summary": "off"})
    );
    assert!(g.settings()["hooks"].get("MessageDisplay").is_none());
}

#[test]
fn status_line_auto_keeps_a_foreign_one() {
    let g = Gen::new();
    g.write_settings(&json!({"statusLine": user_status()}));
    let res = g.install_(Opts::default());
    assert_eq!(res.status_line, StatusLineOutcome::Kept);
    assert_eq!(g.settings()["statusLine"], user_status());
    assert!(!res.settings_changed.contains(&"statusLine".to_string()));
    assert!(g.manifest()["settings"]["statusLine"].is_null());
}

#[test]
fn status_line_keep() {
    let g = Gen::new();
    g.write_settings(&json!({"statusLine": user_status()}));
    let res = g.install_(Opts::default().status(StatusLineMode::Keep));
    assert_eq!(res.status_line, StatusLineOutcome::Kept);
    assert_eq!(g.settings()["statusLine"], user_status());
    // With nothing to keep, keep adds nothing either.
    g.write_settings(&json!({}));
    let res = g.install_(Opts::default().status(StatusLineMode::Keep));
    assert_eq!(res.status_line, StatusLineOutcome::Absent);
    assert!(g.settings().get("statusLine").is_none());
}

#[test]
fn status_line_replace_is_undone_by_uninstall() {
    let g = Gen::new();
    g.write_settings(&json!({"statusLine": user_status()}));
    let res = g.install_(Opts::default().status(StatusLineMode::Replace));
    assert_eq!(res.status_line, StatusLineOutcome::Replaced);
    assert_eq!(g.settings()["statusLine"], ours_status(&g.install));
    assert_eq!(g.manifest()["status_line_displaced"], user_status());
    // A later plain re-install keeps ours and still remembers the user's.
    assert_eq!(
        g.install_(Opts::default()).status_line,
        StatusLineOutcome::Ours
    );
    let res = g.uninstall();
    assert!(res.status_line_restored);
    assert_eq!(g.settings()["statusLine"], user_status());
}

#[test]
fn status_line_ours_is_refreshed_in_place() {
    let g = Gen::new();
    g.install_(Opts::default());
    let mut s = g.settings();
    s["statusLine"]["padding"] = json!(2);
    g.write_settings(&s);
    assert_eq!(
        g.install_(Opts::default()).status_line,
        StatusLineOutcome::Ours
    );
    let mut want = ours_status(&g.install);
    want["padding"] = json!(2);
    assert_eq!(g.settings()["statusLine"], want);
}

#[test]
fn uninstall_removes_exactly_ours() {
    let g = Gen::new();
    let user = json!({
        "env": {"MY_VAR": "1"},
        "hooks": {"PostToolUse": [user_post(), user_post_2()], "MessageDisplay": [user_display()]},
        "statusLine": user_status(),
    });
    g.write_settings(&user);
    g.install_(Opts::default());
    let res = g.uninstall();
    let s = g.settings();
    assert_eq!(s["hooks"], user["hooks"]);
    assert_eq!(s["statusLine"], user_status());
    assert_eq!(s["env"]["MY_VAR"], "1");
    assert!(!res.status_line_restored);
    let mut removed = res.settings_removed.clone();
    removed.sort();
    assert_eq!(removed, ["hooks.MessageDisplay", "hooks.PostToolUse"]);
}

#[test]
fn uninstall_after_fresh_install_leaves_no_trace_in_settings() {
    let g = Gen::new();
    g.install_(Opts::default());
    let res = g.uninstall();
    let s = g.settings();
    assert!(s.get("hooks").is_none());
    assert!(s.get("statusLine").is_none());
    let mut removed = res.settings_removed.clone();
    removed.sort();
    assert_eq!(
        removed,
        ["hooks.MessageDisplay", "hooks.PostToolUse", "statusLine"]
    );
    assert!(!g.claude.join("commands").join("consult.md").exists());
    assert!(!g.claude.join("skills").join("openrouter-workflow").exists());
}

#[test]
fn uninstall_keeps_user_empty_lists_and_odd_shapes() {
    let g = Gen::new();
    g.install_(Opts::default());
    let mut s = g.settings();
    s["hooks"]["Stop"] = json!([]);
    s["hooks"]["Weird"] = json!("not a list");
    g.write_settings(&s);
    g.uninstall();
    assert_eq!(
        g.settings()["hooks"],
        json!({"Stop": [], "Weird": "not a list"})
    );
}

// ---- entries from other installs and dangling ones ------------------------

#[test]
fn install_elsewhere_takes_over_the_old_installs_entries() {
    let g = Gen::new();
    let old = g.make_install("old");
    g.write_settings(
        &json!({"hooks": {"PostToolUse": [user_post()]}, "statusLine": user_status()}),
    );
    g.install_(Opts::default().status(StatusLineMode::Replace).at(&old));
    let res = g.install_(Opts::default());
    let s = g.settings();
    // Left in place, the old entries would show every summary twice.
    assert_eq!(
        s["hooks"],
        json!({"PostToolUse": [user_post(), ours_post(&g.install)], "MessageDisplay": [ours_display(&g.install)]})
    );
    assert_eq!(s["statusLine"], ours_status(&g.install));
    assert_eq!(res.status_line, StatusLineOutcome::Ours);
    // The user's status line that the old install stood in for still comes back.
    assert!(g.uninstall().status_line_restored);
    let s = g.settings();
    assert_eq!(s["hooks"], json!({"PostToolUse": [user_post()]}));
    assert_eq!(s["statusLine"], user_status());
}

#[test]
fn uninstall_leaves_a_live_install_elsewhere_alone() {
    // Moving the install and then uninstalling the old copy must not take the summary
    // and status line away from the one now in use.
    let g = Gen::new();
    let live = g.make_install("live");
    g.install_(Opts::default());
    g.install_(Opts::default().at(&live));
    assert!(g.uninstall().settings_removed.is_empty());
    let s = g.settings();
    assert_eq!(
        s["hooks"],
        json!({"PostToolUse": [ours_post(&live)], "MessageDisplay": [ours_display(&live)]})
    );
    assert_eq!(s["statusLine"], ours_status(&live));
}

#[test]
fn entries_whose_binary_is_gone_are_replaced() {
    // A missing program fails the hook, which Claude Code takes as a blocking hook
    // error on every consult.
    let g = Gen::new();
    let gone = g.root.join("gone");
    // Our form, give or take slashes, case and surrounding whitespace.
    let evened = format!(
        " {}\n",
        hook_command(&gone, HookKind::Summary)
            .replace('/', "\\")
            .replace("\\bin\\claude-consult", "\\BIN\\Claude-Consult")
    );
    let mixed = json!({"matcher": "mcp__.*", "hooks": [
        {"type": "command", "command": "echo mine"},
        {"type": "command", "command": evened}]});
    g.write_settings(
        &json!({"hooks": {"PostToolUse": [ours_post(&gone), user_post(), mixed],
                                       "MessageDisplay": [ours_display(&gone)]},
                             "statusLine": ours_status(&gone)}),
    );
    let res = g.install_(Opts::default());
    let s = g.settings();
    assert_eq!(
        s["hooks"]["PostToolUse"],
        json!([
        ours_post(&g.install), user_post(),
        {"matcher": "mcp__.*", "hooks": [{"type": "command", "command": "echo mine"}]}])
    );
    assert_eq!(
        s["hooks"]["MessageDisplay"],
        json!([ours_display(&g.install)])
    );
    assert_eq!(s["statusLine"], ours_status(&g.install));
    assert_eq!(res.status_line, StatusLineOutcome::Ours);
}

#[test]
fn uninstall_removes_entries_whose_binary_is_gone() {
    let g = Gen::new();
    g.install_(Opts::default());
    let gone = g.root.join("gone");
    let mut s = g.settings();
    s["hooks"]["PostToolUse"]
        .as_array_mut()
        .expect("arr")
        .insert(0, ours_post(&gone));
    s["hooks"]["MessageDisplay"]
        .as_array_mut()
        .expect("arr")
        .push(legacy_display(&gone));
    g.write_settings(&s);
    g.uninstall();
    assert!(g.settings().get("hooks").is_none());
}

fn group(command: &str) -> Value {
    json!({"hooks": [{"type": "command", "command": command}]})
}

#[test]
fn lookalike_user_entries_are_kept() {
    let g = Gen::new();
    let mine = g.root.join("mine").join("hooks");
    std::fs::create_dir_all(&mine).expect("mkdir");
    std::fs::write(mine.join("statusline.py"), "# the user's own\n").expect("write");
    let status = json!({"type": "command", "command": format!("python \"{}\"", mine.join("statusline.py").display())});
    let gone = g.root.join("gone");
    let other = g.make_install("other");
    let gone_exe = gone.join("bin").join(exe_name());
    let other_exe = other.join("bin").join(exe_name());

    let lookalikes = vec![
        // Present, in a folder that is no claude-consult install.
        group(&format!(
            "python \"{}\"",
            mine.join("statusline.py").display()
        )),
        group(&legacy_hook_command(
            mine.parent().expect("parent"),
            HookKind::Statusline,
        )),
        // Not resolvable here, so not known to be gone.
        group("python hooks/summary_hook.py"),
        group("python \"$CLAUDE_PROJECT_DIR\"/.claude/hooks/display_hook.py"),
        group("python %USERPROFILE%\\gone\\hooks\\summary_hook.py"),
        group(
            "\"%LOCALAPPDATA%/gone/.venv/Scripts/python.exe\" \"%LOCALAPPDATA%/gone/hooks/summary_hook.py\"",
        ),
        group("\"%LOCALAPPDATA%/gone/bin/claude-consult.exe\" hook summary"),
        group("\"$HOME/gone/bin/claude-consult\" hook summary"),
        group("\"./.venv/Scripts/python.exe\" \"./hooks/summary_hook.py\""),
        group("\"./bin/claude-consult.exe\" hook summary"),
        group("\"bin/claude-consult\" hook display"),
        // Gone, but not our program, or not where an install keeps it.
        group(&format!(
            "python \"{}\"",
            gone.join("hooks").join("other.py").display()
        )),
        group(&format!(
            "python \"{}\"",
            gone.join("summary_hook.py").display()
        )),
        group(&format!(
            "\"{}\" hook summary",
            gone.join("bin").join("other.exe").display()
        )),
        group(&format!(
            "\"{}\" hook summary",
            gone.join(exe_name()).display()
        )),
        group(&format!("\"{}\" hook other", gone_exe.display())),
        // Our program, even gone or from an install, run any other way than exactly as
        // the installer writes it.
        group(&format!(
            "python \"{}\\HOOKS\\Summary_Hook.py\"",
            gone.display()
        )),
        group(&format!("\"{}\" hook summary --old", other_exe.display())),
        group(&format!("\"{}\"", other_exe.display())),
        group(&format!("{} hook summary", other_exe.display())),
        group(&hook_command(&gone, HookKind::Display).replace("\" hook", "\"  hook")),
        group(&legacy_hook_command(&gone, HookKind::Display).replace("\" \"", "\"  \"")),
        group(&format!(
            "\"{}\" \"{}\"",
            gone.join(".venv")
                .join("Scripts")
                .join("python.exe")
                .display(),
            other.join("hooks").join("display_hook.py").display()
        )),
        group(&(hook_command(&g.install, HookKind::Summary) + " || exit 0")),
    ];
    g.write_settings(
        &json!({"hooks": {"PostToolUse": lookalikes.clone(), "MessageDisplay": lookalikes.clone()},
                             "statusLine": status.clone()}),
    );
    let res = g.install_(Opts::default());
    let s = g.settings();
    let mut with_post = lookalikes.clone();
    with_post.push(ours_post(&g.install));
    let mut with_display = lookalikes.clone();
    with_display.push(ours_display(&g.install));
    assert_eq!(s["hooks"]["PostToolUse"], Value::Array(with_post));
    assert_eq!(s["hooks"]["MessageDisplay"], Value::Array(with_display));
    assert_eq!(s["statusLine"], status);
    assert_eq!(res.status_line, StatusLineOutcome::Kept);
    g.uninstall();
    let s = g.settings();
    assert_eq!(
        s["hooks"],
        json!({"PostToolUse": lookalikes.clone(), "MessageDisplay": lookalikes})
    );
    assert_eq!(s["statusLine"], status);
}

#[test]
fn a_command_that_pipes_to_our_hook_is_the_users() {
    let g = Gen::new();
    let other = g.make_install("other");
    let gone = g.root.join("gone");
    let status = json!({"type": "command", "command": piped(&g.install, HookKind::Statusline)});
    let post = json!({"matcher": CONSULT_TOOLS, "hooks": [{"type": "command", "command": piped(&gone, HookKind::Summary)}]});
    let display =
        json!({"hooks": [{"type": "command", "command": piped(&other, HookKind::Display)}]});
    let user = json!({"hooks": {"PostToolUse": [post.clone()], "MessageDisplay": [display.clone()]}, "statusLine": status.clone()});
    g.write_settings(&user);
    for _ in ["install", "re-install"] {
        assert_eq!(
            g.install_(Opts::default()).status_line,
            StatusLineOutcome::Kept
        );
        let s = g.settings();
        assert_eq!(s["statusLine"], status);
        assert_eq!(
            s["hooks"],
            json!({"PostToolUse": [post.clone(), ours_post(&g.install)],
                                      "MessageDisplay": [display.clone(), ours_display(&g.install)]})
        );
    }
    g.uninstall();
    let s = g.settings();
    assert_eq!(s["hooks"], user["hooks"]);
    assert_eq!(s["statusLine"], status);
}

#[test]
fn ours_and_theirs_are_told_apart() {
    // The commands the installer replaces without asking, and those it must never touch.
    let g = Gen::new();
    let other = g.make_install("other");
    let gone = g.root.join("gone");
    let mine = g.root.join("mine");
    std::fs::create_dir_all(mine.join("hooks")).expect("mkdir");
    std::fs::write(
        mine.join("hooks").join("statusline.py"),
        "# the user's own\n",
    )
    .expect("write");
    let status = g.cmd(HookKind::Statusline);
    let ours = [
        status.clone(),
        format!("  {}\n", status.replace('/', "\\").to_uppercase()),
        hook_command(&other, HookKind::Statusline),
        hook_command(&gone, HookKind::Display),
        legacy_hook_command(&g.install, HookKind::Statusline),
        legacy_hook_command(&gone, HookKind::Statusline),
    ];
    let theirs = [
        piped(&g.install, HookKind::Statusline),
        piped(&gone, HookKind::Statusline),
        format!("{status} --old"),
        format!("python \"{}\"", g.install.join("hooks").join("statusline.py").display()),
        legacy_hook_command(&mine, HookKind::Statusline),
        format!(
            "\"{}\" \"{}\"",
            gone.join(".venv").join("Scripts").join("python.exe").display(),
            other.join("hooks").join("statusline.py").display()
        ),
        "\"%LOCALAPPDATA%/gone/.venv/Scripts/python.exe\" \"%LOCALAPPDATA%/gone/hooks/statusline.py\"".to_string(),
        "\"%LOCALAPPDATA%/gone/bin/claude-consult.exe\" hook statusline".to_string(),
        "\"./.venv/Scripts/python.exe\" \"./hooks/statusline.py\"".to_string(),
        user_status()["command"].as_str().expect("cmd").to_string(),
    ];
    let test = ours_test(&g.install, true);
    for c in &ours {
        assert!(test.is_ours(&json!(c)), "should be ours: {c}");
    }
    for c in &theirs {
        assert!(!test.is_ours(&json!(c)), "should be theirs: {c}");
    }
}

// ---- upgrading from the Python install ------------------------------------------

#[test]
fn the_python_installs_entries_in_this_dir_are_replaced_in_place() {
    let g = Gen::new();
    g.write_settings(
        &json!({"hooks": {"PostToolUse": [user_post(), legacy_post(&g.install), user_post_2()],
                                       "MessageDisplay": [legacy_display(&g.install)]},
                             "statusLine": legacy_status(&g.install)}),
    );
    let res = g.install_(Opts::default());
    let s = g.settings();
    assert_eq!(
        s["hooks"]["PostToolUse"],
        json!([user_post(), ours_post(&g.install), user_post_2()])
    );
    assert_eq!(
        s["hooks"]["MessageDisplay"],
        json!([ours_display(&g.install)])
    );
    assert_eq!(s["statusLine"], ours_status(&g.install));
    assert_eq!(res.status_line, StatusLineOutcome::Ours);
}

#[test]
fn a_python_install_elsewhere_is_taken_over_with_its_displaced_status_line() {
    let g = Gen::new();
    let py = g.make_legacy_install("python-install");
    std::fs::write(
        py.join("manifest.json"),
        json!({"files": [], "status_line_displaced": user_status()}).to_string(),
    )
    .expect("write");
    let mut status = legacy_status(&py);
    status["padding"] = json!(3);
    g.write_settings(
        &json!({"hooks": {"PostToolUse": [legacy_post(&py), user_post()],
                                       "MessageDisplay": [user_display(), legacy_display(&py)]},
                             "statusLine": status}),
    );
    let res = g.install_(Opts::default());
    assert_eq!(res.status_line, StatusLineOutcome::Ours);
    let s = g.settings();
    assert_eq!(
        s["hooks"]["PostToolUse"],
        json!([ours_post(&g.install), user_post()])
    );
    assert_eq!(
        s["hooks"]["MessageDisplay"],
        json!([user_display(), ours_display(&g.install)])
    );
    let mut want = ours_status(&g.install);
    want["padding"] = json!(3);
    assert_eq!(s["statusLine"], want);
    assert_eq!(g.manifest()["status_line_displaced"], user_status());
    // Uninstalling puts back the line the Python install stood in for.
    let res = g.uninstall();
    assert!(res.status_line_restored);
    let s = g.settings();
    assert_eq!(s["statusLine"], user_status());
    assert_eq!(
        s["hooks"],
        json!({"PostToolUse": [user_post()], "MessageDisplay": [user_display()]})
    );
}

#[test]
fn a_python_install_whose_scripts_are_gone_is_cleaned_up() {
    let g = Gen::new();
    let gone = g.root.join("old-python");
    g.write_settings(&json!({"hooks": {"PostToolUse": [legacy_post(&gone)]}, "statusLine": legacy_status(&gone)}));
    let res = g.install_(Opts::default());
    assert_eq!(res.status_line, StatusLineOutcome::Ours);
    assert_eq!(
        g.settings()["hooks"]["PostToolUse"],
        json!([ours_post(&g.install)])
    );
    // And uninstall, which spares other live installs, still removes dead ones.
    let mut s = g.settings();
    s["hooks"]["PostToolUse"]
        .as_array_mut()
        .expect("arr")
        .push(legacy_post(&gone));
    g.write_settings(&s);
    g.uninstall();
    assert!(g.settings().get("hooks").is_none());
}

#[test]
fn uninstall_spares_a_live_python_install_elsewhere() {
    let g = Gen::new();
    let py = g.make_legacy_install("python-install");
    g.install_(Opts::default());
    let mut s = g.settings();
    s["hooks"]["PostToolUse"]
        .as_array_mut()
        .expect("arr")
        .push(legacy_post(&py));
    g.write_settings(&s);
    g.uninstall();
    assert_eq!(
        g.settings()["hooks"],
        json!({"PostToolUse": [legacy_post(&py)]})
    );
}
