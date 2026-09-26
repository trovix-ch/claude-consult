//! Per-session consult totals, as the summary hook leaves them in `state/`.
//!
//! Each consult appends one line `{"cost": .., "tin": .., "tout": ..}` to
//! `<session_id>.jsonl`; the display hook's `<session_id>.pending` sits beside it until
//! drawn. Read the way the status line reads them: a damaged line costs only itself.

use std::fs;
use std::io;
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;

/// One session's totals.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// The session id (the file name without `.jsonl`).
    pub id: String,
    /// Consult calls recorded.
    pub calls: u64,
    /// Their cost, USD.
    pub cost_usd: f64,
    /// Tokens sent.
    pub tokens_in: u64,
    /// Tokens received.
    pub tokens_out: u64,
    /// When the log was last written.
    pub modified: Option<SystemTime>,
}

/// Every session's totals in `state_dir`, most recently written first. A missing dir is
/// no sessions.
pub fn list_sessions(state_dir: &Path) -> io::Result<Vec<Session>> {
    let entries = match fs::read_dir(state_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if !safe_name(id) || !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).unwrap_or_default();
        let mut s = Session {
            id: id.to_string(),
            calls: 0,
            cost_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            modified: entry.metadata().and_then(|m| m.modified()).ok(),
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Some((cost, tin, tout)) = call(line) {
                s.calls += 1;
                s.cost_usd += cost;
                s.tokens_in = s.tokens_in.saturating_add(tin);
                s.tokens_out = s.tokens_out.saturating_add(tout);
            }
        }
        out.push(s);
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Deletes a session's files (`.jsonl` and `.pending`); the number removed.
///
/// An id that is not a plain token is refused: it becomes a file name, and `..` or a
/// separator would reach outside the state dir.
pub fn delete_session(state_dir: &Path, id: &str) -> io::Result<usize> {
    if !safe_name(id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a session id: {id}"),
        ));
    }
    let mut removed = 0;
    for ext in ["jsonl", "pending"] {
        match fs::remove_file(state_dir.join(format!("{id}.{ext}"))) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// The hooks' rule for a session id that may become a file name.
pub fn safe_name(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One call's cost and tokens, when the line is a whole, well-typed call record.
fn call(line: &str) -> Option<(f64, u64, u64)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let call = value.as_object()?;
    let cost = match call.get("cost")? {
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    Some((
        cost,
        call.get("tin")?.as_u64()?,
        call.get("tout")?.as_u64()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_skip_damaged_lines_and_deletion_refuses_paths() {
        let dir = tempfile::tempdir().expect("tmp");
        fs::write(
            dir.path().join("abc.jsonl"),
            "{\"cost\": 0.5, \"tin\": 10, \"tout\": 2}\nnot json\n{\"cost\": 1, \"tin\": 5, \"tout\": 1}\n{\"cost\": true, \"tin\": 1, \"tout\": 1}\n",
        )
        .expect("write");
        fs::write(dir.path().join("abc.pending"), "x").expect("write");
        fs::write(dir.path().join("notes.txt"), "x").expect("write");
        let s = list_sessions(dir.path()).expect("list");
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].calls, s[0].tokens_in, s[0].tokens_out), (2, 15, 3));
        assert!((s[0].cost_usd - 1.5).abs() < 1e-9);
        assert!(delete_session(dir.path(), "../x").is_err());
        assert_eq!(delete_session(dir.path(), "abc").expect("delete"), 2);
        assert!(list_sessions(dir.path()).expect("list").is_empty());
        assert!(
            list_sessions(&dir.path().join("missing"))
                .expect("none")
                .is_empty()
        );
    }
}
