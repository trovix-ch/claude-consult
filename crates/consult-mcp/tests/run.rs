//! The one-shot runner end to end against a wiremock OpenRouter, and the HTTP transport
//! on a loopback port.

mod common;

use std::time::Duration;

use common::{A, B, Harness, read, reply};
use consult_mcp::{ConsultServer, Error, RunArgs, RunIo, run_with, serve_http_on};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

async fn run(h: &Harness, args: RunArgs, stdin: &str, keyless: bool) -> Result<Output, Error> {
    let consultant = if keyless { h.keyless() } else { h.consultant() };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_with(
        &args,
        &consultant,
        RunIo {
            stdin: &mut stdin.as_bytes(),
            stdin_is_terminal: false,
            stdout: &mut out,
            stderr: &mut err,
        },
    )
    .await?;
    Ok(Output {
        code,
        stdout: String::from_utf8(out).expect("utf-8"),
        stderr: String::from_utf8(err).expect("utf-8"),
    })
}

#[tokio::test]
async fn clean_json_has_the_python_shape() {
    let h = Harness::new().await;
    h.script(
        vec![(A, vec![reply("Café design.")]), (B, vec![reply("Other.")])],
        Duration::ZERO,
    )
    .await;
    let args = RunArgs {
        clean: true,
        json: true,
        models: Some("alpha, beta".into()),
        ..RunArgs::default()
    };
    let out = run(&h, args, "Design X.", false).await.expect("ran");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.is_empty());
    // json.dumps(indent=2) escapes non-ASCII.
    assert!(out.stdout.contains("Caf\\u00e9 design."), "{}", out.stdout);
    assert!(out.stdout.is_ascii());
    assert!(out.stdout.starts_with("{\n  \"root\": "), "{}", out.stdout);
    assert!(out.stdout.ends_with("}\n"));
    let v: Value = serde_json::from_str(&out.stdout).expect("json");
    let keys: Vec<&str> = v
        .as_object()
        .expect("obj")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "root",
            "brief_chars",
            "elapsed_seconds",
            "total_cost_usd",
            "cache_hit_pct",
            "reviews"
        ]
    );
    assert_eq!(v["root"], "(clean room — no project context supplied)");
    assert_eq!(v["brief_chars"], 9);
    let reviews = v["reviews"].as_array().expect("reviews");
    assert_eq!(reviews.len(), 2);
    assert_eq!(
        (
            &reviews[0]["alias"],
            &reviews[0]["mode"],
            &reviews[0]["status"],
            &reviews[0]["review"]
        ),
        (
            &json!("alpha"),
            &json!("clean-room"),
            &json!("ok"),
            &json!("Café design.")
        )
    );
    assert_eq!(reviews[1]["alias"], "beta");
    let review_keys: Vec<&str> = reviews[0]
        .as_object()
        .expect("obj")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        review_keys,
        [
            "alias",
            "short",
            "model",
            "lens",
            "mode",
            "investigated",
            "truncated",
            "status",
            "complete",
            "finish",
            "capped",
            "incomplete_reason",
            "review",
            "trace",
            "cost_usd",
            "tokens_in",
            "tokens_out",
            "tokens_reasoning",
            "tokens_cached",
            "cache_hit_pct",
            "seconds"
        ]
    );
}

#[tokio::test]
async fn grounded_markdown_ends_with_its_records() {
    let h = Harness::new().await;
    h.script(vec![(A, vec![read(), reply("A review.")])], Duration::ZERO)
        .await;
    let args = RunArgs {
        root: h.project.to_string_lossy().into_owned(),
        question: Some("Is this sound?".into()),
        models: Some("al".into()),
        mode: "diagnose".into(),
        ..RunArgs::default()
    };
    let out = run(&h, args, "", false).await.expect("ran");
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stdout.contains("\nA review.\n"), "{}", out.stdout);
    let records = consult_core::records::trailing_records(&out.stdout).expect("records");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, "ok");
}

#[tokio::test]
async fn a_failed_consult_prints_to_stderr_and_exits_1() {
    let h = Harness::new().await;
    let args = RunArgs {
        clean: true,
        ..RunArgs::default()
    };
    let out = run(&h, args, "Design X.", true).await.expect("ran");
    assert_eq!(out.code, 1);
    assert!(out.stdout.is_empty());
    assert!(out.stderr.starts_with("consult failed: "), "{}", out.stderr);
    assert_eq!(h.chat_requests().await, 0);
}

#[tokio::test]
async fn http_transport_serves_stateless_requests_and_shuts_down() {
    let h = Harness::new().await;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let shutdown = CancellationToken::new();
    let server = ConsultServer::from_consultant(h.consultant());
    let served = tokio::spawn(serve_http_on(server, listener, shutdown.clone()));

    let http = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/mcp");
    // No initialize and no session: each request stands alone.
    let resp = http
        .post(&url)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string())
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("body");
    let data = body
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("an SSE data line");
    let v: Value = serde_json::from_str(data).expect("json");
    let mut names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    names.sort();
    assert_eq!(names, ["consult", "consult_clean", "list_reviewers"]);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), served)
        .await
        .expect("shut down in time")
        .expect("joined")
        .expect("served cleanly");
}
