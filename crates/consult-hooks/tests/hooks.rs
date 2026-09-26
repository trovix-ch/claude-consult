//! The hooks run the way Claude Code runs them, one event at a time, against a throwaway
//! install dir. Nothing here touches a real install, the real environment or the network.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use consult_core::records::Record;
use consult_hooks::{HookKind, display, run_with, statusline, summary};
use serde_json::{Value, json};
use tempfile::TempDir;

const SESSION: &str = "5f0c2a9e-1d3b-4c6a-9e8f-0a1b2c3d4e5f";
const CONSULT: &str = "mcp__openrouter__consult";
const CLEANROOM: &str = "mcp__openrouter__consult_clean";
const DAY: u64 = 24 * 3600;

#[allow(clippy::too_many_arguments)]
fn record(
    alias: Option<&str>,
    short: Option<&str>,
    status: &str,
    finish: Option<&str>,
    cost: f64,
    tin: u64,
    tout: u64,
    seconds: f64,
    tool_calls: u64,
) -> String {
    Record {
        alias: alias.map(str::to_string),
        short: short.map(str::to_string),
        status: status.to_string(),
        complete: status == "ok",
        finish: finish.map(str::to_string),
        capped: false,
        tool_calls,
        cost_usd: cost,
        tokens_in: tin,
        tokens_out: tout,
        seconds,
    }
    .line()
}

/// The shape Claude Code passes once the server returns plain text content.
fn blocks(text: &str) -> Value {
    json!([{"type": "text", "text": text}])
}

fn panel() -> Vec<String> {
    vec![
        record(
            Some("deepseek-v4-pro"),
            Some("deepseek"),
            "ok",
            Some("stop"),
            0.2293,
            373_451,
            18_017,
            414.4,
            4,
        ),
        record(
            Some("glm-5.2"),
            Some("glm"),
            "ok",
            Some("stop"),
            0.1536,
            690_720,
            25_265,
            340.8,
            4,
        ),
        record(
            Some("gpt-5.6-luna-pro"),
            Some("luna"),
            "incomplete",
            Some("content_filter"),
            0.0372,
            276_732,
            13_143,
            69.4,
            4,
        ),
    ]
}

/// A reviewer quoting the format mid-review. Its numbers must never be counted.
fn body() -> String {
    let quoted = record(
        Some("quoted"),
        Some("quoted"),
        "ok",
        Some("stop"),
        9.0,
        9_000_000,
        9_000_000,
        999.0,
        4,
    );
    format!(
        "# Panel review — 3 reviewers\n\n## deepseek-v4-pro — verification lens\n\nEvery result ends with lines like\n\n{quoted}\n\nso a parser must anchor to the end.\n\n---\n\n"
    )
}

fn result() -> String {
    body() + &panel().join("\n") + "\n"
}

fn failed() -> String {
    "Consult failed: no reviewers configured\n\n".to_string()
        + &record(None, None, "failed", None, 0.0, 0, 0, 0.0, 0)
}

// Written out by hand rather than rebuilt from the contract's recipe, so a formatting
// slip in the hook cannot be mirrored here and pass.
const HEAD: &str = "consult · 2/3 complete · 0.4201 USD · 1.34M in / 56k out · 414s";

fn dim_summary() -> String {
    format!(
        "\x1b[2m{HEAD} · \x1b[32m✓\x1b[0m\x1b[2m deepseek \x1b[32m✓\x1b[0m\x1b[2m glm \x1b[31m✗ luna\x1b[0m\x1b[2m\x1b[0m"
    )
}

fn plain() -> String {
    format!("{HEAD} · ✓ deepseek ✓ glm ✗ luna")
}

const NO_SIGNAL: &str = "\x1b[2mconsult · no status lines in the result · no signal\x1b[0m";

#[derive(Debug, Clone, Copy)]
struct Total {
    calls: u64,
    cost: f64,
    tin: u64,
    tout: u64,
}

const TOTAL_AFTER_ONE: Total = Total {
    calls: 1,
    cost: 0.4201,
    tin: 1_340_903,
    tout: 56_425,
};
const TOTAL_AFTER_TWO: Total = Total {
    calls: 2,
    cost: 0.8402,
    tin: 2_681_806,
    tout: 112_850,
};

struct Case {
    install: TempDir,
}

impl Case {
    fn new() -> Self {
        Self {
            install: tempfile::Builder::new()
                .prefix("consult-hooks-")
                .tempdir()
                .expect("tempdir"),
        }
    }

