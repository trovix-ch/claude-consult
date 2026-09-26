//! End to end through the built binary: install into temp dirs, run the installed copy's
//! hooks, server and runner, then uninstall.
//!
//! Offline and side-effect free: every OpenRouter URL points at a closed loopback port,
//! `CLAUDE_CONFIG_DIR` at a temp dir, the key is a dummy, and the flags keep the
//! installer away from the scheduled task and the `claude` CLI. (Uninstall still asks
//! Task Scheduler, read-only, which dir the task serves from; it is never this one, so
//! the task is left alone.)

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_claude-consult");
const KEY: &str = "sk-or-test-not-a-real-key";
const PANEL: [&str; 2] = ["deepseek-v4-pro", "glm-5.2"];

/// A loopback port nothing listens on: connections to it are refused at once.
fn closed_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr").port()
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    inst: PathBuf,
    claude: PathBuf,
    base_url: String,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = dunce(tmp.path());
        let real_claude = root.join("real-claude");
        std::fs::create_dir_all(&real_claude).expect("mkdir");
        Self {
            inst: root.join("inst"),
            claude: root.join("claude"),
            base_url: format!("http://127.0.0.1:{}/api/v1", closed_port()),
            root,
            _tmp: tmp,
        }
    }

    /// `exe` with an environment that cannot reach anything real.
    fn cmd(&self, exe: &Path) -> Command {
        let mut c = Command::new(exe);
        c.env_remove("OPENROUTER_API_KEY")
            .env_remove("CLAUDE_CONSULT_DIR")
            .env_remove("CLAUDE_CODE_ENTRYPOINT")
            .env_remove("RUST_LOG")
            .env("CLAUDE_CONFIG_DIR", self.root.join("real-claude"))
            .env("CLAUDE_CONSULT_OPENROUTER_BASE_URL", &self.base_url)
            .current_dir(&self.root)
            .stdin(Stdio::null());
        c
    }

    fn installed(&self) -> PathBuf {
        self.inst
            .join("bin")
            .join(format!("claude-consult{}", std::env::consts::EXE_SUFFIX))
    }

    fn settings(&self) -> Value {
        let text = std::fs::read_to_string(self.claude.join("settings.json")).expect("settings");
        serde_json::from_str(&text).expect("settings json")
    }
}

/// The temp dir without a `\\?\` prefix, as the installer writes paths.
fn dunce(p: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(p).expect("canonicalize");
    let s = canon.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

fn posix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Runs to completion with `input` on stdin, failing the test after `limit`.
fn run_with_input(mut cmd: Command, input: &str, limit: Duration) -> (Output, Duration) {
    let started = Instant::now();
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let out = wait_limited(child, limit);
    (out, started.elapsed())
}

fn run(cmd: Command, limit: Duration) -> (Output, Duration) {
    run_with_input(cmd, "", limit)
}

fn wait_limited(child: Child, limit: Duration) -> Output {
    let (tx, rx) = mpsc::channel();
    let pid = child.id();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(out) => out.expect("wait"),
        Err(_) => panic!("process {pid} still running after {limit:?}"),
    }
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}\n--- stderr\n{}",
        out.status.code(),
        text(&out.stdout),
        text(&out.stderr)
    )
}

#[test]
fn version_and_help() {
    let env = Env::new();
    let mut c = env.cmd(Path::new(BIN));
    c.arg("--version");
    let (out, _) = run(c, Duration::from_secs(30));
    assert!(out.status.success(), "{}", describe(&out));
    assert_eq!(
        text(&out.stdout).trim(),
        format!("claude-consult {}", env!("CARGO_PKG_VERSION"))
    );

    let mut c = env.cmd(Path::new(BIN));
    c.arg("--help");
    let (out, _) = run(c, Duration::from_secs(30));
    assert!(out.status.success(), "{}", describe(&out));
    let help = text(&out.stdout);
    for sub in [
        "install",
        "uninstall",
        "manage",
        "serve",
        "run",
        "reviewers",
        "service",
        "hook",
        "check-catalog",
        "--install-dir",
        "--claude-dir",
    ] {
        assert!(help.contains(sub), "{sub} missing from:\n{help}");
    }

    // The TUI refuses redirected streams instead of drawing into a pipe.
    let mut c = env.cmd(Path::new(BIN));
    c.arg("manage");
    let (out, _) = run(c, Duration::from_secs(30));
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        text(&out.stderr).starts_with("error: claude-consult manage needs a terminal"),
        "{}",
        describe(&out)
    );

    // A usage error exits 2, as clap reports them.
    let mut c = env.cmd(Path::new(BIN));
    c.args(["install", "--summary-style", "loud"]);
    let (out, _) = run(c, Duration::from_secs(30));
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
}

