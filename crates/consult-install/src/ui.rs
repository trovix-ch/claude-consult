//! What the install and uninstall flows ask and tell, as a trait: the flows own every
//! decision, a [`Ui`] only shows and asks.

use consult_core::catalog::Catalog;
use consult_core::generate::{Display, panel_keys};
use consult_tui::{PickerRow, StepKind};

use crate::InstallError;

/// The panel picker's inputs.
#[derive(Clone, Copy, Debug)]
pub struct PanelRequest<'a> {
    /// The favourites (the plain list shows their tier and what they play to).
    pub catalog: &'a Catalog,
    /// Every model on offer, favourites first ([`consult_tui::rows_from`]).
    pub rows: &'a [PickerRow],
    /// The recommended panel, ticked at the start.
    pub recommended: &'a [String],
    /// The budget panel, for Ctrl+B or `b`.
    pub budget: &'a [String],
    /// Whether the listing was unreachable: favourites only, no prices.
    pub offline: bool,
}

/// What a [`Ui`] made of the panel question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelAnswer {
    /// Chosen in the picker, which offers only models that may sit on a panel: taken as
    /// it is.
    Picked(Vec<String>),
    /// Typed, as in the plain list: the flow checks it and asks again if it fails.
    Typed(Vec<String>),
    /// Given up (Ctrl+C): the install stops.
    Cancelled,
}

/// The questions an install or uninstall asks, and the lines it reports.
///
/// Implementations: [`crate::TuiUi`] (the full-screen wizard), [`crate::PlainUi`]
/// (line prompts, for a console without the full screen or with redirected input), and
/// [`crate::UnattendedUi`] (asks nothing). A test drives the flows with its own.
///
/// Never hand a `Ui` the key in clear: the flows pass it only masked.
pub trait Ui {
    /// Whether this UI asks nothing, so the flow behaves as `--unattended`.
    fn unattended(&self) -> bool {
        false
    }

    /// The first screen. `false` quits before anything happens.
    fn welcome(&mut self) -> Result<bool, InstallError> {
        Ok(true)
    }

    /// One line of output: a section heading, `[ok]`, `[!!]` or detail.
    fn report_step(&mut self, kind: StepKind, text: &str);

    /// Something slow is running (a network fetch, the service starting). A UI may show
    /// it until the next call; the plain output shows nothing.
    fn busy(&mut self, _what: &str) {}

    /// A yes/no question. The flow never calls this in unattended mode; it takes the
    /// default itself.
    fn confirm(&mut self, question: &str, default: bool) -> Result<bool, InstallError>;

    /// The API key, typed hidden. Returned as typed; the flow trims it.
    fn enter_key(&mut self, prompt: &str) -> Result<String, InstallError>;

    /// The panel.
    fn choose_panel(&mut self, request: &PanelRequest<'_>) -> Result<PanelAnswer, InstallError>;

    /// Whether to replace the user's own status line (`theirs` is its command). The
    /// default reports the question's lines and asks with [`Ui::confirm`].
    fn status_line_question(&mut self, theirs: &str) -> Result<bool, InstallError> {
        self.report_step(StepKind::Step, "Status line");
        self.report_step(
            StepKind::Info,
            &format!("You already have a status line: {theirs}"),
        );
        self.report_step(
            StepKind::Info,
            "consult can show this session's consult calls and cost there instead.",
        );
        self.confirm("Replace your status line with it?", false)
    }

    /// The display styles, starting from `current`. The default keeps them.
    fn choose_display(&mut self, current: Display) -> Result<Display, InstallError> {
        Ok(current)
    }

    /// Shows the plan and asks whether to go ahead. The default reports the heading and
    /// the lines and asks `Proceed?`.
    fn confirm_plan(&mut self, lines: &[String]) -> Result<bool, InstallError> {
        self.report_step(StepKind::Step, "Ready to install");
        for line in lines {
            self.report_step(StepKind::Info, line);
        }
        self.confirm("Proceed?", true)
    }

    /// The last screen: a headline and what to do next.
    fn done(&mut self, headline: &str, lines: &[String]) {
        self.report_step(StepKind::Ok, headline);
        for line in lines {
            self.report_step(StepKind::Info, line);
        }
    }

    /// Why the run stopped; the flow returns [`InstallError::Stopped`] with the same
    /// text right after, so the caller need not show it again.
    fn fail(&mut self, message: &str) {
        self.report_step(StepKind::Fail, message);
    }
}

/// The plain list's answer as panel entries: empty is the recommended panel, `b` the
/// budget one, otherwise numbers (1-based, into `favourites`) and aliases or ids
/// separated by commas, semicolons or spaces. A favourite named by its id becomes its
/// alias. `Err(Some(note))` for a number out of range, `Err(None)` when nothing is left.
pub fn parse_panel_answer(
    answer: &str,
    catalog: &Catalog,
    favourites: &[&PickerRow],
    recommended: &[String],
    budget: &[String],
) -> Result<Vec<String>, Option<String>> {
    let answer = answer.trim().to_lowercase();
    if answer.is_empty() {
        return Ok(recommended.to_vec());
    }
    if answer == "b" {
        return Ok(budget.to_vec());
    }
    let mut chosen = Vec::new();
    for tok in answer
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        let Ok(n) = tok.parse::<i64>() else {
            chosen.push(tok.to_string());
            continue;
        };
        if n < 1 || n as usize > favourites.len() {
            return Err(Some(format!(
                "'{tok}' is not a number from 1 to {}.",
                favourites.len()
            )));
        }
        chosen.push(favourites[n as usize - 1].key.clone());
    }
    if chosen.is_empty() {
        return Err(None);
    }
    Ok(panel_keys(catalog, &chosen))
}