    fn dir(&self) -> &Path {
        self.install.path()
    }

    fn state(&self) -> PathBuf {
        self.dir().join("state")
    }

    fn summary_input(response: &Value, tool: &str) -> Vec<u8> {
        json!({"session_id": SESSION, "hook_event_name": "PostToolUse", "tool_name": tool,
               "tool_input": {}, "tool_response": response, "tool_use_id": "toolu_test"})
        .to_string()
        .into_bytes()
    }

    fn summarise_env(&self, response: &Value, tool: &str, entrypoint: Option<&str>) {
        let env = env_with(entrypoint);
        summary(self.dir(), &Self::summary_input(response, tool), &env);
    }

    fn summarise(&self, response: &Value) {
        self.summarise_env(response, CONSULT, None);
    }

    fn configure(&self, summary: &str) {
        fs::write(
            self.dir().join("display.json"),
            json!({"_comment": "test", "progress": "full", "summary": summary}).to_string(),
        )
        .expect("display.json");
    }

    fn pending(&self) -> Option<String> {
        fs::read_to_string(self.state().join(format!("{SESSION}.pending"))).ok()
    }

    fn log(&self) -> PathBuf {
        self.state().join(format!("{SESSION}.jsonl"))
    }

    /// The session total as summed from the log, one line per call; lines that are not a
    /// call record are left out, as the status line does.
    fn total(&self) -> Option<Total> {
        let text = fs::read_to_string(self.log()).ok()?;
        let mut total = Total {
            calls: 0,
            cost: 0.0,
            tin: 0,
            tout: 0,
        };
        for line in text.lines() {
            let Ok(Value::Object(call)) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let mut keys: Vec<&str> = call.keys().map(String::as_str).collect();
            keys.sort_unstable();
            if keys != ["cost", "tin", "tout"] {
                continue;
            }
            total.calls += 1;
            total.cost += call["cost"].as_f64().expect("cost");
            total.tin += call["tin"].as_u64().expect("tin");
            total.tout += call["tout"].as_u64().expect("tout");
        }
        Some(total)
    }

    #[track_caller]
    fn assert_total(&self, expected: Total) {
        let total = self.total().expect("a log");
        assert_eq!(total.calls, expected.calls, "calls");
        assert!(
            (total.cost - expected.cost).abs() < 1e-9,
            "cost {} != {}",
            total.cost,
            expected.cost
        );
        assert_eq!(total.tin, expected.tin, "tin");
        assert_eq!(total.tout, expected.tout, "tout");
    }

    fn park(&self, summary: &str) {
        fs::create_dir_all(self.state()).expect("state");
        fs::write(self.state().join(format!("{SESSION}.pending")), summary).expect("park");
    }

    fn flush_input(delta: Value, is_final: Value) -> Vec<u8> {
        json!({"session_id": SESSION, "turn_id": "turn-1", "message_id": "msg-1", "index": 3,
               "final": is_final, "delta": delta})
        .to_string()
        .into_bytes()
    }

    fn flush_env(&self, delta: Value, is_final: Value, entrypoint: Option<&str>) -> Option<String> {
        display(
            self.dir(),
            &Self::flush_input(delta, is_final),
            &env_with(entrypoint),
        )
    }

    fn flush(&self, delta: &str) -> Option<String> {
        self.flush_env(json!(delta), json!(true), None)
    }

    fn status(&self, session_id: &str) -> String {
        statusline(
            self.dir(),
            json!({"session_id": session_id, "model": {"id": "claude"},
                   "workspace": {"current_dir": "."}})
            .to_string()
            .as_bytes(),
        )
    }
}

fn env_with(entrypoint: Option<&str>) -> impl Fn(&str) -> Option<String> {
    let entrypoint = entrypoint.map(str::to_string);
    move |key: &str| {
        if key == "CLAUDE_CODE_ENTRYPOINT" {
            entrypoint.clone()
        } else {
            None
        }
    }
}

/// What the display hook's line shows, checked to be the event Claude Code expects.
#[track_caller]
fn shown(line: Option<String>) -> String {
    let line = line.expect("a display line");
    // Every character outside ASCII escaped, so no code page can mangle it.
    assert!(line.is_ascii(), "{line}");
    assert!(!line.contains('\n'));
    let payload: Value = serde_json::from_str(&line).expect("json");
    let output = &payload["hookSpecificOutput"];
    assert_eq!(output["hookEventName"], "MessageDisplay");
    output["displayContent"]
        .as_str()
        .expect("displayContent")
        .to_string()
}

