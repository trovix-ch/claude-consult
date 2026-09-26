//! PostToolUse hook for consult and consult_clean: tallies the call and parks a summary.
//!
//! Reads the status records the server appends to every result, appends the call's
//! cost and tokens as one line to this session's log (the status line sums it), and
//! leaves a one-line summary in the state directory for the display hook to draw under
//! Claude's next reply, unless the summary is off or the run is headless. It prints
//! nothing: the summary is for the user's eyes only, and the display hook shows it
//! without it entering Claude's context.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use consult_core::display::{SummaryStyle, load_summary_style};
use consult_core::records::{ParsedRecord, response_text, trailing_records};
use consult_core::util::{py_float_str, py_str, tokens};
use serde_json::Value;

use crate::{headless, log_path, parse_object, pending_path, safe_name};

const PRUNE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Runs the summary hook on one PostToolUse event. Writes only under
/// `<install_dir>/state`, prints nothing, and swallows every failure.
pub fn summary(install_dir: &Path, stdin: &[u8], env: &impl Fn(&str) -> Option<String>) {
    let _ = try_summary(install_dir, stdin, env);
}

fn try_summary(
    install_dir: &Path,
    stdin: &[u8],
    env: &impl Fn(&str) -> Option<String>,
) -> io::Result<()> {
    let Some(data) = parse_object(stdin) else {
        return Ok(());
    };
    let Some(session_id) = data
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| safe_name(s))
    else {
        return Ok(());
    };
    let label = match data.get("tool_name") {
        Some(Value::String(tool)) if tool.ends_with("consult_clean") => "cleanroom",
        _ => "consult",
    };
    let text = data
        .get("tool_response")
        .map(response_text)
        .unwrap_or_default();
    let records = trailing_records(&text);

    let state = consult_core::paths::state_dir(install_dir);
    // Only the state dir itself, never its parents: a missing install is no place to
    // start writing, and a state path that is a file is left alone.
    if let Err(e) = fs::create_dir(&state)
        && !(e.kind() == io::ErrorKind::AlreadyExists && state.is_dir())
    {
        return Err(e);
    }
    if let Some(records) = &records {
        record_call(&log_path(install_dir, session_id), records)?;
    }
    let pending = pending_path(install_dir, session_id);
    let style = load_summary_style(install_dir);
    if headless(env) || style == SummaryStyle::Off {
        // Nothing will draw this call's summary: a headless run (claude -p is sdk-cli,
        // and each SDK has its own sdk-*) never shows one, and with the summary off
        // there is none. An older one still parked would surface under a later reply as
        // if it described this call.
        if let Err(e) = fs::remove_file(&pending)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e);
        }
    } else {
        write_whole(&pending, &summary_line(label, records.as_deref(), style))?;
    }
    prune(&state)
}

fn styled(text: &str, style: SummaryStyle) -> String {
    match style {
        SummaryStyle::Italic => format!("*{text}*"),
        SummaryStyle::Quote => format!("> {text}"),
        SummaryStyle::Dim | SummaryStyle::Off => format!("{DIM}{text}{RESET}"),
    }
}

/// Python truthiness of a JSON value, which decides the name shown for a reviewer.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn reviewer_name(record: &ParsedRecord) -> String {
    ["short", "alias"]
        .iter()
        .filter_map(|key| record.raw.get(*key))
        .find(|value| truthy(value))
        .map(py_str)
        .unwrap_or_else(|| "?".to_string())
}

fn sum_tokens(records: &[ParsedRecord], field: fn(&ParsedRecord) -> u64) -> u64 {
    records
        .iter()
        .fold(0u64, |total, r| total.saturating_add(field(r)))
}

fn sum_cost(records: &[ParsedRecord]) -> f64 {
    records.iter().fold(0.0, |total, r| total + r.cost_usd)
}

/// The one-line summary of a call as the display hook draws it, or the no-signal line
/// when `records` is `None`. `label` is `consult` or `cleanroom`.
pub fn summary_line(label: &str, records: Option<&[ParsedRecord]>, style: SummaryStyle) -> String {
    let Some(records) = records else {
        return styled(
            &format!("{label} · no status lines in the result · no signal"),
            style,
        );
    };
    if records.iter().any(|r| r.status == "failed") {
        return styled(&format!("{label} · failed · no signal"), style);
    }
    let complete = records.iter().filter(|r| r.complete).count();
    let seconds = records
        .iter()
        .map(|r| r.seconds)
        .fold(f64::NEG_INFINITY, f64::max);
    let head = format!(
        "{label} · {complete}/{} complete · {:.4} USD · {} in / {} out · {seconds:.0}s",
        records.len(),
        sum_cost(records),
        tokens(sum_tokens(records, |r| r.tokens_in)),
        tokens(sum_tokens(records, |r| r.tokens_out)),
    );
    let marks: Vec<String> = records
        .iter()
        .map(|r| {
            let name = reviewer_name(r);
            if style != SummaryStyle::Dim {
                format!("{} {name}", if r.complete { "✓" } else { "✗" })
            } else if r.complete {
                format!("{GREEN}✓{RESET}{DIM} {name}")
            } else {
                // Red runs to the end of the name, then dim resumes for the rest.
                format!("{RED}✗ {name}{RESET}{DIM}")
            }
        })
        .collect();
    styled(&format!("{head} · {}", marks.join(" ")), style)
}

