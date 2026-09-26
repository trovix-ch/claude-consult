//! Small formatting and parsing helpers shared by every module.
//!
//! Several of these reproduce a Python behaviour exactly (`repr`, `str.splitlines`,
//! `float()`, `json.dumps` defaults), because the strings they build are compared
//! byte for byte by the ported tests and read by users who saw the Python output.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use serde_json::Value;

/// Seconds since an arbitrary process-wide epoch, as a float.
///
/// Every live timestamp in the panel (`Progress::started`, `ReviewerState::active`)
/// is in this unit, so tests can place them at fixed values and pass `now` explicitly.
pub fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// A token count for a one-line display: exact below a thousand, then `k`, then `M`.
pub fn tokens(n: u64) -> String {
    // Below a thousand "0k" would hide a small call entirely, so the exact count.
    if n < 1_000 {
        return n.to_string();
    }
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else {
        format!("{:.0}k", n as f64 / 1e3)
    }
}

/// USD with two decimals, except that a paid amount never reads as free.
pub fn usd(x: f64) -> String {
    // A paid model must not read as free.
    if 0.0 < x && x < 0.005 {
        "<0.01".to_string()
    } else {
        format!("{x:.2}")
    }
}

/// A byte count as `512B`, `1.5KB`, `3.2MB`, `1.0GB`.
pub fn human_size(n: u64) -> String {
    let mut x = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if x < 1024.0 || unit == "GB" {
            return if unit == "B" {
                format!("{x:.0}B")
            } else {
                format!("{x:.1}{unit}")
            };
        }
        x /= 1024.0;
    }
    format!("{x}B")
}

/// A value made safe for one markdown table cell.
pub fn cell(value: &str) -> String {
    value.replace('\n', " ").replace('|', "\\|")
}

/// An integer with thousands separators, as Python's `f"{n:,}"`.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// An ISO timestamp as `YYYY-MM-DD HH:MM UTC`; the text itself when it does not parse,
/// `None` when there is nothing to show.
pub fn when(iso: Option<&str>) -> Option<String> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
    let iso = iso?.trim();
    if iso.is_empty() {
        return None;
    }
    let fmt = |t: DateTime<Utc>| t.format("%Y-%m-%d %H:%M UTC").to_string();
    if let Ok(t) = DateTime::parse_from_rfc3339(iso) {
        return Some(fmt(t.with_timezone(&Utc)));
    }
    let spaced = iso.replacen(' ', "T", 1);
    for pattern in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M%:z"] {
        if let Ok(t) = DateTime::parse_from_str(&spaced, pattern) {
            return Some(fmt(t.with_timezone(&Utc)));
        }
    }
    // No offset: read as UTC, as the install writes it.
    for pattern in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(t) = NaiveDateTime::parse_from_str(&spaced, pattern) {
            return Some(fmt(t.and_utc()));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(iso, "%Y-%m-%d")
        && let Some(t) = d.and_hms_opt(0, 0, 0)
    {
        return Some(fmt(t.and_utc()));
    }
    Some(iso.to_string())
}

/// The first `n` characters (not bytes) of `s`, as Python's `s[:n]`.
pub fn clip_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The last `n` characters of `s`, as Python's `s[-n:]`.
pub fn tail_chars(s: &str, n: usize) -> &str {
    let count = s.chars().count();
    if count <= n {
        return s;
    }
    match s.char_indices().nth(count - n) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

/// Character count, as Python's `len(s)`.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// `x` rounded to `places` decimals, as Python's `round(x, places)`.
pub fn round_to(x: f64, places: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.places$}").parse().unwrap_or(x)
}

/// Lines as Python's `str.splitlines()`: every line boundary Python knows, and no
/// empty last line for a trailing separator.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let boundary = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        out.push(&s[start..i]);
        let mut end = i + c.len_utf8();
        if c == '\r'
            && let Some(&(j, '\n')) = chars.peek()
        {
            chars.next();
            end = j + 1;
        }
        start = end;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// A string as Python's `repr()` shows it, quotes included.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {
                let n = c as u32;
                if n < 0x100 {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n < 0x10000 {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// A float as Python's `str()` shows it: `1.0`, `12.3`.
pub fn py_float_str(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_string();
    }
    format!("{x:?}")
}

fn py_number(n: &serde_json::Number) -> String {
    match n.as_f64() {
        Some(x) if !n.is_i64() && !n.is_u64() => py_float_str(x),
        _ => n.to_string(),
    }
}

/// A JSON value as Python's `str()` shows it once `json.loads` has made it a Python object.
pub fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => py_repr_value(other),
    }
}

