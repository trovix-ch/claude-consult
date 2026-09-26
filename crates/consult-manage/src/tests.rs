//! The state machine driven with keys against a fake backend, and the screens rendered
//! into a test terminal. Nothing touches the network, the real dirs, the task or the
//! `claude` CLI: the only files are in temp dirs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use consult_core::catalog;
use consult_core::display::{ProgressStyle, SummaryStyle};
use consult_core::generate::Display;
use consult_core::key::KeyCheck;
use consult_service::{ProcessInfo, ServiceStatus};
use consult_tui::{Key, StepKind};
use indexmap::IndexMap;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use serde_json::Value;

use crate::actions::{
    Backend, CatalogVerdict, KeyInfo, KeySource, PanelMember, Reinstall, Secret, ServiceAction,
    Sink, StatusReport, TransportState,
};
use crate::app::{App, Screen};
use crate::sessions::{self, Session};
use crate::ui;

const SECRET: &str = "sk-or-v1-TOPSECRETPART0123456789";

struct Fake {
    state_dir: PathBuf,
    supported: bool,
    slow: bool,
    key_check: KeyCheck,
    statuses: AtomicUsize,
    listings: AtomicUsize,
    reinstalls: Mutex<Vec<Reinstall>>,
    keys_checked: Mutex<Vec<String>>,
    services: Mutex<Vec<ServiceAction>>,
    uninstalls: Mutex<Vec<bool>>,
}

impl Fake {
    fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            supported: true,
            slow: false,
            key_check: KeyCheck::Valid {
                usage: Some(1.5),
                limit: Some(10.0),
                data: Value::Null,
            },
            statuses: AtomicUsize::new(0),
            listings: AtomicUsize::new(0),
            reinstalls: Mutex::new(Vec::new()),
            keys_checked: Mutex::new(Vec::new()),
            services: Mutex::new(Vec::new()),
            uninstalls: Mutex::new(Vec::new()),
        }
    }
}

fn service_status() -> ServiceStatus {
    ServiceStatus {
        supported: true,
        registered: true,
        state: Some("Running".into()),
        runs_as: Some("HOST\\me".into()),
        logon_type: Some("S4U".into()),
        triggers: vec!["Boot".into(), "Logon".into()],
        port: 8765,
        listening: true,
        processes: vec![ProcessInfo {
            pid: 4242,
            rss_bytes: 18 * 1024 * 1024,
            command_line: "claude-consult serve --http".into(),
        }],
        working_dir: Some(PathBuf::from("/fake/install")),
    }
}

impl Backend for Fake {
    fn status(&self) -> StatusReport {
        self.statuses.fetch_add(1, Ordering::SeqCst);
        let cat = catalog::embedded().expect("catalog");
        StatusReport {
            version: "0.1.0".into(),
            install_dir: PathBuf::from("/fake/install"),
            claude_dir: PathBuf::from("/fake/claude"),
            installed: true,
            manifest_date: Some("2026-09-26T10:00:00".into()),
            key: KeyInfo {
                source: KeySource::Settings,
                masked: Some("sk-or-v1-...abcd".into()),
                shadowed: None,
            },
            panel: Ok(cat
                .default_panel
                .iter()
                .map(|a| PanelMember {
                    alias: a.clone(),
                    id: cat.models.get(a).map(|m| m.id.clone()),
                })
                .collect()),
            priced_at: Some("2026-09-26 10:00 UTC".into()),
            display: Display {
                progress: ProgressStyle::Full,
                summary: SummaryStyle::Dim,
            },
            service: Ok(service_status()),
            transport: TransportState::Service { port: 8765 },
            stdio_hint: "claude mcp add --transport stdio --scope user openrouter -- \"/fake/install/bin/claude-consult\" serve".into(),
        }
    }

