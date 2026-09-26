//! The markdown a consult returns, and the reviewers table.

use serde_json::Value;

use super::progress::tool_calls;
use super::{PanelResult, ReviewResult};
use crate::listing::LiveListing;
use crate::records::Record;
use crate::util::{
    cell, clip_chars, number, positive_int, py_float_str, py_str, thousands, usd, when,
};

/// Formats a panel result as markdown for the calling assistant to read, closed by one
/// status record per reviewer.
pub fn render(result: &PanelResult) -> String {
    let reviews = &result.reviews;
    let clean_room = !reviews.is_empty() && reviews.iter().all(|r| r.mode == "clean-room");
    let n = reviews.len();
    let plural = if n != 1 { "s" } else { "" };
    let mut lines: Vec<String> = if clean_room {
        vec![
            format!("# Clean-room answer — {n} responder{plural}"),
            format!(
                "No project context supplied · {}s · ${:.4} total",
                py_float_str(result.elapsed_seconds),
                result.total_cost_usd
            ),
            String::new(),
            "*Answered from the problem statement alone — no repository context, no code, no tools. Each response states the assumptions it rests on and the fact that would change it; nothing here has been checked against this codebase.*".to_string(),
            String::new(),
        ]
    } else {
        vec![
            format!("# Panel review — {n} reviewer{plural}"),
            format!(
                "Root: `{}` · {}s · ${:.4} total · {:.0}% prompt cache hit",
                result.root,
                py_float_str(result.elapsed_seconds),
                result.total_cost_usd,
                result.cache_hit_pct
            ),
            String::new(),
        ]
    };
    // An incomplete review still says something worth reading, so it is shown
    // in full under its warning rather than reduced to a failure notice.
    let written = reviews
        .iter()
        .filter(|r| r.status == "ok" || r.status == "incomplete");
    let bad = reviews
        .iter()
        .filter(|r| r.status != "ok" && r.status != "incomplete");

    for r in written {
        let mut warns = Vec::new();
        if !r.investigated {
            warns.push(
                "⚠️ **did not open a single file — treat as opinion on the brief only**"
                    .to_string(),
            );
        }
        if r.status == "incomplete" {
            warns.push(format!(
                "⚠️ **incomplete — {}; the review below may be cut off**",
                r.incomplete_reason.as_deref().unwrap_or("None")
            ));
        }
        let reasoning = if r.tokens_reasoning > 0 {
            format!(" (incl. {} reasoning)", thousands(r.tokens_reasoning))
        } else {
            String::new()
        };
        let is_clean = r.mode == "clean-room";
        let suffix = if is_clean { "" } else { " lens" };
        // Tool-call and cache counts are meaningless for an answer with no tools.
        let calls = if is_clean {
            String::new()
        } else {
            format!(
                "{} tool calls · {} in ({:.0}% cached) / ",
                tool_calls(&r.trace),
                thousands(r.tokens_in),
                r.cache_hit_pct
            )
        };
        lines.push(format!(
            "## {} — {}{suffix}  (`{}`)",
            r.alias, r.lens, r.model
        ));
        lines.push(format!(
            "*{calls}{} out{reasoning} · ${:.4} · {}s*{}",
            thousands(r.tokens_out),
            r.cost_usd,
            py_float_str(r.seconds),
            warns.iter().map(|w| format!("  {w}")).collect::<String>()
        ));
        lines.push(String::new());
        lines.push(r.review.clone());
        lines.push(String::new());
        if !r.trace.is_empty() {
            let items: Vec<String> = r
                .trace
                .iter()
                .take(60)
                .map(|t| format!("- `{t}`"))
                .collect();
            lines.push(format!(
                "<details><summary>investigation trace</summary>\n\n{}\n\n</details>",
                items.join("\n")
            ));
            lines.push(String::new());
        }
        lines.push("---".to_string());
        lines.push(String::new());
    }
    for r in bad {
        lines.push(format!("## {} — FAILED ({})", r.alias, r.status));
        lines.push("```".to_string());
        lines.push(clip_chars(&r.review, 1500).to_string());
        lines.push("```".to_string());
        lines.push(String::new());
    }
    lines.extend(reviews.iter().map(|r| record_of(r).line()));
    lines.join("\n")
}

