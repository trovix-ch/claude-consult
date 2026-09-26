//! Small stateful widgets: a text input, a yes/no question, a step log and the frame
//! every wizard screen sits in.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use crate::keys::Key;
use crate::picker::clip;
use crate::theme::Theme;

// ---- step log ----------------------------------------------------------------------

/// The kind of a line of installer output, after the PowerShell installer's
/// `Write-Step`, `Write-Ok`, `Write-Note`, `Write-Info` and `Stop-Install`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StepKind {
    /// A section heading: `==> text`, after a blank line.
    Step,
    /// Something done: `  [ok] text`.
    Ok,
    /// Something to know, or a warning: `  [!!] text`.
    Note,
    /// Plain detail, indented to line up with the text of the others.
    Info,
    /// Why the run stopped: `  [xx] text`, after a blank line.
    Fail,
}

impl StepKind {
    /// The prefix before the text.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Step => "==> ",
            Self::Ok => "  [ok] ",
            Self::Note => "  [!!] ",
            Self::Info => "       ",
            Self::Fail => "  [xx] ",
        }
    }

    /// Whether a blank line goes before it.
    pub fn spaced(self) -> bool {
        matches!(self, Self::Step | Self::Fail)
    }
}

/// One line as the plain output prints it (without the blank line before a heading).
/// Text over several lines continues indented under the first line's text.
pub fn format_step(kind: StepKind, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            out.push(format!("{}{line}", kind.prefix()));
        } else {
            out.push(format!("{}{line}", StepKind::Info.prefix()));
        }
    }
    if out.is_empty() {
        out.push(kind.prefix().trim_end().to_string());
    }
    out
}

/// A running list of installer output, drawn with the latest line at the bottom.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepLog {
    entries: Vec<(StepKind, String)>,
}

impl StepLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a line.
    pub fn push(&mut self, kind: StepKind, text: impl Into<String>) {
        self.entries.push((kind, text.into()));
    }

    /// Adds a section heading.
    pub fn step(&mut self, text: impl Into<String>) {
        self.push(StepKind::Step, text);
    }

    /// Adds an `[ok]` line.
    pub fn ok(&mut self, text: impl Into<String>) {
        self.push(StepKind::Ok, text);
    }

    /// Adds an `[!!]` line.
    pub fn note(&mut self, text: impl Into<String>) {
        self.push(StepKind::Note, text);
    }

    /// Adds an indented detail line.
    pub fn info(&mut self, text: impl Into<String>) {
        self.push(StepKind::Info, text);
    }

    /// Adds an `[xx]` line.
    pub fn fail(&mut self, text: impl Into<String>) {
        self.push(StepKind::Fail, text);
    }

    /// Every entry, oldest first.
    pub fn entries(&self) -> &[(StepKind, String)] {
        &self.entries
    }

    /// The most recent heading, if any.
    pub fn section(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| *k == StepKind::Step)
            .map(|(_, t)| t.as_str())
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The log as text, with its kinds: a blank line before each heading and failure
    /// (except at the very top).
    pub fn lines(&self) -> Vec<(StepKind, String)> {
        let mut out = Vec::new();
        for (kind, text) in &self.entries {
            if kind.spaced() && !out.is_empty() {
                out.push((StepKind::Info, String::new()));
            }
            out.extend(format_step(*kind, text).into_iter().map(|l| (*kind, l)));
        }
        out
    }

    /// Paints the last lines that fit into `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let lines = self.lines();
        let height = usize::from(area.height);
        let skip = lines.len().saturating_sub(height);
        let width = usize::from(area.width);
        for (i, (kind, line)) in lines.iter().skip(skip).enumerate() {
            buf.set_stringn(
                area.x,
                area.y + i as u16,
                clip(line, width),
                width,
                theme.for_kind(*kind),
            );
        }
    }
}

// ---- text input ----------------------------------------------------------------------

/// What a key did to a [`TextInput`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputOutcome {
    /// Still editing.
    Pending,
    /// Enter: the value is final.
    Submitted,
    /// Esc: give up on this input.
    Cancelled,
    /// Ctrl+C: quit.
    Aborted,
}

