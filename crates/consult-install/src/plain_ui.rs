//! The line-by-line UIs: [`PlainUi`] asks on stdin, [`UnattendedUi`] asks nothing.

use std::io::{self, BufRead, Write};

use consult_core::generate::Display;
use consult_tui::picker::{format_context, format_price};
use consult_tui::plain::{self, confirm_from, prompt_line_from, stdout_is_tty, write_step};
use consult_tui::{PickerRow, StepKind};
use ratatui::crossterm::style::Stylize;

use crate::InstallError;
use crate::ui::{PanelAnswer, PanelRequest, Ui, parse_panel_answer};

/// The banner the PowerShell installer opened with.
pub const BANNER: &str =
    "  claude-consult - outside reviews from non-Claude models, in Claude Code";
const RULE: &str = "  -----------------------------------------------------------------------";

fn write_banner(out: &mut dyn Write, color: bool) -> io::Result<()> {
    writeln!(out)?;
    if color {
        writeln!(out, "{}", BANNER.cyan())?;
        writeln!(out, "{}", RULE.dark_cyan())?;
    } else {
        writeln!(out, "{BANNER}")?;
        writeln!(out, "{RULE}")?;
    }
    out.flush()
}

fn write_done(
    out: &mut dyn Write,
    color: bool,
    headline: &str,
    lines: &[String],
) -> io::Result<()> {
    writeln!(out)?;
    let head = format!("  {headline}");
    if color {
        writeln!(out, "{}", head.green())?;
    } else {
        writeln!(out, "{head}")?;
    }
    if !lines.is_empty() {
        writeln!(out)?;
        for line in lines {
            writeln!(out, "{}{line}", StepKind::Info.prefix())?;
        }
    }
    writeln!(out)?;
    out.flush()
}

/// Line prompts on stdin and output on stdout, as the PowerShell installer ran in a
/// console without the full-screen picker: a numbered list of the favourites for the
/// panel, `[Y/n]` questions, a hidden prompt for the key.
pub struct PlainUi {
    input: Option<Box<dyn BufRead>>,
    output: Box<dyn Write>,
    color: bool,
    shown_catalog: bool,
}

impl PlainUi {
    /// On the process's stdin and stdout; coloured when stdout is a terminal. The key
    /// is read without echo when stdin is a console.
    pub fn stdio() -> Self {
        Self {
            input: None,
            output: Box::new(io::stdout()),
            color: stdout_is_tty(),
            shown_catalog: false,
        }
    }

    /// On the given streams, uncoloured; the key is read as a plain line. For tests.
    pub fn new(input: impl BufRead + 'static, output: impl Write + 'static) -> Self {
        Self {
            input: Some(Box::new(input)),
            output: Box::new(output),
            color: false,
            shown_catalog: false,
        }
    }

    fn line(&mut self, prompt: &str) -> io::Result<String> {
        match self.input.as_mut() {
            Some(input) => prompt_line_from(input.as_mut(), self.output.as_mut(), prompt),
            None => prompt_line_from(&mut io::stdin().lock(), self.output.as_mut(), prompt),
        }
    }

    fn paint(&self, text: &str, style: fn(&str) -> String) -> String {
        if self.color {
            style(text)
        } else {
            text.to_string()
        }
    }

    fn show_catalog(&mut self, request: &PanelRequest<'_>, favs: &[&PickerRow]) -> io::Result<()> {
        let row = |n: &str, alias: &str, lab: &str, tier: &str, i: &str, o: &str, c: &str| {
            format!("  {n:>3}  {alias:<21} {lab:<12} {tier:<9} {i:>8} {o:>8} {c:>7}")
        };
        let dark: fn(&str) -> String = |s| s.dark_grey().to_string();
        let white: fn(&str) -> String = |s| s.white().to_string();
        let grey: fn(&str) -> String = |s| s.grey().to_string();
        writeln!(self.output)?;
        let header = row("#", "alias", "lab", "tier", "$/M in", "$/M out", "context");
        writeln!(self.output, "{}", self.paint(&header, dark))?;
        for (i, r) in favs.iter().enumerate() {
            let m = request.catalog.models.get(&r.key);
            let tier = m.and_then(|m| m.tier.as_deref()).unwrap_or("");
            let style = match tier {
                "panel" => white,
                "budget" => grey,
                _ => dark,
            };
            let line = row(
                &(i + 1).to_string(),
                &r.key,
                &r.lab,
                tier,
                &format_price(r.price_in),
                &format_price(r.price_out),
                &format_context(r.context),
            );
            writeln!(self.output, "{}", self.paint(&line, style))?;
            let plays = format!(
                "{}{}",
                " ".repeat(32),
                m.map(|m| m.plays_to.as_str()).unwrap_or("")
            );
            writeln!(self.output, "{}", self.paint(&plays, dark))?;
        }
        writeln!(self.output)?;
        Ok(())
    }
}

