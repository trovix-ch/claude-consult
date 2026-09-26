//! Runs reviewer models against OpenRouter, each in its own agentic loop.
//!
//! Every reviewer starts from a clean context: an orientation brief about the project
//! plus the question being asked. From there it investigates on its own using the
//! read-only tools in [`crate::sandbox`]. Reviewers run concurrently and never see each
//! other's output, so agreement between them is real corroboration rather than one
//! model echoing another.

pub mod progress;
pub mod prompts;
pub mod render;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::ConsultError;
use crate::key::load_api_key_from;
use crate::listing::ListingCache;
use crate::models::{ResolvedModel, load_models, load_models_value, resolve_listed};
use crate::openrouter::Client;
use crate::sandbox::{Sandbox, dispatch, sanitize, tool_schemas};
use crate::util::{char_len, clip_chars, monotonic, py_str, round_to, tail_chars};

pub use progress::{
    Meter, Progress, ReviewerState, SharedState, lock, progress_line, shared, tool_calls,
};
pub use prompts::*;
pub use render::{failed, record_of, render, reviewers_table};

/// Investigation rounds per reviewer when the caller names none.
pub const DEFAULT_MAX_STEPS: usize = 24;
/// The most a caller may ask for.
pub const MAX_STEPS_CEILING: usize = 60;
// Diagnose mode reasons far harder than review: measured on the same repo, same
// model and same tool-call count, 795 reasoning tokens and 93s in review became
// 38,086 and 855s in diagnose. Ranking competing explanations and falsifying the
// leading one is real work, not waste — but at the old 900s ceiling that run had
// 45s to spare, and a bigger codebase would have been cut off mid-investigation
// and forced to write up a half-finished diagnosis. Cost is not the binding
// constraint here ($0.08 against a $1.00 budget); wall clock is.
// Must stay below MCP_TOOL_TIMEOUT in ~/.claude/settings.json, with room for the
// final write-up on top.
/// Wall-clock budget per reviewer's investigation.
pub const REVIEWER_TIMEOUT: Duration = Duration::from_secs(1500);
/// A tool result longer than this is cut.
pub const MAX_TOOL_RESULT_CHARS: usize = 60_000;
/// A reviewer that keeps finding more to read can otherwise run up an unbounded bill.
/// On hitting this it stops investigating and writes up what it has.
pub const MAX_REVIEWER_COST_USD: f64 = 1.00;

// `max_tokens` is a ceiling on *completion* tokens, and on OpenRouter that
// includes a reasoning model's private thinking. Kimi K3 measured at ~4.8k
// reasoning tokens on a single design question and far more once it has a large
// investigated context behind it, so a tight combined ceiling silently eats the
// review: the model thinks its way through the budget and gets cut off
// mid-sentence with nothing to show. Budget for thinking *and* writing, and
// treat a cut-off response as something to continue rather than accept.
/// Investigation turns, which are mostly short tool calls.
pub const MAX_STEP_TOKENS: u64 = 16_000;
/// The final written review.
pub const MAX_REVIEW_TOKENS: u64 = 32_000;
/// Stitch attempts when a response still hits the ceiling.
pub const MAX_CONTINUATIONS: usize = 3;

/// The stops that cut an investigation short. Running out of things to say, or a
/// provider returning nothing, is not a budget the reviewer ran into.
pub const BUDGET_STOPS: [&str; 3] = ["step cap", "time budget", "cost budget"];

/// One reviewer's result. Field names and order are the Python dict's, so `run --json`
/// has the same shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewResult {
    /// Registry alias.
    pub alias: String,
    /// Short display name.
    pub short: String,
    /// OpenRouter model id.
    pub model: String,
    /// The lens (or clean-room stance) it was given.
    pub lens: String,
    /// `grounded` or `clean-room`.
    pub mode: String,
    /// Whether it opened anything (always true in clean room).
    pub investigated: bool,
    /// Whether its last response hit the token ceiling.
    pub truncated: bool,
    /// `ok`, `incomplete`, `empty` or `error`.
    pub status: String,
    /// True only for `ok`.
    pub complete: bool,
    /// The last finish reason.
    pub finish: Option<String>,
    /// A budget cut the investigation short.
    pub capped: bool,
    /// Why an incomplete review is incomplete.
    pub incomplete_reason: Option<String>,
    /// The review, or the error.
    pub review: String,
    /// The investigation trace.
    pub trace: Vec<String>,
    /// USD, rounded to 4 places.
    pub cost_usd: f64,
    /// Prompt tokens.
    pub tokens_in: u64,
    /// Completion tokens.
    pub tokens_out: u64,
    /// Reasoning tokens.
    pub tokens_reasoning: u64,
    /// Cached prompt tokens.
    pub tokens_cached: u64,
    /// Cache hit percentage, 1 place.
    pub cache_hit_pct: f64,
    /// Wall clock, 1 place.
    pub seconds: f64,
}