/// A one-line text input. In masked mode (the API key) it shows one `*` per character
/// and never the text.
#[derive(Clone, PartialEq, Eq)]
pub struct TextInput {
    label: String,
    value: Vec<char>,
    cursor: usize,
    masked: bool,
}

// The value may be the API key: Debug never shows it.
impl std::fmt::Debug for TextInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextInput")
            .field("label", &self.label)
            .field("len", &self.value.len())
            .field("cursor", &self.cursor)
            .field("masked", &self.masked)
            .finish()
    }
}

impl TextInput {
    /// A plain input.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: Vec::new(),
            cursor: 0,
            masked: false,
        }
    }

    /// A masked input.
    pub fn masked(label: impl Into<String>) -> Self {
        Self {
            masked: true,
            ..Self::new(label)
        }
    }

    /// The label shown before the value.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether it is masked.
    pub fn is_masked(&self) -> bool {
        self.masked
    }

    /// The value as typed.
    pub fn value(&self) -> String {
        self.value.iter().collect()
    }

    /// Takes the value out, leaving the input empty.
    pub fn take_value(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.value).into_iter().collect()
    }

    /// Sets the value, the cursor at its end.
    pub fn set_value(&mut self, value: &str) {
        self.value = value.chars().collect();
        self.cursor = self.value.len();
    }

    /// The cursor, in characters from the start.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// What is shown for the value: the text, or one `*` per character when masked.
    pub fn display(&self) -> String {
        if self.masked {
            "*".repeat(self.value.len())
        } else {
            self.value()
        }
    }

    /// Applies one key.
    pub fn handle(&mut self, key: &Key) -> InputOutcome {
        match key {
            Key::Char(c) => {
                self.value.insert(self.cursor, *c);
                self.cursor += 1;
            }
            Key::Space => {
                self.value.insert(self.cursor, ' ');
                self.cursor += 1;
            }
            Key::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.value.remove(self.cursor);
            }
            Key::Delete if self.cursor < self.value.len() => {
                self.value.remove(self.cursor);
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.value.len()),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.value.len(),
            Key::Enter => return InputOutcome::Submitted,
            Key::Esc => return InputOutcome::Cancelled,
            Key::CtrlC => return InputOutcome::Aborted,
            _ => {}
        }
        InputOutcome::Pending
    }

    /// Paints `label: value_` on the first line of `area`, the cursor as a reversed cell.
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let head = format!("{}: ", self.label);
        let shown: Vec<char> = self.display().chars().collect();
        let text: String = shown.iter().collect();
        let line = format!("{head}{text}");
        buf.set_stringn(area.x, area.y, clip(&line, width), width, theme.text);
        let at = head.chars().count() + self.cursor;
        if at < width {
            let under = shown.get(self.cursor).copied().unwrap_or(' ');
            buf.set_stringn(
                area.x + at as u16,
                area.y,
                under.to_string(),
                1,
                theme.choice,
            );
        }
    }
}

// ---- confirm ---------------------------------------------------------------------------

/// What a key did to a [`Confirm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// No answer yet.
    Pending,
    /// Answered.
    Answered(bool),
    /// Ctrl+C: quit.
    Aborted,
}

/// A yes/no question with a default, as the installer's `Confirm-Choice`: `y`/`n`
/// answer at once, Enter takes the highlighted answer (the default until moved), the
/// arrows and Tab move the highlight, Esc answers no.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    question: String,
    default: bool,
    choice: bool,
}

impl Confirm {
    /// A question with its default answer.
    pub fn new(question: impl Into<String>, default: bool) -> Self {
        Self {
            question: question.into(),
            default,
            choice: default,
        }
    }

    /// The question.
    pub fn question(&self) -> &str {
        &self.question
    }

    /// The default answer.
    pub fn default_answer(&self) -> bool {
        self.default
    }

    /// The highlighted answer.
    pub fn choice(&self) -> bool {
        self.choice
    }