fn set_age(path: &Path, days: u64) {
    let when = SystemTime::now() - Duration::from_secs(days * DAY);
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(when)
        .expect("set mtime");
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------- summary hook

#[test]
fn content_blocks_default_to_dim() {
    let c = Case::new();
    c.summarise(&blocks(&result()));
    assert_eq!(c.pending(), Some(dim_summary()));
    c.assert_total(TOTAL_AFTER_ONE);
}

#[test]
fn counts_below_a_thousand_are_exact() {
    let c = Case::new();
    let small = record(
        Some("deepseek-v4-pro"),
        Some("deepseek"),
        "ok",
        Some("stop"),
        0.0003,
        312,
        45,
        12.0,
        4,
    );
    c.summarise(&blocks(&format!("# Panel review\n\n---\n\n{small}\n")));
    let pending = c.pending().expect("pending");
    assert!(
        pending.contains("0.0003 USD · 312 in / 45 out · 12s"),
        "{pending}"
    );
}

#[test]
fn structured_result_string_from_an_older_server() {
    let c = Case::new();
    c.summarise(&json!(json!({"result": result()}).to_string()));
    assert_eq!(c.pending(), Some(dim_summary()));
    c.assert_total(TOTAL_AFTER_ONE);
}

#[test]
fn plain_string() {
    let c = Case::new();
    c.summarise(&json!(result()));
    assert_eq!(c.pending(), Some(dim_summary()));
    c.assert_total(TOTAL_AFTER_ONE);
}

#[test]
fn dict_shapes() {
    for response in [
        json!({"text": result()}),
        json!({"content": blocks(&result())}),
        json!({"result": result()}),
    ] {
        let c = Case::new();
        c.summarise(&response);
        assert_eq!(c.pending(), Some(dim_summary()), "{response}");
        c.assert_total(TOTAL_AFTER_ONE);
    }
}

#[test]
fn each_style() {
    let c = Case::new();
    for (style, expected) in [
        ("dim", dim_summary()),
        ("italic", format!("*{}*", plain())),
        ("quote", format!("> {}", plain())),
    ] {
        c.configure(style);
        c.summarise(&blocks(&result()));
        assert_eq!(c.pending(), Some(expected), "{style}");
    }
}

#[test]
fn config_with_a_bom_is_read() {
    let c = Case::new();
    fs::write(
        c.dir().join("display.json"),
        "\u{feff}{\"summary\": \"quote\"}",
    )
    .expect("display.json");
    c.summarise(&blocks(&result()));
    assert_eq!(c.pending(), Some(format!("> {}", plain())));
}

#[test]
fn off_counts_the_call_but_parks_nothing() {
    let c = Case::new();
    c.configure("off");
    c.summarise(&blocks(&result()));
    assert_eq!(c.pending(), None);
    c.assert_total(TOTAL_AFTER_ONE);
}

#[test]
fn off_clears_a_summary_parked_before() {
    // Left in place, it would surface under some later reply once the summary is
    // switched back on, describing a call long gone.
    let c = Case::new();
    c.summarise(&blocks(&result()));
    assert_eq!(c.pending(), Some(dim_summary()));
    fs::write(c.state().join("another-session.pending"), "OTHER").expect("other");
    c.configure("off");
    c.summarise(&blocks(&result()));
    assert_eq!(c.pending(), None);
    assert_eq!(
        fs::read_to_string(c.state().join("another-session.pending")).expect("other"),
        "OTHER"
    );
    c.assert_total(TOTAL_AFTER_TWO);
}

#[test]
fn headless_run_counts_the_call_but_parks_nothing() {
    // claude -p never draws the summary, so one parked there is only ever stale.
    let c = Case::new();
    c.summarise(&blocks(&result()));
    c.summarise_env(&blocks(&result()), CONSULT, Some("sdk-cli"));
    assert_eq!(c.pending(), None);
    c.assert_total(TOTAL_AFTER_TWO);
    c.summarise_env(&blocks(&result()), CONSULT, Some("cli"));
    assert_eq!(c.pending(), Some(dim_summary()));
}

#[test]
fn any_sdk_entrypoint_is_headless() {
    // The TypeScript and Python SDKs run Claude Code headless too.
    let c = Case::new();
    c.summarise(&blocks(&result()));
    c.summarise_env(&blocks(&result()), CONSULT, Some("sdk-ts"));
    assert_eq!(c.pending(), None);
    c.assert_total(TOTAL_AFTER_TWO);
}

#[test]
fn concurrent_calls_are_all_counted() {
    // Consults from parallel subagents can finish together in one session. A
    // read-modify-write total kept two of twelve.
    let c = Case::new();
    let n = 12;
    let payload = Case::summary_input(&blocks(&result()), CONSULT);
    let barrier = std::sync::Barrier::new(n);
    let env = env_with(None);
    std::thread::scope(|scope| {
        for _ in 0..n {
            let (payload, barrier, env, dir) = (&payload, &barrier, &env, c.dir());
            scope.spawn(move || {
                barrier.wait();
                summary(dir, payload, env);
            });
        }
    });
    c.assert_total(Total {
        calls: n as u64,
        cost: TOTAL_AFTER_ONE.cost * n as f64,
        tin: TOTAL_AFTER_ONE.tin * n as u64,
        tout: TOTAL_AFTER_ONE.tout * n as u64,
    });
    assert_eq!(c.pending(), Some(dim_summary()));
}

#[test]
fn unusable_config_falls_back_to_dim() {
    let c = Case::new();
    for (name, content) in [
        ("unknown value", "{\"summary\": \"loud\"}"),
        ("not json", "{summary: off"),
        ("not an object", "[\"off\"]"),
        ("wrong type", "{\"summary\": [\"off\"]}"),
    ] {
        fs::write(c.dir().join("display.json"), content).expect("display.json");
        c.summarise(&blocks(&result()));
        assert_eq!(c.pending(), Some(dim_summary()), "{name}");
    }
    fs::remove_file(c.dir().join("display.json")).expect("remove");
    fs::create_dir(c.dir().join("display.json")).expect("a directory");
    c.summarise(&blocks(&result()));
    assert_eq!(c.pending(), Some(dim_summary()), "a directory");
}

#[test]
fn totals_accumulate_and_the_newest_summary_wins() {
    let c = Case::new();
    c.summarise(&blocks(&result()));
    c.summarise_env(&blocks(&failed()), CLEANROOM, None);
    assert_eq!(
        c.pending().as_deref(),
        Some("\x1b[2mcleanroom · failed · no signal\x1b[0m")
    );
    c.assert_total(Total {
        calls: 2,
        ..TOTAL_AFTER_ONE
    });
    c.summarise(&blocks(&result()));
    assert_eq!(c.pending(), Some(dim_summary()));
    c.assert_total(Total {
        calls: 3,
        ..TOTAL_AFTER_TWO
    });
}

#[test]
fn a_damaged_log_is_appended_to_as_it_is() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    let damage = "{\"calls\": \"many\"}\nnot json\n";
    fs::write(c.log(), damage).expect("log");
    c.summarise(&blocks(&result()));
    let log = fs::read_to_string(c.log()).expect("log");
    assert!(log.starts_with(damage), "{log}");
    assert!(log.ends_with("}\n"), "{log}");
    assert_eq!(log[damage.len()..].matches('\n').count(), 1, "{log}");
    c.assert_total(TOTAL_AFTER_ONE);
}

