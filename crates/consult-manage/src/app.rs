//! The management TUI's state: which screen is up, what each screen holds, which
//! question is open, which jobs run. [`App::handle_key`] and [`App::tick`] are the only
//! ways it changes; [`crate::ui`] draws it.
//!
//! Anything that may block runs as a job on a thread of its own and reports back over a
//! channel, so a key is always handled at once. At most one job that changes the
//! machine runs at a time.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use consult_core::catalog::{self, Catalog};
use consult_core::display::{ProgressStyle, SummaryStyle};
use consult_core::generate::Display;
use consult_core::key::{KeyCheck, clean_key, looks_like_key, validate_key_format};
use consult_tui::picker::CHROME_LINES;
use consult_tui::{
    Confirm, ConfirmOutcome, InputOutcome, Key, Picker, StepKind, StepLog, TextInput, rows_from,
};
use indexmap::IndexMap;
use serde_json::Value;

use crate::actions::{Backend, CatalogVerdict, Reinstall, Secret, ServiceAction, StatusReport};
use crate::sessions::Session;

/// A screen, one per tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Screen {
    /// The install at a glance.
    Status,
    /// Re-pick the panel.
    Panel,
    /// Rotate the key.
    Key,
    /// Progress and summary styles.
    Display,
    /// Per-session consult totals.
    Sessions,
    /// Check the favourites against OpenRouter.
    Catalog,
    /// The scheduled task.
    Service,
    /// Remove everything.
    Uninstall,
}

impl Screen {
    /// In tab order.
    pub const ALL: [Screen; 8] = [
        Self::Status,
        Self::Panel,
        Self::Key,
        Self::Display,
        Self::Sessions,
        Self::Catalog,
        Self::Service,
        Self::Uninstall,
    ];

    /// The tab's name.
    pub fn title(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Panel => "Panel",
            Self::Key => "Key",
            Self::Display => "Display",
            Self::Sessions => "Sessions",
            Self::Catalog => "Catalog",
            Self::Service => "Service",
            Self::Uninstall => "Uninstall",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
}

/// What each progress style shows, as the README's table says.
pub const PROGRESS_HELP: [(ProgressStyle, &str); 7] = [
    (
        ProgressStyle::Full,
        "elapsed time, cost, tokens in and out, reviewers finished",
    ),
    (
        ProgressStyle::Ticker,
        "elapsed time, cost so far, reviewers finished",
    ),
    (
        ProgressStyle::Count,
        "reviewers finished, tool calls so far",
    ),
    (
        ProgressStyle::Marks,
        "each reviewer's step while it works, then its mark",
    ),
    (
        ProgressStyle::Latest,
        "the newest step of the most recently active reviewer",
    ),
    (
        ProgressStyle::Percent,
        "reviewers finished, with Claude Code's own percentage after it",
    ),
    (ProgressStyle::Quiet, "only who was asked"),
];

/// What each summary style shows, as the README's table says.
pub const SUMMARY_HELP: [(SummaryStyle, &str); 4] = [
    (SummaryStyle::Dim, "dimmed, coloured marks"),
    (SummaryStyle::Italic, "markdown italics, no colour"),
    (SummaryStyle::Quote, "a markdown quote, no colour"),
    (
        SummaryStyle::Off,
        "none, and its display hook isn't registered at all",
    ),
];

/// Rows the frame takes from the terminal around a screen's body: the title bar, the
/// screen heading and its blank line, the message line and the key hints.
pub const FRAME_ROWS: u16 = 5;

/// What a finished job hands back.
#[derive(Debug)]
enum Outcome {
    Status(Box<StatusReport>),
    Listing(Option<IndexMap<String, Value>>),
    KeyCheck(KeyCheck),
    Changed(Result<String, String>),
    Catalog(CatalogVerdict),
    Sessions(Result<Vec<Session>, String>),
    Uninstalled(Result<String, String>),
    /// A job that has nothing to hand back failed.
    Failed(String),
}

enum Msg {
    Line(StepKind, String),
    Done(Outcome),
}

/// A job's kind: at most one of each runs, and at most one that writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobKind {
    Status,
    Listing,
    KeyCheck,
    Reinstall,
    Service,
    Catalog,
    Sessions,
    Uninstall,
}

