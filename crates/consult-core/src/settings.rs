//! Claude Code's `settings.json`: the key, the timeout floors, our hooks and status line.
//!
//! Every merge keeps the user's own entries, their order and their key order, and
//! touches only commands in exactly the form this installer writes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::display::SummaryStyle;
use crate::error::GenerateError;
use crate::key::KEY_VAR;
use crate::util::{as_posix, strip_bom, write_atomic};

/// The PostToolUse matcher for our summary hook.
pub const CONSULT_TOOLS: &str = "mcp__openrouter__consult|mcp__openrouter__consult_clean";

/// A panel runs for up to 25 minutes (`REVIEWER_TIMEOUT`) and the HTTP transport aborts
/// a call after 5 idle minutes by default. Lower values than these make long consults
/// fail, so they are raised but never lowered.
pub const REQUIRED_ENV_FLOORS: [(&str, i128); 2] = [
    ("MCP_TOOL_TIMEOUT", 2_400_000),
    ("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", 1_800_000),
];

/// One of the commands the installer registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookKind {
    /// PostToolUse on consult: tallies the call and parks a summary.
    Summary,
    /// MessageDisplay: draws the parked summary.
    Display,
    /// The status line.
    Statusline,
}

impl HookKind {
    /// The `hook` subcommand's argument.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Display => "display",
            Self::Statusline => "statusline",
        }
    }

    /// The script the Python install ran for it.
    pub fn legacy_script(self) -> &'static str {
        match self {
            Self::Summary => "summary_hook.py",
            Self::Display => "display_hook.py",
            Self::Statusline => "statusline.py",
        }
    }

    fn from_word(word: &str) -> Option<Self> {
        match word.to_lowercase().as_str() {
            "summary" | "summary_hook.py" => Some(Self::Summary),
            "display" | "display_hook.py" => Some(Self::Display),
            "statusline" | "statusline.py" => Some(Self::Statusline),
            _ => None,
        }
    }
}

/// Reads settings.json. Missing or blank is empty; invalid JSON is an error, never
/// rewritten: that would destroy whatever the user has in it.
pub fn load_settings(path: &Path) -> Result<Map<String, Value>, GenerateError> {
    if !path.is_file() {
        return Ok(Map::new());
    }
    let bytes = std::fs::read(path).map_err(|e| GenerateError::io(path, e))?;
    let raw = String::from_utf8(bytes).map_err(|e| {
        GenerateError::invalid(format!(
            "{} is not valid JSON ({e}); fix it and re-run",
            path.display()
        ))
    })?;
    let raw = strip_bom(&raw);
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(raw) {
        Err(e) => Err(GenerateError::invalid(format!(
            "{} is not valid JSON ({e}); fix it and re-run",
            path.display()
        ))),
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(GenerateError::invalid(format!(
            "{} does not contain a JSON object",
            path.display()
        ))),
    }
}

/// Writes settings.json atomically, two-space indented, non-ASCII kept as is.
pub fn save_settings(path: &Path, data: &Map<String, Value>) -> Result<(), GenerateError> {
    let text = serde_json::to_string_pretty(data)
        .map_err(|e| GenerateError::invalid(e.to_string()))?
        + "\n";
    write_atomic(path, &text).map_err(|e| GenerateError::io(path, e))
}

fn env_int(v: Option<&Value>) -> i128 {
    match v {
        None => 0,
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                0
            } else {
                s.parse().unwrap_or(0)
            }
        }
        Some(Value::Number(n)) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
            .unwrap_or(0),
        _ => 0,
    }
}

/// Sets the key (when given) and raises the timeout floors. Returns the variables changed.
pub fn merge_env(
    data: &mut Map<String, Value>,
    path: &Path,
    key: Option<&str>,
) -> Result<Vec<String>, GenerateError> {
    if data.get("env").is_none_or(Value::is_null) {
        data.insert("env".into(), json!({}));
    }
    let Some(env) = data.get_mut("env").and_then(Value::as_object_mut) else {
        return Err(GenerateError::invalid(format!(
            "'env' in {} is not an object",
            path.display()
        )));
    };
    let mut changed = Vec::new();
    if let Some(key) = key {
        if env.get(KEY_VAR).and_then(Value::as_str) != Some(key) {
            changed.push(KEY_VAR.to_string());
        }
        env.insert(KEY_VAR.into(), json!(key));
    }
    for (var, floor) in REQUIRED_ENV_FLOORS {
        if env_int(env.get(var)) < floor {
            env.insert(var.into(), json!(floor.to_string()));
            changed.push(var.to_string());
        }
    }
    Ok(changed)
}

