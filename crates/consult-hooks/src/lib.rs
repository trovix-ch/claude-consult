//! The three Claude Code hooks of claude-consult.
//!
//! - [`summary`] (PostToolUse on `consult` and `consult_clean`) tallies the call in the
//!   session's log and parks a one-line summary for the display hook.
//! - [`display`] (MessageDisplay) draws that parked summary under the final flush of
//!   Claude's next reply, without it entering Claude's context.
//! - [`statusline`] prints what consult has cost in the session so far.
//!
//! Every hook swallows every failure and exits 0: a broken hook must never cost the user
//! a consult result they have already paid for, and a status line is no place for an
//! error. [`run`] is the entry point the binary calls; the three functions it dispatches
//! to are pure over their inputs (install dir, stdin bytes, environment) so they can be
//! tested without a process per event.

use std::io::{Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

mod display;
mod statusline;
mod summary;

pub use display::display;
pub use statusline::statusline;
pub use summary::{summary, summary_line};

/// Which hook to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HookKind {
    /// The PostToolUse hook that tallies a consult and parks its summary.
    Summary,
    /// The MessageDisplay hook that draws a parked summary.
    Display,
    /// The status line.
    Statusline,
}

impl HookKind {
    /// Every hook, in the order the settings list them.
    pub const ALL: [HookKind; 3] = [Self::Summary, Self::Display, Self::Statusline];

    /// The name used on the command line (`claude-consult hook <name>`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Display => "display",
            Self::Statusline => "statusline",
        }
    }

    /// The hook with this exact name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }
}

impl std::fmt::Display for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runs one hook as Claude Code runs it: stdin is the event JSON, the install dir is
/// resolved by [`consult_core::paths::install_dir`], and the process environment is read.
///
/// Always returns exit code 0, whatever fails.
pub fn run(kind: HookKind, install_dir: Option<&Path>) -> i32 {
    let dir = consult_core::paths::install_dir(install_dir);
    let env = |key: &str| std::env::var(key).ok();
    run_with(
        kind,
        &dir,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        &env,
    )
}

/// [`run`] with its inputs and output given: reads all of `stdin`, runs the hook against
/// `install_dir`, and writes to `stdout` exactly what the hook prints.
///
/// Always returns exit code 0, whatever fails, a panic included.
pub fn run_with(
    kind: HookKind,
    install_dir: &Path,
    stdin: &mut impl Read,
    stdout: &mut impl Write,
    env: &impl Fn(&str) -> Option<String>,
) -> i32 {
    // Drained before anything else, so Claude Code never writes into a closed pipe. Input
    // that cannot be read in full is no input: a hook acting on half an event could
    // misfile a call.
    let mut raw = Vec::new();
    if stdin.read_to_end(&mut raw).is_err() {
        raw.clear();
    }
    let output = catch_unwind(AssertUnwindSafe(|| match kind {
        HookKind::Summary => {
            summary(install_dir, &raw, env);
            None
        }
        HookKind::Display => display(install_dir, &raw, env).map(|line| line + "\n"),
        HookKind::Statusline => Some(statusline(install_dir, &raw) + "\n"),
    }));
    let output = match output {
        Ok(output) => output,
        // A status line always prints its one line, empty when there is nothing to say.
        Err(_) => (kind == HookKind::Statusline).then(|| "\n".to_string()),
    };
    if let Some(text) = output {
        // A closed stdout is not worth a failure either.
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    }
    0
}

/// Session ids become file names, so anything but a plain token is refused.
pub fn safe_name(session_id: &str) -> bool {
    (1..=128).contains(&session_id.len())
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A headless run: `claude -p` is `sdk-cli`, and each SDK has its own `sdk-*` entrypoint.
fn headless(env: &impl Fn(&str) -> Option<String>) -> bool {
    env("CLAUDE_CODE_ENTRYPOINT").is_some_and(|e| e.starts_with("sdk"))
}

/// The pending summary of a session, drawn once by the display hook.
fn pending_path(install_dir: &Path, session_id: &str) -> std::path::PathBuf {
    consult_core::paths::state_dir(install_dir).join(format!("{session_id}.pending"))
}

/// The session's log, one line per call.
fn log_path(install_dir: &Path, session_id: &str) -> std::path::PathBuf {
    consult_core::paths::state_dir(install_dir).join(format!("{session_id}.jsonl"))
}

/// Parses hook input the way the Python hooks did: undecodable bytes are replaced, not
/// fatal, and anything but a JSON object is no input.
fn parse_object(stdin: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str(&String::from_utf8_lossy(stdin)).ok()? {
        serde_json::Value::Object(map) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        for ok in [
            "5f0c2a9e-1d3b-4c6a-9e8f-0a1b2c3d4e5f",
            "a",
            "A_b-9",
            &"x".repeat(128),
        ] {
            assert!(safe_name(ok), "{ok}");
        }
        for bad in [
            "",
            "../escape",
            "..\\escape",
            "C:/escape",
            "a b",
            "a.b",
            "\u{e9}",
            "\u{661}",
            &"x".repeat(129),
        ] {
            assert!(!safe_name(bad), "{bad}");
        }
    }

    #[test]
    fn hook_names_round_trip() {
        for kind in HookKind::ALL {
            assert_eq!(HookKind::parse(kind.as_str()), Some(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(HookKind::parse("Summary"), None);
    }

    #[test]
    fn headless_is_any_sdk_entrypoint() {
        for (value, expected) in [
            (None, false),
            (Some("cli"), false),
            (Some("sdk-cli"), true),
            (Some("sdk-ts"), true),
            (Some("sdk-py"), true),
            (Some("SDK-cli"), false),
        ] {
            let env =
                |k: &str| (k == "CLAUDE_CODE_ENTRYPOINT").then(|| value.map(str::to_string))?;
            assert_eq!(headless(&env), expected, "{value:?}");
        }
    }

    #[test]
    fn unreadable_stdin_is_no_input() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken pipe"))
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let env = |_: &str| None;
        for kind in HookKind::ALL {
            let mut out = Vec::new();
            assert_eq!(run_with(kind, dir.path(), &mut Broken, &mut out, &env), 0);
            let expected: &[u8] = if kind == HookKind::Statusline {
                b"\n"
            } else {
                b""
            };
            assert_eq!(out, expected, "{kind}");
        }
        assert_eq!(std::fs::read_dir(dir.path()).expect("list").count(), 0);
    }
}