#[test]
fn missing_records_are_no_signal() {
    for response in [
        blocks("# Panel review\n\nNo status lines here.\n"),
        json!(""),
        Value::Null,
        json!([]),
        json!({"x": 1}),
    ] {
        let c = Case::new();
        c.summarise(&response);
        assert_eq!(c.pending().as_deref(), Some(NO_SIGNAL), "{response}");
        assert!(c.total().is_none(), "{response}");
    }
    // No tool_response at all is no signal too.
    let c = Case::new();
    summary(
        c.dir(),
        json!({"session_id": SESSION, "tool_name": CONSULT})
            .to_string()
            .as_bytes(),
        &env_with(None),
    );
    assert_eq!(c.pending().as_deref(), Some(NO_SIGNAL));
}

#[test]
fn record_shaped_line_in_the_body_is_ignored() {
    let c = Case::new();
    c.summarise(&blocks(&body()));
    assert_eq!(c.pending().as_deref(), Some(NO_SIGNAL), "no trailing block");
    assert!(c.total().is_none());

    c.summarise(&blocks(&(result() + "\nOne more paragraph.\n")));
    assert_eq!(c.pending().as_deref(), Some(NO_SIGNAL), "more prose after");
    assert!(c.total().is_none());

    c.summarise(&blocks(&(result() + "\n  \n")));
    assert_eq!(c.pending(), Some(dim_summary()), "a trailing block");
    c.assert_total(TOTAL_AFTER_ONE);
}