    fn fetch_listing(&self) -> Option<IndexMap<String, Value>> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        None
    }

    fn check_key(&self, key: &Secret) -> KeyCheck {
        self.keys_checked
            .lock()
            .expect("lock")
            .push(key.expose().to_string());
        self.key_check.clone()
    }

    fn reinstall(&self, request: &Reinstall, log: Sink<'_>) -> Result<String, String> {
        if self.slow {
            std::thread::sleep(Duration::from_millis(300));
        }
        log(StepKind::Step, "Installing the binary");
        log(StepKind::Ok, "Wrote models.json");
        self.reinstalls.lock().expect("lock").push(request.clone());
        Ok("Re-installed".into())
    }

    fn service_supported(&self) -> bool {
        self.supported
    }

    fn service_action(&self, action: ServiceAction, log: Sink<'_>) -> Result<String, String> {
        log(StepKind::Ok, "done");
        self.services.lock().expect("lock").push(action);
        Ok(format!("{} done", action.label()))
    }

    fn catalog_check(&self) -> CatalogVerdict {
        CatalogVerdict {
            lines: vec![
                "ok  deepseek-v4-pro        deepseek/deepseek-v4-pro".into(),
                "!!  glm-5.2: z-ai/glm-5.2 is no longer listed".into(),
                "2 favourites checked, 1 problem(s)".into(),
            ],
            code: 1,
        }
    }

    fn sessions(&self) -> Result<Vec<Session>, String> {
        sessions::list_sessions(&self.state_dir).map_err(|e| e.to_string())
    }

    fn delete_session(&self, id: &str) -> Result<usize, String> {
        sessions::delete_session(&self.state_dir, id).map_err(|e| e.to_string())
    }

    fn uninstall(&self, remove_key: bool, log: Sink<'_>) -> Result<String, String> {
        log(StepKind::Ok, "Removed scheduled task 'OpenRouterMCP'");
        self.uninstalls.lock().expect("lock").push(remove_key);
        Ok("claude-consult is uninstalled.".into())
    }
}

fn setup(edit: impl FnOnce(&mut Fake)) -> (tempfile::TempDir, Arc<Fake>, App) {
    let tmp = tempfile::tempdir().expect("tmp");
    let mut fake = Fake::new(tmp.path().join("state"));
    edit(&mut fake);
    let fake = Arc::new(fake);
    let mut app = App::new(fake.clone());
    app.settle();
    (tmp, fake, app)
}

fn keys(app: &mut App, keys: impl IntoIterator<Item = Key>) {
    for k in keys {
        app.handle_key(k);
    }
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_key(if c == ' ' { Key::Space } else { Key::Char(c) });
    }
}

fn screen_text(app: &App, width: u16, height: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    term.draw(|f| ui::draw(f, app)).expect("draw");
    buffer_text(term.backend().buffer())
}

fn buffer_text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn message(app: &App) -> String {
    app.message().map(|(_, m)| m.clone()).unwrap_or_default()
}

#[test]
fn tabs_move_with_arrows_tab_and_digits_and_q_quits() {
    let (_t, _f, mut app) = setup(|_| {});
    assert_eq!(app.screen(), Screen::Status);
    keys(&mut app, [Key::Right]);
    assert_eq!(app.screen(), Screen::Panel);
    keys(&mut app, [Key::Tab]);
    assert_eq!(app.screen(), Screen::Key);
    keys(&mut app, [Key::BackTab, Key::Left]);
    assert_eq!(app.screen(), Screen::Status);
    keys(&mut app, [Key::Left]);
    assert_eq!(app.screen(), Screen::Uninstall);
    keys(&mut app, [Key::Char('6')]);
    assert_eq!(app.screen(), Screen::Catalog);
    keys(&mut app, [Key::Char('?')]);
    assert!(app.help_shown());
    assert!(screen_text(&app, 100, 30).contains("Help"));
    keys(&mut app, [Key::Char('q')]);
    assert!(
        !app.help_shown() && !app.should_quit(),
        "a key closes the help only"
    );
    keys(&mut app, [Key::Esc]);
    assert!(app.should_quit());
}

#[test]
fn status_loads_at_start_and_refreshes_with_r() {
    let (_t, fake, mut app) = setup(|_| {});
    assert_eq!(fake.statuses.load(Ordering::SeqCst), 1);
    assert!(app.status.is_some());
    keys(&mut app, [Key::Char('r')]);
    app.settle();
    assert_eq!(fake.statuses.load(Ordering::SeqCst), 2);
}