/// A JSON value as Python's `repr()` shows it once `json.loads` has made it a Python object.
pub fn py_repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => py_number(n),
        Value::String(s) => py_repr(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr_value).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr(k), py_repr_value(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn push_json_string(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7e => push_u_escape(out, c),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_u_escape(out: &mut String, c: char) {
    let mut buf = [0u16; 2];
    for unit in c.encode_utf16(&mut buf) {
        out.push_str(&format!("\\u{unit:04x}"));
    }
}

/// A JSON value as Python's `json.dumps` writes it with its defaults: `", "` and `": "`
/// separators (or `","` and `":"` when `compact`) and every non-ASCII character escaped.
pub fn py_dumps(value: &Value, compact: bool) -> String {
    let mut out = String::new();
    write_py_json(&mut out, value, compact);
    out
}

fn write_py_json(out: &mut String, value: &Value, compact: bool) {
    let (item_sep, key_sep) = if compact { (",", ":") } else { (", ", ": ") };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_json_string(out, s, true),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                write_py_json(out, item, compact);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(item_sep);
                }
                push_json_string(out, k, true);
                out.push_str(key_sep);
                write_py_json(out, v, compact);
            }
            out.push('}');
        }
    }
}

/// Escapes every non-ASCII character (and DEL) of serialized JSON as `\uXXXX`, which is
/// what Python's `json.dumps` does by default. Valid on any serde_json output, because
/// such characters can only occur inside strings there.
pub fn json_ascii(serialized: &str) -> String {
    let mut out = String::with_capacity(serialized.len());
    for c in serialized.chars() {
        if (c as u32) > 0x7e {
            push_u_escape(&mut out, c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Python's `float(value)` on a value `json.loads` produced, kept only when finite and
/// not negative. Anything else is "unknown".
pub fn number(value: Option<&Value>) -> Option<f64> {
    let x = py_float_of(value?)?;
    (x.is_finite() && x >= 0.0).then_some(x)
}

/// Whether Python's `float(value)` is negative.
pub fn negative(value: Option<&Value>) -> bool {
    value.and_then(py_float_of).is_some_and(|x| x < 0.0)
}

fn py_float_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Python's `int(value)` on a value `json.loads` produced, kept only when positive.
pub fn positive_int(value: Option<&Value>) -> Option<u64> {
    let n: i128 = match value? {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i128::from(i)
            } else if let Some(u) = n.as_u64() {
                i128::from(u)
            } else {
                let x = n.as_f64()?;
                if !x.is_finite() {
                    return None;
                }
                x.trunc() as i128
            }
        }
        Value::String(s) => s.trim().parse::<i128>().ok()?,
        Value::Bool(b) => i128::from(*b),
        _ => return None,
    };
    if n > 0 { u64::try_from(n).ok() } else { None }
}

/// The text without a leading UTF-8 byte order mark.
///
/// Notepad and PowerShell 5.1 both write one on a hand edit, and failing on it would
/// discard whatever the file says.
pub fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// A path with forward slashes, as Python's `Path.as_posix()`.
pub fn as_posix(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// `~` and `~/...` expanded to the home directory, as Python's `expanduser()`.
pub fn expand_user(raw: &str) -> PathBuf {
    if (raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        let rest = raw[1..].trim_start_matches(['/', '\\']);
        return if rest.is_empty() {
            home
        } else {
            home.join(rest)
        };
    }
    PathBuf::from(raw)
}

/// An absolute path with `.` and `..` collapsed lexically, never touching the disk.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never above the root: "/.." is "/", as the OS resolves it.
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir | Component::Prefix(_))
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A path made absolute and resolved as far as it exists, following symlinks, as
/// Python's non-strict `Path.resolve()`. The part that does not exist is appended as
/// written, after `.` and `..` were collapsed.
pub fn resolve_lenient(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    };
    let normal = normalize_lexically(&absolute);
    if let Ok(real) = dunce::canonicalize(&normal) {
        return real;
    }
    let mut tail = Vec::new();
    let mut cur = normal.as_path();
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(real) = dunce::canonicalize(parent) {
            let mut out = real;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        cur = parent;
    }
    normal
}

/// Replaces a file in one step, so no reader ever sees it half-written.
///
/// Written with `\n` line endings and UTF-8, whatever the platform.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, path)
}

