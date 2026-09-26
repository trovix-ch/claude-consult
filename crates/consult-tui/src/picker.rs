//! The panel picker: every model to choose from, ticked with Space, filtered by typing.
//!
//! A port of the PowerShell installer's `New-PickerRows`, `Select-PickerRows`,
//! `New-PickerState`, `Step-Picker` and `Format-Picker`, with the same rows, the same
//! cursor and viewport arithmetic, and the same lines. [`Picker::lines`] is the text;
//! [`render`] only paints it.

use consult_core::catalog::{Catalog, priced_per_request};
use consult_core::generate::per_million;
use indexmap::IndexMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::Value;

use crate::keys::Key;
use crate::theme::Theme;

/// What Enter says when nothing is ticked.
pub const NOTHING_SELECTED: &str = "Nothing selected: Space ticks the model under the cursor.";
/// Appended to the filter line when the listing could not be fetched.
pub const OFFLINE_NOTE: &str =
    "   OFFLINE: OpenRouter's listing is unreachable, favourites only, prices unknown";
/// The key legend, the picker's last line.
pub const LEGEND: &str = "  Space tick   type to filter, Esc clears   Ctrl+R recommended   Ctrl+B budget   Enter confirm   Ctrl+C quit";
/// Lines drawn besides the rows: filter, header, selection, note, legend.
pub const CHROME_LINES: usize = 5;

/// One model the picker offers.
#[derive(Clone, Debug, PartialEq)]
pub struct PickerRow {
    /// What the panel names it by: a favourite's alias, else its OpenRouter id.
    pub key: String,
    /// OpenRouter model id.
    pub id: String,
    /// A favourite's display name, else the listing's name.
    pub name: String,
    /// A favourite's lab, else the id's provider part.
    pub lab: String,
    /// The text after the columns: a favourite's tagline, else the listing's name.
    pub text: String,
    /// Whether it is one of the curated favourites.
    pub fav: bool,
    /// The id's provider part, lower case: two picks with one vendor share a lab.
    pub vendor: String,
    /// Live input price, USD per million tokens.
    pub price_in: Option<f64>,
    /// Live output price, USD per million tokens.
    pub price_out: Option<f64>,
    /// Live context window, tokens.
    pub context: Option<u64>,
}

// consult exists to ask anyone but Claude, and a :batch variant serves OpenRouter's
// batch processing while a review needs a live tool loop.
fn never_offered(id: &str) -> bool {
    let low = id.to_lowercase();
    low.strip_prefix('~')
        .unwrap_or(&low)
        .starts_with("anthropic/")
        || low.ends_with(":batch")
}

fn vendor_of(id: &str) -> &str {
    id.split('/').next().unwrap_or("")
}

fn row(
    key: &str,
    id: &str,
    name: &str,
    lab: &str,
    text: &str,
    fav: bool,
    listed: Option<&Value>,
) -> PickerRow {
    let (price_in, price_out, context) = match listed {
        Some(m) => (
            per_million(m.pointer("/pricing/prompt")),
            per_million(m.pointer("/pricing/completion")),
            m.get("context_length")
                .and_then(Value::as_u64)
                .filter(|c| *c > 0),
        ),
        None => (None, None, None),
    };
    PickerRow {
        key: key.to_string(),
        id: id.to_string(),
        name: name.to_string(),
        lab: lab.to_string(),
        text: text.to_string(),
        fav,
        vendor: vendor_of(id).to_lowercase(),
        price_in,
        price_out,
        context,
    }
}

/// The picker's rows: favourites first in catalog order, keyed by alias, then every
/// other model in listing order, keyed by id.
///
/// `live_models` is the listing's models by id; `None` means offline: favourites only,
/// unpriced. A favourite missing from a listing that did arrive is retired and left out.
/// Anthropic ids, `:batch` variants and routers (priced at -1, since they pick the
/// model per request and the pick can be Claude) are never offered, even from a listing
/// that was not filtered on the way in.
pub fn rows_from(
    catalog: &Catalog,
    live_models: Option<&IndexMap<String, Value>>,
) -> Vec<PickerRow> {
    let mut live: IndexMap<&str, &Value> = IndexMap::new();
    for (id, m) in live_models.into_iter().flatten() {
        if !never_offered(id) && !priced_per_request(m) {
            live.insert(id.as_str(), m);
        }
    }
    let mut rows = Vec::new();
    for (alias, c) in &catalog.models {
        if never_offered(&c.id) || (live_models.is_some() && !live.contains_key(c.id.as_str())) {
            continue;
        }
        let listed = live.get(c.id.as_str()).copied();
        rows.push(row(
            alias, &c.id, &c.display, &c.lab, &c.tagline, true, listed,
        ));
        live.shift_remove(c.id.as_str());
    }
    for (id, m) in live {
        let name = m.get("name").and_then(Value::as_str).unwrap_or("");
        rows.push(row(id, id, name, vendor_of(id), name, false, Some(m)));
    }
    rows
}