/// A whole call's result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelResult {
    /// The review root, or the clean-room marker.
    pub root: String,
    /// Characters of brief (or question) each reviewer got.
    pub brief_chars: usize,
    /// Wall clock of the whole panel.
    pub elapsed_seconds: f64,
    /// Sum of the reviewers' cost.
    pub total_cost_usd: f64,
    /// Cache hit percentage over all reviewers.
    pub cache_hit_pct: f64,
    /// Every reviewer, in panel order.
    pub reviews: Vec<ReviewResult>,
}

/// Where the API key comes from.
#[derive(Clone, Debug)]
pub enum KeySource {
    /// This key.
    Fixed(String),
    /// `OPENROUTER_API_KEY`, else the settings.json of this Claude dir.
    Lookup(PathBuf),
    /// Only the settings.json of this Claude dir.
    SettingsOnly(PathBuf),
}

impl KeySource {
    /// The key, or why there is none.
    pub fn load(&self) -> Result<String, ConsultError> {
        match self {
            Self::Fixed(k) => Ok(k.clone()),
            Self::Lookup(dir) => {
                load_api_key_from(std::env::var(crate::key::KEY_VAR).ok().as_deref(), dir)
            }
            Self::SettingsOnly(dir) => load_api_key_from(None, dir),
        }
    }
}

/// A grounded consult's parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsultRequest {
    /// The plan, design, problem or question.
    pub question: String,
    /// The project the reviewers may read.
    pub root: String,
    /// Reviewer names; `None` for the default panel.
    pub models: Option<Vec<String>>,
    /// Project-relative files placed in front of every reviewer.
    pub attachments: Option<Vec<String>>,
    /// Investigation rounds per reviewer.
    pub max_steps: usize,
    /// Spend ceiling per reviewer, USD.
    pub max_cost_usd: f64,
    /// `review` or `diagnose`.
    pub mode: String,
}

impl ConsultRequest {
    /// A request with the defaults.
    pub fn new(question: impl Into<String>, root: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            root: root.into(),
            models: None,
            attachments: None,
            max_steps: DEFAULT_MAX_STEPS,
            max_cost_usd: MAX_REVIEWER_COST_USD,
            mode: "review".to_string(),
        }
    }
}

/// Accepts a list or a comma-separated string; models sometimes send either. Empty
/// means `None`.
pub fn split_list(value: &Value) -> Option<Vec<String>> {
    let items: Vec<String> = match value {
        Value::Null => return None,
        Value::String(s) => s.split(',').map(|v| v.trim().to_string()).collect(),
        Value::Array(a) => a.iter().map(|v| py_str(v).trim().to_string()).collect(),
        other => vec![py_str(other).trim().to_string()],
    };
    let items: Vec<String> = items.into_iter().filter(|i| !i.is_empty()).collect();
    (!items.is_empty()).then_some(items)
}

/// A caller's `max_steps`, defaulted and held to 1..=60.
pub fn clamp_steps(max_steps: Option<i64>) -> usize {
    let n = match max_steps {
        None | Some(0) => DEFAULT_MAX_STEPS as i64,
        Some(n) => n,
    };
    n.clamp(1, MAX_STEPS_CEILING as i64) as usize
}

/// A caller's mode, trimmed and lowercased; empty means `review`.
pub fn normalize_mode(mode: Option<&str>) -> String {
    let m = mode.unwrap_or("").trim().to_lowercase();
    if m.is_empty() {
        "review".to_string()
    } else {
        m
    }
}

/// Everything a consult needs from its surroundings; the base URL, key, registry and
/// listing cache are all injectable.
#[derive(Clone, Debug)]
pub struct Consultant {
    /// The OpenRouter client.
    pub client: Client,
    /// Where the key comes from.
    pub key: KeySource,
    /// The install dir holding models.json.
    pub install_dir: PathBuf,
    /// The live listing, shared by every call this consultant makes.
    pub listing: Arc<ListingCache>,
}