/// Replaces a file in one step, so no reader ever sees it half-written.
fn write_whole(path: &Path, text: &str) -> io::Result<()> {
    // The pid keeps hook processes apart, the counter threads of one process.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(name);
    fs::write(&tmp, text.as_bytes())?;
    let mut result = Ok(());
    for _ in 0..5 {
        match fs::rename(&tmp, path) {
            Ok(()) => break,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // Windows refuses to replace a file another process has open, and the
                // status line reads the total on every refresh.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    result
}

/// Adds one whole line at the end of a file, however many processes append at once.
///
/// The C runtime's append mode on Windows seeks to the end and then writes, so two
/// writers can land on the same offset and one line is lost: measured 2026-09-25, 16
/// processes appending 300 lines each through os.open with O_APPEND kept about 4,450 of
/// 4,800. A handle opened for appending only makes Windows itself put each write at the
/// end of the file (all 4,800 kept). `OpenOptions::append` opens exactly such a handle
/// (FILE_APPEND_DATA without FILE_WRITE_DATA), and O_APPEND elsewhere; the line goes out
/// in one write so it is never interleaved.
fn append_line(path: &Path, line: &str) -> io::Result<()> {
    let mut data = String::with_capacity(line.len() + 1);
    data.push_str(line);
    data.push('\n');
    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(data.as_bytes())
}

/// One line per call, never a running total rewritten in place: consults that finish
/// together would each read the same old total and all but one of them would be lost.
fn record_call(path: &Path, records: &[ParsedRecord]) -> io::Result<()> {
    // Written as json.dumps wrote it, an integer total staying an integer.
    let integral = records
        .iter()
        .all(|r| matches!(r.raw.get("cost_usd"), Some(Value::Number(n)) if !n.is_f64()));
    let cost = if integral {
        records
            .iter()
            .map(|r| match r.raw.get("cost_usd") {
                Some(Value::Number(n)) => n
                    .as_i64()
                    .map(i128::from)
                    .or_else(|| n.as_u64().map(i128::from))
                    .unwrap_or(0),
                _ => 0,
            })
            .sum::<i128>()
            .to_string()
    } else {
        py_float_str(sum_cost(records))
    };
    append_line(
        path,
        &format!(
            "{{\"cost\": {cost}, \"tin\": {}, \"tout\": {}}}",
            sum_tokens(records, |r| r.tokens_in),
            sum_tokens(records, |r| r.tokens_out),
        ),
    )
}

/// Removes state files older than a week; one that cannot be checked or removed is
/// skipped.
fn prune(state: &Path) -> io::Result<()> {
    let Some(cutoff) = SystemTime::now().checked_sub(PRUNE_AFTER) else {
        return Ok(());
    };
    for entry in fs::read_dir(state)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let _ = (|| -> io::Result<()> {
            let meta = fs::metadata(&path)?;
            if meta.is_file() && meta.modified()? < cutoff {
                fs::remove_file(&path)?;
            }
            Ok(())
        })();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_appends_keep_every_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.jsonl");
        let (writers, lines) = (16, 300);
        let barrier = std::sync::Barrier::new(writers);
        std::thread::scope(|scope| {
            for w in 0..writers {
                let (path, barrier) = (&path, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for i in 0..lines {
                        append_line(path, &format!("{{\"writer\": {w}, \"line\": {i}}}"))
                            .expect("append");
                    }
                });
            }
        });
        let text = fs::read_to_string(&path).expect("read");
        let mut seen: Vec<&str> = text.lines().collect();
        assert_eq!(seen.len(), writers * lines);
        assert!(text.ends_with('\n'));
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), writers * lines, "a line was lost or torn");
        for line in seen {
            assert!(serde_json::from_str::<Value>(line).is_ok(), "{line}");
        }
    }

    #[test]
    fn write_whole_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.pending");
        write_whole(&path, "one").expect("first");
        write_whole(&path, "two").expect("second");
        assert_eq!(fs::read_to_string(&path).expect("read"), "two");
        assert_eq!(fs::read_dir(dir.path()).expect("list").count(), 1);
    }

    #[test]
    fn names_fall_back_as_python_or_does() {
        let text = concat!(
            r#"<!-- consult-result v1 {"alias":"a1","short":"","status":"ok","complete":true,"cost_usd":0,"tokens_in":1,"tokens_out":2,"seconds":1} -->"#,
            "\n",
            r#"<!-- consult-result v1 {"alias":null,"short":null,"status":"ok","complete":false,"cost_usd":1,"tokens_in":1,"tokens_out":2,"seconds":2.5} -->"#,
        );
        let records = trailing_records(text).expect("records");
        assert_eq!(
            summary_line("consult", Some(&records), SummaryStyle::Quote),
            "> consult · 1/2 complete · 1.0000 USD · 2 in / 4 out · 2s · ✓ a1 ✗ ?"
        );
    }

    #[test]
    fn integer_costs_stay_integers_in_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.jsonl");
        let int = r#"<!-- consult-result v1 {"short":"x","status":"ok","complete":true,"cost_usd":0,"tokens_in":1,"tokens_out":2,"seconds":1} -->"#;
        let float = r#"<!-- consult-result v1 {"short":"y","status":"ok","complete":true,"cost_usd":0.5,"tokens_in":3,"tokens_out":4,"seconds":1} -->"#;
        record_call(&path, &trailing_records(int).expect("int")).expect("int");
        record_call(
            &path,
            &trailing_records(&format!("{int}\n{float}")).expect("mixed"),
        )
        .expect("mixed");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "{\"cost\": 0, \"tin\": 1, \"tout\": 2}\n{\"cost\": 0.5, \"tin\": 4, \"tout\": 6}\n"
        );
    }
}