/// A price as the picker shows it: two to three decimals, `?` when unknown.
pub fn format_price(usd: Option<f64>) -> String {
    let Some(x) = usd else {
        return "?".to_string();
    };
    // .NET's "0.00#": at least two decimals, a third only when it is not zero.
    let s = format!("{x:.3}");
    match s.strip_suffix('0') {
        Some(two) if two.contains('.') => two.to_string(),
        _ => s,
    }
}

/// A context size in K (1024 tokens, rounded half to even as .NET does), `?` when unknown.
pub fn format_context(tokens: Option<u64>) -> String {
    match tokens {
        None | Some(0) => "?".to_string(),
        Some(t) => format!("{}K", (t as f64 / 1024.0).round_ties_even()),
    }
}

/// `text` cut to `max` characters, the last one replaced by `~` when anything was cut.
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('~');
    out
}

#[allow(clippy::too_many_arguments)]
fn format_row(
    cursor: &str,
    tick: &str,
    star: &str,
    key: &str,
    lab: &str,
    price_in: &str,
    price_out: &str,
    context: &str,
    text: &str,
) -> String {
    format!(
        "{cursor}{tick}{star} {key:<36} {lab:<11} {price_in:>8} {price_out:>7} {context:>7}  {text}"
    )
}

/// The picker's state. Built with [`Picker::new`], driven with [`Picker::step`], drawn
/// with [`Picker::lines`] or [`render`].
#[derive(Clone, Debug, PartialEq)]
pub struct Picker {
    rows: Vec<PickerRow>,
    filter: String,
    cursor: usize,
    top: usize,
    height: usize,
    offline: bool,
    recommended: Vec<String>,
    budget: Vec<String>,
    selected: Vec<String>,
    note: String,
    done: bool,
    aborted: bool,
}

impl Picker {
    /// A picker over `rows` showing `height` of them at a time. The recommended panel,
    /// minus anything not among the rows, starts ticked; the budget panel is kept the
    /// same way for Ctrl+B. `offline` only changes the filter line.
    pub fn new(
        rows: Vec<PickerRow>,
        recommended: &[String],
        budget: &[String],
        height: usize,
        offline: bool,
    ) -> Self {
        let known = |k: &&String| rows.iter().any(|r| &r.key == *k);
        let recommended: Vec<String> = recommended.iter().filter(known).cloned().collect();
        let budget: Vec<String> = budget.iter().filter(known).cloned().collect();
        Self {
            selected: recommended.clone(),
            rows,
            filter: String::new(),
            cursor: 0,
            top: 0,
            height: height.max(1),
            offline,
            recommended,
            budget,
            note: String::new(),
            done: false,
            aborted: false,
        }
    }

    /// Starts with `selected` ticked instead of the recommended panel, minus anything
    /// not among the rows (the management TUI re-picks starting from the installed
    /// panel). Nothing known leaves nothing ticked, rather than quietly offering the
    /// recommended panel as if it were the installed one.
    pub fn with_selected(mut self, selected: &[String]) -> Self {
        self.selected = selected
            .iter()
            .filter(|k| self.rows.iter().any(|r| &r.key == *k))
            .cloned()
            .collect();
        self
    }

    /// The viewport height the installer picks for a terminal this tall: every row if
    /// they fit, never fewer than three.
    pub fn height_for(rows: usize, terminal_height: u16) -> usize {
        rows.min(usize::from(terminal_height).saturating_sub(8))
            .max(3)
    }

    /// Every row, filtered or not.
    pub fn rows(&self) -> &[PickerRow] {
        &self.rows
    }

