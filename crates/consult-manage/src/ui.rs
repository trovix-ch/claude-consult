//! Drawing: a pure function of [`App`] into a buffer. Nothing here changes state.
//!
//! The frame is a title bar, the tab list on the left, the screen on the right (a
//! heading, a blank line, the body), the message line and the key hints.

use chrono::{DateTime, Local};
use consult_core::util::tokens;
use consult_tui::picker::{clip, render_with};
use consult_tui::widgets::paint_lines;
use consult_tui::{StepLog, Theme};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Widget};

use crate::actions::{KeySource, ServiceAction, StatusReport, TransportState};
use crate::app::{App, Ask, PROGRESS_HELP, SUMMARY_HELP, Screen};

/// The tab list's width, separator included.
const TABS_WIDTH: u16 = 15;

/// Draws the app into a frame.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    render(app, area, frame.buffer_mut());
}

/// Draws the app into `area` of `buf`.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let theme = Theme::default();
    if area.height < 6 || area.width < 30 {
        buf.set_stringn(
            area.x,
            area.y,
            "Terminal too small",
            usize::from(area.width),
            theme.note,
        );
        return;
    }
    let width = usize::from(area.width);
    // Title bar.
    buf.set_style(Rect::new(area.x, area.y, area.width, 1), theme.title);
    buf.set_stringn(area.x, area.y, " claude-consult manage", width, theme.title);
    if let Some(s) = &app.status {
        let right = format!("{}  v{} ", s.install_dir.display(), s.version);
        let rw = right.chars().count();
        if rw + 24 <= width {
            buf.set_stringn(
                area.x + (width - rw) as u16,
                area.y,
                &right,
                rw,
                theme.title,
            );
        }
    }
    // Tabs.
    let middle = Rect::new(area.x, area.y + 1, area.width, area.height - 3);
    for (i, screen) in Screen::ALL.iter().enumerate() {
        let y = middle.y + 1 + i as u16;
        if y >= middle.y + middle.height {
            break;
        }
        let label = format!(" {} {:<10}", i + 1, screen.title());
        let style = if *screen == app.screen() {
            theme.choice
        } else {
            theme.text
        };
        buf.set_stringn(middle.x, y, &label, usize::from(TABS_WIDTH - 1), style);
    }
    for y in middle.y..middle.y + middle.height {
        buf.set_stringn(middle.x + TABS_WIDTH - 1, y, "│", 1, theme.dim);
    }
    // Screen.
    let body = Rect::new(
        middle.x + TABS_WIDTH + 1,
        middle.y,
        middle.width.saturating_sub(TABS_WIDTH + 2),
        middle.height,
    );
    buf.set_stringn(
        body.x,
        body.y,
        heading(app.screen()),
        usize::from(body.width),
        theme.heading,
    );
    let mut content = Rect::new(
        body.x,
        body.y + 2,
        body.width,
        body.height.saturating_sub(2),
    );
    if let Some(ask) = app.ask() {
        content = render_ask(ask, content, buf, &theme);
    }
    render_screen(app, content, buf, &theme);
    // Message and hints.
    let msg_y = area.y + area.height - 2;
    let busy = app.busy();
    if !busy.is_empty() {
        let line = format!(" {} {} ...", app.spinner(), busy.join(", "));
        buf.set_stringn(area.x, msg_y, clip(&line, width), width, theme.cursor);
    } else if let Some((kind, text)) = app.message() {
        let line = format!(" {text}");
        buf.set_stringn(
            area.x,
            msg_y,
            clip(&line, width),
            width,
            theme.for_kind(*kind),
        );
    }
    let hints = format!(" {}", hints(app));
    buf.set_stringn(
        area.x,
        area.y + area.height - 1,
        clip(&hints, width),
        width,
        theme.dim,
    );
    if app.help_shown() {
        render_help(area, buf, &theme);
    }
}

fn heading(screen: Screen) -> &'static str {
    match screen {
        Screen::Status => "Status",
        Screen::Panel => "Panel: who reviews",
        Screen::Key => "OpenRouter API key",
        Screen::Display => "Display styles",
        Screen::Sessions => "Sessions: consult totals per Claude Code session",
        Screen::Catalog => "Catalog: are the favourites still offered?",
        Screen::Service => "Shared service",
        Screen::Uninstall => "Uninstall",
    }
}

