//! The machine-readable status records that close every consult result.
//!
//! One line per reviewer, so what reads the result (a hook, a script) never has to
//! infer the outcome from prose. Parsers take these only from the block at the very
//! end, which a reviewer's text cannot reach.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::util::py_str;

/// Anything starting with this belongs to the trailing block, so a damaged record voids
/// the block instead of silently shortening it.
pub const RECORD_PREFIX: &str = "<!-- consult-result";

/// One status record, fields in the order they are written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The reviewer's alias; null when the whole call failed.
    pub alias: Option<String>,
    /// The short name the display uses; null when the whole call failed.
    pub short: Option<String>,
    /// `ok`, `incomplete`, `empty`, `error`, or `failed`.
    pub status: String,
    /// True only for `ok`.
    pub complete: bool,
    /// The last `finish_reason`, `"missing"` if none was given, null if nothing answered.
    pub finish: Option<String>,
    /// A step, time or cost budget cut the investigation short.
    pub capped: bool,
    /// Tool calls made.
    pub tool_calls: u64,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Prompt tokens.
    pub tokens_in: u64,
    /// Completion tokens.
    pub tokens_out: u64,
    /// Wall-clock seconds.
    pub seconds: f64,
}

impl Record {
    /// The record of a call that failed before any reviewer ran.
    pub fn failed() -> Self {
        Self {
            alias: None,
            short: None,
            status: "failed".to_string(),
            complete: false,
            finish: None,
            capped: false,
            tool_calls: 0,
            cost_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            seconds: 0.0,
        }
    }

    /// The record as its line: `<!-- consult-result v1 {compact json} -->`.
    pub fn line(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        format!("<!-- consult-result v1 {json} -->")
    }
}

/// A record read back from a result, holding the fields a reader relies on.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedRecord {
    /// `alias`, when not null.
    pub alias: Option<String>,
    /// `short`, when not null.
    pub short: Option<String>,
    /// `status`.
    pub status: String,
    /// `complete`.
    pub complete: bool,
    /// `cost_usd`.
    pub cost_usd: f64,
    /// `tokens_in`.
    pub tokens_in: u64,
    /// `tokens_out`.
    pub tokens_out: u64,
    /// `seconds`.
    pub seconds: f64,
    /// Every field as written.
    pub raw: Map<String, Value>,
}

fn is_number(v: Option<&Value>) -> bool {
    // serde_json has no NaN or Infinity, so every number it parsed is finite; bools are
    // not numbers here, as they are in Python.
    matches!(v, Some(Value::Number(_)))
}

fn is_count(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Number(n)) if n.is_u64())
}

/// Whether a parsed record has every field a reader relies on, with the right types.
pub fn well_formed(record: &Value) -> bool {
    let Some(r) = record.as_object() else {
        return false;
    };
    r.get("status").is_some_and(Value::is_string)
        && r.get("complete").is_some_and(Value::is_boolean)
        && matches!(r.get("short"), Some(Value::String(_) | Value::Null))
        && is_number(r.get("cost_usd"))
        && is_number(r.get("seconds"))
        && is_count(r.get("tokens_in"))
        && is_count(r.get("tokens_out"))
}

fn record_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^<!-- consult-result v1 (\{.*\}) -->$").expect("static regex"))
}

/// The records at the very end of the text, or `None` when there is no signal.
///
/// Only the trailing block counts: a reviewer quoting the record format in its review
/// must not be read as a status. One bad line voids the whole block, since a partial
/// block would misstate how many reviewers ran.
pub fn trailing_records(text: &str) -> Option<Vec<ParsedRecord>> {
    let mut block = Vec::new();
    for line in text.trim_end().split('\n').rev() {
        let line = line.trim_end();
        if !line.starts_with(RECORD_PREFIX) {
            break;
        }
        block.push(line);
    }
    let mut records = Vec::new();
    for line in block.into_iter().rev() {
        let json = record_regex().captures(line)?.get(1)?.as_str();
        let value: Value = serde_json::from_str(json).ok()?;
        if !well_formed(&value) {
            return None;
        }
        let raw = value.as_object()?.clone();
        let text_of = |k: &str| match raw.get(k) {
            None | Some(Value::Null) => None,
            Some(v) => Some(py_str(v)),
        };
        records.push(ParsedRecord {
            alias: text_of("alias"),
            short: text_of("short"),
            status: raw.get("status")?.as_str()?.to_string(),
            complete: raw.get("complete")?.as_bool()?,
            cost_usd: raw.get("cost_usd")?.as_f64()?,
            tokens_in: raw.get("tokens_in")?.as_u64()?,
            tokens_out: raw.get("tokens_out")?.as_u64()?,
            seconds: raw.get("seconds")?.as_f64()?,
            raw,
        });
    }
    (!records.is_empty()).then_some(records)
}

