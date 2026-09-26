//! The one-shot runner behind `claude-consult run`, and `claude-consult reviewers`.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use consult_core::panel::{
    ConsultRequest, Consultant, DEFAULT_MAX_STEPS, MAX_REVIEWER_COST_USD, render,
};
use consult_core::paths;
use consult_core::util::json_ascii;

use crate::Error;

/// What `claude-consult run` was given. The binary's clap layer fills it; the defaults
/// are those of the Python `--cli` mode.
#[derive(Clone, Debug, PartialEq)]
pub struct RunArgs {
    /// `--root`: the project the reviewers may read.
    pub root: String,
    /// `--question`.
    pub question: Option<String>,
    /// `--question-file`: read the question from this file instead (it wins over
    /// `--question`).
    pub question_file: Option<PathBuf>,
    /// `--models`: comma-separated aliases, command names or OpenRouter ids.
    pub models: Option<String>,
    /// `--attach`: comma-separated project-relative files to include up front.
    pub attach: Option<String>,
    /// `--max-steps`, passed through unclamped as the Python CLI does.
    pub max_steps: usize,
    /// `--max-cost`: per-reviewer spend ceiling in USD.
    pub max_cost: f64,
    /// `--clean`: no project context, no tools, no repo access.
    pub clean: bool,
    /// `--mode`: `review` or `diagnose` (the CLI layer restricts it to those).
    pub mode: String,
    /// `--json`: emit the raw result as JSON.
    pub json: bool,
    /// `--install-dir`; `None` resolves through [`paths::install_dir`].
    pub install_dir: Option<PathBuf>,
    /// `--claude-dir`; `None` resolves through [`paths::claude_dir`].
    pub claude_dir: Option<PathBuf>,
}

impl Default for RunArgs {
    fn default() -> Self {
        Self {
            root: ".".to_string(),
            question: None,
            question_file: None,
            models: None,
            attach: None,
            max_steps: DEFAULT_MAX_STEPS,
            max_cost: MAX_REVIEWER_COST_USD,
            clean: false,
            mode: "review".to_string(),
            json: false,
            install_dir: None,
            claude_dir: None,
        }
    }
}

/// The streams a run reads and writes, injectable for tests.
pub struct RunIo<'a> {
    /// Where a question not given on the command line is read from.
    pub stdin: &'a mut dyn Read,
    /// Whether `stdin` is a terminal: then it is never read, and a missing question is
    /// a usage error.
    pub stdin_is_terminal: bool,
    /// The result goes here.
    pub stdout: &'a mut dyn Write,
    /// `consult failed: ...` goes here.
    pub stderr: &'a mut dyn Write,
}

/// Runs one consult against the real OpenRouter and prints it: the rendered markdown,
/// or the result as JSON with `--json`.
///
/// Returns the exit code: 0, or 1 after printing `consult failed: ...` to stderr when
/// the consult could not run. [`Error::NoQuestion`] is a usage error (the Python CLI
/// exited 2 with argparse's usage line); the binary reports it through clap.
pub async fn run(args: RunArgs) -> Result<i32, Error> {
    let consultant = Consultant::new(
        paths::install_dir(args.install_dir.as_deref()),
        paths::claude_dir(args.claude_dir.as_deref()),
    );
    let stdin = std::io::stdin();
    let is_terminal = stdin.is_terminal();
    let mut stdin = stdin.lock();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with(
        &args,
        &consultant,
        RunIo {
            stdin: &mut stdin,
            stdin_is_terminal: is_terminal,
            stdout: &mut stdout,
            stderr: &mut stderr,
        },
    )
    .await
}

/// [`run`] with the consultant and streams given.
pub async fn run_with(
    args: &RunArgs,
    consultant: &Consultant,
    io: RunIo<'_>,
) -> Result<i32, Error> {
    let question = question(args, io.stdin, io.stdin_is_terminal)?;
    let models = split(args.models.as_deref());
    let result = if args.clean {
        consultant
            .consult_clean(&question, models.as_deref(), None)
            .await
    } else {
        let req = ConsultRequest {
            question,
            root: args.root.clone(),
            models,
            attachments: split(args.attach.as_deref()),
            max_steps: args.max_steps,
            max_cost_usd: args.max_cost,
            mode: args.mode.clone(),
        };
        consultant.consult(&req, None).await
    };
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            writeln!(io.stderr, "consult failed: {e}").map_err(Error::Io)?;
            return Ok(1);
        }
    };
    let out = if args.json {
        // Python's json.dumps(indent=2): the same layout, and non-ASCII escaped.
        json_ascii(&serde_json::to_string_pretty(&result).map_err(Error::Json)?)
    } else {
        render(&result)
    };
    writeln!(io.stdout, "{out}").map_err(Error::Io)?;
    io.stdout.flush().map_err(Error::Io)?;
    Ok(0)
}

fn question(args: &RunArgs, stdin: &mut dyn Read, is_terminal: bool) -> Result<String, Error> {
    let mut question = args.question.clone().unwrap_or_default();
    if let Some(file) = &args.question_file {
        question = std::fs::read_to_string(file).map_err(|source| Error::QuestionFile {
            path: file.clone(),
            source,
        })?;
    }
    if question.trim().is_empty() {
        if is_terminal {
            return Err(Error::NoQuestion);
        }
        question.clear();
        stdin.read_to_string(&mut question).map_err(Error::Io)?;
    }
    Ok(question)
}

fn split(value: Option<&str>) -> Option<Vec<String>> {
    consult_core::panel::split_list(&serde_json::Value::String(value?.to_string()))
}

/// The reviewers table `list_reviewers` returns, for `claude-consult reviewers`.
pub async fn reviewers(install_dir: Option<&Path>, claude_dir: Option<&Path>) -> String {
    Consultant::new(
        paths::install_dir(install_dir),
        paths::claude_dir(claude_dir),
    )
    .list_reviewers()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(args: RunArgs, stdin: &str, tty: bool) -> Result<String, Error> {
        question(&args, &mut stdin.as_bytes(), tty)
    }

    #[test]
    fn the_question_comes_from_the_flag_the_file_or_stdin() {
        let q = |s: &str| RunArgs {
            question: Some(s.to_string()),
            ..RunArgs::default()
        };
        assert_eq!(ask(q("flag"), "piped", false).expect("q"), "flag");
        assert_eq!(ask(q("  "), "piped", false).expect("q"), "piped");
        assert_eq!(ask(RunArgs::default(), "piped", false).expect("q"), "piped");
        assert!(matches!(
            ask(RunArgs::default(), "piped", true),
            Err(Error::NoQuestion)
        ));

        let tmp = tempfile::tempdir().expect("tmp");
        let file = tmp.path().join("q.md");
        std::fs::write(&file, "from the file").expect("write");
        let args = RunArgs {
            question_file: Some(file),
            ..q("flag")
        };
        assert_eq!(ask(args, "", true).expect("q"), "from the file");
    }

    #[test]
    fn a_missing_question_says_what_argparse_said() {
        assert_eq!(
            Error::NoQuestion.to_string(),
            "provide --question, --question-file, or pipe the question on stdin"
        );
    }

    #[test]
    fn lists_split_on_commas() {
        assert_eq!(
            split(Some("a, b,,")),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert_eq!(split(Some(" ")), None);
        assert_eq!(split(None), None);
    }
}