impl Consultant {
    /// A consultant against the real OpenRouter.
    pub fn new(install_dir: PathBuf, claude_dir: PathBuf) -> Self {
        Self {
            client: Client::new(),
            key: KeySource::Lookup(claude_dir),
            install_dir,
            listing: Arc::new(ListingCache::new()),
        }
    }

    /// Runs the panel concurrently and returns every reviewer's result.
    ///
    /// `progress`, when given, is filled with each reviewer's live state so the caller
    /// can report on the run while it happens. The run never waits on it.
    pub async fn consult(
        &self,
        req: &ConsultRequest,
        progress: Option<&Progress>,
    ) -> Result<PanelResult, ConsultError> {
        if req.question.trim().is_empty() {
            return Err(ConsultError::msg("question is empty"));
        }
        let mode = if MODES.contains(&req.mode.as_str()) {
            req.mode.clone()
        } else {
            "review".to_string()
        };
        let key = self.key.load()?;
        let sandbox = Sandbox::new(&req.root)?;
        let registry = load_models(&self.install_dir)?;
        let chosen = resolve_listed(
            &registry,
            req.models.as_deref(),
            &self.client,
            &self.listing,
        )
        .await?;
        if chosen.is_empty() {
            return Err(ConsultError::msg("no reviewer models resolved"));
        }
        let states = track(progress, &chosen);

        let brief = {
            let sandbox = sandbox.clone();
            let question = req.question.clone();
            let attachments = req.attachments.clone();
            tokio::task::spawn_blocking(move || {
                build_brief(&sandbox, &question, attachments.as_deref())
            })
            .await
            .map_err(|e| ConsultError::Other {
                kind: "RuntimeError",
                message: e.to_string(),
            })?
        };
        let brief: Arc<str> = Arc::from(brief);
        let started = monotonic();
        let lenses = lenses(&mode);

        let mut handles = Reviewers::default();
        for (i, m) in chosen.iter().enumerate() {
            let (client, key, model) = (self.client.clone(), key.clone(), m.clone());
            let (brief, sandbox, state) =
                (Arc::clone(&brief), sandbox.clone(), Arc::clone(&states[i]));
            let (lens, mode, max_steps, max_cost) = (
                lenses[i % lenses.len()],
                mode.clone(),
                req.max_steps,
                req.max_cost_usd,
            );
            handles.0.push(tokio::spawn(async move {
                let r = run_reviewer(
                    &client,
                    &key,
                    &model,
                    &brief,
                    &sandbox,
                    max_steps,
                    lens,
                    max_cost,
                    &mode,
                    Arc::clone(&state),
                )
                .await;
                lock(&state).end(&r.status);
                r
            }));
        }
        let results = join_all(&mut handles, &chosen, "grounded").await;

        let tok_in: u64 = results.iter().map(|r| r.tokens_in).sum();
        let cached: u64 = results.iter().map(|r| r.tokens_cached).sum();
        Ok(PanelResult {
            root: sandbox.root().display().to_string(),
            brief_chars: char_len(&brief),
            elapsed_seconds: round_to(monotonic() - started, 1),
            total_cost_usd: round_to(results.iter().map(|r| r.cost_usd).sum(), 4),
            cache_hit_pct: if tok_in > 0 {
                round_to(cached as f64 / tok_in as f64 * 100.0, 1)
            } else {
                0.0
            },
            reviews: results,
        })
    }

    /// Asks without any project context, so the answer is not anchored to what exists.
    ///
    /// Defaults to the full panel: with no repo to read, a clean-room run costs cents,
    /// and divergent stances on the same problem are the main thing worth having.
    pub async fn consult_clean(
        &self,
        question: &str,
        models: Option<&[String]>,
        progress: Option<&Progress>,
    ) -> Result<PanelResult, ConsultError> {
        if question.trim().is_empty() {
            return Err(ConsultError::msg("question is empty"));
        }
        let key = self.key.load()?;
        let registry = load_models(&self.install_dir)?;
        let chosen = resolve_listed(&registry, models, &self.client, &self.listing).await?;
        if chosen.is_empty() {
            return Err(ConsultError::msg("no reviewer models resolved"));
        }
        let states = track(progress, &chosen);
        let multi = chosen.len() > 1;
        let started = monotonic();

        let mut handles = Reviewers::default();
        for (i, m) in chosen.iter().enumerate() {
            let (client, key, model) = (self.client.clone(), key.clone(), m.clone());
            let (question, state) = (question.to_string(), Arc::clone(&states[i]));
            let lens = multi.then(|| CLEAN_LENSES[i % CLEAN_LENSES.len()]);
            handles.0.push(tokio::spawn(async move {
                let r =
                    run_clean_reviewer(&client, &key, &model, &question, lens, Arc::clone(&state))
                        .await;
                lock(&state).end(&r.status);
                r
            }));
        }
        let results = join_all(&mut handles, &chosen, "clean-room").await;
        Ok(PanelResult {
            root: "(clean room — no project context supplied)".to_string(),
            brief_chars: char_len(question),
            elapsed_seconds: round_to(monotonic() - started, 1),
            total_cost_usd: round_to(results.iter().map(|r| r.cost_usd).sum(), 4),
            cache_hit_pct: 0.0,
            reviews: results,
        })
    }