    /// The filter as typed.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The rows the filter lets through: those whose alias, id, name or lab contains
    /// it, ignoring case.
    ///
    /// The filter never holds a space (Space ticks), so fields joined by one cannot
    /// produce a match that straddles two of them.
    pub fn filtered(&self) -> Vec<&PickerRow> {
        let needle = self.filter.to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                format!("{} {} {} {}", r.key, r.id, r.name, r.lab)
                    .to_lowercase()
                    .contains(&needle)
            })
            .collect()
    }

    /// The cursor's index into [`Picker::filtered`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The first filtered row shown.
    pub fn top(&self) -> usize {
        self.top
    }

    /// Rows shown at a time.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Changes the viewport height (after a terminal resize), keeping the cursor in view.
    pub fn set_height(&mut self, height: usize) {
        self.height = height.max(1);
        self.clamp();
    }

    /// Whether the listing was unreachable.
    pub fn offline(&self) -> bool {
        self.offline
    }

    /// The recommended panel, as far as the rows carry it.
    pub fn recommended(&self) -> &[String] {
        &self.recommended
    }

    /// The budget panel, as far as the rows carry it.
    pub fn budget(&self) -> &[String] {
        &self.budget
    }

    /// The ticked keys, in the order they were ticked.
    pub fn selected(&self) -> &[String] {
        &self.selected
    }

    /// What the last key had to say, or empty.
    pub fn note(&self) -> &str {
        &self.note
    }

    /// Whether Enter confirmed a selection.
    pub fn done(&self) -> bool {
        self.done
    }

    /// Whether Ctrl+C quit.
    pub fn aborted(&self) -> bool {
        self.aborted
    }

    /// The labs that hold more than one pick, each named by its first row's lab.
    pub fn same_lab(&self) -> Vec<String> {
        let mut groups: IndexMap<&str, (usize, &str)> = IndexMap::new();
        for r in self.rows.iter().filter(|r| self.selected.contains(&r.key)) {
            groups
                .entry(r.vendor.as_str())
                .or_insert((0, r.lab.as_str()))
                .0 += 1;
        }
        groups
            .into_values()
            .filter(|(n, _)| *n > 1)
            .map(|(_, lab)| lab.to_string())
            .collect()
    }

    /// Applies one key.
    pub fn step(&mut self, key: Key) {
        let view_len = self.filtered().len() as i64;
        let before = self.filter.clone();
        let height = self.height as i64;
        let mut cursor = self.cursor as i64;
        self.note.clear();
        match key {
            Key::Up => cursor -= 1,
            Key::Down => cursor += 1,
            Key::PageUp => cursor -= height,
            Key::PageDown => cursor += height,
            Key::Home => cursor = 0,
            Key::End => cursor = view_len - 1,
            Key::Space | Key::Char(' ') => {
                if view_len > 0 {
                    let k = self.filtered()[self.cursor].key.clone();
                    if self.selected.contains(&k) {
                        self.selected.retain(|s| *s != k);
                    } else {
                        self.selected.push(k);
                    }
                }
            }
            Key::Enter => {
                if self.selected.is_empty() {
                    self.note = NOTHING_SELECTED.to_string();
                } else {
                    self.done = true;
                }
            }
            Key::Esc => self.filter.clear(),
            Key::Backspace => {
                self.filter.pop();
            }
            Key::CtrlR => self.selected = self.recommended.clone(),
            Key::CtrlB => self.selected = self.budget.clone(),
            Key::CtrlC => self.aborted = true,
            Key::Char(c) => self.filter.push(c),
            _ => {}
        }
        if self.filter != before {
            cursor = 0;
            self.top = 0;
        }
        let n = self.filtered().len() as i64;
        self.cursor = cursor.min(n - 1).max(0) as usize;
        self.clamp();
    }

    fn clamp(&mut self) {
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        if self.cursor >= self.top + self.height {
            self.top = self.cursor + 1 - self.height;
        }
    }

    /// The picker as text: always `height + 5` lines, none wider than `width`. Only the
    /// cursor row starts with `>`.
    pub fn lines(&self, width: usize) -> Vec<String> {
        let view = self.filtered();
        let mut lines = Vec::with_capacity(self.height + CHROME_LINES);
        let mut first = format!(
            "  Filter: {}_   {} of {} models",
            self.filter,
            view.len(),
            self.rows.len()
        );
        if self.offline {
            first.push_str(OFFLINE_NOTE);
        }
        lines.push(first);
        lines.push(format_row(
            " ",
            "   ",
            " ",
            "model (* recommended)",
            "lab",
            "USD/M in",
            "out",
            "context",
            "",
        ));
        for i in self.top..self.top + self.height {
            let Some(r) = view.get(i) else {
                lines.push(String::new());
                continue;
            };
            lines.push(format_row(
                if i == self.cursor { ">" } else { " " },
                if self.selected.contains(&r.key) {
                    "[x]"
                } else {
                    "[ ]"
                },
                if self.recommended.contains(&r.key) {
                    "*"
                } else {
                    " "
                },
                &clip(&r.key, 36),
                &clip(&r.lab, 11),
                &format_price(r.price_in),
                &format_price(r.price_out),
                &format_context(r.context),
                &r.text,
            ));
        }
        lines.push(format!(
            "  {} selected: {}",
            self.selected.len(),
            self.selected.join(", ")
        ));
        let dupes = self.same_lab();
        lines.push(if !self.note.is_empty() {
            format!("  {}", self.note)
        } else if !dupes.is_empty() {
            format!(
                "  Two picks from one lab ({}): three labs is the point.",
                dupes.join(", ")
            )
        } else {
            String::new()
        });
        lines.push(LEGEND.to_string());
        lines.into_iter().map(|l| clip(&l, width)).collect()
    }
}

/// Paints the picker into `area` with the default theme. The viewport height is the
/// picker's own; size it with [`Picker::set_height`] to `area.height - 5` first.
pub fn render(picker: &Picker, area: Rect, buf: &mut Buffer) {
    render_with(picker, area, buf, &Theme::default());
}

/// [`render`] with a theme: the cursor row in the cursor colour, the header and legend
/// dim, the footer note as a note.
pub fn render_with(picker: &Picker, area: Rect, buf: &mut Buffer, theme: &Theme) {
    let width = usize::from(area.width);
    let lines = picker.lines(width);
    let note_line = picker.height() + 3;
    for (i, line) in lines.iter().enumerate().take(usize::from(area.height)) {
        let style = if i == 1 || i == lines.len() - 1 {
            theme.dim
        } else if i == note_line {
            theme.note
        } else if line.starts_with('>') {
            theme.cursor
        } else {
            theme.text
        };
        buf.set_stringn(area.x, area.y + i as u16, line, width, style);
    }
}

#[cfg(test)]
mod tests;