/// The key hints for what is up.
pub fn hints(app: &App) -> String {
    if app.help_shown() {
        return "any key closes the help".into();
    }
    if app.uninstall.done {
        return "any key exits".into();
    }
    if app.ask().is_some() {
        return "y yes   n no   Enter the highlighted answer   ←/→ move   Esc no".into();
    }
    if app.screen() == Screen::Panel && app.panel.picker.is_some() {
        return "Space tick   type to filter   Esc clears, then closes   Ctrl+R recommended   Ctrl+B budget   Enter confirm".into();
    }
    if app.screen() == Screen::Key && app.key.input.is_some() {
        return "Enter submit   Esc cancel".into();
    }
    let screen = match app.screen() {
        Screen::Status => "r refresh",
        Screen::Panel => "Enter re-pick   r refresh",
        Screen::Key => "Enter rotate   r refresh",
        Screen::Display => "↑↓ move   Space pick   Enter apply",
        Screen::Sessions => "↑↓ move   d delete   r reload",
        Screen::Catalog => "Enter check",
        Screen::Service if app.service_supported() => "↑↓ move   Enter run",
        Screen::Service => "r refresh",
        Screen::Uninstall => "Enter uninstall",
    };
    format!("{screen}   ←/→ Tab screens   1-8 jump   ? help   q quit")
}

/// Wraps `text` at spaces to lines of at most `width` characters.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split(' ') {
            let n = line.chars().count();
            if n > 0 && n + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

/// Paints the open question at the bottom of `area`; returns what is left above it.
fn render_ask(ask: &Ask, area: Rect, buf: &mut Buffer, theme: &Theme) -> Rect {
    let width = usize::from(area.width);
    let c = &ask.confirm;
    let mut lines: Vec<(Style, String)> = wrap(&format!("{} {}", c.question(), c.hint()), width)
        .into_iter()
        .map(|l| (theme.heading, l))
        .collect();
    lines.push((theme.text, String::new()));
    let h = (lines.len() as u16 + 1).min(area.height);
    let top = area.y + area.height - h;
    let region = Rect::new(area.x, top, area.width, h);
    buf.set_style(region, Style::reset());
    for x in region.x..region.x + region.width {
        for y in region.y..region.y + region.height {
            buf[(x, y)].set_symbol(" ");
        }
    }
    paint_lines(&lines, region, buf);
    let y = top + h - 1;
    let mut x = area.x;
    for (label, value) in [(" Yes ", true), (" No ", false)] {
        let style = if c.choice() == value {
            theme.choice
        } else {
            theme.dim
        };
        if x + 5 <= area.x + area.width {
            buf.set_stringn(x, y, label, 5, style);
        }
        x += 7;
    }
    Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(h + 1),
    )
}

fn render_screen(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    match app.screen() {
        Screen::Status => {
            text(status_lines(app, theme), area, buf);
        }
        Screen::Panel => render_panel(app, area, buf, theme),
        Screen::Key => render_key(app, area, buf, theme),
        Screen::Display => {
            let lines = display_lines(app, theme);
            let rest = text(lines, area, buf);
            log(&app.display.log, rest, buf, theme);
        }
        Screen::Sessions => {
            text(session_lines(app, theme), area, buf);
        }
        Screen::Catalog => {
            text(catalog_lines(app, theme), area, buf);
        }
        Screen::Service => {
            let rest = text(service_lines(app, theme), area, buf);
            log(&app.service.log, rest, buf, theme);
        }
        Screen::Uninstall => {
            let rest = text(uninstall_lines(app, theme), area, buf);
            log(&app.uninstall.log, rest, buf, theme);
        }
    };
}

/// Paints lines at the top of `area`, clipped; returns the area below them (after one
/// blank line).
fn text(lines: Vec<(Style, String)>, area: Rect, buf: &mut Buffer) -> Rect {
    paint_lines(&lines, area, buf);
    let used = (lines.len() as u16 + 1).min(area.height);
    Rect::new(area.x, area.y + used, area.width, area.height - used)
}