// ---- hooks and status line in settings.json ------------------------------------------

/// The command settings.json runs for a hook: the install's own copy of the binary,
/// quoted, with forward slashes (Claude Code may hand the command to a POSIX shell,
/// where a backslash inside double quotes can escape the character after it).
pub fn hook_command(install_dir: &Path, kind: HookKind) -> String {
    let exe = as_posix(&crate::paths::binary_path(install_dir));
    format!("\"{exe}\" hook {}", kind.as_str())
}

/// The command the Python install wrote for a hook, recognised so an upgrade replaces
/// it instead of leaving it beside ours.
pub fn legacy_hook_command(install_dir: &Path, kind: HookKind) -> String {
    let py = as_posix(&install_dir.join(".venv").join("Scripts").join("python.exe"));
    let script = as_posix(&install_dir.join("hooks").join(kind.legacy_script()));
    format!("\"{py}\" \"{script}\"")
}

/// A command in exactly one of our forms, taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandParts {
    /// The install dir as written, forward slashes.
    pub dir: String,
    /// Which hook.
    pub kind: HookKind,
    /// The Python form: `"<dir>/.venv/Scripts/python.exe" "<dir>/hooks/<script>"`.
    pub legacy: bool,
    /// For the binary form, the file name as written (`claude-consult` or `.exe`); for
    /// the legacy form, the script name as written.
    pub file: String,
}

// What hook_command writes, once slashes, case and surrounding whitespace are evened
// out, and nothing looser. A command that merely runs one of our hooks, like a status
// line piping its stdin to ours, is the user's own, and replacing or removing it would
// destroy their work.
fn our_command() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)^"(?P<dir>[^"]+)/bin/(?P<file>claude-consult(?:\.exe)?)" hook (?P<which>summary|display|statusline)$"#,
        )
        .expect("static regex")
    })
}

fn legacy_command() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)^"(?P<python>[^"]+)/\.venv/Scripts/python\.exe" "(?P<dir>[^"]+)/hooks/(?P<file>summary_hook\.py|display_hook\.py|statusline\.py)"$"#,
        )
        .expect("static regex")
    })
}

/// The install dir and hook of a command in exactly our form (current or legacy), else
/// `None`. In the legacy form both halves must name the same install: a venv of one
/// install running a script of another is no command the installer ever wrote.
pub fn command_parts(cmd: &Value) -> Option<CommandParts> {
    let cmd = cmd.as_str()?.trim().replace('\\', "/");
    if let Some(m) = our_command().captures(&cmd) {
        return Some(CommandParts {
            dir: m["dir"].to_string(),
            kind: HookKind::from_word(&m["which"])?,
            legacy: false,
            file: m["file"].to_string(),
        });
    }
    let m = legacy_command().captures(&cmd)?;
    if m["python"].to_lowercase() != m["dir"].to_lowercase() {
        return None;
    }
    Some(CommandParts {
        dir: m["dir"].to_string(),
        kind: HookKind::from_word(&m["file"])?,
        legacy: true,
        file: m["file"].to_string(),
    })
}

/// Whether a folder holds a claude-consult install of this form: the binary in `bin/`,
/// or for the legacy form the Python server's `panel.py` and `server.py`.
pub fn is_install(folder: &Path, legacy: bool) -> bool {
    if legacy {
        folder.join("panel.py").is_file() && folder.join("server.py").is_file()
    } else {
        let bin = folder.join("bin");
        bin.join("claude-consult.exe").is_file() || bin.join("claude-consult").is_file()
    }
}

fn known_gone(path: &Path) -> bool {
    // Only a file known to be missing may make someone else's entry ours.
    matches!(path.try_exists(), Ok(false))
}

/// The install dir of a command in our form that is ours whichever install runs it.
///
/// Ours are such commands whose program is gone, since the command then fails and
/// Claude Code blocks on it; and, with `other_installs`, ones that run from another
/// claude-consult install, which would otherwise show every summary twice. `None` when
/// the command is not ours by either test. A dir that cannot be resolved here, relative
/// or holding a variable, depends on where and how Claude Code runs the command, so it
/// can never be shown to be either.
pub fn owner_elsewhere(cmd: &Value, other_installs: bool) -> Option<PathBuf> {
    let parts = command_parts(cmd)?;
    if parts.dir.contains('$') || parts.dir.contains('%') {
        return None;
    }
    let folder = PathBuf::from(&parts.dir);
    if !folder.is_absolute() {
        return None;
    }
    let program = if parts.legacy {
        folder.join("hooks").join(&parts.file)
    } else {
        folder.join("bin").join(&parts.file)
    };
    (known_gone(&program) || (other_installs && is_install(&folder, parts.legacy)))
        .then_some(folder)
}

