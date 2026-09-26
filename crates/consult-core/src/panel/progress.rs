//! Live state of a call, read by a progress ticker while the reviewers run.

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::display::ProgressStyle;
use crate::util::{monotonic, py_str, tokens};

/// Running cost and token totals for one reviewer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meter {
    /// USD, from OpenRouter's own accounting.
    pub cost: f64,
    /// Prompt tokens.
    pub tok_in: u64,
    /// Completion tokens.
    pub tok_out: u64,
    /// Reasoning tokens, part of the completion.
    pub tok_reasoning: u64,
    /// Prompt tokens served from cache.
    pub tok_cached: u64,
    /// The last `finish_reason`: kept because text alone does not say a review ended,
    /// only the provider's reason for stopping does. `None` until anything answered,
    /// `"missing"` when a response gave none.
    pub finish: Option<String>,
}

fn f64_of(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn u64_of(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|x| *x >= 0.0).map(|x| x as u64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// The `finish_reason` of a response's first choice, `"missing"` when there is none.
pub fn finish_of(data: &Value) -> String {
    let reason = data
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"));
    match reason {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        None | Some(Value::Null) | Some(Value::String(_)) | Some(Value::Bool(false)) => {
            "missing".to_string()
        }
        Some(other) => py_str(other),
    }
}

impl Meter {
    /// Adds one response's usage and records its finish reason.
    pub fn add(&mut self, data: &Value) {
        let u = data.get("usage").filter(|u| u.is_object());
        let field = |k: &str| u.and_then(|u| u.get(k));
        self.cost += f64_of(field("cost"));
        self.tok_in += u64_of(field("prompt_tokens"));
        self.tok_out += u64_of(field("completion_tokens"));
        self.tok_reasoning +=
            u64_of(field("completion_tokens_details").and_then(|d| d.get("reasoning_tokens")));
        self.tok_cached +=
            u64_of(field("prompt_tokens_details").and_then(|d| d.get("cached_tokens")));
        self.finish = Some(finish_of(data));
    }

    /// Share of prompt tokens served from cache, in percent.
    pub fn cache_hit_pct(&self) -> f64 {
        if self.tok_in == 0 {
            0.0
        } else {
            self.tok_cached as f64 / self.tok_in as f64 * 100.0
        }
    }
}

/// One reviewer's live state, read by the progress line while the call runs.
///
/// It owns the reviewer's real meter and trace rather than copies, so the line cannot
/// drift from what the reviewer has actually done.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewerState {
    /// Short name for display.
    pub short: String,
    /// Cost and tokens so far.
    pub meter: Meter,
    /// One entry per tool call; bookkeeping entries are parenthesised.
    pub trace: Vec<String>,
    /// Investigation rounds started.
    pub steps: usize,
    /// `None` while the reviewer is still running.
    pub status: Option<String>,
    /// When it last did anything, in [`monotonic`] seconds.
    pub active: f64,
}

impl ReviewerState {
    /// A reviewer that has not started.
    pub fn new(short: impl Into<String>) -> Self {
        Self {
            short: short.into(),
            meter: Meter::default(),
            trace: Vec::new(),
            steps: 0,
            status: None,
            active: monotonic(),
        }
    }

    /// Marks it active now.
    pub fn touch(&mut self) {
        self.active = monotonic();
    }

    /// Marks it finished with this status.
    pub fn end(&mut self, status: &str) {
        self.status = Some(status.to_string());
        self.touch();
    }
}

/// A reviewer state shared between the reviewer and whoever reports on it.
pub type SharedState = Arc<Mutex<ReviewerState>>;

/// A new shared state.
pub fn shared(short: impl Into<String>) -> SharedState {
    Arc::new(Mutex::new(ReviewerState::new(short)))
}

/// Locks a shared state, recovering it if a panicking holder poisoned the lock: a
/// progress line must never be the thing that fails.
pub fn lock(state: &SharedState) -> MutexGuard<'_, ReviewerState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Live state of one call, for a caller that reports on it while it runs.
#[derive(Debug)]
pub struct Progress {
    /// `consult` or `cleanroom`.
    pub label: String,
    /// When the call started, in [`monotonic`] seconds.
    pub started: f64,
    reviewers: Mutex<Vec<SharedState>>,
}

impl Progress {
    /// A call starting now.
    pub fn new(label: impl Into<String>) -> Self {
        Self::with_started(label, monotonic())
    }

    /// A call that started at `started` ([`monotonic`] seconds).
    pub fn with_started(label: impl Into<String>, started: f64) -> Self {
        Self {
            label: label.into(),
            started,
            reviewers: Mutex::new(Vec::new()),
        }
    }

    /// Sets the reviewers being tracked.
    pub fn set_reviewers(&self, states: Vec<SharedState>) {
        *self.reviewers.lock().unwrap_or_else(|e| e.into_inner()) = states;
    }

    /// The reviewers being tracked.
    pub fn reviewers(&self) -> Vec<SharedState> {
        self.reviewers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Tool calls in a trace; bookkeeping entries — nudges, caps, continuations — are
/// parenthesised and not counted.
pub fn tool_calls(trace: &[String]) -> usize {
    trace.iter().filter(|t| !t.starts_with('(')).count()
}

/// One progress notification for the call's current state: (message, progress, total).
///
/// A single line on purpose: the client shows only the latest message, on one collapsed
/// row, and appends a percentage itself whenever a total is sent. `now` defaults to
/// [`monotonic`].
pub fn progress_line(
    progress: &Progress,
    style: ProgressStyle,
    now: Option<f64>,
) -> (String, f64, Option<f64>) {
    let label = &progress.label;
    let revs: Vec<ReviewerState> = progress
        .reviewers()
        .iter()
        .map(|s| lock(s).clone())
        .collect();
    let elapsed = (now.unwrap_or_else(monotonic) - progress.started).trunc() as i64;
    let n = revs.len();
    let finished = revs.iter().filter(|r| r.status.is_some()).count();
    if style == ProgressStyle::Percent {
        return (
            format!("{label} · reviewers finished"),
            finished as f64,
            (n > 0).then_some(n as f64),
        );
    }
    let cost: f64 = revs.iter().map(|r| r.meter.cost).sum();
    let message = match style {
        ProgressStyle::Quiet => format!(
            "{label} · asking {}",
            revs.iter()
                .map(|r| r.short.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ProgressStyle::Count => {
            let calls: usize = revs.iter().map(|r| tool_calls(&r.trace)).sum();
            format!("{label} · {finished}/{n} finished · {calls} tool calls")
        }
        ProgressStyle::Marks => {
            let marks: Vec<String> = revs
                .iter()
                .map(|r| match r.status.as_deref() {
                    None => format!("{} ▸{}", r.short, r.steps),
                    Some("ok") => format!("{} ✓", r.short),
                    Some(_) => format!("{} ✗", r.short),
                })
                .collect();
            format!("{label} · {}", marks.join(" · "))
        }
        ProgressStyle::Latest => latest(&revs).unwrap_or_else(|| label.clone()),
        ProgressStyle::Ticker => {
            format!("{label} · {elapsed}s · {cost:.2} USD so far · {finished}/{n} finished")
        }
        _ => {
            let tin: u64 = revs.iter().map(|r| r.meter.tok_in).sum();
            let tout: u64 = revs.iter().map(|r| r.meter.tok_out).sum();
            format!(
                "{label} · {elapsed}s · {cost:.2} USD · {} in / {} out · {finished}/{n} finished",
                tokens(tin),
                tokens(tout)
            )
        }
    };
    (message, elapsed as f64, None)
}

fn latest(revs: &[ReviewerState]) -> Option<String> {
    // The first of the most recently active, as Python's max().
    let mut best: Option<&ReviewerState> = None;
    for r in revs {
        if best.is_none_or(|b| r.active > b.active) {
            best = Some(r);
        }
    }
    let r = best?;
    Some(match r.status.as_deref() {
        None => format!(
            "{}: {}",
            r.short,
            r.trace.last().map(String::as_str).unwrap_or("starting")
        ),
        Some("ok") => format!("{}: finished", r.short),
        Some(status) => {
            let reason = match (&r.meter.finish, status) {
                (Some(f), "incomplete") if !f.is_empty() => f.as_str(),
                _ => status,
            };
            format!("{}: stopped ({reason})", r.short)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three reviewers mid-call: one running, one finished, one stopped.
    fn fixed_progress(label: &str) -> Progress {
        let p = Progress::with_started(label, 100.0);
        let mut run = ReviewerState::new("deepseek");
        run.steps = 4;
        run.active = 110.0;
        run.trace = vec!["read_file(path=a.py)".into(), "grep(pattern=x)".into()];
        run.meter.cost = 0.12;
        run.meter.tok_in = 250_000;
        run.meter.tok_out = 9_000;
        let mut done = ReviewerState::new("glm");
        done.steps = 6;
        done.status = Some("ok".into());
        done.active = 105.0;
        done.trace = vec!["read_file(path=b.py)".into(); 3];
        done.trace
            .push("(hit the 24-step investigation cap)".into());
        done.meter.cost = 0.20;
        done.meter.tok_in = 400_000;
        done.meter.tok_out = 12_000;
        done.meter.finish = Some("stop".into());
        let mut stop = ReviewerState::new("luna");
        stop.steps = 2;
        stop.status = Some("incomplete".into());
        stop.active = 120.0;
        stop.trace = vec!["glob(pattern=*)".into()];
        stop.meter.cost = 0.05;
        stop.meter.tok_in = 600_000;
        stop.meter.tok_out = 3_400;
        stop.meter.finish = Some("content_filter".into());
        p.set_reviewers(vec![
            Arc::new(Mutex::new(run)),
            Arc::new(Mutex::new(done)),
            Arc::new(Mutex::new(stop)),
        ]);
        p
    }

    fn line(style: ProgressStyle, p: &Progress) -> (String, f64, Option<f64>) {
        progress_line(p, style, Some(165.7))
    }

    fn msg(s: &str, v: f64, t: Option<f64>) -> (String, f64, Option<f64>) {
        (s.to_string(), v, t)
    }

    #[test]
    fn every_style() {
        let p = fixed_progress("consult");
        assert_eq!(
            line(ProgressStyle::Full, &p),
            msg(
                "consult · 65s · 0.37 USD · 1.25M in / 24k out · 2/3 finished",
                65.0,
                None
            )
        );
        assert_eq!(
            line(ProgressStyle::Quiet, &p),
            msg("consult · asking deepseek, glm, luna", 65.0, None)
        );
        assert_eq!(
            line(ProgressStyle::Count, &p),
            msg("consult · 2/3 finished · 6 tool calls", 65.0, None)
        );
        assert_eq!(
            line(ProgressStyle::Marks, &p),
            msg("consult · deepseek ▸4 · glm ✓ · luna ✗", 65.0, None)
        );
        assert_eq!(
            line(ProgressStyle::Percent, &p),
            msg("consult · reviewers finished", 2.0, Some(3.0))
        );
        assert_eq!(
            line(ProgressStyle::Ticker, &p),
            msg("consult · 65s · 0.37 USD so far · 2/3 finished", 65.0, None)
        );
    }

    #[test]
    fn latest_follows_the_most_recently_active_reviewer() {
        let p = fixed_progress("consult");
        assert_eq!(
            line(ProgressStyle::Latest, &p).0,
            "luna: stopped (content_filter)"
        );
        let revs = p.reviewers();
        lock(&revs[2]).active = 100.0;
        assert_eq!(
            line(ProgressStyle::Latest, &p).0,
            "deepseek: grep(pattern=x)"
        );
        lock(&revs[1]).active = 130.0;
        assert_eq!(line(ProgressStyle::Latest, &p).0, "glm: finished");
    }

    #[test]
    fn latest_names_the_status_when_there_is_no_finish() {
        let p = fixed_progress("consult");
        {
            let revs = p.reviewers();
            let mut luna = lock(&revs[2]);
            luna.status = Some("error".into());
            luna.meter.finish = None;
        }
        assert_eq!(line(ProgressStyle::Latest, &p).0, "luna: stopped (error)");
    }

    #[test]
    fn clean_room_label() {
        let p = fixed_progress("cleanroom");
        assert!(
            line(ProgressStyle::Full, &p)
                .0
                .starts_with("cleanroom · 65s")
        );
    }

    #[test]
    fn token_format() {
        let p = fixed_progress("consult");
        let revs = p.reviewers();
        for r in &revs {
            let mut r = lock(r);
            r.meter.tok_in = 0;
            r.meter.tok_out = 0;
        }
        {
            let mut r = lock(&revs[0]);
            r.meter.tok_in = 999_999;
            r.meter.tok_out = 1_000_000;
        }
        assert!(
            line(ProgressStyle::Full, &p)
                .0
                .contains("1000k in / 1.00M out")
        );
        // Below a thousand the exact count, not "0k".
        {
            let mut r = lock(&revs[0]);
            r.meter.tok_in = 312;
            r.meter.tok_out = 0;
        }
        assert!(line(ProgressStyle::Full, &p).0.contains("312 in / 0 out"));
    }

    #[test]
    fn no_reviewers_yet() {
        let p = Progress::new("consult");
        for style in ProgressStyle::ALL {
            let (message, _, total) = progress_line(&p, style, None);
            assert!(message.starts_with("consult"), "{style}");
            assert_eq!(total, None, "{style}");
        }
    }

    #[test]
    fn meter_adds_usage() {
        let mut m = Meter::default();
        m.add(&serde_json::json!({
            "choices": [{"finish_reason": "stop"}],
            "usage": {"cost": 0.01, "prompt_tokens": 1000, "completion_tokens": 100,
                      "completion_tokens_details": {"reasoning_tokens": 40},
                      "prompt_tokens_details": {"cached_tokens": 850}}}));
        m.add(&serde_json::json!({"choices": [], "usage": {}}));
        assert_eq!(
            (m.tok_in, m.tok_out, m.tok_reasoning, m.tok_cached),
            (1000, 100, 40, 850)
        );
        assert_eq!(m.finish.as_deref(), Some("missing"));
        assert!((m.cache_hit_pct() - 85.0).abs() < 1e-9);
    }
}