fn log(log: &StepLog, area: Rect, buf: &mut Buffer, theme: &Theme) {
    log.render(area, buf, theme);
}

fn kv(theme: &Theme, key: &str, value: impl Into<String>) -> (Style, String) {
    (theme.text, format!("{key:<14}: {}", value.into()))
}

/// The status screen's lines.
pub fn status_lines(app: &App, theme: &Theme) -> Vec<(Style, String)> {
    let Some(s) = &app.status else {
        return vec![(theme.dim, "Reading the status ...".into())];
    };
    status_report_lines(s, theme)
}

fn status_report_lines(s: &StatusReport, theme: &Theme) -> Vec<(Style, String)> {
    let mut out = vec![
        kv(theme, "Version", s.version.clone()),
        kv(theme, "Install dir", s.install_dir.display().to_string()),
    ];
    if !s.installed {
        out.push((
            theme.note,
            "                not installed here: run claude-consult install".into(),
        ));
    }
    out.push(kv(theme, "Claude dir", s.claude_dir.display().to_string()));
    out.push(kv(
        theme,
        "Generated on",
        s.manifest_date
            .clone()
            .unwrap_or_else(|| "unknown (no manifest)".into()),
    ));
    let key = match (&s.key.masked, s.key.source) {
        (Some(m), src) => format!("{m}  (from {})", src.label()),
        (None, src) => src.label().to_string(),
    };
    out.push((
        if s.key.source == KeySource::NotFound {
            theme.fail
        } else {
            theme.text
        },
        format!("{:<14}: {key}", "Key"),
    ));
    if let Some(sh) = &s.key.shadowed {
        out.push((
            theme.note,
            format!("                settings.json holds another key ({sh}), unused while the env var is set"),
        ));
    }
    out.push(kv(theme, "Transport", transport_text(s)));
    match &s.panel {
        Ok(p) if !p.is_empty() => {
            for (i, m) in p.iter().enumerate() {
                let label = if i == 0 { "Panel" } else { "" };
                let id = m.id.as_deref().unwrap_or("?");
                out.push(kv(theme, label, format!("{:<24} {id}", m.alias)));
            }
        }
        Ok(_) => out.push(kv(theme, "Panel", "none named in models.json")),
        Err(e) => out.push((theme.note, format!("{:<14}: unreadable: {e}", "Panel"))),
    }
    if let Some(at) = &s.priced_at {
        out.push(kv(theme, "Priced at", at.clone()));
    }
    out.push(kv(
        theme,
        "Display",
        format!(
            "progress {}, summary {}",
            s.display.progress, s.display.summary
        ),
    ));
    out.push((theme.text, String::new()));
    out.extend(service_status_lines(s, theme));
    out
}

fn transport_text(s: &StatusReport) -> String {
    match &s.transport {
        TransportState::Service { port } => {
            format!("service, http://127.0.0.1:{port}/mcp")
        }
        TransportState::OtherInstall { dir, port } => format!(
            "stdio here; the task on port {port} serves another install ({})",
            dir.display()
        ),
        TransportState::Stdio => {
            "stdio: Claude Code starts claude-consult serve for each session".into()
        }
    }
}

