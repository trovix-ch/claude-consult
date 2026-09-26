//! The full-screen installer: a wizard over the same flow the plain UIs drive.

use std::io::{self, Write};

use consult_core::display::{ProgressStyle, SummaryStyle};
use consult_core::generate::Display;
use consult_tui::picker::render_with;
use consult_tui::plain::{stdout_is_tty, write_step};
use consult_tui::widgets::paint_lines;
use consult_tui::{
    Confirm, ConfirmOutcome, InputOutcome, Key, Picker, StepKind, StepLog, Terminal, TextInput,
    Theme, Wizard, read_key,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::InstallError;
use crate::plain_ui::BANNER;
use crate::ui::{PanelAnswer, PanelRequest, Ui};

const TITLE: &str = "claude-consult";

/// Stands for "nothing pressed, draw again" (Ctrl+L, the usual redraw key).
const REDRAW: Key = Key::Ctrl('l');

/// The wizard: welcome, prerequisites, key, panel picker, status line, display styles,
/// the plan, the steps, done. Every line the flow reports stays in a log shown above
/// the current question, and is printed to the normal screen once the wizard closes,
/// so the paths and commands it listed survive it.
///
/// The terminal is entered on the first screen and restored by [`Ui::done`],
/// [`Ui::fail`], or dropping this, whichever comes first (a panic included).
pub struct TuiUi {
    term: Option<Terminal>,
    log: StepLog,
    theme: Theme,
    busy: Option<String>,
    finished: bool,
}

impl Default for TuiUi {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiUi {
    /// A wizard on this terminal. Needs stdin and stdout to be a terminal
    /// ([`consult_tui::plain::is_interactive`]).
    pub fn new() -> Self {
        Self {
            term: None,
            log: StepLog::new(),
            theme: Theme::default(),
            busy: None,
            finished: false,
        }
    }

    fn frame(
        &mut self,
        hint: &str,
        body: impl FnOnce(Rect, &mut Buffer, &Theme, &StepLog),
    ) -> Result<(), InstallError> {
        if self.term.is_none() {
            self.term = Some(Terminal::enter()?);
        }
        let Self {
            term,
            log,
            theme,
            busy,
            ..
        } = self;
        let section = log.section().unwrap_or("Welcome").to_string();
        let hint = busy
            .as_deref()
            .map_or_else(|| hint.to_string(), |b| format!("{b} ..."));
        if let Some(term) = term.as_mut() {
            term.draw(|f| {
                let area = f.area();
                let buf = f.buffer_mut();
                let inner = Wizard {
                    title: TITLE,
                    section: &section,
                    hint: &hint,
                }
                .render(area, buf, theme);
                body(inner, buf, theme, log);
            })?;
        }
        Ok(())
    }

    /// The next key, or [`REDRAW`] for a resize or any other event, after which the
    /// caller's loop draws again.
    fn key(&mut self) -> Result<Key, InstallError> {
        Ok(read_key()?.unwrap_or(REDRAW))
    }

    /// Leaves the full screen and prints the log as the plain output would have.
    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.term = None;
        let mut out = io::stdout();
        let color = stdout_is_tty();
        for (kind, text) in self.log.entries() {
            let _ = write_step(&mut out, *kind, text, color);
        }
        let _ = out.flush();
    }

    fn abort(&mut self) -> InstallError {
        let message = "Cancelled.".to_string();
        self.fail(&message);
        InstallError::Stopped(message)
    }

    fn wait_any_key(&mut self, lines: Vec<(ratatui::style::Style, String)>) {
        loop {
            let drawn = self.frame("Press any key to exit", |area, buf, theme, log| {
                let extra = lines.len() as u16;
                let log_area = Rect {
                    height: area.height.saturating_sub(extra),
                    ..area
                };
                log.render(log_area, buf, theme);
                paint_lines(
                    &lines,
                    Rect {
                        y: area.y + log_area.height,
                        height: extra.min(area.height),
                        ..area
                    },
                    buf,
                );
            });
            if drawn.is_err() {
                return;
            }
            match read_key() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {}
            }
        }
    }

    fn question_frame(
        &mut self,
        hint: &str,
        paint: impl FnOnce(Rect, &mut Buffer, &Theme),
    ) -> Result<(), InstallError> {
        self.frame(hint, |area, buf, theme, log| {
            let log_area = Rect {
                height: area.height.saturating_sub(2),
                ..area
            };
            log.render(log_area, buf, theme);
            let q = Rect {
                y: area.y + area.height.saturating_sub(1),
                height: area.height.min(1),
                ..area
            };
            paint(q, buf, theme);
        })
    }
}