/// Decides which commands are ours for one install.
///
/// An entry is ours when its command is exactly the one [`hook_command`] (or the legacy
/// installer) writes for this install, or when [`owner_elsewhere`] claims it. Identity
/// comes from the path rather than from the manifest, so an entry that points into the
/// install dir is cleaned up even if the manifest that recorded it is gone: once the
/// install dir is deleted it could only fail.
#[derive(Clone, Debug)]
pub struct Ours {
    here: String,
    other_installs: bool,
}

impl Ours {
    /// Whether this command is ours.
    pub fn is_ours(&self, cmd: &Value) -> bool {
        command_parts(cmd).is_some_and(|p| {
            p.dir.trim_end_matches('/').to_lowercase() == self.here
                || owner_elsewhere(cmd, self.other_installs).is_some()
        })
    }
}

/// The "ours" test for this install dir.
pub fn ours_test(install_dir: &Path, other_installs: bool) -> Ours {
    Ours {
        here: as_posix(install_dir).trim_end_matches('/').to_lowercase(),
        other_installs,
    }
}

/// Drops our hooks from one event's matcher groups.
///
/// Groups holding none of ours pass through unchanged, in the same order; a group that
/// mixes a user's hooks with ours loses only ours. Also returns where the first group
/// made only of ours stood, so its replacement goes back into the same slot instead of
/// moving to the end.
pub fn strip_ours(groups: &[Value], ours: &Ours) -> (Vec<Value>, Option<usize>) {
    let mine = |h: &Value| h.is_object() && ours.is_ours(h.get("command").unwrap_or(&Value::Null));
    let mut kept = Vec::new();
    let mut slot = None;
    for g in groups {
        let inner = g
            .as_object()
            .and_then(|o| o.get("hooks"))
            .and_then(Value::as_array);
        let Some(inner) = inner.filter(|i| i.iter().any(mine)) else {
            kept.push(g.clone());
            continue;
        };
        let rest: Vec<Value> = inner.iter().filter(|h| !mine(h)).cloned().collect();
        if !rest.is_empty() {
            let mut group = g.as_object().cloned().unwrap_or_default();
            group.insert("hooks".into(), Value::Array(rest));
            kept.push(Value::Object(group));
        } else if slot.is_none() {
            slot = Some(kept.len());
        }
    }
    (kept, slot)
}

/// Puts our entry for one event in place of the old one, or just removes ours.
pub fn place_hook(
    hooks: &mut Map<String, Value>,
    event: &str,
    entry: Option<Value>,
    ours: &Ours,
    path: &Path,
) -> Result<(), GenerateError> {
    let groups: Vec<Value> = match hooks.get(event) {
        None => Vec::new(),
        Some(Value::Array(a)) => a.clone(),
        Some(_) => {
            return Err(GenerateError::invalid(format!(
                "'hooks.{event}' in {} is not a list",
                path.display()
            )));
        }
    };
    let (mut kept, slot) = strip_ours(&groups, ours);
    if let Some(entry) = entry {
        kept.insert(slot.unwrap_or(kept.len()), entry);
    }
    if !kept.is_empty() {
        hooks.insert(event.into(), Value::Array(kept));
    } else if !groups.is_empty() {
        hooks.shift_remove(event);
    }
    Ok(())
}

/// Registers our PostToolUse hook, and our MessageDisplay hook unless the summary is off.
pub fn merge_hooks(
    data: &mut Map<String, Value>,
    path: &Path,
    install_dir: &Path,
    summary: SummaryStyle,
) -> Result<(), GenerateError> {
    if data.get("hooks").is_none_or(Value::is_null) {
        data.insert("hooks".into(), json!({}));
    }
    let Some(hooks) = data.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Err(GenerateError::invalid(format!(
            "'hooks' in {} is not an object",
            path.display()
        )));
    };
    let ours = ours_test(install_dir, true);
    place_hook(
        hooks,
        "PostToolUse",
        Some(json!({
            "matcher": CONSULT_TOOLS,
            "hooks": [{"type": "command", "command": hook_command(install_dir, HookKind::Summary)}],
        })),
        &ours,
        path,
    )?;
    // The display hook runs on every flush of every reply in every session; with the
    // summary off it would only ever find nothing to show, so it goes.
    let display = (summary != SummaryStyle::Off).then(|| {
        json!({"hooks": [{"type": "command", "command": hook_command(install_dir, HookKind::Display)}]})
    });
    place_hook(hooks, "MessageDisplay", display, &ours, path)
}

