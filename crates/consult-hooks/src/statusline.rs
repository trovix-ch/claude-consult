//! Status line: what consult has cost in this Claude Code session so far.
//!
//! The summary hook appends one line per consult to the session's log; this sums them
//! and returns the total dimmed, or an empty line before the session's first consult.
//! Also meant to be called from a user's own status line script with the same stdin, to
//! add the total to it.

use std::fs;
use std::path::Path;

use consult_core::util::{splitlines, tokens};
use serde_json::Value;

use crate::{log_path, parse_object, safe_name};

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Runs the status line on one event and returns the line to print, without its newline.
///
/// Any failure is the empty line: a status line is no place for an error, and the total
/// is a convenience, never worth a broken one.
pub fn statusline(install_dir: &Path, stdin: &[u8]) -> String {
    line(install_dir, stdin).unwrap_or_default()
}

fn line(install_dir: &Path, stdin: &[u8]) -> Option<String> {
    let data = parse_object(stdin)?;
    let session_id = data
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| safe_name(s))?;
    let bytes = fs::read(log_path(install_dir, session_id)).ok()?;
    let log = String::from_utf8_lossy(&bytes);
    let (mut calls, mut cost, mut tin, mut tout) = (0u64, 0.0f64, 0u64, 0u64);
    for entry in splitlines(&log) {
        // A damaged line, or one caught half-written, costs only itself: the rest of the
        // session's calls still count.
        let Some((c, i, o)) = call(entry) else {
            continue;
        };
        calls += 1;
        cost += c;
        tin = tin.saturating_add(i);
        tout = tout.saturating_add(o);
    }
    if calls == 0 {
        return None;
    }
    let plural = if calls != 1 { "s" } else { "" };
    Some(format!(
        "{DIM}consult this session · {calls} call{plural} · {cost:.4} USD · {} in / {} out{RESET}",
        tokens(tin),
        tokens(tout)
    ))
}

/// One call's cost and tokens, when the line is a whole, well-typed call record.
fn call(entry: &str) -> Option<(f64, u64, u64)> {
    let value: Value = serde_json::from_str(entry).ok()?;
    let call = value.as_object()?;
    // serde_json has no NaN or Infinity, so any number it parsed is finite; a count is
    // a non-negative integer, never a float or a bool.
    let cost = match call.get("cost")? {
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    let tin = call.get("tin")?.as_u64()?;
    let tout = call.get("tout")?.as_u64()?;
    Some((cost, tin, tout))
}