impl Drop for TuiUi {
    fn drop(&mut self) {
        self.term = None;
    }
}

const PROGRESS_HELP: [(&str, &str); 7] = [
    ("full", "elapsed, cost, tokens and how many have finished"),
    ("quiet", "only who is being asked"),
    ("count", "how many have finished, and tool calls"),
    ("marks", "one mark per reviewer"),
    ("percent", "finished reviewers as a percentage"),
    (
        "latest",
        "the latest step of the most recently active reviewer",
    ),
    ("ticker", "elapsed time and cost so far"),
];

const SUMMARY_HELP: [(&str, &str); 4] = [
    ("dim", "dim text under Claude's reply"),
    ("italic", "markdown italics"),
    ("quote", "a markdown quote"),
    ("off", "no summary, and no hook running on every reply"),
];

fn help(table: &[(&str, &str)], name: &str) -> String {
    table
        .iter()
        .find(|(n, _)| *n == name)
        .map_or_else(String::new, |(_, h)| h.to_string())
}

fn cycle<T: Copy + PartialEq>(all: &[T], current: T, forward: bool) -> T {
    let i = all.iter().position(|x| *x == current).unwrap_or(0);
    let n = all.len();
    all[if forward {
        (i + 1) % n
    } else {
        (i + n - 1) % n
    }]
}

impl Ui for TuiUi {
    fn welcome(&mut self) -> Result<bool, InstallError> {
        let lines = vec![
            (self.theme.heading, BANNER.trim().to_string()),
            (self.theme.text, String::new()),
            (
                self.theme.text,
                "This asks for your OpenRouter API key and the models on the review panel,"
                    .to_string(),
            ),
            (
                self.theme.text,
                "shows what it will do, and does it only once you say so:".to_string(),
            ),
            (self.theme.text, String::new()),
            (
                self.theme.text,
                "  - copies claude-consult into its install dir".to_string(),
            ),
            (
                self.theme.text,
                "  - generates /consult, /cleanroom and a quick command per panel model"
                    .to_string(),
            ),
            (
                self.theme.text,
                "  - stores the key and adds its hooks to Claude Code's settings.json".to_string(),
            ),
            (
                self.theme.text,
                "  - starts the shared MCP service and registers it with Claude Code".to_string(),
            ),
            (self.theme.text, String::new()),
            (
                self.theme.dim,
                "Re-running is the upgrade path; your own files and hooks are kept.".to_string(),
            ),
        ];
        loop {
            self.frame("Enter start   Esc quit", |area, buf, _, _| {
                paint_lines(&lines, area, buf);
            })?;
            match self.key()? {
                Key::Enter => return Ok(true),
                Key::Esc | Key::CtrlC | Key::Char('q') => {
                    self.term = None;
                    self.finished = true;
                    return Ok(false);
                }
                _ => {}
            }
        }
    }

    fn report_step(&mut self, kind: StepKind, text: &str) {
        self.busy = None;
        self.log.push(kind, text);
        if !self.finished {
            let _ = self.frame("", |area, buf, theme, log| log.render(area, buf, theme));
        }
    }

    fn busy(&mut self, what: &str) {
        self.busy = Some(what.to_string());
        let _ = self.frame("", |area, buf, theme, log| log.render(area, buf, theme));
    }

    fn confirm(&mut self, question: &str, default: bool) -> Result<bool, InstallError> {
        self.busy = None;
        let mut c = Confirm::new(question, default);
        loop {
            self.question_frame(
                "y yes   n no   Enter the highlighted answer",
                |q, buf, theme| {
                    c.render(q, buf, theme);
                },
            )?;
            match c.handle(&self.key()?) {
                ConfirmOutcome::Pending => {}
                ConfirmOutcome::Answered(yes) => {
                    self.log.push(
                        StepKind::Info,
                        format!("{question} {}", if yes { "yes" } else { "no" }),
                    );
                    return Ok(yes);
                }
                ConfirmOutcome::Aborted => return Err(self.abort()),
            }
        }
    }

