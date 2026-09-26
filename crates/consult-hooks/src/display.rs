//! MessageDisplay hook: draws a parked consult summary under Claude's reply.
//!
//! The summary hook parks a summary when a consult returns; this appends it to the final
//! flush of Claude's next reply and deletes it. Display only: what Claude stored, and
//! what it reads on the next turn, is unchanged.
//!
//! Claude Code runs this on every flush of every reply in every session, so the common
//! path, a flush that is not final, must cost little more than starting the process:
//! the event is scanned without building the reply text, and nothing on disk is touched.

use std::borrow::Cow;
use std::fs;
use std::path::Path;

use consult_core::util::json_ascii;
use serde::Deserialize;
use serde_json::Value;

use crate::{headless, parse_object, pending_path, safe_name};

/// The two fields every flush is judged on. Every other field, the reply's text among
/// them, is skipped by the parser without being stored.
#[derive(Deserialize)]
struct Flush<'a> {
    /// Anything but a JSON `true` fails to deserialize, which is "not final" too.
    #[serde(rename = "final", default)]
    is_final: Option<bool>,
    #[serde(borrow, default)]
    session_id: Option<Cow<'a, str>>,
}

/// Runs the display hook on one MessageDisplay event and returns the JSON line to print
/// (without its newline), or `None` when this flush shows nothing.
pub fn display(
    install_dir: &Path,
    stdin: &[u8],
    env: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    // A headless run prints display output to stdout, where a summary would end up inside
    // whatever a script captures.
    if headless(env) {
        return None;
    }
    let text = String::from_utf8_lossy(stdin);
    // serde would also read a struct from a JSON array, which the hook must not accept.
    if !text.trim_start().starts_with('{') {
        return None;
    }
    let flush: Flush = serde_json::from_str(&text).ok()?;
    if flush.is_final != Some(true) {
        return None;
    }
    let session_id = flush.session_id.filter(|s| safe_name(s))?;

    let pending = pending_path(install_dir, &session_id);
    let bytes = fs::read(&pending).ok()?;
    let summary = String::from_utf8(bytes).ok()?;
    // Shown only once it is gone: a summary that could not be removed would otherwise
    // repeat under every reply that follows.
    fs::remove_file(&pending).ok()?;
    if summary.is_empty() {
        return None;
    }
    // Read as text, as the Python hook did, so a hand-written CRLF reads as one newline.
    let summary = if summary.contains('\r') {
        summary.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        summary
    };

    // Only now is the reply's text worth building.
    let delta = parse_object(stdin)
        .and_then(|mut data| match data.remove("delta") {
            Some(Value::String(s)) => Some(s),
            _ => None,
        })
        .unwrap_or_default();
    let mut content = delta;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push('\n');
    content.push_str(&summary);
    content.push('\n');
    let content = serde_json::to_string(&content).ok()?;
    // Every non-ASCII character escaped, as json.dumps does, so no code page can mangle it.
    Some(format!(
        "{{\"hookSpecificOutput\": {{\"hookEventName\": \"MessageDisplay\", \"displayContent\": {}}}}}",
        json_ascii(&content)
    ))
}