impl Ui for PlainUi {
    fn welcome(&mut self) -> Result<bool, InstallError> {
        write_banner(self.output.as_mut(), self.color)?;
        Ok(true)
    }

    fn report_step(&mut self, kind: StepKind, text: &str) {
        let _ = write_step(self.output.as_mut(), kind, text, self.color);
    }

    fn confirm(&mut self, question: &str, default: bool) -> Result<bool, InstallError> {
        Ok(match self.input.as_mut() {
            Some(input) => confirm_from(input.as_mut(), self.output.as_mut(), question, default)?,
            None => confirm_from(
                &mut io::stdin().lock(),
                self.output.as_mut(),
                question,
                default,
            )?,
        })
    }

    fn enter_key(&mut self, prompt: &str) -> Result<String, InstallError> {
        let prompt = format!("  {prompt}");
        if self.input.is_none() {
            self.output.flush()?;
            return Ok(plain::prompt_hidden(&prompt)?);
        }
        Ok(self.line(&prompt)?)
    }

    fn choose_panel(&mut self, request: &PanelRequest<'_>) -> Result<PanelAnswer, InstallError> {
        let favs: Vec<&PickerRow> = request.rows.iter().filter(|r| r.fav).collect();
        if !self.shown_catalog {
            self.shown_catalog = true;
            self.show_catalog(request, &favs)?;
            self.report_step(
                StepKind::Info,
                &format!("Recommended : {}", request.recommended.join(", ")),
            );
            self.report_step(
                StepKind::Info,
                &format!("Budget      : {}", request.budget.join(", ")),
            );
        }
        let answer = self.line(
            "  Panel [Enter = recommended, b = budget, numbers like 1,5,7, or OpenRouter ids]",
        )?;
        match parse_panel_answer(
            &answer,
            request.catalog,
            &favs,
            request.recommended,
            request.budget,
        ) {
            Ok(chosen) => Ok(PanelAnswer::Typed(chosen)),
            Err(note) => {
                if let Some(note) = note {
                    self.report_step(StepKind::Note, &note);
                }
                Ok(PanelAnswer::Typed(Vec::new()))
            }
        }
    }

    fn choose_display(&mut self, current: Display) -> Result<Display, InstallError> {
        Ok(current)
    }

    fn done(&mut self, headline: &str, lines: &[String]) {
        let _ = write_done(self.output.as_mut(), self.color, headline, lines);
    }
}

/// Asks nothing: every question takes its default, as `--unattended`. Output goes to
/// stdout (or the given writer), coloured when it is a terminal.
pub struct UnattendedUi {
    output: Box<dyn Write>,
    color: bool,
}

impl UnattendedUi {
    /// On stdout.
    pub fn stdio() -> Self {
        Self {
            output: Box::new(io::stdout()),
            color: stdout_is_tty(),
        }
    }

    /// On the given writer, uncoloured. For tests.
    pub fn new(output: impl Write + 'static) -> Self {
        Self {
            output: Box::new(output),
            color: false,
        }
    }
}

impl Ui for UnattendedUi {
    fn unattended(&self) -> bool {
        true
    }

    fn welcome(&mut self) -> Result<bool, InstallError> {
        write_banner(self.output.as_mut(), self.color)?;
        Ok(true)
    }

    fn report_step(&mut self, kind: StepKind, text: &str) {
        let _ = write_step(self.output.as_mut(), kind, text, self.color);
    }

    fn confirm(&mut self, _question: &str, default: bool) -> Result<bool, InstallError> {
        Ok(default)
    }

    fn enter_key(&mut self, _prompt: &str) -> Result<String, InstallError> {
        Err(InstallError::Stopped(
            "No usable OpenRouter key. For --unattended, set OPENROUTER_API_KEY first.".to_string(),
        ))
    }

    fn choose_panel(&mut self, _request: &PanelRequest<'_>) -> Result<PanelAnswer, InstallError> {
        Ok(PanelAnswer::Cancelled)
    }

    fn done(&mut self, headline: &str, lines: &[String]) {
        let _ = write_done(self.output.as_mut(), self.color, headline, lines);
    }
}