    /// `[Y/n]` or `[y/N]`.
    pub fn hint(&self) -> &'static str {
        if self.default { "[Y/n]" } else { "[y/N]" }
    }

    /// Applies one key.
    pub fn handle(&mut self, key: &Key) -> ConfirmOutcome {
        match key {
            Key::Char('y' | 'Y') => ConfirmOutcome::Answered(true),
            Key::Char('n' | 'N') => ConfirmOutcome::Answered(false),
            Key::Enter => ConfirmOutcome::Answered(self.choice),
            Key::Esc => ConfirmOutcome::Answered(false),
            Key::CtrlC => ConfirmOutcome::Aborted,
            Key::Left | Key::Right | Key::Up | Key::Down | Key::Tab | Key::BackTab => {
                self.choice = !self.choice;
                ConfirmOutcome::Pending
            }
            _ => ConfirmOutcome::Pending,
        }
    }

    /// Paints `question [Y/n]   Yes   No` on the first line of `area`.
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if area.height == 0 {
            return;
        }
        let width = usize::from(area.width);
        let head = format!("{} {}   ", self.question, self.hint());
        buf.set_stringn(area.x, area.y, clip(&head, width), width, theme.text);
        let mut x = head.chars().count();
        for (label, value) in [(" Yes ", true), (" No ", false)] {
            if x >= width {
                break;
            }
            let style = if self.choice == value {
                theme.choice
            } else {
                theme.dim
            };
            buf.set_stringn(area.x + x as u16, area.y, label, width - x, style);
            x += label.chars().count() + 2;
        }
    }
}

// ---- wizard frame -------------------------------------------------------------------------

/// The frame of a wizard screen: a title bar, the body, and a hint line at the bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wizard<'a> {
    /// Left of the title bar.
    pub title: &'a str,
    /// Right of the title bar: the current section.
    pub section: &'a str,
    /// The bottom line: which keys do what.
    pub hint: &'a str,
}

impl Wizard<'_> {
    /// Paints the frame and returns the body's area (everything between the title bar
    /// and the hint line, with a one-column margin).
    pub fn render(&self, area: Rect, buf: &mut Buffer, theme: &Theme) -> Rect {
        if area.height < 3 || area.width < 4 {
            return Rect::new(area.x, area.y, 0, 0);
        }
        let width = usize::from(area.width);
        let bar = Rect::new(area.x, area.y, area.width, 1);
        buf.set_style(bar, theme.title);
        let left = format!(" {}", self.title);
        let right = format!("{} ", self.section);
        buf.set_stringn(area.x, area.y, clip(&left, width), width, theme.title);
        let (lw, rw) = (left.chars().count(), right.chars().count());
        if lw + rw + 2 <= width {
            buf.set_stringn(
                area.x + (width - rw) as u16,
                area.y,
                &right,
                rw,
                theme.title,
            );
        }
        let hint_y = area.y + area.height - 1;
        buf.set_stringn(
            area.x,
            hint_y,
            clip(&format!(" {}", self.hint), width),
            width,
            theme.dim,
        );
        Rect::new(
            area.x + 1,
            area.y + 2,
            area.width.saturating_sub(2),
            area.height.saturating_sub(4),
        )
    }
}