    /// The `consult` tool's text: the rendered result, or `Consult failed: ...` closed
    /// by a failed record.
    pub async fn consult_text(&self, req: &ConsultRequest, progress: Option<&Progress>) -> String {
        match self.consult(req, progress).await {
            Ok(result) => render(&result),
            Err(e) => failed(&format!("Consult failed: {e}")),
        }
    }

    /// The `consult_clean` tool's text.
    pub async fn consult_clean_text(
        &self,
        question: &str,
        models: Option<&[String]>,
        progress: Option<&Progress>,
    ) -> String {
        match self.consult_clean(question, models, progress).await {
            Ok(result) => render(&result),
            Err(e) => failed(&format!("Clean-room consult failed: {e}")),
        }
    }

    /// The `list_reviewers` tool's text. What someone runs to find out why a reviewer
    /// misbehaves, so it answers however broken the network or models.json is.
    pub async fn list_reviewers(&self) -> String {
        match load_models_value(&self.install_dir) {
            Err(e) => format!("Could not read the reviewer registry (models.json): {e}"),
            Ok(cfg) => reviewers_table(&cfg, self.listing.get(&self.client).await.as_ref()),
        }
    }
}

fn track(progress: Option<&Progress>, chosen: &[ResolvedModel]) -> Vec<SharedState> {
    let states: Vec<SharedState> = chosen.iter().map(|m| shared(m.short())).collect();
    if let Some(p) = progress {
        p.set_reviewers(states.clone());
    }
    states
}

/// The panel's reviewer tasks, aborted when dropped. Spawned tasks outlive the future
/// that spawned them, so without this a consult cancelled by its caller (a client that
/// went away, a `notifications/cancelled`) would keep every reviewer running, and
/// billing, until it finished on its own.
#[derive(Default)]
struct Reviewers(Vec<tokio::task::JoinHandle<ReviewResult>>);