#[test]
fn malformed_trailing_block_is_no_signal() {
    let p = panel();
    let broken: Vec<(&str, Vec<String>)> = vec![
        (
            "bad json",
            vec![
                p[0].clone(),
                p[1].clone(),
                r#"<!-- consult-result v1 {"alias":"gpt-5.6-luna-pro",} -->"#.to_string(),
            ],
        ),
        (
            "cut off",
            vec![p[0].clone(), p[1].clone(), p[2][..60].to_string()],
        ),
        (
            "missing field",
            vec![
                p[0].clone(),
                p[1].clone(),
                p[2].replace("\"cost_usd\":0.0372,", ""),
            ],
        ),
        (
            "bad line mid-block",
            vec![
                p[0].clone(),
                "<!-- consult-result v1 {oops} -->".to_string(),
                p[2].clone(),
            ],
        ),
        (
            "unknown version",
            vec![p[0].clone(), p[1].clone(), p[2].replace(" v1 ", " v2 ")],
        ),
        (
            "not finite",
            vec![
                p[0].clone(),
                p[1].clone(),
                p[2].replace("\"cost_usd\":0.0372", "\"cost_usd\":NaN"),
            ],
        ),
    ];
    for (name, lines) in broken {
        assert_ne!(lines, p, "{name}");
        let c = Case::new();
        c.summarise(&blocks(&(body() + &lines.join("\n"))));
        assert_eq!(c.pending().as_deref(), Some(NO_SIGNAL), "{name}");
        assert!(c.total().is_none(), "{name}");
    }
}

#[test]
fn whole_call_failure() {
    let c = Case::new();
    let cases = [
        (CONSULT, "dim", "\x1b[2mconsult · failed · no signal\x1b[0m"),
        (CLEANROOM, "italic", "*cleanroom · failed · no signal*"),
        (CONSULT, "quote", "> consult · failed · no signal"),
    ];
    for (calls, (tool, style, expected)) in (1..).zip(cases) {
        c.configure(style);
        c.summarise_env(&blocks(&failed()), tool, None);
        assert_eq!(c.pending().as_deref(), Some(expected), "{style}");
        c.assert_total(Total {
            calls,
            cost: 0.0,
            tin: 0,
            tout: 0,
        });
    }
}

#[test]
fn no_signal_follows_the_style() {
    let c = Case::new();
    for (style, expected) in [
        (
            "italic",
            "*cleanroom · no status lines in the result · no signal*",
        ),
        (
            "quote",
            "> cleanroom · no status lines in the result · no signal",
        ),
    ] {
        c.configure(style);
        c.summarise_env(&blocks("no records"), CLEANROOM, None);
        assert_eq!(c.pending().as_deref(), Some(expected), "{style}");
    }
}

#[test]
fn prunes_state_older_than_a_week() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    let old: Vec<PathBuf> = ["old.jsonl", "old.pending", "old.123.tmp"]
        .iter()
        .map(|n| c.state().join(n))
        .collect();
    let recent = c.state().join("recent.jsonl");
    for path in old.iter().chain([&recent]) {
        fs::write(path, "{}").expect("write");
    }
    for path in &old {
        set_age(path, 8);
    }
    set_age(&recent, 6);
    fs::create_dir(c.state().join("a-directory")).expect("dir");
    c.summarise(&blocks(&result()));
    let mut expected = vec![
        "a-directory".to_string(),
        "recent.jsonl".to_string(),
        format!("{SESSION}.jsonl"),
        format!("{SESSION}.pending"),
    ];
    expected.sort();
    assert_eq!(names(&c.state()), expected);
}

#[test]
fn unusable_state_dir() {
    let c = Case::new();
    fs::write(c.state(), "not a directory").expect("state file");
    c.summarise(&blocks(&result()));
    assert_eq!(
        fs::read_to_string(c.state()).expect("state"),
        "not a directory"
    );
}

#[test]
fn missing_install_dir_is_not_created() {
    let c = Case::new();
    let gone = c.dir().join("not-installed");
    summary(
        &gone,
        &Case::summary_input(&blocks(&result()), CONSULT),
        &env_with(None),
    );
    assert!(!gone.exists());
}

// ---------------------------------------------------------------- display hook