/// Whether ours may take the status line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusLineMode {
    /// Set ours only where there is none.
    #[default]
    Auto,
    /// Also over the user's own (put back on uninstall).
    Replace,
    /// Never add or replace one.
    Keep,
}

impl StatusLineMode {
    /// The name on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Replace => "replace",
            Self::Keep => "keep",
        }
    }

    /// The mode with this name.
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Auto, Self::Replace, Self::Keep]
            .into_iter()
            .find(|m| m.as_str() == name)
    }
}

/// What happened to the status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusLineOutcome {
    /// It was already ours, and was refreshed in place.
    Ours,
    /// There was none, and ours was set.
    Set,
    /// There was none, and `keep` left it so.
    Absent,
    /// The user's was replaced (and remembered).
    Replaced,
    /// The user's was kept.
    Kept,
}

impl StatusLineOutcome {
    /// The report's string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ours => "ours",
            Self::Set => "set",
            Self::Absent => "absent",
            Self::Replaced => "replaced",
            Self::Kept => "kept",
        }
    }
}

/// Sets or refreshes our status line. Returns what happened and the user's status line
/// that ours now stands in for, which uninstall puts back, the way a displaced file is
/// restored from backup.
pub fn merge_status_line(
    data: &mut Map<String, Value>,
    install_dir: &Path,
    mode: StatusLineMode,
    displaced: Option<Value>,
) -> (StatusLineOutcome, Option<Value>) {
    let ours = ours_test(install_dir, true);
    let command = hook_command(install_dir, HookKind::Statusline);
    let new = json!({"type": "command", "command": command});
    let cur = data.get("statusLine").cloned();
    if let Some(Value::Object(cur_map)) = &cur
        && ours.is_ours(cur_map.get("command").unwrap_or(&Value::Null))
    {
        let mut displaced = displaced;
        if displaced.is_none() {
            // An install elsewhere may have stood in for the user's own status
            // line; taking over its line means taking over putting theirs back.
            if let Some(other) =
                owner_elsewhere(cur_map.get("command").unwrap_or(&Value::Null), true)
            {
                displaced = crate::generate::load_manifest(&other)
                    .get("status_line_displaced")
                    .filter(|v| !v.is_null())
                    .cloned();
            }
        }
        let mut merged = cur_map.clone();
        merged.insert("type".into(), json!("command"));
        merged.insert("command".into(), json!(command));
        data.insert("statusLine".into(), Value::Object(merged));
        return (StatusLineOutcome::Ours, displaced);
    }
    match cur {
        None | Some(Value::Null) => {
            if mode == StatusLineMode::Keep {
                return (StatusLineOutcome::Absent, None);
            }
            data.insert("statusLine".into(), new);
            (StatusLineOutcome::Set, None)
        }
        Some(cur) if mode == StatusLineMode::Replace => {
            data.insert("statusLine".into(), new);
            (StatusLineOutcome::Replaced, Some(cur))
        }
        Some(_) => (StatusLineOutcome::Kept, None),
    }
}

/// Takes out every hook and the status line in our exact form that run from the install
/// dir, and those whose program is gone. Returns what changed and whether a displaced
/// status line was put back.
///
/// Another install's entries stay: uninstalling a copy that was moved away from must not
/// take the summary from the install now in use. Tolerates any shape: uninstall must not
/// be blocked by settings it did not write.
pub fn remove_ours(
    data: &mut Map<String, Value>,
    install_dir: &Path,
    displaced: Option<Value>,
) -> (Vec<String>, bool) {
    let ours = ours_test(install_dir, false);
    let mut changed = Vec::new();
    let mut drop_hooks = false;
    if let Some(hooks) = data.get_mut("hooks").and_then(Value::as_object_mut)
        && !hooks.is_empty()
    {
        let events: Vec<String> = hooks.keys().cloned().collect();
        for event in events {
            let Some(groups) = hooks.get(&event).and_then(Value::as_array).cloned() else {
                continue;
            };
            let (kept, _) = strip_ours(&groups, &ours);
            if kept == groups {
                continue;
            }
            changed.push(format!("hooks.{event}"));
            if kept.is_empty() {
                hooks.shift_remove(&event);
            } else {
                hooks.insert(event, Value::Array(kept));
            }
        }
        drop_hooks = hooks.is_empty();
    }
    if drop_hooks {
        data.shift_remove("hooks");
    }
    let mut restored = false;
    let is_ours = data
        .get("statusLine")
        .and_then(Value::as_object)
        .is_some_and(|cur| ours.is_ours(cur.get("command").unwrap_or(&Value::Null)));
    if is_ours {
        match displaced {
            Some(d) => {
                data.insert("statusLine".into(), d);
                restored = true;
            }
            None => {
                data.shift_remove("statusLine");
            }
        }
        changed.push("statusLine".to_string());
    }
    (changed, restored)
}