/// Paints lines of text into `area`, clipped, top to bottom, one style each.
pub fn paint_lines(lines: &[(Style, String)], area: Rect, buf: &mut Buffer) {
    let width = usize::from(area.width);
    for (i, (style, line)) in lines.iter().take(usize::from(area.height)).enumerate() {
        buf.set_stringn(area.x, area.y + i as u16, clip(line, width), width, *style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn step_lines_carry_the_installer_prefixes() {
        let mut log = StepLog::new();
        log.step("Checking prerequisites");
        log.ok("git found");
        log.note("careful\nsecond line");
        log.info("detail");
        log.step("Next");
        log.fail("Cancelled.");
        let text: Vec<String> = log.lines().into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            text,
            [
                "==> Checking prerequisites",
                "  [ok] git found",
                "  [!!] careful",
                "       second line",
                "       detail",
                "",
                "==> Next",
                "",
                "  [xx] Cancelled.",
            ]
        );
        assert_eq!(log.section(), Some("Next"));
    }

    #[test]
    fn step_log_shows_the_latest_lines() {
        let mut log = StepLog::new();
        for i in 0..10 {
            log.ok(format!("line {i}"));
        }
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        log.render(area, &mut buf, &Theme::default());
        assert!(row(&buf, 0).starts_with("  [ok] line 7"));
        assert!(row(&buf, 2).starts_with("  [ok] line 9"));
        assert_eq!(buf[(3, 2)].fg, Color::Green);
    }

    #[test]
    fn text_input_edits_and_masks() {
        let mut t = TextInput::masked("OpenRouter API key");
        for c in "abcd".chars() {
            assert_eq!(t.handle(&Key::Char(c)), InputOutcome::Pending);
        }
        t.handle(&Key::Left);
        t.handle(&Key::Backspace);
        t.handle(&Key::Space);
        assert_eq!(t.value(), "ab d");
        assert_eq!(t.display(), "****");
        assert!(!format!("{t:?}").contains("ab d"));
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        t.render(area, &mut buf, &Theme::default());
        assert!(row(&buf, 0).starts_with("OpenRouter API key: ****"));
        assert!(!row(&buf, 0).contains("ab"));
        t.handle(&Key::Home);
        t.handle(&Key::Delete);
        t.handle(&Key::End);
        assert_eq!(t.value(), "b d");
        assert_eq!(t.cursor(), 3);
        assert_eq!(t.handle(&Key::Enter), InputOutcome::Submitted);
        assert_eq!(t.take_value(), "b d");
        assert_eq!(t.value(), "");
        assert_eq!(t.handle(&Key::Esc), InputOutcome::Cancelled);
        assert_eq!(t.handle(&Key::CtrlC), InputOutcome::Aborted);
        let mut plain = TextInput::new("Panel");
        plain.set_value("x,y");
        assert_eq!(plain.display(), "x,y");
    }

    #[test]
    fn confirm_answers_like_confirm_choice() {
        let mut c = Confirm::new("Proceed?", true);
        assert_eq!(c.hint(), "[Y/n]");
        assert_eq!(c.handle(&Key::Enter), ConfirmOutcome::Answered(true));
        assert_eq!(c.handle(&Key::Char('n')), ConfirmOutcome::Answered(false));
        assert_eq!(c.handle(&Key::Char('x')), ConfirmOutcome::Pending);
        assert_eq!(c.handle(&Key::Right), ConfirmOutcome::Pending);
        assert_eq!(c.handle(&Key::Enter), ConfirmOutcome::Answered(false));
        assert_eq!(c.handle(&Key::CtrlC), ConfirmOutcome::Aborted);
        let mut c = Confirm::new("Use it anyway?", false);
        assert_eq!(c.hint(), "[y/N]");
        assert_eq!(c.handle(&Key::Enter), ConfirmOutcome::Answered(false));
        assert_eq!(c.handle(&Key::Char('Y')), ConfirmOutcome::Answered(true));
        assert_eq!(c.handle(&Key::Esc), ConfirmOutcome::Answered(false));
        let area = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(area);
        c.render(area, &mut buf, &Theme::default());
        assert!(row(&buf, 0).starts_with("Use it anyway? [y/N]    Yes    No "));
    }

    #[test]
    fn wizard_frame_leaves_a_body() {
        let area = Rect::new(0, 0, 50, 12);
        let mut buf = Buffer::empty(area);
        let body = Wizard {
            title: "claude-consult",
            section: "OpenRouter API key",
            hint: "Enter confirm",
        }
        .render(area, &mut buf, &Theme::default());
        assert_eq!(body, Rect::new(1, 2, 48, 8));
        assert!(row(&buf, 0).starts_with(" claude-consult"));
        assert!(row(&buf, 0).ends_with("OpenRouter API key "));
        assert!(row(&buf, 11).starts_with(" Enter confirm"));
    }
}