#[test]
fn final_flush_appends_the_summary_once() {
    let c = Case::new();
    c.park("SUMMARY");
    assert_eq!(shown(c.flush("Done.")), "Done.\n\nSUMMARY\n");
    assert_eq!(c.pending(), None);
    assert_eq!(c.flush("The next reply."), None);
}

#[test]
fn delta_shapes() {
    let c = Case::new();
    for (delta, expected) in [
        (json!("Done.\n"), "Done.\n\nSUMMARY\n"),
        (json!(""), "\nSUMMARY\n"),
        (Value::Null, "\nSUMMARY\n"),
        (json!(7), "\nSUMMARY\n"),
        (json!(["Done."]), "\nSUMMARY\n"),
    ] {
        c.park("SUMMARY");
        assert_eq!(
            shown(c.flush_env(delta.clone(), json!(true), None)),
            expected,
            "{delta}"
        );
    }
    // No delta key at all.
    c.park("SUMMARY");
    let line = display(
        c.dir(),
        json!({"session_id": SESSION, "final": true})
            .to_string()
            .as_bytes(),
        &env_with(None),
    );
    assert_eq!(shown(line), "\nSUMMARY\n");
}

#[test]
fn non_ascii_is_escaped_and_survives() {
    let c = Case::new();
    c.park(&dim_summary());
    let line = c.flush("Fertig — “gut”.");
    assert!(line.as_deref().is_some_and(|l| l.contains("\\u2014")));
    assert_eq!(
        shown(line),
        format!("Fertig — “gut”.\n\n{}\n", dim_summary())
    );
}

#[test]
fn non_final_flush_leaves_the_summary() {
    let c = Case::new();
    c.park("SUMMARY");
    for is_final in [json!(false), Value::Null, json!("true"), json!(1)] {
        assert_eq!(
            c.flush_env(json!("Done."), is_final.clone(), None),
            None,
            "{is_final}"
        );
    }
    let line = display(
        c.dir(),
        json!({"session_id": SESSION, "delta": "no final key"})
            .to_string()
            .as_bytes(),
        &env_with(None),
    );
    assert_eq!(line, None);
    assert_eq!(c.pending().as_deref(), Some("SUMMARY"));
}

#[test]
fn a_final_flush_shaped_as_an_array_is_no_input() {
    let c = Case::new();
    c.park("SUMMARY");
    let line = display(
        c.dir(),
        json!([true, SESSION]).to_string().as_bytes(),
        &env_with(None),
    );
    assert_eq!(line, None);
    assert_eq!(c.pending().as_deref(), Some("SUMMARY"));
}

#[test]
fn headless_run_shows_nothing() {
    let c = Case::new();
    c.park("SUMMARY");
    assert_eq!(
        c.flush_env(json!("Done."), json!(true), Some("sdk-cli")),
        None
    );
    assert_eq!(c.pending().as_deref(), Some("SUMMARY"));
    assert_eq!(
        shown(c.flush_env(json!("Done."), json!(true), Some("cli"))),
        "Done.\n\nSUMMARY\n"
    );
}

#[test]
fn any_sdk_entrypoint_shows_nothing() {
    let c = Case::new();
    c.park("SUMMARY");
    for entrypoint in ["sdk-ts", "sdk-py"] {
        assert_eq!(
            c.flush_env(json!("Done."), json!(true), Some(entrypoint)),
            None,
            "{entrypoint}"
        );
    }
    assert_eq!(c.pending().as_deref(), Some("SUMMARY"));
}

#[test]
fn nothing_parked() {
    let c = Case::new();
    assert_eq!(c.flush("Done."), None);
    fs::create_dir(c.state()).expect("state");
    assert_eq!(c.flush("Done."), None);
    // An empty summary is removed and shows nothing.
    c.park("");
    assert_eq!(c.flush("Done."), None);
    assert_eq!(c.pending(), None);
}

#[test]
fn draws_what_the_summary_hook_parked() {
    let c = Case::new();
    c.summarise(&blocks(&result()));
    assert_eq!(
        shown(c.flush("Here is what the panel found.")),
        format!("Here is what the panel found.\n\n{}\n", dim_summary())
    );
}

// ---------------------------------------------------------------- status line

#[test]
fn session_total() {
    let c = Case::new();
    c.summarise(&blocks(&result()));
    assert_eq!(
        c.status(SESSION),
        "\x1b[2mconsult this session · 1 call · 0.4201 USD · 1.34M in / 56k out\x1b[0m"
    );
}