#[test]
fn panel_repick_starts_from_the_installed_panel_and_reinstalls_the_new_one() {
    let (_t, fake, mut app) = setup(|_| {});
    let cat = catalog::embedded().expect("catalog");
    keys(&mut app, [Key::Char('2'), Key::Enter]);
    app.settle();
    assert_eq!(fake.listings.load(Ordering::SeqCst), 1);
    let picker = app.panel.picker.as_ref().expect("picker open");
    assert!(picker.offline());
    assert_eq!(picker.selected(), cat.default_panel.as_slice());
    // Esc with an empty filter closes without a change.
    keys(&mut app, [Key::Esc]);
    assert!(app.panel.picker.is_none());
    keys(&mut app, [Key::Enter]);
    app.settle();
    keys(&mut app, [Key::CtrlB, Key::Enter]);
    assert!(app.panel.picker.is_none());
    let q = app.ask().expect("confirm").confirm.question().to_string();
    assert!(q.contains(&cat.budget_panel.join(", ")), "{q}");
    keys(&mut app, [Key::Char('y')]);
    app.settle();
    let done = fake.reinstalls.lock().expect("lock").clone();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].panel.as_deref(), Some(cat.budget_panel.as_slice()));
    assert!(done[0].key.is_none() && done[0].progress.is_none());
    assert!(
        app.panel
            .log
            .entries()
            .iter()
            .any(|(k, t)| *k == StepKind::Ok && t == "Re-installed")
    );
    // The status is read again after a change.
    assert_eq!(fake.statuses.load(Ordering::SeqCst), 2);
}

#[test]
fn key_rotation_refuses_a_bad_format_and_never_shows_the_key() {
    let (_t, fake, mut app) = setup(|_| {});
    keys(&mut app, [Key::Char('3'), Key::Enter]);
    assert!(app.key.input.is_some());
    type_text(&mut app, "sk-or-v1 TOPSECRETPART");
    keys(&mut app, [Key::Enter]);
    assert!(
        message(&app).contains("cannot be used"),
        "{}",
        message(&app)
    );
    assert!(app.key.input.is_some(), "the input stays open");
    assert_eq!(
        app.key.input.as_ref().map(|i| i.value()),
        Some(String::new())
    );
    assert!(fake.keys_checked.lock().expect("lock").is_empty());

    type_text(&mut app, SECRET);
    let shown = screen_text(&app, 120, 30);
    assert!(!shown.contains("TOPSECRET"), "{shown}");
    assert!(shown.contains(&"*".repeat(SECRET.len())));
    keys(&mut app, [Key::Enter]);
    assert!(app.ask().is_some(), "asks whether to check");
    keys(&mut app, [Key::Char('y')]);
    app.settle();
    assert_eq!(*fake.keys_checked.lock().expect("lock"), [SECRET]);
    let shown = screen_text(&app, 120, 30);
    assert!(shown.contains("Key accepted by OpenRouter"), "{shown}");
    assert!(shown.contains("sk-or-v1-...6789"), "{shown}");
    assert!(!shown.contains("TOPSECRET"), "{shown}");
    assert!(!format!("{app:?}").contains("TOPSECRET"));
    assert!(!format!("{:?}", app.key).contains("TOPSECRET"));
    keys(&mut app, [Key::Enter]);
    app.settle();
    let done = fake.reinstalls.lock().expect("lock").clone();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].key.as_ref().map(Secret::expose), Some(SECRET));
    assert!(!done[0].allow_unusual_key);
    assert!(done[0].panel.is_none());
    assert!(app.key.pending.is_none());
}

#[test]
fn a_rejected_key_is_dropped_and_an_unusual_one_needs_a_yes() {
    let (_t, fake, mut app) = setup(|f| f.key_check = KeyCheck::Rejected { status: 401 });
    keys(&mut app, [Key::Char('3'), Key::Enter]);
    type_text(&mut app, "abcdefghijklmnopqrstuvwxyz");
    keys(&mut app, [Key::Enter]);
    let q = app.ask().expect("unusual").confirm.question().to_string();
    assert!(q.contains("does not look like"), "{q}");
    keys(&mut app, [Key::Char('y'), Key::Char('y')]);
    app.settle();
    assert!(app.key.pending.is_none());
    assert!(message(&app).contains("rejected"), "{}", message(&app));
    assert!(fake.reinstalls.lock().expect("lock").is_empty());
    // Declining the unusual key drops it without a check.
    keys(&mut app, [Key::Enter]);
    type_text(&mut app, "abcdefghijklmnopqrstuvwxyz");
    keys(&mut app, [Key::Enter, Key::Char('n')]);
    assert!(app.key.pending.is_none());
    assert_eq!(fake.keys_checked.lock().expect("lock").len(), 1);
}