impl JobKind {
    fn writes(self) -> bool {
        matches!(self, Self::Reinstall | Self::Service | Self::Uninstall)
    }
}

#[derive(Debug)]
struct Running {
    id: u64,
    kind: JobKind,
    screen: Screen,
    label: String,
}

/// What a yes to the open question does.
#[derive(Clone, Debug)]
enum Then {
    Panel(Vec<String>),
    KeyUnusual,
    KeyCheck,
    KeySave,
    Display(Display),
    DeleteSession(String),
    Service(ServiceAction),
    /// Re-register was confirmed; yes goes through the administrator prompt.
    ServiceElevate,
    UninstallFirst,
    UninstallSecond,
    UninstallKey,
}

/// A yes/no question, and what it leads to.
#[derive(Debug)]
pub struct Ask {
    /// The question.
    pub confirm: Confirm,
    then: Then,
}

/// The panel screen.
#[derive(Debug, Default)]
pub struct PanelState {
    /// The picker, while re-picking.
    pub picker: Option<Picker>,
    /// The last re-install's output.
    pub log: StepLog,
}

/// The key screen.
#[derive(Debug, Default)]
pub struct KeyState {
    /// The masked input, while typing a new key.
    pub input: Option<TextInput>,
    /// A new key on its way to being saved. Never drawn but masked.
    pub pending: Option<Secret>,
    allow_unusual: bool,
    /// The checks' and the re-install's output.
    pub log: StepLog,
}

/// The display screen.
#[derive(Debug, Default)]
pub struct DisplayState {
    /// The row under the cursor: the progress styles, then the summary styles.
    pub cursor: usize,
    /// The styles chosen so far; `None` until the status arrives.
    pub chosen: Option<Display>,
    /// The last re-install's output.
    pub log: StepLog,
}

/// The sessions screen.
#[derive(Debug, Default)]
pub struct SessionsState {
    /// The sessions, once read.
    pub list: Option<Result<Vec<Session>, String>>,
    /// The row under the cursor.
    pub cursor: usize,
}

/// The service screen.
#[derive(Debug, Default)]
pub struct ServiceState {
    /// The action under the cursor.
    pub cursor: usize,
    /// The last action's output.
    pub log: StepLog,
}

/// The uninstall screen.
#[derive(Debug, Default)]
pub struct UninstallState {
    /// Its output.
    pub log: StepLog,
    /// Uninstalled: the next key quits.
    pub done: bool,
}

/// The whole TUI's state.
pub struct App {
    backend: Arc<dyn Backend>,
    catalog: Result<Catalog, String>,
    screen: Screen,
    tx: Sender<(u64, Msg)>,
    rx: Receiver<(u64, Msg)>,
    next_id: u64,
    running: Vec<Running>,
    spinner: usize,
    quit: bool,
    help: bool,
    message: Option<(StepKind, String)>,
    ask: Option<Ask>,
    height: u16,
    /// The last status snapshot.
    pub status: Option<StatusReport>,
    /// The panel screen.
    pub panel: PanelState,
    /// The key screen.
    pub key: KeyState,
    /// The display screen.
    pub display: DisplayState,
    /// The sessions screen.
    pub sessions: SessionsState,
    /// The catalog screen's last verdict.
    pub catalog_check: Option<CatalogVerdict>,
    /// The service screen.
    pub service: ServiceState,
    /// The uninstall screen.
    pub uninstall: UninstallState,
}