#[test]
fn plural_and_small_counts() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    fs::write(
        c.log(),
        "{\"cost\": 0.02, \"tin\": 12000, \"tout\": 500}\n{\"cost\": 0.03, \"tin\": 345, \"tout\": 499}\n",
    )
    .expect("log");
    assert_eq!(
        c.status(SESSION),
        "\x1b[2mconsult this session · 2 calls · 0.0500 USD · 12k in / 999 out\x1b[0m"
    );
}

#[test]
fn status_counts_below_a_thousand_are_exact() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    fs::write(c.log(), "{\"cost\": 0.0003, \"tin\": 312, \"tout\": 45}\n").expect("log");
    assert_eq!(
        c.status(SESSION),
        "\x1b[2mconsult this session · 1 call · 0.0003 USD · 312 in / 45 out\x1b[0m"
    );
}

#[test]
fn missing_session_file_prints_an_empty_line() {
    let c = Case::new();
    assert_eq!(c.status(SESSION), "");
    fs::create_dir(c.state()).expect("state");
    assert_eq!(c.status(SESSION), "");
    assert_eq!(c.status("another-session"), "");
}

/// Each one is a line that is not a call record, and is skipped.
const DAMAGED: [&str; 11] = [
    "",
    "{",
    "[]",
    "null",
    "{\"cost\": \"1\", \"tin\": 1, \"tout\": 1}",
    "{\"cost\": 1, \"tin\": 1.5, \"tout\": 1}",
    "{\"cost\": 1, \"tin\": -1, \"tout\": 1}",
    "{\"cost\": NaN, \"tin\": 1, \"tout\": 1}",
    "{\"cost\": 1, \"tin\": true, \"tout\": 1}",
    "{\"cost\": 1, \"tin\": 1}",
    "{\"cost\": 0.1, \"ti",
];

#[test]
fn a_log_of_only_damaged_lines_prints_an_empty_line() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    for content in DAMAGED {
        fs::write(c.log(), content).expect("log");
        assert_eq!(c.status(SESSION), "", "{content}");
    }
}

#[test]
fn damaged_lines_are_skipped() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    let good = "{\"cost\": 0.25, \"tin\": 1000, \"tout\": 2000}";
    let mut lines = vec![good];
    lines.extend(DAMAGED);
    lines.push(good);
    fs::write(c.log(), lines.join("\n") + "\n").expect("log");
    assert_eq!(
        c.status(SESSION),
        "\x1b[2mconsult this session · 2 calls · 0.5000 USD · 2k in / 4k out\x1b[0m"
    );
}

#[test]
fn crlf_and_invalid_utf8_in_the_log() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    let mut log = b"{\"cost\": 0.25, \"tin\": 1000, \"tout\": 2000}\r\n\xff\xfe\r\n".to_vec();
    log.extend_from_slice(b"{\"cost\": 0.25, \"tin\": 1000, \"tout\": 2000}\r\n");
    fs::write(c.log(), log).expect("log");
    assert_eq!(
        c.status(SESSION),
        "\x1b[2mconsult this session · 2 calls · 0.5000 USD · 2k in / 4k out\x1b[0m"
    );
}

// ---------------------------------------------------------------- bad input

#[test]
fn no_hook_fails_or_prints_on_bad_input() {
    let c = Case::new();
    fs::create_dir(c.state()).expect("state");
    // What a session id that climbs out of state/ would reach. Must stay untouched.
    fs::write(c.dir().join("escape.pending"), "LEAKED").expect("escape");
    fs::write(
        c.dir().join("escape.jsonl"),
        "{\"cost\": 1.0, \"tin\": 1, \"tout\": 1}\n",
    )
    .expect("escape");
    let event = json!({"final": true, "delta": "x", "tool_name": CONSULT,
                       "tool_response": blocks(&result())});
    let with_session = |id: Value| {
        let mut e = event.clone();
        e["session_id"] = id;
        e.to_string().into_bytes()
    };
    let inputs: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"not json".to_vec(),
        b"\xff\xfe\x00\x81".to_vec(),
        b"[]".to_vec(),
        b"null".to_vec(),
        b"\"a string\"".to_vec(),
        b"42".to_vec(),
        event.to_string().into_bytes(),
        with_session(json!(42)),
        with_session(json!("")),
        with_session(json!("../escape")),
        with_session(json!("..\\escape")),
        with_session(json!("C:/escape")),
    ];
    let env = env_with(None);
    for stdin in &inputs {
        for kind in HookKind::ALL {
            let mut out = Vec::new();
            let code = run_with(kind, c.dir(), &mut stdin.as_slice(), &mut out, &env);
            assert_eq!(code, 0);
            // A status line always prints its one line, empty here.
            let expected: &[u8] = if kind == HookKind::Statusline {
                b"\n"
            } else {
                b""
            };
            assert_eq!(out, expected, "{kind} {:?}", String::from_utf8_lossy(stdin));
        }
    }
    assert_eq!(names(c.dir()), ["escape.jsonl", "escape.pending", "state"]);
    assert!(names(&c.state()).is_empty());
    assert_eq!(
        fs::read_to_string(c.dir().join("escape.pending")).expect("escape"),
        "LEAKED"
    );
}