/// The tool's text, from whichever shape Claude Code hands it over in: a string, a list
/// of content blocks, or an object with `text`, `result` or `content`.
pub fn response_text(response: &Value) -> String {
    match response {
        Value::String(s) => {
            // A server still returning structured output arrives as the JSON string
            // {"result": "..."}. A consult result itself is prose, never a JSON
            // document, so parsing here only ever unwraps.
            match serde_json::from_str::<Value>(s) {
                Ok(inner @ Value::Array(_)) => response_text(&inner),
                Ok(inner @ Value::Object(_)) if inner.get("result").is_some() => {
                    response_text(&inner)
                }
                _ => s.clone(),
            }
        }
        Value::Array(blocks) => blocks
            .iter()
            .map(response_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => ["text", "result", "content"]
            .iter()
            .find_map(|k| map.get(*k))
            .map(response_text)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn record_line_is_compact_and_ordered() {
        assert_eq!(
            Record::failed().line(),
            r#"<!-- consult-result v1 {"alias":null,"short":null,"status":"failed","complete":false,"finish":null,"capped":false,"tool_calls":0,"cost_usd":0.0,"tokens_in":0,"tokens_out":0,"seconds":0.0} -->"#
        );
    }

    #[test]
    fn trailing_block_is_read() {
        let result = body() + &panel().join("\n") + "\n";
        let recs = trailing_records(&result).expect("records");
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].short.as_deref(), Some("deepseek"));
        assert_eq!(recs[2].status, "incomplete");
        assert!(!recs[2].complete);
        assert_eq!(recs[1].tokens_in, 690_720);
        // Trailing blank lines are allowed.
        assert!(trailing_records(&(result.clone() + "\n  \n")).is_some());
        // Prose after the records voids them.
        assert!(trailing_records(&(result + "\nOne more paragraph.\n")).is_none());
        assert!(trailing_records(&body()).is_none());
        assert!(trailing_records("").is_none());
    }

    #[test]
    fn malformed_trailing_block_is_no_signal() {
        let p = panel();
        let cases: Vec<(&str, Vec<String>)> = vec![
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
                    p[2].replace(r#""cost_usd":0.0372,"#, ""),
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
                    p[2].replace(r#""cost_usd":0.0372"#, r#""cost_usd":NaN"#),
                ],
            ),
            (
                "float count",
                vec![
                    p[0].clone(),
                    p[1].clone(),
                    p[2].replace(r#""tokens_out":13143"#, r#""tokens_out":13143.0"#),
                ],
            ),
        ];
        for (name, lines) in cases {
            assert_ne!(lines, p, "{name}");
            assert!(
                trailing_records(&(body() + &lines.join("\n"))).is_none(),
                "{name}"
            );
        }
    }

    #[test]
    fn well_formed_types() {
        let ok = json!({"status": "ok", "complete": true, "short": null, "cost_usd": 0, "seconds": 1.5, "tokens_in": 0, "tokens_out": 3});
        assert!(well_formed(&ok));
        let mut bad = ok.clone();
        bad["complete"] = json!(1);
        assert!(!well_formed(&bad));
        let mut bad = ok.clone();
        bad["tokens_in"] = json!(-1);
        assert!(!well_formed(&bad));
        let mut bad = ok;
        bad["cost_usd"] = json!(true);
        assert!(!well_formed(&bad));
        assert!(!well_formed(&json!([1])));
    }

    #[test]
    fn response_shapes() {
        let text = "hello";
        assert_eq!(
            response_text(&json!([{"type": "text", "text": text}])),
            text
        );
        assert_eq!(response_text(&json!(text)), text);
        assert_eq!(response_text(&json!(r#"{"result": "hello"}"#)), text);
        assert_eq!(
            response_text(&json!({"content": [{"text": "a"}, {"text": "b"}]})),
            "a\nb"
        );
        assert_eq!(response_text(&json!(null)), "");
        assert_eq!(response_text(&json!({"x": 1})), "");
        assert_eq!(response_text(&json!(r#"{"x": 1}"#)), r#"{"x": 1}"#);
    }
}