impl Drop for Reviewers {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

async fn join_all(
    handles: &mut Reviewers,
    chosen: &[ResolvedModel],
    mode: &str,
) -> Vec<ReviewResult> {
    let mut out = Vec::with_capacity(handles.0.len());
    for (h, m) in handles.0.iter_mut().zip(chosen) {
        out.push(match h.await {
            Ok(r) => r,
            Err(e) => {
                let mut r = result(
                    m,
                    "",
                    "error",
                    format!("RuntimeError: {e}"),
                    &ReviewerState::new(m.short()),
                    monotonic(),
                    mode,
                    false,
                    None,
                );
                r.investigated = mode == "clean-room";
                r
            }
        });
    }
    out
}

/// Assembles the orientation a reviewer gets before its first tool call.
pub fn build_brief(sandbox: &Sandbox, question: &str, attachments: Option<&[String]>) -> String {
    let mut parts: Vec<String> = vec![
        "# Review request".into(),
        question.trim().into(),
        String::new(),
        "---".into(),
        "# Project orientation".into(),
        String::new(),
        format!("Project root: `{}`", sandbox.root().display()),
        String::new(),
        "## Directory layout (depth 3)".into(),
        "```".into(),
        sanitize(&sandbox.tree(3)),
        "```".into(),
    ];

    for doc in ["CLAUDE.md", "AGENTS.md", "README.md"] {
        let p = sandbox.root().join(doc);
        if !p.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let total = char_len(&text);
        let mut clipped = sanitize(clip_chars(&text, 6000));
        if total > 6000 {
            clipped.push_str(&format!(
                "\n... ({} more chars; read_file it if relevant)",
                total - 6000
            ));
        }
        parts.extend([
            String::new(),
            format!("## {doc}"),
            "```markdown".into(),
            clipped,
            "```".into(),
        ]);
        break;
    }

    if sandbox.is_git_repo() {
        parts.extend([String::new(), "## Git state".into(), "```".into()]);
        for (label, sub, args) in [
            ("status", "status", vec!["--short", "--branch"]),
            ("recent commits", "log", vec!["-n", "10", "--oneline"]),
            ("uncommitted changes", "diff", vec!["--stat"]),
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            let out = match sandbox.git(sub, &args) {
                Ok(o) => sanitize(&o),
                Err(e) => format!("({e})"),
            };
            parts.push(format!(
                "$ git {sub} ({label})\n{}\n",
                clip_chars(&out, 4000)
            ));
        }
        parts.push("```".into());
    }

    for rel in attachments.unwrap_or_default() {
        match sandbox.read_file(rel, 1, 400) {
            Ok(text) => parts.extend([
                String::new(),
                format!("## Attached: {rel}"),
                "```".into(),
                sanitize(&text),
                "```".into(),
            ]),
            Err(e) => parts.extend([
                String::new(),
                format!("## Attached: {rel}"),
                format!("(could not read: {e})"),
            ]),
        }
    }

    parts.extend([
        String::new(),
        "---".into(),
        "Investigate with your tools as needed, then write your review.".into(),
    ]);
    parts.join("\n")
}

/// Pulls text out of a message, tolerating providers that return content parts.
pub fn text_of(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

fn first_choice(data: &Value) -> Value {
    data.get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn finish_reason(choice: &Value) -> Option<String> {
    match choice.get("finish_reason") {
        None | Some(Value::Null) => None,
        Some(v) => Some(py_str(v)),
    }
}

/// Continues a response that hit the token ceiling, stitching the parts together.
///
/// Raising the ceiling alone is not enough — a reasoning model with a large
/// investigated context can exhaust any fixed budget on thinking. Continuing is what
/// actually guarantees a complete review.
///
/// Returns the text and, if a continuation request failed, why. A failure keeps what
/// was already written: a cut-off review marked incomplete tells the reader far more
/// than the error that replaced it would.
#[allow(clippy::too_many_arguments)]
pub async fn complete_fully(
    client: &Client,
    key: &str,
    model_id: &str,
    messages: &[Value],
    mut text: String,
    mut finish: Option<String>,
    state: &SharedState,
) -> (String, Option<String>) {
    let mut conts = 0;
    let mut convo = messages.to_vec();
    while finish.as_deref() == Some("length") && conts < MAX_CONTINUATIONS {
        conts += 1;
        lock(state).trace.push(format!(
            "(hit token ceiling — continuing {conts}/{MAX_CONTINUATIONS})"
        ));
        convo.push(json!({"role": "assistant", "content": text}));
        convo.push(json!({"role": "user", "content": CONTINUE}));
        let payload = json!({
            "model": model_id,
            "messages": convo,
            "max_tokens": MAX_REVIEW_TOKENS,
            "usage": {"include": true},
        });
        let data = match client.post_chat(key, &payload).await {
            Ok(d) => d,
            Err(e) => {
                let failure = clip_chars(&e.typed(), 300).to_string();
                lock(state)
                    .trace
                    .push(format!("(continuation failed — {failure})"));
                return (text, Some(failure));
            }
        };
        lock(state).meter.add(&data);
        let choice = first_choice(&data);
        text.push_str(&text_of(choice.get("message").unwrap_or(&Value::Null)));
        finish = finish_reason(&choice);
    }
    (text, None)
}

fn brief_args(args: &Map<String, Value>) -> String {
    let bits: Vec<String> = args
        .iter()
        .take(3)
        .map(|(k, v)| {
            let s = py_str(v);
            let s = if char_len(&s) > 48 {
                // For paths the tail is the informative half; elsewhere it's the head.
                if k == "path" {
                    format!("…{}", tail_chars(&s, 47))
                } else {
                    format!("{}…", clip_chars(&s, 47))
                }
            } else {
                s
            };
            format!("{k}={s}")
        })
        .collect();
    bits.join(", ")
}

/// Executes the tool calls of one turn concurrently. Returns (trace label, output) per
/// call, in call order.
async fn run_tools(sandbox: &Sandbox, calls: &[Value]) -> Vec<(String, String)> {
    enum Pending {
        Done(String, String),
        Running(String, tokio::task::JoinHandle<String>),
    }
    let mut pending = Vec::new();
    for call in calls {
        let func = call.get("function").cloned().unwrap_or_else(|| json!({}));
        let name = func
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let parsed: Result<Map<String, Value>, String> = match func.get("arguments") {
            None | Some(Value::Null) => Ok(Map::new()),
            Some(Value::String(s)) if s.is_empty() => Ok(Map::new()),
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(Value::Object(m)) => Ok(m),
                _ => Err(s.clone()),
            },
            Some(Value::Object(m)) => Ok(m.clone()),
            Some(other) => Err(py_str(other)),
        };
        match parsed {
            Err(raw) => pending.push(Pending::Done(
                format!("{name}(<bad args>)"),
                format!(
                    "ERROR: arguments were not valid JSON: {}",
                    clip_chars(&raw, 200)
                ),
            )),
            Ok(args) => {
                let label = format!("{name}({})", brief_args(&args));
                let sandbox = sandbox.clone();
                // Independent reads have no reason to be serialised; running them
                // together keeps thorough reviewers inside the time budget.
                let handle = tokio::task::spawn_blocking(move || dispatch(&sandbox, &name, &args));
                pending.push(Pending::Running(label, handle));
            }
        }
    }
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        out.push(match p {
            Pending::Done(label, text) => (label, text),
            Pending::Running(label, handle) => {
                let mut text = handle
                    .await
                    .unwrap_or_else(|e| format!("ERROR: RuntimeError: {e}"));
                if char_len(&text) > MAX_TOOL_RESULT_CHARS {
                    text = format!(
                        "{}\n... (tool output truncated)",
                        clip_chars(&text, MAX_TOOL_RESULT_CHARS)
                    );
                }
                (label, text)
            }
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn result(
    model: &ResolvedModel,
    lens: &str,
    status: &str,
    review: String,
    state: &ReviewerState,
    started: f64,
    mode: &str,
    capped: bool,
    failure: Option<String>,
) -> ReviewResult {
    let meter = &state.meter;
    // Text alone is not a finished review; only the provider's "stop" says the
    // model ended it. Filtered, cut at the ceiling or ended for no stated reason,
    // the text is kept but must not pass as complete.
    let status = if status == "ok" && meter.finish.as_deref() != Some("stop") {
        "incomplete"
    } else {
        status
    };
    ReviewResult {
        alias: model.alias.clone(),
        short: model.short().to_string(),
        model: model.id.clone(),
        lens: lens.to_string(),
        mode: mode.to_string(),
        // Clean-room answers are supposed to have no tool calls; only a grounded
        // reviewer that opened nothing is a problem worth flagging.
        investigated: mode == "clean-room" || tool_calls(&state.trace) > 0,
        truncated: meter.finish.as_deref() == Some("length"),
        status: status.to_string(),
        complete: status == "ok",
        finish: meter.finish.clone(),
        capped,
        incomplete_reason: (status == "incomplete")
            .then(|| why_incomplete(meter.finish.as_deref(), failure.as_deref())),
        review,
        trace: state.trace.clone(),
        cost_usd: round_to(meter.cost, 4),
        tokens_in: meter.tok_in,
        tokens_out: meter.tok_out,
        tokens_reasoning: meter.tok_reasoning,
        tokens_cached: meter.tok_cached,
        cache_hit_pct: round_to(meter.cache_hit_pct(), 1),
        seconds: round_to(monotonic() - started, 1),
    }
}

fn why_incomplete(finish: Option<&str>, failure: Option<&str>) -> String {
    if let Some(f) = failure {
        return format!("a continuation request failed: {f}");
    }
    match finish {
        Some("length") => {
            format!("still at the token ceiling after {MAX_CONTINUATIONS} continuations")
        }
        Some("missing") => "the provider gave no finish reason".to_string(),
        Some(f) => format!("stopped by `{f}`"),
        None => "stopped by `None`".to_string(),
    }
}

/// Drives one model through investigate-then-answer. Never fails: an error becomes a
/// result with status `error`.
#[allow(clippy::too_many_arguments)]
pub async fn run_reviewer(
    client: &Client,
    key: &str,
    model: &ResolvedModel,
    brief: &str,
    sandbox: &Sandbox,
    max_steps: usize,
    lens: (&str, &str),
    max_cost: f64,
    mode: &str,
    state: SharedState,
) -> ReviewResult {
    let started = monotonic();
    let mut stop_reason: Option<&'static str> = None;
    let outcome = investigate(
        client,
        key,
        model,
        brief,
        sandbox,
        max_steps,
        lens,
        max_cost,
        mode,
        &state,
        started,
        &mut stop_reason,
    )
    .await;
    match outcome {
        Ok(r) => r,
        Err(e) => {
            let capped = stop_reason.is_some_and(|s| BUDGET_STOPS.contains(&s));
            result(
                model,
                lens.0,
                "error",
                e.typed(),
                &lock(&state),
                started,
                "grounded",
                capped,
                None,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn investigate(
    client: &Client,
    key: &str,
    model: &ResolvedModel,
    brief: &str,
    sandbox: &Sandbox,
    max_steps: usize,
    lens: (&str, &str),
    max_cost: f64,
    mode: &str,
    state: &SharedState,
    started: f64,
    stop_reason: &mut Option<&'static str>,
) -> Result<ReviewResult, ConsultError> {
    let system = [SYSTEM_PROMPT, mode_framing(mode), lens.1].join("\n\n");
    let mut messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": brief}),
    ];
    let mut nudged = false;
    let mut broke = false;

    for step in 0..max_steps {
        {
            let mut s = lock(state);
            s.steps = step + 1;
            s.touch();
            if monotonic() - started > REVIEWER_TIMEOUT.as_secs_f64() {
                *stop_reason = Some("time budget");
                s.trace.push("(wall-clock budget exhausted)".into());
                broke = true;
                break;
            }
            if s.meter.cost >= max_cost {
                *stop_reason = Some("cost budget");
                s.trace
                    .push(format!("(cost budget ${max_cost:.2} reached)"));
                broke = true;
                break;
            }
        }

        let payload = json!({
            "model": model.id,
            "messages": messages,
            "tools": tool_schemas(),
            "tool_choice": "auto",
            "max_tokens": MAX_STEP_TOKENS,
            "usage": {"include": true},
        });
        let data = client.post_chat(key, &payload).await?;
        lock(state).meter.add(&data);

        let choices = data
            .get("choices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(choice) = choices.first() else {
            *stop_reason = Some("no choices returned");
            lock(state).trace.push("(no choices returned)".into());
            broke = true;
            break;
        };
        let msg = choice.get("message").cloned().unwrap_or_else(|| json!({}));
        let finish = finish_reason(choice);
        let calls = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if calls.is_empty() {
            let content = text_of(&msg);
            if content.trim().is_empty() {
                *stop_reason = Some("empty response");
                lock(state).trace.push("(empty response)".into());
                broke = true;
                break;
            }
            // A review written without opening a single file is a review of
            // the brief, not of the project. Push back once before accepting.
            let untouched = lock(state).trace.is_empty();
            if untouched && !nudged {
                nudged = true;
                lock(state)
                    .trace
                    .push("(nudged: answered without investigating)".into());
                messages.push(json!({"role": "assistant", "content": content}));
                messages.push(json!({"role": "user", "content": NUDGE}));
                continue;
            }
            // This is the normal finish: the model stopped calling tools and
            // wrote its review. It is also where truncation actually bit, so
            // the ceiling check belongs here, not only on the fallback path.
            let (content, failure) =
                complete_fully(client, key, &model.id, &messages, content, finish, state).await;
            return Ok(result(
                model,
                lens.0,
                "ok",
                content.trim().to_string(),
                &lock(state),
                started,
                "grounded",
                false,
                failure,
            ));
        }

        let content = match msg.get("content") {
            None | Some(Value::Null) => json!(""),
            Some(Value::String(s)) if s.is_empty() => json!(""),
            Some(v) => v.clone(),
        };
        messages.push(json!({"role": "assistant", "content": content, "tool_calls": calls}));
        let outs = run_tools(sandbox, &calls).await;
        let mut s = lock(state);
        for (call, (label, out)) in calls.iter().zip(outs) {
            s.trace.push(label);
            let id = call.get("id").map(py_str).unwrap_or_default();
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": out}));
        }
    }
    if !broke {
        *stop_reason = Some("step cap");
        lock(state)
            .trace
            .push(format!("(hit the {max_steps}-step investigation cap)"));
    }

    lock(state).touch();
    // Investigation ended before the model volunteered a review, so ask for one
    // directly. tool_choice="none" is what forces prose instead of yet another tool
    // call — dropping `tools` alone does not stop every model.
    let reason = stop_reason.unwrap_or("step cap");
    messages.push(json!({"role": "user", "content": budget_spent(reason)}));
    let mut content = String::new();
    let mut failure = None;
    for attempt in 0..2 {
        let payload = json!({
            "model": model.id,
            "messages": messages,
            "tools": tool_schemas(),
            "tool_choice": "none",
            "max_tokens": MAX_REVIEW_TOKENS,
            "usage": {"include": true},
        });
        let data = client.post_chat(key, &payload).await?;
        lock(state).meter.add(&data);
        let choice = first_choice(&data);
        content = text_of(choice.get("message").unwrap_or(&Value::Null));
        if !content.trim().is_empty() {
            (content, failure) = complete_fully(
                client,
                key,
                &model.id,
                &messages,
                content,
                finish_reason(&choice),
                state,
            )
            .await;
            break;
        }
        lock(state)
            .trace
            .push(format!("(empty final response, retry {})", attempt + 1));
    }

    let written = !content.trim().is_empty();
    let review = if written {
        content.trim().to_string()
    } else {
        "(model returned no review)".to_string()
    };
    Ok(result(
        model,
        lens.0,
        if written { "ok" } else { "empty" },
        review,
        &lock(state),
        started,
        "grounded",
        BUDGET_STOPS.contains(&reason),
        failure,
    ))
}

/// One unanchored answer: no project brief, no tools, no repo access at all.
pub async fn run_clean_reviewer(
    client: &Client,
    key: &str,
    model: &ResolvedModel,
    question: &str,
    lens: Option<(&str, &str)>,
    state: SharedState,
) -> ReviewResult {
    let lens_name = lens.map_or("clean room", |l| l.0);
    let system = match lens {
        Some((_, stance)) => format!("{CLEAN_ROOM_PROMPT}\n\nYour particular stance: {stance}"),
        None => CLEAN_ROOM_PROMPT.to_string(),
    };
    let messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": question.trim()}),
    ];
    lock(&state).steps = 1;
    let started = monotonic();
    let payload = json!({
        "model": model.id,
        "messages": messages,
        "max_tokens": MAX_REVIEW_TOKENS,
        "usage": {"include": true},
    });
    let data = match client.post_chat(key, &payload).await {
        Ok(d) => d,
        Err(e) => {
            return result(
                model,
                lens_name,
                "error",
                e.typed(),
                &lock(&state),
                started,
                "clean-room",
                false,
                None,
            );
        }
    };
    lock(&state).meter.add(&data);
    let choice = first_choice(&data);
    let text = text_of(choice.get("message").unwrap_or(&Value::Null));
    if text.trim().is_empty() {
        return result(
            model,
            lens_name,
            "empty",
            "(model returned nothing)".into(),
            &lock(&state),
            started,
            "clean-room",
            false,
            None,
        );
    }
    let (text, failure) = complete_fully(
        client,
        key,
        &model.id,
        &messages,
        text,
        finish_reason(&choice),
        &state,
    )
    .await;
    result(
        model,
        lens_name,
        "ok",
        text.trim().to_string(),
        &lock(&state),
        started,
        "clean-room",
        false,
        failure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_accepts_lists_and_strings() {
        assert_eq!(
            split_list(&json!("a, b,,c")),
            Some(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(split_list(&json!([" a ", ""])), Some(vec!["a".into()]));
        assert_eq!(split_list(&json!("")), None);
        assert_eq!(split_list(&json!([])), None);
        assert_eq!(split_list(&Value::Null), None);
    }

    #[test]
    fn steps_and_mode() {
        assert_eq!(clamp_steps(None), 24);
        assert_eq!(clamp_steps(Some(0)), 24);
        assert_eq!(clamp_steps(Some(-3)), 1);
        assert_eq!(clamp_steps(Some(100)), 60);
        assert_eq!(normalize_mode(Some(" Diagnose ")), "diagnose");
        assert_eq!(normalize_mode(None), "review");
    }

    #[test]
    fn brief_args_clip() {
        let long = "d/".repeat(40);
        let args = json!({"path": long, "pattern": "p".repeat(60), "x": 1, "y": 2});
        let out = brief_args(args.as_object().expect("obj"));
        assert!(out.starts_with("path=…"), "{out}");
        assert!(out.contains(&format!("pattern={}…", "p".repeat(47))));
        assert!(out.ends_with(", x=1"));
    }

    #[test]
    fn content_parts() {
        assert_eq!(
            text_of(&json!({"content": [{"text": "a"}, {"type": "x"}, {"text": "b"}]})),
            "ab"
        );
        assert_eq!(text_of(&json!({"content": null})), "");
        assert_eq!(text_of(&json!({})), "");
    }
}