/// The status record of one review.
pub fn record_of(r: &ReviewResult) -> Record {
    Record {
        alias: Some(r.alias.clone()),
        short: Some(r.short.clone()),
        status: r.status.clone(),
        complete: r.complete,
        finish: r.finish.clone(),
        capped: r.capped,
        tool_calls: tool_calls(&r.trace) as u64,
        cost_usd: r.cost_usd,
        tokens_in: r.tokens_in,
        tokens_out: r.tokens_out,
        seconds: r.seconds,
    }
}

/// A call that failed before any reviewer ran: the message, then one record saying so.
pub fn failed(message: &str) -> String {
    format!("{message}\n\n{}", Record::failed().line())
}

fn context_text(n: Option<u64>) -> String {
    match n {
        Some(n) => thousands(n),
        None => "unknown".to_string(),
    }
}

fn price_text(x: Option<f64>) -> String {
    match x {
        Some(x) => usd(x),
        None => "price unknown".to_string(),
    }
}

/// The registered reviewers as markdown, priced live when the listing answered.
///
/// Reads a models.json of any vintage: from before `command`, from before `priced_at`
/// and `curated`, and with the nulls an install made offline writes.
pub fn reviewers_table(cfg: &Value, live: &LiveListing) -> String {
    let registry: Vec<(&String, &Value)> = cfg
        .get("models")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter(|(_, v)| v.is_object()).collect())
        .unwrap_or_default();
    let listed = live.models();
    let source = match live {
        LiveListing::Live { at, .. } => format!(
            "live from OpenRouter as of {}",
            when(Some(at)).unwrap_or_else(|| "just now".to_string())
        ),
        LiveListing::Unavailable { error } => {
            let reason = if error.is_empty() {
                "no reason given"
            } else {
                error
            };
            let why = format!("OpenRouter's listing is unavailable ({reason})");
            let at = when(cfg.get("priced_at").and_then(Value::as_str));
            let any_price = registry.iter().any(|(_, m)| {
                number(m.get("price_in")).is_some() || number(m.get("price_out")).is_some()
            });
            if any_price {
                format!(
                    "{why}; showing the values recorded at install, {}",
                    match at {
                        Some(at) => format!("priced at {at}"),
                        None => "priced at a time the install did not record".to_string(),
                    }
                )
            } else {
                format!("{why}, and the install recorded none: price unknown")
            }
        }
    };
    let panel: Vec<String> = cfg
        .get("default_panel")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(py_str).collect())
        .unwrap_or_default();
    let mut lines = vec![
        format!("Default panel: {}", panel.join(", ")),
        format!("Prices and context: {source}."),
        String::new(),
        "| alias | command | model | context | $/Mtok in | out | note |".to_string(),
        "|---|---|---|---|---|---|---|".to_string(),
    ];
    for (alias, m) in registry {
        let text = |k: &str| match m.get(k) {
            None | Some(Value::Null) => String::new(),
            Some(v) => py_str(v),
        };
        let mut note = text("note");
        let id = m.get("id").map(py_str).unwrap_or_default();
        let now = listed.and_then(|l| l.get(&id));
        if listed.is_some() && now.is_none() {
            // Withdrawn or no longer tool-capable: a consult with it would fail.
            note = format!(
                "**not in OpenRouter's tool-capable listing now**; install-time values shown. {note}"
            )
            .trim_end()
            .to_string();
        }
        let (context, price_in, price_out) = match now {
            Some(l) => (l.context, l.price_in, l.price_out),
            None => (
                positive_int(m.get("context")),
                number(m.get("price_in")),
                number(m.get("price_out")),
            ),
        };
        let cells = [
            alias.clone(),
            text("command"),
            format!("`{id}`"),
            context_text(context),
            price_text(price_in),
            price_text(price_out),
            note,
        ];
        let cells: Vec<String> = cells.iter().map(|c| cell(c)).collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.push(String::new());
    lines.push(
        "Name a reviewer by its alias, its command or its model id. The id of any other model in OpenRouter's tool-capable listing also works; Anthropic models, routers and presets are refused."
            .to_string(),
    );
    lines.join("\n")
}
