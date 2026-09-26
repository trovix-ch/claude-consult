//! The server driven through rmcp's own client over an in-memory duplex: what Claude
//! Code sees in `tools/list`, plain-text results, whole-call failures, both shapes of
//! `models`, and the heartbeat on the wire. Offline: OpenRouter is a wiremock server.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{A, B, Harness, read, reply};
use consult_core::records::trailing_records;
use consult_mcp::{ConsultServer, INSTRUCTIONS};
use rmcp::model::{CallToolRequestParams, CallToolResult, ProgressNotificationParam};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{Map, Value, json};

/// A client that records every progress notification it receives.
#[derive(Clone, Default)]
struct Recorder {
    progress: Arc<Mutex<Vec<ProgressNotificationParam>>>,
}

impl ClientHandler for Recorder {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.progress.lock().expect("lock").push(params);
    }
}

async fn connect<H: ClientHandler>(
    server: ConsultServer,
    handler: H,
) -> RunningService<RoleClient, H> {
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    handler.serve(client_io).await.expect("client connects")
}

fn args(v: Value) -> Map<String, Value> {
    v.as_object().expect("object").clone()
}

async fn call(
    client: &RunningService<RoleClient, impl ClientHandler>,
    name: &'static str,
    a: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args(a)))
        .await
        .expect("tools/call")
}

fn text(res: &CallToolResult) -> String {
    assert!(
        res.structured_content.is_none(),
        "structured content returned"
    );
    assert_eq!(res.content.len(), 1);
    res.content[0].as_text().expect("text content").text.clone()
}

fn assert_failed(out: &str, prefix: &str) {
    assert!(out.starts_with(prefix), "{out}");
    let records = trailing_records(out).expect("a record block");
    assert_eq!(records.len(), 1);
    assert_eq!(
        Value::Object(records[0].raw.clone()),
        json!({"alias": null, "short": null, "status": "failed", "complete": false,
               "finish": null, "capped": false, "tool_calls": 0, "cost_usd": 0.0,
               "tokens_in": 0, "tokens_out": 0, "seconds": 0.0})
    );
}

// The live Python server's tools/list, 2026-09-26: these are what Claude Code shows.
const CONSULT: &str = "Get an independent opinion from non-Claude models running on OpenRouter. Two modes: `review` (default) assesses a plan, design, proposal or idea; `diagnose` works out what is causing a problem whose cause is not yet established, ranking competing explanations and naming the cheapest check that would settle it. Each reviewer gets a clean context plus read-only access to the project (glob, grep, read_file, read-only git) and investigates on its own before answering; none can modify anything. Three reviewers from different labs run concurrently, so disagreement between them is a useful signal. Put everything that matters in `question` — they cannot see the conversation.";
const CLEAN: &str = "Ask a design or architecture question with NO project context at all — no file tree, no code, no git, no tools. Use this when the existing implementation would anchor the answer: showing a model the current design reliably produces a patch to what exists rather than what a competent engineer would actually build. Ask for the design you want judged on its merits, describing requirements and constraints only. Defaults to the full panel, each responder pointed at a different aspect of the problem (simplicity / scale-and-failure / question-the-frame / data-and-state) so the answers cover more ground; with no repo to read this costs only a few cents. Name a single model for a quick one-off. The answer is unanchored and therefore also unverified against the real codebase — check it before acting.";
const LIST: &str = "Show the registered reviewer models, the names they answer to, their context windows and per-million-token prices, live from OpenRouter when it answers.";

#[tokio::test]
async fn tools_are_listed_with_their_names_titles_and_descriptions() {
    let h = Harness::new().await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;

    let info = client.peer_info().expect("server info");
    assert_eq!(
        info.server_info.as_ref().map(|s| s.name.as_str()),
        Some("openrouter")
    );
    assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));

    let tools = client.list_all_tools().await.expect("tools/list");
    let got: Vec<(&str, Option<&str>, Option<&str>)> = tools
        .iter()
        .map(|t| {
            (
                t.name.as_ref(),
                t.title.as_deref(),
                t.description.as_deref(),
            )
        })
        .collect();
    let mut got = got;
    got.sort();
    assert_eq!(
        got,
        [
            ("consult", Some("Consult a review panel"), Some(CONSULT)),
            (
                "consult_clean",
                Some("Clean-room question (no project context)"),
                Some(CLEAN)
            ),
            ("list_reviewers", Some("List reviewer models"), Some(LIST)),
        ]
    );
    for t in &tools {
        assert!(
            t.output_schema.is_none(),
            "{} declares an output schema",
            t.name
        );
    }

    let consult = tools.iter().find(|t| t.name == "consult").expect("consult");
    let props = consult.input_schema["properties"]
        .as_object()
        .expect("properties");
    let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "attachments",
            "max_steps",
            "mode",
            "models",
            "question",
            "root"
        ]
    );
    assert_eq!(consult.input_schema["required"], json!(["question"]));
    // The defaults the Python signature advertised.
    assert_eq!(
        (
            &props["root"]["default"],
            &props["max_steps"]["default"],
            &props["mode"]["default"]
        ),
        (&json!(""), &json!(24), &json!("review")),
        "{props:#?}"
    );
    assert_eq!(
        props["max_steps"]["description"],
        "Cap on investigation rounds per reviewer. Default 24."
    );
    assert_eq!(
        props["root"]["description"],
        "Absolute path to the project the reviewers may read. Defaults to the server's working directory; pass it explicitly when in doubt."
    );
    for p in props.values() {
        let d = p["description"].as_str().expect("described");
        assert!(!d.contains('\n') && !d.contains("  "), "{d:?}");
    }
    let clean = tools
        .iter()
        .find(|t| t.name == "consult_clean")
        .expect("clean");
    assert_eq!(clean.input_schema["required"], json!(["question"]));
    client.cancel().await.expect("close");
}