    fn enter_key(&mut self, prompt: &str) -> Result<String, InstallError> {
        self.busy = None;
        let mut input = TextInput::masked(prompt);
        loop {
            self.question_frame(
                "Enter confirm   Esc clear   Ctrl+C quit",
                |q, buf, theme| {
                    input.render(q, buf, theme);
                },
            )?;
            match input.handle(&self.key()?) {
                InputOutcome::Pending => {}
                InputOutcome::Submitted => return Ok(input.take_value()),
                InputOutcome::Cancelled => {
                    input.take_value();
                }
                InputOutcome::Aborted => return Err(self.abort()),
            }
        }
    }

    fn choose_panel(&mut self, request: &PanelRequest<'_>) -> Result<PanelAnswer, InstallError> {
        self.busy = None;
        let mut picker = Picker::new(
            request.rows.to_vec(),
            request.recommended,
            request.budget,
            3,
            request.offline,
        );
        loop {
            self.frame("", |area, buf, theme, _| {
                let height = usize::from(area.height)
                    .saturating_sub(consult_tui::picker::CHROME_LINES)
                    .max(1);
                picker.set_height(height);
                render_with(&picker, area, buf, theme);
            })?;
            let key = self.key()?;
            if key == REDRAW {
                continue;
            }
            picker.step(key);
            if picker.aborted() {
                return Ok(PanelAnswer::Cancelled);
            }
            if picker.done() {
                return Ok(PanelAnswer::Picked(picker.selected().to_vec()));
            }
        }
    }

    fn choose_display(&mut self, current: Display) -> Result<Display, InstallError> {
        self.busy = None;
        let mut d = current;
        let mut on_summary = false;
        loop {
            let (theme, focus) = (self.theme, on_summary);
            let lines = vec![
                (theme.heading, "Display styles".to_string()),
                (theme.text, String::new()),
                (
                    if focus { theme.text } else { theme.cursor },
                    format!(
                        "{} Progress, while a consult runs : < {:<7} >  {}",
                        if focus { " " } else { ">" },
                        d.progress.as_str(),
                        help(&PROGRESS_HELP, d.progress.as_str())
                    ),
                ),
                (
                    if focus { theme.cursor } else { theme.text },
                    format!(
                        "{} Summary, under the reply       : < {:<7} >  {}",
                        if focus { ">" } else { " " },
                        d.summary.as_str(),
                        help(&SUMMARY_HELP, d.summary.as_str())
                    ),
                ),
                (theme.text, String::new()),
                (
                    theme.dim,
                    "The installed styles are kept unless you change them here.".to_string(),
                ),
            ];
            self.frame(
                "Up/Down choose   Left/Right change   Enter confirm   Esc keep",
                |area, buf, _, _| paint_lines(&lines, area, buf),
            )?;
            match self.key()? {
                Key::Up | Key::Down | Key::Tab | Key::BackTab => on_summary = !on_summary,
                k @ (Key::Left | Key::Right | Key::Space) => {
                    let forward = k != Key::Left;
                    if on_summary {
                        d.summary = cycle(&SummaryStyle::ALL, d.summary, forward);
                    } else {
                        d.progress = cycle(&ProgressStyle::ALL, d.progress, forward);
                    }
                }
                Key::Enter => return Ok(d),
                Key::Esc => return Ok(current),
                Key::CtrlC => return Err(self.abort()),
                _ => {}
            }
        }
    }

    fn confirm_plan(&mut self, lines: &[String]) -> Result<bool, InstallError> {
        self.log.step("Ready to install");
        for line in lines {
            self.log.info(line);
        }
        self.confirm("Proceed?", true)
    }

    fn done(&mut self, headline: &str, lines: &[String]) {
        self.busy = None;
        let mut shown = vec![(self.theme.ok, format!("  {headline}"))];
        shown.extend(
            lines
                .iter()
                .map(|l| (self.theme.text, format!("       {l}"))),
        );
        self.wait_any_key(shown);
        self.log.ok(headline);
        for line in lines {
            self.log.info(line);
        }
        self.finish();
    }

    fn fail(&mut self, message: &str) {
        self.busy = None;
        self.log.fail(message);
        if !self.finished {
            self.wait_any_key(Vec::new());
        }
        self.finish();
    }
}