/// Reads a text file leniently: a BOM dropped, invalid UTF-8 replaced.
pub fn read_text_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(strip_bom(&String::from_utf8_lossy(&bytes)).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_format() {
        assert_eq!(tokens(312), "312");
        assert_eq!(tokens(0), "0");
        assert_eq!(tokens(999_999), "1000k");
        assert_eq!(tokens(1_000_000), "1.00M");
        assert_eq!(tokens(56_425), "56k");
    }

    #[test]
    fn usd_never_reads_free() {
        assert_eq!(usd(0.001), "<0.01");
        assert_eq!(usd(0.0), "0.00");
        assert_eq!(usd(1.10142), "1.10");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5KB");
        assert_eq!(human_size(4_000_000), "3.8MB");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(100_000), "100,000");
        assert_eq!(thousands(1_048_576), "1,048,576");
    }

    #[test]
    fn install_time_stamps_read_as_utc() {
        for (iso, shown) in [
            ("2026-09-25T12:00:00Z", Some("2026-09-25 12:00 UTC")),
            (
                "2026-09-25T12:00:00.123456+00:00",
                Some("2026-09-25 12:00 UTC"),
            ),
            ("2026-09-25T14:00:00+02:00", Some("2026-09-25 12:00 UTC")),
            ("2026-09-25T12:00:00", Some("2026-09-25 12:00 UTC")),
            ("yesterday", Some("yesterday")),
            ("", None),
        ] {
            assert_eq!(when(Some(iso)).as_deref(), shown, "{iso}");
        }
        assert_eq!(when(None), None);
    }

    #[test]
    fn splitlines_like_python() {
        assert_eq!(splitlines("a\nb\r\nc\rd\n"), vec!["a", "b", "c", "d"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("\n\n"), vec!["", ""]);
        assert_eq!(splitlines("x\u{2028}y"), vec!["x", "y"]);
    }

    #[test]
    fn repr_like_python() {
        assert_eq!(py_repr("nope"), "'nope'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\nb"), "'a\\nb'");
        assert_eq!(
            py_repr_value(&json!(["-n", 10, true, null])),
            "['-n', 10, True, None]"
        );
    }

    #[test]
    fn dumps_like_python() {
        assert_eq!(py_dumps(&json!(["a", "b"]), false), r#"["a", "b"]"#);
        assert_eq!(py_dumps(&json!(["a", "b"]), true), r#"["a","b"]"#);
        assert_eq!(
            py_dumps(&json!({"code": 500, "m": "é"}), false),
            "{\"code\": 500, \"m\": \"\\u00e9\"}"
        );
        assert_eq!(json_ascii("\"é😀\""), "\"\\u00e9\\ud83d\\ude00\"");
    }

    #[test]
    fn numbers_like_python() {
        assert_eq!(number(Some(&json!("0.000001"))), Some(0.000001));
        assert_eq!(number(Some(&json!("abc"))), None);
        assert_eq!(number(Some(&json!(""))), None);
        assert_eq!(number(Some(&json!("-1"))), None);
        assert_eq!(number(Some(&json!("inf"))), None);
        assert_eq!(number(None), None);
        assert!(negative(Some(&json!("-1"))));
        assert!(!negative(Some(&json!("x"))));
        assert_eq!(positive_int(Some(&json!("big"))), None);
        assert_eq!(positive_int(Some(&json!(1_048_576))), Some(1_048_576));
        assert_eq!(positive_int(Some(&json!("131072"))), Some(131_072));
        assert_eq!(positive_int(Some(&json!(0))), None);
    }

    #[test]
    fn rounding() {
        assert_eq!(round_to(0.05000000001, 4), 0.05);
        assert_eq!(round_to(12.34, 1), 12.3);
    }

    #[test]
    fn clipping_counts_characters() {
        assert_eq!(clip_chars("héllo", 2), "hé");
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(clip_chars("ab", 5), "ab");
    }
}