impl App {
    /// The app on `backend`, with a status refresh already running.
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            backend,
            catalog: catalog::embedded().map_err(|e| e.to_string()),
            screen: Screen::Status,
            tx,
            rx,
            next_id: 0,
            running: Vec::new(),
            spinner: 0,
            quit: false,
            help: false,
            message: None,
            ask: None,
            height: 24,
            status: None,
            panel: PanelState::default(),
            key: KeyState::default(),
            display: DisplayState::default(),
            sessions: SessionsState::default(),
            catalog_check: None,
            service: ServiceState::default(),
            uninstall: UninstallState::default(),
        };
        app.refresh_status();
        app
    }

    // ---- read access for the renderer ------------------------------------------------

    /// The screen shown.
    pub fn screen(&self) -> Screen {
        self.screen
    }

    /// Whether the app is done.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether the help overlay is up.
    pub fn help_shown(&self) -> bool {
        self.help
    }

    /// The open question, if any.
    pub fn ask(&self) -> Option<&Ask> {
        self.ask.as_ref()
    }

    /// The message line: the last thing worth saying.
    pub fn message(&self) -> Option<&(StepKind, String)> {
        self.message.as_ref()
    }

    /// What runs, for the spinner: the labels of the running jobs.
    pub fn busy(&self) -> Vec<&str> {
        self.running.iter().map(|r| r.label.as_str()).collect()
    }

    /// The spinner's frame.
    pub fn spinner(&self) -> char {
        ['|', '/', '-', '\\'][self.spinner % 4]
    }

    /// Whether this screen has the shared service at all.
    pub fn service_supported(&self) -> bool {
        self.backend.service_supported()
    }

    /// The favourites, if the embedded catalog is valid.
    pub fn catalog(&self) -> Option<&Catalog> {
        self.catalog.as_ref().ok()
    }

    /// The exit code for the process.
    pub fn exit_code(&self) -> i32 {
        0
    }

    /// Whether a job of this screen runs.
    pub fn screen_busy(&self, screen: Screen) -> bool {
        self.running.iter().any(|r| r.screen == screen)
    }

    // ---- driving --------------------------------------------------------------------------

    /// The terminal's size changed (or is known for the first time): the picker's
    /// viewport follows it.
    pub fn resize(&mut self, height: u16) {
        self.height = height;
        if let Some(p) = self.panel.picker.as_mut() {
            p.set_height(Self::picker_rows(height));
        }
    }

    fn picker_rows(height: u16) -> usize {
        usize::from(height.saturating_sub(FRAME_ROWS)).saturating_sub(CHROME_LINES)
    }

    /// Takes in what finished jobs sent, and moves the spinner. Never blocks.
    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        while let Ok((id, msg)) = self.rx.try_recv() {
            self.receive(id, msg);
        }
    }

    /// Waits until every job has finished, taking in what they send. For tests: the
    /// real loop only ever calls [`App::tick`].
    #[cfg(test)]
    pub(crate) fn settle(&mut self) {
        while !self.running.is_empty() {
            match self.rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok((id, msg)) => self.receive(id, msg),
                Err(_) => panic!("a job did not finish"),
            }
        }
    }

    /// Applies one key.
    pub fn handle_key(&mut self, key: Key) {
        if self.help {
            self.help = false;
            return;
        }
        if self.uninstall.done {
            self.quit = true;
            return;
        }
        if key == Key::CtrlC {
            self.try_quit();
            return;
        }
        if let Some(ask) = self.ask.as_mut() {
            match ask.confirm.handle(&key) {
                ConfirmOutcome::Pending => {}
                ConfirmOutcome::Answered(yes) => {
                    if let Some(ask) = self.ask.take() {
                        self.resolve(ask.then, yes);
                    }
                }
                ConfirmOutcome::Aborted => self.try_quit(),
            }
            return;
        }
        if self.panel.picker.is_some() && self.screen == Screen::Panel {
            self.picker_key(key);
            return;
        }
        if self.key.input.is_some() && self.screen == Screen::Key {
            self.key_input(key);
            return;
        }
        match key {
            Key::Char('q') | Key::Esc => self.try_quit(),
            Key::Char('?') => self.help = true,
            Key::Tab | Key::Right => self.go(Screen::ALL[(self.screen.index() + 1) % 8]),
            Key::BackTab | Key::Left => self.go(Screen::ALL[(self.screen.index() + 7) % 8]),
            Key::Char(c @ '1'..='8') => {
                let n = c as usize - '1' as usize;
                self.go(Screen::ALL[n]);
            }
            other => self.screen_key(other),
        }
    }

    fn try_quit(&mut self) {
        if self.running.iter().any(|r| r.kind.writes()) {
            self.say(
                StepKind::Note,
                "A change is still running; quit once it has finished.",
            );
        } else {
            self.quit = true;
        }
    }

    fn say(&mut self, kind: StepKind, text: impl Into<String>) {
        self.message = Some((kind, text.into()));
    }

    fn go(&mut self, screen: Screen) {
        self.screen = screen;
        self.message = None;
        if screen == Screen::Sessions && self.sessions.list.is_none() {
            self.load_sessions();
        }
    }

    fn screen_key(&mut self, key: Key) {
        match (self.screen, key) {
            (_, Key::Char('r')) => self.refresh(),
            (Screen::Panel, Key::Enter) => self.start_picker(),
            (Screen::Key, Key::Enter) => {
                self.key.input = Some(TextInput::masked("New OpenRouter API key"));
                self.say(StepKind::Info, "Paste the key; it is shown as stars.");
            }
            (Screen::Display, Key::Up) => {
                self.display.cursor = self.display.cursor.saturating_sub(1);
            }
            (Screen::Display, Key::Down) => {
                self.display.cursor =
                    (self.display.cursor + 1).min(PROGRESS_HELP.len() + SUMMARY_HELP.len() - 1);
            }
            (Screen::Display, Key::Space) => self.pick_style(),
            (Screen::Display, Key::Enter) => self.apply_display(),
            (Screen::Sessions, Key::Up) => {
                self.sessions.cursor = self.sessions.cursor.saturating_sub(1);
            }
            (Screen::Sessions, Key::Down) => {
                let n = self.session_count();
                self.sessions.cursor = (self.sessions.cursor + 1).min(n.saturating_sub(1));
            }
            (Screen::Sessions, Key::Char('d') | Key::Delete) => self.ask_delete_session(),
            (Screen::Catalog, Key::Enter) => self.run_catalog(),
            (Screen::Service, Key::Up) => {
                self.service.cursor = self.service.cursor.saturating_sub(1);
            }
            (Screen::Service, Key::Down) => {
                self.service.cursor = (self.service.cursor + 1).min(ServiceAction::ALL.len() - 1);
            }
            (Screen::Service, Key::Enter) => self.ask_service(),
            (Screen::Uninstall, Key::Enter) => self.open(
                Confirm::new(
                    "Uninstall claude-consult: the service, the MCP registration, the generated commands, the hooks and the install dir?",
                    false,
                ),
                Then::UninstallFirst,
            ),
            _ => {}
        }
    }

    fn refresh(&mut self) {
        match self.screen {
            Screen::Sessions => self.load_sessions(),
            Screen::Catalog => self.run_catalog(),
            _ => self.refresh_status(),
        }
    }

    fn open(&mut self, confirm: Confirm, then: Then) {
        self.ask = Some(Ask { confirm, then });
    }

    // ---- jobs ----------------------------------------------------------------------------

    fn spawn<F>(&mut self, kind: JobKind, screen: Screen, label: &str, job: F) -> bool
    where
        F: FnOnce(&dyn Backend, &mut dyn FnMut(StepKind, &str)) -> Outcome + Send + 'static,
    {
        if self.running.iter().any(|r| r.kind == kind) {
            return false;
        }
        if kind.writes() && self.running.iter().any(|r| r.kind.writes()) {
            self.say(StepKind::Note, "Another change is still running.");
            return false;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.running.push(Running {
            id,
            kind,
            screen,
            label: label.to_string(),
        });
        let tx = self.tx.clone();
        let backend = Arc::clone(&self.backend);
        std::thread::spawn(move || {
            let line_tx = tx.clone();
            let mut sink = move |k: StepKind, text: &str| {
                let _ = line_tx.send((id, Msg::Line(k, text.to_string())));
            };
            let outcome = catch_unwind(AssertUnwindSafe(|| job(backend.as_ref(), &mut sink)));
            let outcome = outcome.unwrap_or_else(|_| failed(kind, "the job panicked"));
            // The app may be gone (quit mid-job); nothing is left to tell then.
            let _ = tx.send((id, Msg::Done(outcome)));
        });
        true
    }

    fn receive(&mut self, id: u64, msg: Msg) {
        let Some(pos) = self.running.iter().position(|r| r.id == id) else {
            return;
        };
        let screen = self.running[pos].screen;
        match msg {
            Msg::Line(kind, text) => {
                if let Some(log) = self.log_of(screen) {
                    log.push(kind, text);
                }
            }
            Msg::Done(outcome) => {
                self.running.remove(pos);
                self.finish(screen, outcome);
            }
        }
    }

    fn log_of(&mut self, screen: Screen) -> Option<&mut StepLog> {
        match screen {
            Screen::Panel => Some(&mut self.panel.log),
            Screen::Key => Some(&mut self.key.log),
            Screen::Display => Some(&mut self.display.log),
            Screen::Service => Some(&mut self.service.log),
            Screen::Uninstall => Some(&mut self.uninstall.log),
            _ => None,
        }
    }

    fn finish(&mut self, screen: Screen, outcome: Outcome) {
        match outcome {
            Outcome::Status(report) => {
                if self.display.chosen.is_none() {
                    self.display.chosen = Some(report.display);
                    self.display.cursor = PROGRESS_HELP
                        .iter()
                        .position(|(s, _)| *s == report.display.progress)
                        .unwrap_or(0);
                }
                self.status = Some(*report);
            }
            Outcome::Listing(live) => self.open_picker(live),
            Outcome::KeyCheck(check) => self.key_checked(check),
            Outcome::Changed(result) => {
                if let Some(log) = self.log_of(screen) {
                    match &result {
                        Ok(h) => log.ok(h.clone()),
                        Err(e) => log.fail(e.clone()),
                    }
                }
                match result {
                    Ok(h) => self.say(StepKind::Ok, h),
                    Err(e) => self.say(StepKind::Fail, e),
                }
                // Styles shown as chosen are now the installed ones, or are again.
                self.display.chosen = None;
                self.refresh_status();
            }
            Outcome::Catalog(v) => {
                let text = match v.code {
                    0 => "Every favourite checks out.",
                    1 => "At least one favourite has a problem.",
                    _ => "Nothing was checked: the listing could not be read.",
                };
                self.say(
                    if v.code == 0 {
                        StepKind::Ok
                    } else {
                        StepKind::Note
                    },
                    text,
                );
                self.catalog_check = Some(v);
            }
            Outcome::Sessions(list) => {
                let n = list.as_ref().map_or(0, Vec::len);
                self.sessions.cursor = self.sessions.cursor.min(n.saturating_sub(1));
                self.sessions.list = Some(list);
            }
            Outcome::Failed(why) => self.say(StepKind::Fail, why),
            Outcome::Uninstalled(result) => match result {
                Ok(h) => {
                    self.uninstall.log.ok(h);
                    self.uninstall.done = true;
                    self.say(StepKind::Ok, "Uninstalled. Press any key to exit.");
                }
                Err(e) => {
                    self.uninstall.log.fail(e.clone());
                    self.say(StepKind::Fail, e);
                }
            },
        }
    }

    fn refresh_status(&mut self) {
        self.spawn(
            JobKind::Status,
            Screen::Status,
            "Reading the status",
            |b, _| Outcome::Status(Box::new(b.status())),
        );
    }

    fn load_sessions(&mut self) {
        self.spawn(
            JobKind::Sessions,
            Screen::Sessions,
            "Reading the sessions",
            |b, _| Outcome::Sessions(b.sessions()),
        );
    }

    fn run_catalog(&mut self) {
        if self.spawn(
            JobKind::Catalog,
            Screen::Catalog,
            "Checking the favourites against OpenRouter",
            |b, _| Outcome::Catalog(b.catalog_check()),
        ) {
            self.say(StepKind::Info, "Checking ...");
        }
    }

    fn reinstall(&mut self, screen: Screen, request: Reinstall) {
        if let Some(log) = self.log_of(screen) {
            log.clear();
        }
        self.spawn(
            JobKind::Reinstall,
            screen,
            "Re-installing",
            move |b, log| Outcome::Changed(b.reinstall(&request, log)),
        );
    }

    // ---- panel ----------------------------------------------------------------------------

    fn start_picker(&mut self) {
        if let Err(e) = &self.catalog {
            let e = format!("The built-in catalog is invalid: {e}");
            self.say(StepKind::Fail, e);
            return;
        }
        if self.spawn(
            JobKind::Listing,
            Screen::Panel,
            "Fetching OpenRouter's model listing",
            |b, _| Outcome::Listing(b.fetch_listing()),
        ) {
            self.say(StepKind::Info, "Fetching OpenRouter's model listing ...");
        }
    }

    fn open_picker(&mut self, live: Option<IndexMap<String, Value>>) {
        let Ok(catalog) = &self.catalog else {
            return;
        };
        let rows = rows_from(catalog, live.as_ref());
        // The picker's keys: a favourite by alias, an outside model by its id.
        let current: Vec<String> = match self.status.as_ref().map(|s| &s.panel) {
            Some(Ok(members)) => members
                .iter()
                .map(|m| {
                    if catalog.models.contains_key(&m.alias) {
                        m.alias.clone()
                    } else {
                        m.id.clone().unwrap_or_else(|| m.alias.clone())
                    }
                })
                .collect(),
            _ => catalog.default_panel.clone(),
        };
        let picker = Picker::new(
            rows,
            &catalog.default_panel,
            &catalog.budget_panel,
            Self::picker_rows(self.height),
            live.is_none(),
        )
        .with_selected(&current);
        self.say(
            StepKind::Info,
            if live.is_none() {
                "OpenRouter's listing is unreachable: favourites only, without prices."
            } else {
                "Space ticks, Enter confirms; Esc clears the filter, then closes."
            },
        );
        self.panel.picker = Some(picker);
    }

    fn picker_key(&mut self, key: Key) {
        let Some(picker) = self.panel.picker.as_mut() else {
            return;
        };
        if key == Key::Esc && picker.filter().is_empty() {
            self.panel.picker = None;
            self.say(StepKind::Info, "Panel unchanged.");
            return;
        }
        picker.step(key);
        if picker.done() {
            let chosen = picker.selected().to_vec();
            self.panel.picker = None;
            self.open(
                Confirm::new(
                    format!("Re-install with the panel {}?", chosen.join(", ")),
                    true,
                ),
                Then::Panel(chosen),
            );
        }
    }

    // ---- key --------------------------------------------------------------------------------

    fn key_input(&mut self, key: Key) {
        let Some(input) = self.key.input.as_mut() else {
            return;
        };
        match input.handle(&key) {
            InputOutcome::Pending => {}
            InputOutcome::Cancelled | InputOutcome::Aborted => {
                self.key.input = None;
                self.say(StepKind::Info, "Key unchanged.");
            }
            InputOutcome::Submitted => {
                let raw = input.take_value();
                let key = clean_key(&raw).to_string();
                if let Err(why) = validate_key_format(&key) {
                    // The input stays open, empty: the key is typed again.
                    self.say(StepKind::Note, format!("That key cannot be used: {why}."));
                    return;
                }
                self.key.input = None;
                self.key.log.clear();
                self.key.allow_unusual = false;
                self.key.pending = Some(Secret::new(key.clone()));
                if looks_like_key(&key) {
                    self.ask_check_key();
                } else {
                    self.open(
                        Confirm::new(
                            "That does not look like an OpenRouter key (they start with 'sk-or-'). Use it anyway?",
                            false,
                        ),
                        Then::KeyUnusual,
                    );
                }
            }
        }
    }

    fn ask_check_key(&mut self) {
        self.open(
            Confirm::new(
                "Check the key with OpenRouter first (free, touches no model)?",
                true,
            ),
            Then::KeyCheck,
        );
    }

    fn ask_save_key(&mut self, unchecked: bool) {
        let masked = self
            .key
            .pending
            .as_ref()
            .map(Secret::masked)
            .unwrap_or_default();
        let question = if unchecked {
            format!("Save {masked} without checking, and re-install?")
        } else {
            format!("Save {masked} to settings.json and re-install?")
        };
        self.open(Confirm::new(question, !unchecked), Then::KeySave);
    }

    fn key_checked(&mut self, check: KeyCheck) {
        match check {
            KeyCheck::Valid { usage, limit, .. } => {
                let limit = limit.map_or_else(|| "none set".to_string(), |l| format!("${l:.2}"));
                self.key.log.ok(format!(
                    "Key accepted by OpenRouter (spent so far: ${:.2}, credit limit: {limit})",
                    usage.unwrap_or(0.0)
                ));
                self.ask_save_key(false);
            }
            rejected @ KeyCheck::Rejected { .. } => {
                let why = format!("That key {}.", rejected.reason().unwrap_or_default());
                self.key.log.fail(why.clone());
                self.key.pending = None;
                self.say(StepKind::Fail, format!("{why} Key unchanged."));
            }
            KeyCheck::Unreachable { reason } => {
                self.key.log.note(format!(
                    "Could not reach OpenRouter to check the key: {reason}"
                ));
                self.ask_save_key(true);
            }
        }
    }

    // ---- display ---------------------------------------------------------------------------

    fn pick_style(&mut self) {
        let Some(chosen) = self.display.chosen.as_mut() else {
            self.say(StepKind::Note, "Waiting for the status.");
            return;
        };
        let i = self.display.cursor;
        if let Some((s, _)) = PROGRESS_HELP.get(i) {
            chosen.progress = *s;
        } else if let Some((s, _)) = SUMMARY_HELP.get(i - PROGRESS_HELP.len()) {
            chosen.summary = *s;
        }
    }

    fn apply_display(&mut self) {
        let (Some(chosen), Some(status)) = (self.display.chosen, self.status.as_ref()) else {
            self.say(StepKind::Note, "Waiting for the status.");
            return;
        };
        if chosen == status.display {
            self.say(
                StepKind::Info,
                "Nothing changed: Space picks a style, Enter applies.",
            );
            return;
        }
        self.open(
            Confirm::new(
                format!(
                    "Re-install with progress {}, summary {}?",
                    chosen.progress, chosen.summary
                ),
                true,
            ),
            Then::Display(chosen),
        );
    }

    // ---- sessions ---------------------------------------------------------------------------

    fn session_count(&self) -> usize {
        match &self.sessions.list {
            Some(Ok(l)) => l.len(),
            _ => 0,
        }
    }

    fn ask_delete_session(&mut self) {
        let Some(Ok(list)) = &self.sessions.list else {
            return;
        };
        let Some(s) = list.get(self.sessions.cursor) else {
            return;
        };
        let id = s.id.clone();
        self.open(
            Confirm::new(format!("Delete the files of session {id}?"), false),
            Then::DeleteSession(id),
        );
    }

    // ---- service ----------------------------------------------------------------------------

    fn ask_service(&mut self) {
        if !self.backend.service_supported() {
            return;
        }
        let action = ServiceAction::ALL[self.service.cursor.min(ServiceAction::ALL.len() - 1)];
        let mut question = format!("{}: {}?", action.label(), action.describe());
        if let Some(crate::actions::TransportState::OtherInstall { dir, .. }) =
            self.status.as_ref().map(|s| &s.transport)
        {
            question = format!(
                "The task serves another install ({}). {question}",
                dir.display()
            );
        }
        self.open(Confirm::new(question, false), Then::Service(action));
    }

    fn run_service(&mut self, action: ServiceAction) {
        self.service.log.clear();
        self.service.log.step(action.label());
        self.spawn(
            JobKind::Service,
            Screen::Service,
            action.label(),
            move |b, log| Outcome::Changed(b.service_action(action, log)),
        );
    }

    // ---- answers ----------------------------------------------------------------------------

    fn resolve(&mut self, then: Then, yes: bool) {
        match (then, yes) {
            (Then::Panel(panel), true) => self.reinstall(
                Screen::Panel,
                Reinstall {
                    panel: Some(panel),
                    ..Reinstall::default()
                },
            ),
            (Then::Panel(_), false) => self.say(StepKind::Info, "Panel unchanged."),
            (Then::KeyUnusual, true) => {
                self.key.allow_unusual = true;
                self.ask_check_key();
            }
            (Then::KeyCheck, true) => {
                let Some(key) = self.key.pending.clone() else {
                    return;
                };
                if self.spawn(
                    JobKind::KeyCheck,
                    Screen::Key,
                    "Checking the key with OpenRouter",
                    move |b, _| Outcome::KeyCheck(b.check_key(&key)),
                ) {
                    self.say(StepKind::Info, "Checking the key with OpenRouter ...");
                }
            }
            (Then::KeyCheck, false) => self.ask_save_key(true),
            (Then::KeySave, true) => {
                let Some(key) = self.key.pending.take() else {
                    return;
                };
                let request = Reinstall {
                    key: Some(key),
                    allow_unusual_key: self.key.allow_unusual,
                    ..Reinstall::default()
                };
                self.reinstall(Screen::Key, request);
            }
            (Then::KeyUnusual | Then::KeySave, false) => {
                self.key.pending = None;
                self.say(StepKind::Info, "Key unchanged.");
            }
            (Then::Display(d), true) => self.reinstall(
                Screen::Display,
                Reinstall {
                    progress: Some(d.progress),
                    summary: Some(d.summary),
                    ..Reinstall::default()
                },
            ),
            (Then::Display(_), false) => self.say(StepKind::Info, "Display unchanged."),
            (Then::DeleteSession(id), true) => {
                match self.backend.delete_session(&id) {
                    Ok(n) => self.say(
                        StepKind::Ok,
                        format!("Deleted {n} file(s) of session {id}."),
                    ),
                    Err(e) => self.say(StepKind::Fail, format!("Could not delete {id}: {e}")),
                }
                self.load_sessions();
            }
            (Then::DeleteSession(_), false) => self.say(StepKind::Info, "Nothing deleted."),
            (Then::Service(ServiceAction::Register), true) if self.backend.needs_elevation() => {
                self.open(
                    Confirm::new(
                        format!(
                            "{} Windows will ask for permission; no keeps a logon-only task.",
                            consult_service::ELEVATE_QUESTION
                        ),
                        true,
                    ),
                    Then::ServiceElevate,
                );
            }
            (Then::ServiceElevate, yes) => self.run_service(if yes {
                ServiceAction::RegisterElevated
            } else {
                ServiceAction::Register
            }),
            (Then::Service(action), true) => self.run_service(action),
            (Then::Service(_), false) => self.say(StepKind::Info, "Nothing done."),
            (Then::UninstallFirst, true) => self.open(
                Confirm::new("Really uninstall? This cannot be undone.", false),
                Then::UninstallSecond,
            ),
            (Then::UninstallSecond, true) => self.open(
                Confirm::new(
                    "Also delete OPENROUTER_API_KEY from Claude Code settings?",
                    false,
                ),
                Then::UninstallKey,
            ),
            (Then::UninstallKey, remove_key) => {
                self.uninstall.log.clear();
                self.spawn(
                    JobKind::Uninstall,
                    Screen::Uninstall,
                    "Uninstalling",
                    move |b, log| Outcome::Uninstalled(b.uninstall(remove_key, log)),
                );
            }
            (Then::UninstallFirst | Then::UninstallSecond, false) => {
                self.say(StepKind::Info, "Nothing removed.");
            }
        }
    }
}

fn failed(kind: JobKind, why: &str) -> Outcome {
    let why = why.to_string();
    match kind {
        JobKind::Status | JobKind::Listing => Outcome::Failed(why),
        JobKind::KeyCheck => Outcome::KeyCheck(KeyCheck::Unreachable { reason: why }),
        JobKind::Reinstall | JobKind::Service => Outcome::Changed(Err(why)),
        JobKind::Catalog => Outcome::Catalog(CatalogVerdict {
            lines: vec![format!("!!  {why}")],
            code: 2,
        }),
        JobKind::Sessions => Outcome::Sessions(Err(why)),
        JobKind::Uninstall => Outcome::Uninstalled(Err(why)),
    }
}

/// Shows the per-screen jobs map in a test's failure message.
impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let running: HashMap<u64, &str> = self
            .running
            .iter()
            .map(|r| (r.id, r.label.as_str()))
            .collect();
        f.debug_struct("App")
            .field("screen", &self.screen)
            .field("running", &running)
            .field("message", &self.message)
            .field("ask", &self.ask.as_ref().map(|a| a.confirm.question()))
            .finish_non_exhaustive()
    }
}