fn service_status_lines(s: &StatusReport, theme: &Theme) -> Vec<(Style, String)> {
    let st = match &s.service {
        Ok(st) => st,
        Err(e) => return vec![(theme.note, format!("{:<14}: {e}", "Task"))],
    };
    let mut out = Vec::new();
    if !st.supported {
        out.push(kv(
            theme,
            "Task",
            "none: the shared service is Windows only",
        ));
    } else if !st.registered {
        out.push(kv(theme, "Task", "OpenRouterMCP not registered"));
    } else {
        out.push(kv(
            theme,
            "Task",
            format!(
                "OpenRouterMCP, {}",
                st.state.as_deref().unwrap_or("state unknown")
            ),
        ));
        out.push(kv(
            theme,
            "Runs as",
            format!(
                "{} ({})",
                st.runs_as.as_deref().unwrap_or("?"),
                st.logon_type.as_deref().unwrap_or("?")
            ),
        ));
        out.push(kv(
            theme,
            "Triggers",
            if st.triggers.is_empty() {
                "none".to_string()
            } else {
                st.triggers.join(", ")
            },
        ));
        if let Some(dir) = &st.working_dir {
            out.push(kv(theme, "Serves from", dir.display().to_string()));
        }
    }
    out.push((
        if st.listening || !st.registered {
            theme.text
        } else {
            theme.note
        },
        format!(
            "{:<14}: {}",
            format!("Port {}", st.port),
            if st.listening {
                "listening"
            } else {
                "not listening"
            }
        ),
    ));
    if st.supported {
        out.push(kv(theme, "Processes", st.processes.len().to_string()));
        for p in &st.processes {
            out.push((
                theme.text,
                format!(
                    "                pid {}  RSS {:.1} MB",
                    p.pid,
                    p.rss_bytes as f64 / (1024.0 * 1024.0)
                ),
            ));
        }
    }
    out
}

fn render_panel(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    if let Some(p) = &app.panel.picker {
        render_with(p, area, buf, theme);
        return;
    }
    let mut lines = Vec::new();
    match app.status.as_ref().map(|s| &s.panel) {
        Some(Ok(p)) if !p.is_empty() => {
            lines.push((theme.text, "Installed panel:".to_string()));
            for m in p {
                lines.push((
                    theme.text,
                    format!("  {:<24} {}", m.alias, m.id.as_deref().unwrap_or("?")),
                ));
            }
        }
        Some(Err(e)) => lines.push((theme.note, format!("No panel installed: {e}"))),
        _ => lines.push((theme.dim, "Reading the installed panel ...".into())),
    }
    lines.push((theme.text, String::new()));
    for l in wrap(
        "Enter opens the picker with this panel ticked. Confirming re-runs the install with the new panel, keeping the dirs, port, key, styles and transport.",
        usize::from(area.width),
    ) {
        lines.push((theme.dim, l));
    }
    let rest = text(lines, area, buf);
    log(&app.panel.log, rest, buf, theme);
}

fn render_key(app: &App, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let mut lines = Vec::new();
    match &app.status {
        Some(s) => {
            let shown = s.key.masked.clone().unwrap_or_else(|| "none".into());
            lines.push(kv(theme, "Key in use", shown));
            lines.push(kv(theme, "Source", s.key.source.label()));
            if s.key.source == KeySource::EnvVar {
                for l in wrap(
                    "OPENROUTER_API_KEY is set in the environment and wins over settings.json: a new key is written to settings.json, but the environment's stays in use until it is unset.",
                    usize::from(area.width),
                ) {
                    lines.push((theme.note, l));
                }
            }
        }
        None => lines.push((theme.dim, "Reading the key ...".into())),
    }
    lines.push((theme.text, String::new()));
    for l in wrap(
        "Enter types a new key (hidden). It is checked for its format, optionally with OpenRouter's free /key endpoint, then saved by re-running the install.",
        usize::from(area.width),
    ) {
        lines.push((theme.dim, l));
    }
    let mut rest = text(lines, area, buf);
    if let Some(input) = &app.key.input {
        input.render(rest, buf, theme);
        rest = Rect::new(
            rest.x,
            rest.y + 2,
            rest.width,
            rest.height.saturating_sub(2),
        );
    }
    if let Some(p) = &app.key.pending {
        buf.set_stringn(
            rest.x,
            rest.y,
            format!("New key: {}", p.masked()),
            usize::from(rest.width),
            theme.text,
        );
        rest = Rect::new(
            rest.x,
            rest.y + 2,
            rest.width,
            rest.height.saturating_sub(2),
        );
    }
    log(&app.key.log, rest, buf, theme);
}