/// Which of our settings differ between two versions of settings.json.
pub fn settings_diff(before: &Map<String, Value>, after: &Map<String, Value>) -> Vec<String> {
    let event = |d: &Map<String, Value>, name: &str| {
        d.get("hooks")
            .and_then(Value::as_object)
            .and_then(|h| h.get(name))
            .cloned()
    };
    let mut changed: Vec<String> = ["PostToolUse", "MessageDisplay"]
        .iter()
        .filter(|e| event(before, e) != event(after, e))
        .map(|e| format!("hooks.{e}"))
        .collect();
    if before.get("statusLine") != after.get("statusLine") {
        changed.push("statusLine".to_string());
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_in_both_forms_are_taken_apart() {
        let dir = Path::new(if cfg!(windows) {
            "C:\\x\\inst"
        } else {
            "/x/inst"
        });
        let cmd = hook_command(dir, HookKind::Summary);
        let parts = command_parts(&json!(cmd)).expect("ours");
        assert_eq!(parts.kind, HookKind::Summary);
        assert!(!parts.legacy);
        assert!(parts.dir.ends_with("/x/inst"));
        let legacy = legacy_hook_command(dir, HookKind::Display);
        let parts = command_parts(&json!(legacy)).expect("legacy");
        assert!(parts.legacy);
        assert_eq!(parts.kind, HookKind::Display);
        // Evened out: backslashes, case, whitespace.
        let evened = format!(" {}\n", cmd.replace('/', "\\").to_uppercase());
        assert!(command_parts(&json!(evened)).is_some());
        for other in [
            format!("{cmd} --old"),
            format!("{cmd} || exit 0"),
            cmd.replace("hook summary", "hook other"),
            cmd.replace("\" hook", "\"  hook"),
            "python hooks/summary_hook.py".to_string(),
        ] {
            assert!(command_parts(&json!(other)).is_none(), "{other}");
        }
        assert!(command_parts(&json!(7)).is_none());
    }

    #[test]
    fn env_floors_are_raised_never_lowered() {
        let mut data = Map::new();
        let p = Path::new("settings.json");
        let changed = merge_env(&mut data, p, Some("k")).expect("merged");
        assert_eq!(
            changed,
            [
                "OPENROUTER_API_KEY",
                "MCP_TOOL_TIMEOUT",
                "CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"
            ]
        );
        assert_eq!(data["env"]["MCP_TOOL_TIMEOUT"], json!("2400000"));
        data["env"]["MCP_TOOL_TIMEOUT"] = json!("9999999");
        data["env"]["CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT"] = json!(" 1800000 ");
        assert!(
            merge_env(&mut data, p, Some("k"))
                .expect("merged")
                .is_empty()
        );
        assert_eq!(data["env"]["MCP_TOOL_TIMEOUT"], json!("9999999"));
        data["env"]["MCP_TOOL_TIMEOUT"] = json!("abc");
        assert_eq!(
            merge_env(&mut data, p, None).expect("merged"),
            ["MCP_TOOL_TIMEOUT"]
        );
        let mut bad: Map<String, Value> = serde_json::from_str(r#"{"env": []}"#).expect("json");
        assert!(merge_env(&mut bad, p, None).is_err());
    }

    #[test]
    fn bad_settings_are_refused_not_rewritten() {
        let tmp = tempfile::tempdir().expect("tmp");
        let p = tmp.path().join("settings.json");
        assert!(load_settings(&p).expect("missing").is_empty());
        std::fs::write(&p, "  \n").expect("write");
        assert!(load_settings(&p).expect("blank").is_empty());
        std::fs::write(&p, "{nope").expect("write");
        let e = load_settings(&p).expect_err("invalid").to_string();
        assert!(
            e.contains("is not valid JSON") && e.ends_with("fix it and re-run"),
            "{e}"
        );
        std::fs::write(&p, "[1]").expect("write");
        assert!(
            load_settings(&p)
                .expect_err("list")
                .to_string()
                .ends_with("does not contain a JSON object")
        );
        std::fs::write(&p, "\u{feff}{\"b\": 1, \"a\": 2}").expect("write");
        let map = load_settings(&p).expect("bom");
        assert_eq!(map.keys().collect::<Vec<_>>(), ["b", "a"]);
    }
}