#[tokio::test]
async fn consult_without_a_key_fails_as_text() {
    let h = Harness::new().await;
    let client = connect(ConsultServer::from_consultant(h.keyless()), ()).await;
    let res = call(
        &client,
        "consult",
        json!({"question": "q", "root": h.project.to_string_lossy()}),
    )
    .await;
    assert_ne!(res.is_error, Some(true));
    assert_failed(&text(&res), "Consult failed: ");
    assert_eq!(h.chat_requests().await, 0);
}

#[tokio::test]
async fn consult_with_a_bad_root_fails_as_text() {
    let h = Harness::new().await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;
    let res = call(
        &client,
        "consult",
        json!({"question": "q", "root": h.project.join("missing").to_string_lossy()}),
    )
    .await;
    assert_failed(&text(&res), "Consult failed: SandboxError");
}

#[tokio::test]
async fn clean_room_with_no_question_fails_as_text() {
    let h = Harness::new().await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;
    let res = call(&client, "consult_clean", json!({"question": "  "})).await;
    assert_failed(&text(&res), "Clean-room consult failed: question is empty");
}

#[tokio::test]
async fn list_reviewers_returns_the_table() {
    let h = Harness::new().await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;
    let out = text(&call(&client, "list_reviewers", json!({})).await);
    assert!(
        out.starts_with("Default panel: alpha, beta, gamma\n"),
        "{out}"
    );
}

#[tokio::test]
async fn list_reviewers_reads_models_json_on_every_call() {
    let h = Harness::new().await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;
    std::fs::write(
        h.install.join("models.json"),
        json!({"default_panel": ["alpha"], "models": {"alpha": {"id": A}}}).to_string(),
    )
    .expect("write");
    let out = text(&call(&client, "list_reviewers", json!({})).await);
    assert!(out.starts_with("Default panel: alpha\n"), "{out}");
    std::fs::write(h.install.join("models.json"), "not json").expect("write");
    let out = text(&call(&client, "list_reviewers", json!({})).await);
    assert!(
        out.starts_with("Could not read the reviewer registry (models.json): "),
        "{out}"
    );
}

async fn clean_aliases(models: Value) -> Vec<Option<String>> {
    let h = Harness::new().await;
    h.script(
        vec![
            (A, vec![reply("From alpha.")]),
            (B, vec![reply("From beta.")]),
        ],
        Duration::ZERO,
    )
    .await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;
    let out = text(
        &call(
            &client,
            "consult_clean",
            json!({"question": "Design X.", "models": models}),
        )
        .await,
    );
    trailing_records(&out)
        .expect("records")
        .into_iter()
        .map(|r| r.alias)
        .collect()
}

#[tokio::test]
async fn models_arrive_as_a_list_or_a_comma_separated_string() {
    let want = vec![Some("alpha".to_string()), Some("beta".to_string())];
    assert_eq!(clean_aliases(json!("alpha, beta,")).await, want);
    assert_eq!(clean_aliases(json!(["alpha", "beta"])).await, want);
}

#[tokio::test]
async fn consult_returns_plain_text_with_one_record() {
    let h = Harness::new().await;
    h.script(vec![(A, vec![read(), reply("A review.")])], Duration::ZERO)
        .await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;
    let out = text(
        &call(
            &client,
            "consult",
            json!({"question": "q", "root": h.project.to_string_lossy(), "models": "al",
                   "attachments": "a.txt", "max_steps": 500, "mode": " Diagnose "}),
        )
        .await,
    );
    assert!(out.contains("\nA review.\n"), "{out}");
    let records = trailing_records(&out).expect("records");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, "ok");
}