#[test]
fn display_change_reinstalls_with_both_styles() {
    let (_t, fake, mut app) = setup(|_| {});
    keys(&mut app, [Key::Char('4'), Key::Enter]);
    assert!(message(&app).contains("Nothing changed"));
    assert!(app.ask().is_none());
    // full -> ticker (second row), then the last row: summary off.
    keys(&mut app, [Key::Down, Key::Space]);
    for _ in 0..20 {
        app.handle_key(Key::Down);
    }
    keys(&mut app, [Key::Space, Key::Enter]);
    let q = app.ask().expect("confirm").confirm.question().to_string();
    assert_eq!(q, "Re-install with progress ticker, summary off?");
    keys(&mut app, [Key::Enter]);
    app.settle();
    let done = fake.reinstalls.lock().expect("lock").clone();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].progress, Some(ProgressStyle::Ticker));
    assert_eq!(done[0].summary, Some(SummaryStyle::Off));
    assert!(done[0].panel.is_none() && done[0].key.is_none());
}

fn write_sessions(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(
        dir.join("aaaa-1111.jsonl"),
        "{\"cost\": 0.25, \"tin\": 1200000, \"tout\": 3000}\n{\"cost\": 0.5, \"tin\": 800000, \"tout\": 2000}\n",
    )
    .expect("write");
    std::fs::write(dir.join("aaaa-1111.pending"), "x").expect("write");
    std::fs::write(
        dir.join("bbbb-2222-a-rather-long-session-id.jsonl"),
        "{\"cost\": 0.125, \"tin\": 500, \"tout\": 40}\n",
    )
    .expect("write");
}