/// The display screen's lines: both lists, the cursor row marked `>`, the chosen style
/// `(*)`, the installed one `(installed)`.
pub fn display_lines(app: &App, theme: &Theme) -> Vec<(Style, String)> {
    let installed = app.status.as_ref().map(|s| s.display);
    let chosen = app.display.chosen.or(installed);
    let mut out = vec![(
        theme.heading,
        "Progress: the line while a consult runs".to_string(),
    )];
    let row = |i: usize, name: &str, desc: &str, picked: bool, current: bool| {
        let cursor = if app.display.cursor == i { ">" } else { " " };
        let mark = if picked { "(*)" } else { "( )" };
        let tag = if current { "  (installed)" } else { "" };
        let style = if app.display.cursor == i {
            theme.cursor
        } else {
            theme.text
        };
        (style, format!("{cursor} {mark} {name:<8} {desc}{tag}"))
    };
    for (i, (s, desc)) in PROGRESS_HELP.iter().enumerate() {
        out.push(row(
            i,
            s.as_str(),
            desc,
            chosen.is_some_and(|c| c.progress == *s),
            installed.is_some_and(|c| c.progress == *s),
        ));
    }
    out.push((theme.text, String::new()));
    out.push((
        theme.heading,
        "Summary: the line under Claude's reply".to_string(),
    ));
    for (j, (s, desc)) in SUMMARY_HELP.iter().enumerate() {
        out.push(row(
            PROGRESS_HELP.len() + j,
            s.as_str(),
            desc,
            chosen.is_some_and(|c| c.summary == *s),
            installed.is_some_and(|c| c.summary == *s),
        ));
    }
    out
}