#[test]
fn check_catalog_without_the_listing_exits_2() {
    let env = Env::new();
    let mut c = env.cmd(Path::new(BIN));
    c.arg("check-catalog");
    let (out, took) = run(c, Duration::from_secs(60));
    assert_eq!(out.status.code(), Some(2), "{}", describe(&out));
    assert!(
        text(&out.stdout).starts_with("!!  could not read "),
        "{}",
        describe(&out)
    );
    eprintln!("check-catalog offline took {took:?}");
}

#[test]
fn install_use_and_uninstall() {
    let env = Env::new();

    // ---- (2) install -------------------------------------------------------------
    let mut c = env.cmd(Path::new(BIN));
    c.env("OPENROUTER_API_KEY", KEY)
        .arg("install")
        .args([
            "--unattended",
            "--skip-service",
            "--skip-mcp-registration",
            "--skip-key-check",
        ])
        .arg("--install-dir")
        .arg(&env.inst)
        .arg("--claude-dir")
        .arg(&env.claude)
        .args(["--panel", &PANEL.join(",")]);
    let (out, took) = run(c, Duration::from_secs(120));
    assert!(out.status.success(), "{}", describe(&out));
    eprintln!("install took {took:?}");
    assert!(
        !text(&out.stdout).contains(KEY) && !text(&out.stderr).contains(KEY),
        "the key was printed:\n{}",
        describe(&out)
    );

    let exe = env.installed();
    for f in [
        exe.clone(),
        env.inst.join("models.json"),
        env.inst.join("display.json"),
        env.inst.join("manifest.json"),
        env.claude.join("commands").join("consult.md"),
        env.claude.join("commands").join("cleanroom.md"),
        env.claude.join("commands").join("deepseek.md"),
        env.claude.join("commands").join("glm.md"),
        env.claude
            .join("skills")
            .join("openrouter-workflow")
            .join("SKILL.md"),
        env.claude.join("workflows").join("verify-claims.js"),
    ] {
        assert!(f.is_file(), "{} missing after install", f.display());
    }
    let settings = env.settings();
    assert_eq!(settings["env"]["OPENROUTER_API_KEY"], KEY);
    assert_eq!(settings["env"]["MCP_TOOL_TIMEOUT"], "2400000");
    assert_eq!(
        settings["env"]["CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"],
        "1800000"
    );
    let exe_cmd = format!("\"{}\"", posix(&exe));
    let post = &settings["hooks"]["PostToolUse"][0];
    assert_eq!(
        post["matcher"],
        "mcp__openrouter__consult|mcp__openrouter__consult_clean"
    );
    assert_eq!(
        post["hooks"][0]["command"],
        format!("{exe_cmd} hook summary")
    );
    let display = &settings["hooks"]["MessageDisplay"][0]["hooks"][0]["command"];
    assert_eq!(display, &json!(format!("{exe_cmd} hook display")));
    assert_eq!(
        settings["statusLine"]["command"],
        format!("{exe_cmd} hook statusline")
    );

    // ---- (3) the installed copy's hooks -------------------------------------------
    let session = "smoke-session-1";
    let hook = |kind: &str, input: &Value| {
        let mut c = env.cmd(&exe);
        c.args(["hook", kind]);
        let (out, _) = run_with_input(c, &input.to_string(), Duration::from_secs(30));
        assert_eq!(out.status.code(), Some(0), "{}", describe(&out));
        text(&out.stdout)
    };
    let status_in = json!({"session_id": session, "model": {"display_name": "Opus"}});
    assert_eq!(hook("statusline", &status_in), "\n");

    let record = |alias: &str, cost: f64, tin: u64, tout: u64| {
        format!(
            "<!-- consult-result v1 {} -->",
            json!({"alias": alias, "short": alias, "status": "ok", "complete": true,
                   "finish": "stop", "capped": false, "tool_calls": 3, "cost_usd": cost,
                   "tokens_in": tin, "tokens_out": tout, "seconds": 12.5})
        )
    };
    let result = format!(
        "# Reviews\n\nAll good.\n\n{}\n{}\n",
        record(PANEL[0], 0.0125, 1500, 400),
        record(PANEL[1], 0.0075, 900, 300)
    );
    let post_in = json!({
        "session_id": session,
        "hook_event_name": "PostToolUse",
        "tool_name": "mcp__openrouter__consult",
        "tool_input": {"question": "q"},
        "tool_response": {"content": [{"type": "text", "text": result}]},
    });
    assert_eq!(hook("summary", &post_in), "");
    let state = env.inst.join("state");
    let log = state.join(format!("{session}.jsonl"));
    let pending = state.join(format!("{session}.pending"));
    assert!(log.is_file(), "no {}", log.display());
    assert!(pending.is_file(), "no {}", pending.display());

    let line = hook("statusline", &status_in);
    assert!(
        line.contains("consult this session · 1 call · 0.0200 USD"),
        "{line:?}"
    );
    assert!(line.starts_with('\u{1b}'), "not dimmed: {line:?}");

    let display_in = json!({"session_id": session, "final": true, "delta": "Here is my reply."});
    let shown = hook("display", &display_in);
    let shown: Value = serde_json::from_str(shown.trim()).expect("display prints JSON");
    let shown_text = shown.to_string();
    assert!(
        shown_text.contains(PANEL[0]) && shown_text.contains("Here is my reply."),
        "{shown_text}"
    );
    assert!(!pending.exists(), "the pending summary was not consumed");

    // ---- (4) the installed copy serves MCP on stdio --------------------------------
    serve_stdio(&env, &exe);

    // ---- (5) reviewers -------------------------------------------------------------
    let mut c = env.cmd(&exe);
    c.arg("reviewers");
    let (out, took) = run(c, Duration::from_secs(60));
    assert!(out.status.success(), "{}", describe(&out));
    let table = text(&out.stdout);
    eprintln!("reviewers took {took:?}:\n{table}");
    for alias in PANEL {
        assert!(table.contains(alias), "{alias} missing:\n{table}");
    }
    assert!(
        table.contains("install recorded none: price unknown"),
        "{table}"
    );

    // ---- (6) a one-shot run that cannot reach OpenRouter ------------------------------
    let mut c = env.cmd(&exe);
    c.env("OPENROUTER_API_KEY", KEY)
        .args(["run", "--clean", "--question", "x"]);
    let (out, took) = run(c, Duration::from_secs(180));
    eprintln!("run --clean offline took {took:?}\n{}", describe(&out));
    assert!(
        !text(&out.stdout).contains(KEY) && !text(&out.stderr).contains(KEY),
        "the key was printed"
    );
    run_offline_outcome(&out);

    // A consult that cannot start (no key anywhere) is `consult failed:` and exit 1.
    let mut c = env.cmd(&exe);
    c.args(["run", "--clean", "--question", "x"]);
    let (out, took) = run(c, Duration::from_secs(60));
    assert_eq!(out.status.code(), Some(1), "{}", describe(&out));
    assert!(
        text(&out.stderr).starts_with("consult failed: "),
        "{}",
        describe(&out)
    );
    assert!(text(&out.stdout).is_empty(), "{}", describe(&out));
    eprintln!("run without a key took {took:?}");

    // ---- (7) uninstall ---------------------------------------------------------------
    let mut c = env.cmd(Path::new(BIN));
    c.args(["uninstall", "--yes", "--skip-mcp-registration"])
        .arg("--install-dir")
        .arg(&env.inst)
        .arg("--claude-dir")
        .arg(&env.claude);
    let (out, _) = run(c, Duration::from_secs(120));
    assert!(out.status.success(), "{}", describe(&out));
    eprintln!("uninstall:\n{}", describe(&out));
    assert!(!env.inst.exists(), "{} left behind", env.inst.display());
    for f in ["consult.md", "cleanroom.md", "deepseek.md", "glm.md"] {
        assert!(!env.claude.join("commands").join(f).exists(), "{f} left");
    }
    assert!(
        !env.claude
            .join("skills")
            .join("openrouter-workflow")
            .join("SKILL.md")
            .exists()
    );
    assert!(
        !env.claude
            .join("workflows")
            .join("verify-claims.js")
            .exists()
    );
    let settings = env.settings();
    assert_eq!(
        settings["env"]["OPENROUTER_API_KEY"], KEY,
        "the key must stay"
    );
    let s = settings.to_string();
    assert!(!s.contains("hook summary"), "{s}");
    assert!(!s.contains("hook display"), "{s}");
    assert!(!s.contains("hook statusline"), "{s}");
    assert!(settings.get("statusLine").is_none(), "{s}");
}