#[test]
fn sessions_list_the_state_files_and_delete_after_a_yes() {
    let (tmp, _f, mut app) = setup(|_| {});
    let state = tmp.path().join("state");
    write_sessions(&state);
    keys(&mut app, [Key::Char('5')]);
    app.settle();
    let list = app.sessions.list.clone().expect("read").expect("ok");
    assert_eq!(list.len(), 2);
    let a = list.iter().find(|s| s.id == "aaaa-1111").expect("a");
    assert_eq!((a.calls, a.tokens_in, a.tokens_out), (2, 2_000_000, 5000));
    let first = list[0].id.clone();
    keys(&mut app, [Key::Char('d')]);
    assert!(app.ask().is_some());
    keys(&mut app, [Key::Char('n')]);
    assert!(state.join(format!("{first}.jsonl")).exists());
    keys(&mut app, [Key::Char('d'), Key::Char('y')]);
    app.settle();
    assert!(!state.join(format!("{first}.jsonl")).exists());
    assert_eq!(
        app.sessions
            .list
            .as_ref()
            .and_then(|l| l.as_ref().ok())
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn catalog_check_shows_the_verdict() {
    let (_t, _f, mut app) = setup(|_| {});
    keys(&mut app, [Key::Char('6')]);
    assert!(app.catalog_check.is_none(), "nothing runs until asked");
    keys(&mut app, [Key::Enter]);
    app.settle();
    assert_eq!(app.catalog_check.as_ref().map(|v| v.code), Some(1));
    let shown = screen_text(&app, 100, 30);
    assert!(shown.contains("Verdict 1: at least one problem"), "{shown}");
    assert!(shown.contains("!!  glm-5.2"), "{shown}");
}

#[test]
fn service_actions_need_a_yes() {
    let (_t, fake, mut app) = setup(|_| {});
    keys(&mut app, [Key::Char('7'), Key::Down, Key::Enter]);
    let q = app.ask().expect("confirm").confirm.question().to_string();
    assert!(q.starts_with("Stop:"), "{q}");
    keys(&mut app, [Key::Enter]);
    assert!(app.ask().is_none(), "the default is no");
    assert!(fake.services.lock().expect("lock").is_empty());
    keys(&mut app, [Key::Down, Key::Enter, Key::Char('y')]);
    app.settle();
    assert_eq!(
        *fake.services.lock().expect("lock"),
        [ServiceAction::Restart]
    );
    assert!(
        app.service
            .log
            .entries()
            .iter()
            .any(|(_, t)| t == "Restart done")
    );
}

#[test]
fn service_screen_elsewhere_shows_the_stdio_hint() {
    let (_t, fake, mut app) = setup(|f| f.supported = false);
    keys(&mut app, [Key::Char('7'), Key::Enter]);
    assert!(app.ask().is_none());
    assert!(fake.services.lock().expect("lock").is_empty());
    let shown = screen_text(&app, 160, 30);
    assert!(shown.contains("Windows only"), "{shown}");
    assert!(
        shown.contains("claude mcp add --transport stdio"),
        "{shown}"
    );
}

#[test]
fn uninstall_asks_twice_then_about_the_key_then_exits() {
    let (_t, fake, mut app) = setup(|_| {});
    keys(
        &mut app,
        [Key::Char('8'), Key::Enter, Key::Char('y'), Key::Char('n')],
    );
    assert!(app.ask().is_none());
    assert!(fake.uninstalls.lock().expect("lock").is_empty());
    keys(&mut app, [Key::Enter, Key::Enter]);
    assert!(app.ask().is_none(), "the first question defaults to no");
    keys(&mut app, [Key::Enter, Key::Char('y'), Key::Char('y')]);
    let q = app
        .ask()
        .expect("key question")
        .confirm
        .question()
        .to_string();
    assert!(q.contains("OPENROUTER_API_KEY"), "{q}");
    keys(&mut app, [Key::Char('y')]);
    app.settle();
    assert_eq!(*fake.uninstalls.lock().expect("lock"), [true]);
    assert!(app.uninstall.done);
    assert!(!app.should_quit());
    keys(&mut app, [Key::Char('x')]);
    assert!(app.should_quit());
}

#[test]
fn quitting_waits_for_a_running_change() {
    let (_t, fake, mut app) = setup(|f| f.slow = true);
    keys(
        &mut app,
        [
            Key::Char('4'),
            Key::Down,
            Key::Space,
            Key::Enter,
            Key::Char('y'),
        ],
    );
    keys(&mut app, [Key::Char('q')]);
    assert!(!app.should_quit());
    assert!(message(&app).contains("still running"), "{}", message(&app));
    // A second change is refused while the first runs.
    keys(&mut app, [Key::Char('2'), Key::Enter]);
    app.settle();
    assert_eq!(fake.reinstalls.lock().expect("lock").len(), 1);
    keys(&mut app, [Key::Esc]);
    // Esc closed the picker opened by the Enter above; the next Esc quits.
    keys(&mut app, [Key::Esc]);
    assert!(app.should_quit());
}

#[test]
fn status_renders_every_field() {
    let (_t, _f, app) = setup(|_| {});
    let shown = screen_text(&app, 120, 40);
    for want in [
        "claude-consult manage",
        "1 Status",
        "8 Uninstall",
        "Version       : 0.1.0",
        "Install dir   : /fake/install",
        "Claude dir    : /fake/claude",
        "Generated on  : 2026-09-26T10:00:00",
        "Key           : sk-or-v1-...abcd  (from settings.json)",
        "Transport     : service, http://127.0.0.1:8765/mcp",
        "Panel         : deepseek-v4-pro",
        "Display       : progress full, summary dim",
        "Task          : OpenRouterMCP, Running",
        "Runs as       : HOST\\me (S4U)",
        "Triggers      : Boot, Logon",
        "Port 8765     : listening",
        "pid 4242  RSS 18.0 MB",
        "r refresh",
    ] {
        assert!(shown.contains(want), "missing {want:?} in\n{shown}");
    }
}

#[test]
fn sessions_render_as_a_table_with_totals() {
    let (tmp, _f, mut app) = setup(|_| {});
    write_sessions(&tmp.path().join("state"));
    keys(&mut app, [Key::Char('5')]);
    app.settle();
    let shown = screen_text(&app, 120, 30);
    assert!(
        shown.contains("session        calls   cost USD       in      out  last modified"),
        "{shown}"
    );
    assert!(
        shown.contains("aaaa-1111          2     0.7500    2.00M       5k"),
        "{shown}"
    );
    assert!(
        shown.contains("bbbb-2222-a-r~     1     0.1250      500       40"),
        "{shown}"
    );
    assert!(
        shown.contains("total (2)          3     0.8750    2.00M       5k"),
        "{shown}"
    );
}

#[test]
fn a_tiny_terminal_says_so() {
    let (_t, _f, app) = setup(|_| {});
    assert!(screen_text(&app, 20, 4).contains("Terminal too sma"));
}