#[tokio::test]
async fn a_bad_models_value_is_a_tool_error_not_a_protocol_error() {
    let h = Harness::new().await;
    let client = connect(ConsultServer::from_consultant(h.consultant()), ()).await;
    let res = client
        .call_tool(
            CallToolRequestParams::new("consult_clean")
                .with_arguments(args(json!({"question": "q", "models": 5}))),
        )
        .await;
    match res {
        Ok(r) => assert_eq!(r.is_error, Some(true)),
        Err(e) => panic!("protocol error instead of a tool error: {e}"),
    }
}

#[tokio::test]
async fn the_heartbeat_runs_for_the_whole_call_and_stops_before_the_result() {
    let h = Harness::new().await;
    std::fs::write(h.install.join("display.json"), r#"{"progress": "count"}"#).expect("write");
    h.script(
        vec![(A, vec![read(), reply("A review.")])],
        Duration::from_millis(150),
    )
    .await;
    let recorder = Recorder::default();
    let server = ConsultServer::from_consultant(h.consultant())
        .with_progress_interval(Duration::from_millis(20));
    let client = connect(server, recorder.clone()).await;
    // rmcp's client puts a progress token on every request.
    let res = call(
        &client,
        "consult",
        json!({"question": "q", "root": h.project.to_string_lossy(), "models": ["alpha"]}),
    )
    .await;
    let out = text(&res);
    assert_eq!(trailing_records(&out).expect("records").len(), 1);
    let sent = recorder.progress.lock().expect("lock").len();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let all = recorder.progress.lock().expect("lock").clone();
    assert_eq!(all.len(), sent, "progress kept coming after the result");
    // Two silent 150ms requests at a 20ms interval: the line keeps coming while nothing
    // changes, since it is also the idle-timeout heartbeat.
    assert!(all.len() > 5, "only {} progress notifications", all.len());
    for p in &all {
        assert_eq!(p.progress_token, all[0].progress_token);
        assert_eq!(p.total, None);
        let m = p.message.as_deref().unwrap_or("");
        assert!(
            m == "consult · 0/1 finished · 0 tool calls"
                || m == "consult · 0/1 finished · 1 tool calls"
                || m == "consult · 1/1 finished · 1 tool calls"
                || m == "consult · 0/0 finished · 0 tool calls",
            "{m}"
        );
    }
}

#[tokio::test]
async fn clean_room_reports_with_its_own_label_and_percent_sends_a_total() {
    let h = Harness::new().await;
    std::fs::write(
        h.install.join("display.json"),
        "\u{feff}{\"progress\": \"percent\"}",
    )
    .expect("write");
    h.script(
        vec![(A, vec![reply("a")]), (B, vec![reply("b")])],
        Duration::from_millis(150),
    )
    .await;
    let recorder = Recorder::default();
    let server = ConsultServer::from_consultant(h.consultant())
        .with_progress_interval(Duration::from_millis(20));
    let client = connect(server, recorder.clone()).await;
    call(
        &client,
        "consult_clean",
        json!({"question": "Design X.", "models": "alpha,beta"}),
    )
    .await;
    let all = recorder.progress.lock().expect("lock").clone();
    assert!(all.len() > 3, "only {} progress notifications", all.len());
    // The first line can go out before the reviewers are known, with no total yet.
    for p in all.iter().skip(1) {
        assert_eq!(p.message.as_deref(), Some("cleanroom · reviewers finished"));
        assert_eq!(p.total, Some(2.0));
    }
}

/// Speaks raw newline-delimited JSON-RPC, since rmcp's client always sends a token.
#[tokio::test]
async fn no_progress_token_means_no_notifications() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let h = Harness::new().await;
    h.script(vec![(A, vec![reply("a")])], Duration::from_millis(100))
        .await;
    let server = ConsultServer::from_consultant(h.consultant())
        .with_progress_interval(Duration::from_millis(10));
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(running) = server.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let (read_half, mut write_half) = tokio::io::split(client_io);
    let mut lines = BufReader::new(read_half).lines();
    let mut send = async |v: Value| {
        write_half
            .write_all(format!("{v}\n").as_bytes())
            .await
            .expect("write");
    };
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}}),
    )
    .await;
    let init: Value =
        serde_json::from_str(&lines.next_line().await.expect("read").expect("line")).expect("json");
    assert_eq!(init["id"], 1);
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
    send(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "consult_clean", "arguments": {"question": "q", "models": ["alpha"]}}}),
    )
    .await;
    let mut before = Vec::new();
    let result = loop {
        let msg: Value =
            serde_json::from_str(&lines.next_line().await.expect("read").expect("line"))
                .expect("json");
        if msg["id"] == 2 {
            break msg;
        }
        before.push(msg);
    };
    assert!(before.is_empty(), "sent without a token: {before:?}");
    assert_eq!(result["result"]["content"][0]["type"], "text");
    assert!(result["result"].get("structuredContent").is_none());
}