// ---------------------------------------------------------------- end to end

/// The three hooks through `run_with`, fed what Claude Code sends, in the order a session
/// meets them: a consult returns, the status line refreshes, Claude's reply streams in.
#[test]
fn a_session_end_to_end() {
    let c = Case::new();
    let env = {
        let vars: HashMap<&str, &str> = HashMap::from([("CLAUDE_CODE_ENTRYPOINT", "cli")]);
        move |k: &str| vars.get(k).map(|v| v.to_string())
    };
    let hook = |kind: HookKind, event: Value| {
        let mut out = Vec::new();
        let stdin = event.to_string().into_bytes();
        assert_eq!(
            run_with(kind, c.dir(), &mut stdin.as_slice(), &mut out, &env),
            0
        );
        String::from_utf8(out).expect("utf-8")
    };

    let status = json!({"hook_event_name": "Status", "session_id": SESSION,
        "transcript_path": "/tmp/t.jsonl", "cwd": "/work",
        "model": {"id": "claude-opus", "display_name": "Opus"},
        "workspace": {"current_dir": "/work", "project_dir": "/work"}, "version": "2.1.0",
        "cost": {"total_cost_usd": 0.5}});
    assert_eq!(hook(HookKind::Statusline, status.clone()), "\n");

    let post = json!({"session_id": SESSION, "transcript_path": "/tmp/t.jsonl",
        "cwd": "/work", "permission_mode": "default", "hook_event_name": "PostToolUse",
        "tool_name": CONSULT, "tool_input": {"question": "Is the plan sound?"},
        "tool_response": blocks(&result()), "tool_use_id": "toolu_01"});
    assert_eq!(hook(HookKind::Summary, post.clone()), "");
    assert_eq!(
        hook(HookKind::Statusline, status.clone()),
        "\x1b[2mconsult this session · 1 call · 0.4201 USD · 1.34M in / 56k out\x1b[0m\n"
    );

    let flush = |index: u64, delta: &str, is_final: bool| {
        json!({"session_id": SESSION, "transcript_path": "/tmp/t.jsonl", "cwd": "/work",
            "hook_event_name": "MessageDisplay", "turn_id": "turn-7", "message_id": "msg-3",
            "index": index, "final": is_final, "delta": delta})
    };
    assert_eq!(hook(HookKind::Display, flush(0, "The panel ", false)), "");
    assert_eq!(
        hook(
            HookKind::Display,
            flush(1, "agrees with two caveats.", false)
        ),
        ""
    );
    let out = hook(HookKind::Display, flush(2, "Both are minor.", true));
    assert!(
        out.ends_with('\n') && out.matches('\n').count() == 1,
        "{out:?}"
    );
    assert_eq!(
        shown(Some(out.trim_end_matches('\n').to_string())),
        format!("Both are minor.\n\n{}\n", dim_summary())
    );
    // Shown once: the next reply draws nothing.
    assert_eq!(hook(HookKind::Display, flush(0, "Next.", true)), "");

    // A second consult, clean room, from a headless subagent: counted, never drawn.
    let headless_env = env_with(Some("sdk-cli"));
    let mut post_clean = post;
    post_clean["tool_name"] = json!(CLEANROOM);
    let stdin = post_clean.to_string().into_bytes();
    let mut out = Vec::new();
    assert_eq!(
        run_with(
            HookKind::Summary,
            c.dir(),
            &mut stdin.as_slice(),
            &mut out,
            &headless_env
        ),
        0
    );
    assert!(out.is_empty());
    assert_eq!(c.pending(), None);
    assert_eq!(
        hook(HookKind::Statusline, status),
        "\x1b[2mconsult this session · 2 calls · 0.8402 USD · 2.68M in / 113k out\x1b[0m\n"
    );
}
