//! The plain-text path, for when there is no terminal to draw on: line prompts, hidden
//! input and the installer's `==>` / `[ok]` / `[!!]` / `[xx]` output.
//!
//! Each prompt has a `*_from` form over any reader and writer, which is what the tests
//! and the installer's plain UI drive.

use std::io::{self, BufRead, IsTerminal, Write};

use ratatui::crossterm::style::Stylize;

use crate::widgets::{StepKind, format_step};

/// Whether stdout is a terminal, and so may be coloured.
pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

/// Whether both stdin and stdout are terminals, which the full-screen UI needs.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// A line of output, coloured as the PowerShell installer coloured it when `color`.
pub fn paint(kind: StepKind, line: &str, color: bool) -> String {
    if !color {
        return line.to_string();
    }
    match kind {
        StepKind::Step => line.cyan().to_string(),
        StepKind::Ok => line.green().to_string(),
        StepKind::Note => line.yellow().to_string(),
        StepKind::Fail => line.red().to_string(),
        StepKind::Info => line.to_string(),
    }
}

/// Writes one step-log entry: the blank line before a heading or failure, the prefix,
/// continuation lines indented.
pub fn write_step(out: &mut dyn Write, kind: StepKind, text: &str, color: bool) -> io::Result<()> {
    if kind.spaced() {
        writeln!(out)?;
    }
    for line in format_step(kind, text) {
        writeln!(out, "{}", paint(kind, &line, color))?;
    }
    out.flush()
}

/// [`write_step`] to stdout, coloured when stdout is a terminal. Output errors are
/// ignored, as `println!` would panic on them instead.
pub fn print_step(kind: StepKind, text: &str) {
    let _ = write_step(&mut io::stdout().lock(), kind, text, stdout_is_tty());
}

/// `==> text`.
pub fn step(text: &str) {
    print_step(StepKind::Step, text);
}

/// `  [ok] text`.
pub fn ok(text: &str) {
    print_step(StepKind::Ok, text);
}

/// `  [!!] text`.
pub fn note(text: &str) {
    print_step(StepKind::Note, text);
}

/// `       text`.
pub fn info(text: &str) {
    print_step(StepKind::Info, text);
}

/// `  [xx] text`.
pub fn fail(text: &str) {
    print_step(StepKind::Fail, text);
}

/// Shows `prompt: ` and reads one line, without its line ending. End of input is an
/// `UnexpectedEof` error, so a loop around a prompt cannot spin on a closed stdin.
pub fn prompt_line_from(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    prompt: &str,
) -> io::Result<String> {
    write!(output, "{prompt}: ")?;
    output.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        writeln!(output)?;
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "no more input",
        ));
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// [`prompt_line_from`] on stdin and stdout.
pub fn prompt_line(prompt: &str) -> io::Result<String> {
    prompt_line_from(&mut io::stdin().lock(), &mut io::stdout(), prompt)
}

/// Reads a line without echoing it (the API key). On a console this goes through
/// `rpassword`, which reads the console itself; with stdin redirected there is nothing
/// to hide, and the line is read from stdin.
pub fn prompt_hidden(prompt: &str) -> io::Result<String> {
    if io::stdin().is_terminal() {
        return rpassword::prompt_password(format!("{prompt}: "));
    }
    prompt_line(prompt)
}

/// Asks a yes/no question as the installer's `Confirm-Choice`: `  question [Y/n]: `,
/// empty is the default, `y`/`yes`/`n`/`no` in any case, anything else asks again. End
/// of input is the default.
pub fn confirm_from(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    question: &str,
    default: bool,
) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        let answer = match prompt_line_from(input, output, &format!("  {question} {hint}")) {
            Ok(a) => a,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(default),
            Err(e) => return Err(e),
        };
        match answer.trim().to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

/// [`confirm_from`] on stdin and stdout.
pub fn confirm(question: &str, default: bool) -> io::Result<bool> {
    confirm_from(
        &mut io::stdin().lock(),
        &mut io::stdout(),
        question,
        default,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(input: &str, default: bool) -> (bool, String) {
        let mut out = Vec::new();
        let got =
            confirm_from(&mut input.as_bytes(), &mut out, "Proceed?", default).expect("answered");
        (got, String::from_utf8(out).expect("utf8"))
    }

    #[test]
    fn confirm_choice_semantics() {
        assert!(ask("\n", true).0);
        assert!(!ask("\n", false).0);
        assert!(ask(" YES \r\n", false).0);
        assert!(!ask("n\n", true).0);
        let (got, out) = ask("maybe\nno\n", true);
        assert!(!got);
        assert_eq!(out, "  Proceed? [Y/n]:   Proceed? [Y/n]: ");
        // A closed stdin takes the default instead of asking forever.
        assert!(ask("", true).0);
        assert!(!ask("what\n", false).0);
    }

    #[test]
    fn prompt_line_strips_the_line_ending_and_reports_eof() {
        let mut out = Vec::new();
        let got = prompt_line_from(&mut "abc \r\n".as_bytes(), &mut out, "  Panel").expect("line");
        assert_eq!(got, "abc ");
        assert_eq!(out, b"  Panel: ");
        let e = prompt_line_from(&mut "".as_bytes(), &mut Vec::new(), "x").expect_err("eof");
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn steps_print_with_prefixes_and_colour_only_when_asked() {
        let mut out = Vec::new();
        write_step(&mut out, StepKind::Step, "Checking prerequisites", false).expect("write");
        write_step(&mut out, StepKind::Ok, "git found", false).expect("write");
        write_step(&mut out, StepKind::Fail, "Cancelled.", false).expect("write");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\n==> Checking prerequisites\n  [ok] git found\n\n  [xx] Cancelled.\n"
        );
        let coloured = paint(StepKind::Ok, "  [ok] x", true);
        assert!(coloured.contains("\u{1b}["));
        assert_eq!(paint(StepKind::Info, "x", true), "x");
    }
}