/// Every reviewer fails to connect. Pinned to what the runner does with that: the
/// consult itself ran, so it prints the failed reviews and exits 0; `consult failed:`
/// and exit 1 are for a consult that could not start.
fn run_offline_outcome(out: &Output) {
    assert_eq!(out.status.code(), Some(0), "{}", describe(out));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("ConnectError"), "{}", describe(out));
}

fn serve_stdio(env: &Env, exe: &Path) {
    let mut c = env.cmd(exe);
    c.arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().expect("spawn serve");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let send = |stdin: &mut std::process::ChildStdin, v: Value| {
        writeln!(stdin, "{v}").expect("write");
        stdin.flush().expect("flush");
    };
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0"}}}),
    );
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    send(
        &mut stdin,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut init = None;
    let mut tools = None;
    while init.is_none() || tools.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = rx
            .recv_timeout(left)
            .expect("serve answered within a minute");
        // stdout carries only the protocol.
        let msg: Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
        match msg["id"].as_i64() {
            Some(1) => init = Some(msg),
            Some(2) => tools = Some(msg),
            _ => {}
        }
    }
    let init = init.expect("init");
    assert_eq!(init["result"]["serverInfo"]["name"], "openrouter", "{init}");
    let tools = tools.expect("tools");
    let mut names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["consult", "consult_clean", "list_reviewers"]);

    drop(stdin);
    let out = wait_limited(child, Duration::from_secs(30));
    let _ = reader.join();
    assert!(out.status.success(), "serve exit: {}", describe(&out));
}
