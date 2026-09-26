//! Colours and styles.

use ratatui::style::{Color, Modifier, Style};

use crate::widgets::StepKind;

/// The styles every screen draws with. The defaults follow the PowerShell installer:
/// cyan headings and cursor, green `[ok]`, yellow `[!!]`, red `[xx]`, dark grey hints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Plain text.
    pub text: Style,
    /// The title bar.
    pub title: Style,
    /// Section headings (`==>`).
    pub heading: Style,
    /// `[ok]` lines.
    pub ok: Style,
    /// `[!!]` lines, and the picker's footer note.
    pub note: Style,
    /// `[xx]` lines.
    pub fail: Style,
    /// Hints, table headers, legends.
    pub dim: Style,
    /// The row under the cursor.
    pub cursor: Style,
    /// The highlighted answer of a question.
    pub choice: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            text: Style::new(),
            title: Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            heading: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ok: Style::new().fg(Color::Green),
            note: Style::new().fg(Color::Yellow),
            fail: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            dim: Style::new().fg(Color::DarkGray),
            cursor: Style::new().fg(Color::Cyan),
            choice: Style::new().fg(Color::Black).bg(Color::Cyan),
        }
    }
}

impl Theme {
    /// The style of a step-log line of this kind.
    pub fn for_kind(&self, kind: StepKind) -> Style {
        match kind {
            StepKind::Step => self.heading,
            StepKind::Ok => self.ok,
            StepKind::Note => self.note,
            StepKind::Info => self.text,
            StepKind::Fail => self.fail,
        }
    }
}