/// The sessions table: a header, a row per session, and the totals.
pub fn session_lines(app: &App, theme: &Theme) -> Vec<(Style, String)> {
    let list = match &app.sessions.list {
        None => return vec![(theme.dim, "Reading the sessions ...".into())],
        Some(Err(e)) => return vec![(theme.fail, format!("Could not read the sessions: {e}"))],
        Some(Ok(l)) => l,
    };
    if list.is_empty() {
        return vec![(theme.dim, "No consults recorded (state/ is empty).".into())];
    }
    let row =
        |cursor: &str, id: &str, calls: &str, cost: &str, tin: &str, tout: &str, when: &str| {
            format!("{cursor} {id:<14} {calls:>5} {cost:>10} {tin:>8} {tout:>8}  {when}")
        };
    let mut out = vec![(
        theme.dim,
        row(
            " ",
            "session",
            "calls",
            "cost USD",
            "in",
            "out",
            "last modified",
        ),
    )];
    let (mut calls, mut cost, mut tin, mut tout) = (0u64, 0.0f64, 0u64, 0u64);
    for (i, s) in list.iter().enumerate() {
        calls += s.calls;
        cost += s.cost_usd;
        tin = tin.saturating_add(s.tokens_in);
        tout = tout.saturating_add(s.tokens_out);
        let when = s
            .modified
            .map(|m| {
                DateTime::<Local>::from(m)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "?".into());
        let at = i == app.sessions.cursor;
        out.push((
            if at { theme.cursor } else { theme.text },
            row(
                if at { ">" } else { " " },
                &clip(&s.id, 14),
                &s.calls.to_string(),
                &format!("{:.4}", s.cost_usd),
                &tokens(s.tokens_in),
                &tokens(s.tokens_out),
                &when,
            ),
        ));
    }
    out.push((
        theme.heading,
        row(
            " ",
            &format!("total ({})", list.len()),
            &calls.to_string(),
            &format!("{cost:.4}"),
            &tokens(tin),
            &tokens(tout),
            "",
        ),
    ));
    out
}

fn catalog_lines(app: &App, theme: &Theme) -> Vec<(Style, String)> {
    let Some(v) = &app.catalog_check else {
        return wrap(
            "Enter checks every favourite against OpenRouter's live model listing: still listed, still able to call tools. Public, no key, no cost.",
            80,
        )
        .into_iter()
        .map(|l| (theme.dim, l))
        .collect();
    };
    let mut out: Vec<(Style, String)> = v
        .lines
        .iter()
        .map(|l| {
            let style = if l.starts_with("ok") {
                theme.ok
            } else if l.starts_with("!!") {
                theme.note
            } else {
                theme.text
            };
            (style, l.clone())
        })
        .collect();
    out.push((theme.text, String::new()));
    let (style, verdict) = match v.code {
        0 => (theme.ok, "0: every favourite checks out"),
        1 => (theme.note, "1: at least one problem"),
        _ => (
            theme.fail,
            "2: the listing could not be read, nothing was checked",
        ),
    };
    out.push((style, format!("Verdict {verdict}")));
    out
}

fn service_lines(app: &App, theme: &Theme) -> Vec<(Style, String)> {
    let mut out = Vec::new();
    if !app.service_supported() {
        out.push((
            theme.note,
            "The shared service is supported on Windows only.".into(),
        ));
        out.push((
            theme.text,
            "Claude Code starts the server itself for each session (stdio). To register it by hand:"
                .into(),
        ));
        if let Some(s) = &app.status {
            out.push((theme.text, format!("  {}", s.stdio_hint)));
        }
        return out;
    }
    if let Some(s) = &app.status {
        out.extend(service_status_lines(s, theme));
        if let Ok(st) = &s.service
            && st.registered
            && st.logon_type.as_deref() == Some("InteractiveToken")
        {
            out.push((theme.text, String::new()));
            for l in wrap(
                "The task is registered for interactive logon only: it starts at logon, not at boot. Boot-start needs the S4U logon type, which only an elevated shell may register: run Re-register from a manage started in an admin terminal, or claude-consult service install there.",
                80,
            ) {
                out.push((theme.note, l));
            }
        }
    } else {
        out.push((theme.dim, "Reading the task ...".into()));
    }
    out.push((theme.text, String::new()));
    for (i, a) in ServiceAction::ALL.iter().enumerate() {
        let at = i == app.service.cursor;
        out.push((
            if at { theme.cursor } else { theme.text },
            format!(
                "{} {:<17} {}",
                if at { ">" } else { " " },
                a.label(),
                a.describe()
            ),
        ));
    }
    out
}

fn uninstall_lines(app: &App, theme: &Theme) -> Vec<(Style, String)> {
    let mut out = Vec::new();
    if let Some(s) = &app.status {
        out.push(kv(
            theme,
            "Install dir",
            s.install_dir.display().to_string(),
        ));
        out.push(kv(theme, "Claude dir", s.claude_dir.display().to_string()));
    }
    for l in wrap(
        "Removes this install's scheduled task, the MCP registration, the generated commands, skill and workflow, the hooks and the status line (putting back whatever they displaced), and the install dir. Enter starts: it asks twice, then whether to delete the key too.",
        80,
    ) {
        out.push((theme.dim, l));
    }
    out
}

fn render_help(area: Rect, buf: &mut Buffer, theme: &Theme) {
    const HELP: [&str; 15] = [
        "Everywhere",
        "  ←/→, Tab/Shift+Tab   previous / next screen;  1-8 jump to one",
        "  ?                    this help;  q or Esc  quit;  Ctrl+C  quit",
        "  r                    refresh the screen's data",
        "",
        "Panel     Enter re-pick (Space tick, type to filter, Enter confirm)",
        "Key       Enter type a new key; it is checked, then saved",
        "Display   ↑↓ move, Space pick a style, Enter apply",
        "Sessions  ↑↓ move, d delete the session's files",
        "Catalog   Enter check the favourites against OpenRouter",
        "Service   ↑↓ move, Enter run the action (Windows only)",
        "Uninstall Enter remove everything (asks twice)",
        "",
        "Questions: y / n, or Enter for the highlighted answer; Esc is no.",
        "Changes re-run the install flow; one runs at a time.",
    ];
    let w = (area.width.saturating_sub(4)).min(76);
    let h = (HELP.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    Clear.render(rect, buf);
    let block = Block::bordered()
        .title(" Help ")
        .border_style(theme.heading);
    let inner = block.inner(rect);
    block.render(rect, buf);
    let lines: Vec<(Style, String)> = HELP
        .iter()
        .map(|l| {
            (
                if l.starts_with(' ') || l.is_empty() {
                    theme.text
                } else {
                    theme.heading
                },
                (*l).to_string(),
            )
        })
        .collect();
    paint_lines(&lines, inner, buf);
}
