//! Renders the curated favourites, the live listing and the templates into the files
//! Claude Code reads, and merges our entries into its settings.json.
//!
//! Nothing here fetches anything: the caller hands in the listing body it saved and when
//! it fetched it. Every check runs before anything is written, so a refusal leaves the
//! machine as it was.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Local, Utc};
use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::catalog::{Catalog, CuratedModel, RESERVED_COMMANDS, refusal};
use crate::display::{ProgressStyle, SummaryStyle};
use crate::error::GenerateError;
use crate::key::{KEY_VAR, clean_key, validate_key_format};
use crate::listing::reviewable_models_text;
use crate::paths::{DISPLAY_FILE, MANIFEST_FILE, MODELS_FILE, binary_path, settings_path};
use crate::settings::{
    HookKind, StatusLineMode, StatusLineOutcome, hook_command, load_settings, merge_env,
    merge_hooks, merge_status_line, remove_ours, save_settings, settings_diff,
};
use crate::util::{
    json_ascii, py_dumps, py_repr, resolve_lenient, round_to, splitlines, strip_bom,
};

pub use crate::util::write_atomic;

/// Spelled-out panel sizes.
pub const NUMBER_WORDS: [&str; 6] = ["one", "two", "three", "four", "five", "six"];
/// What an unpriced model shows.
pub const PRICE_UNKNOWN: &str = "price unknown";

// What a panel member from the live listing gets in place of curated text.
// Nothing is known about it beyond its listing entry, so it claims nothing.
/// `plays_to` of a model from the listing.
pub const FALLBACK_PLAYS_TO: &str = "not curated";
/// `pitch` of a model from the listing.
pub const FALLBACK_PITCH: &str =
    "Not one of the curated favourites, so there are no notes on what it plays to.";
/// `note` of a model from the listing.
pub const FALLBACK_NOTE: &str = "Not one of the curated favourites.";

fn placeholder() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{\{[A-Z_]+\}\}").expect("static regex"))
}

// Claude Code substitutes $0, $1, ... in commands and skills with the caller's
// positional arguments, so "~$0.06" renders as "~<first word>.06". Amounts are
// written "0.06 USD" instead; this pattern refuses any that slip back in.
fn arg_substitution() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\$\d").expect("static regex"))
}

// Loose on purpose: OpenRouter owns the id space. Tight enough that an id can go
// into a markdown cell and a command's YAML description line unharmed.
fn model_id() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9~][\w.~-]*/[\w.:-]+$").expect("static regex"))
}

// ---- templates ---------------------------------------------------------------

/// The command, skill and workflow templates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Templates {
    /// `commands/consult.md`.
    pub consult: String,
    /// `commands/cleanroom.md`.
    pub cleanroom: String,
    /// `commands/quick.md`, rendered once per panel member.
    pub quick: String,
    /// `skills/openrouter-workflow/SKILL.md`.
    pub skill: String,
    /// `workflows/verify-claims.js`.
    pub workflow: String,
}

fn lf(text: &str) -> String {
    // Read as text: a checkout with CRLF endings renders the same files.
    text.replace("\r\n", "\n")
}

impl Templates {
    /// The templates shipped with this build.
    pub fn embedded() -> Self {
        Self {
            consult: lf(include_str!("../../../templates/commands/consult.md")),
            cleanroom: lf(include_str!("../../../templates/commands/cleanroom.md")),
            quick: lf(include_str!("../../../templates/commands/quick.md")),
            skill: lf(include_str!(
                "../../../templates/skills/openrouter-workflow/SKILL.md"
            )),
            workflow: lf(include_str!(
                "../../../templates/workflows/verify-claims.js"
            )),
        }
    }
}

// ---- the listing -------------------------------------------------------------

/// The listing's tool-capable models that could be reviewers, by id, and when it was
/// fetched (to the second). No body, an unreadable one or one without a usable model is
/// offline, never an error: an unreachable listing must not block an install.
pub fn load_listing(
    body: Option<&str>,
    fetched: Option<DateTime<Utc>>,
) -> (IndexMap<String, Value>, Option<DateTime<Utc>>) {
    let Some(body) = body else {
        return (IndexMap::new(), None);
    };
    let live = reviewable_models_text(body);
    if live.is_empty() {
        return (live, None);
    }
    let at = fetched.unwrap_or_else(Utc::now);
    let at = DateTime::from_timestamp(at.timestamp(), 0).unwrap_or(at);
    (live, Some(at))
}

/// USD per million tokens from a per-token price; `None` when unknown.
pub fn per_million(value: Option<&Value>) -> Option<f64> {
    let x = match value? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse::<f64>().ok()?,
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => return None,
    } * 1e6;
    (x.is_finite() && x >= 0.0).then(|| round_to(x, 6))
}

/// Context and USD per million tokens from one listing entry; `None` when unknown.
pub fn live_values(listed: Option<&Value>) -> (Option<u64>, Option<f64>, Option<f64>) {
    let Some(listed) = listed else {
        return (None, None, None);
    };
    let pricing = listed.get("pricing").filter(|p| p.is_object());
    let context = listed
        .get("context_length")
        .and_then(Value::as_u64)
        .filter(|c| *c > 0);
    (
        context,
        per_million(pricing.and_then(|p| p.get("prompt"))),
        per_million(pricing.and_then(|p| p.get("completion"))),
    )
}

// ---- the panel ---------------------------------------------------------------

/// A command name for a model from the listing, clear of every name in `taken`.
pub fn derive_command(model_id: &str, taken: &HashSet<String>) -> Result<String, GenerateError> {
    static NON_ALNUM: OnceLock<Regex> = OnceLock::new();
    let rx = NON_ALNUM.get_or_init(|| Regex::new(r"[^a-z0-9]+").expect("static regex"));
    let tail = model_id.split_once('/').map_or(model_id, |(_, t)| t);
    let base = rx
        .replace_all(&tail.to_lowercase(), "-")
        .trim_matches('-')
        .to_string();
    if base.is_empty() {
        return Err(GenerateError::invalid(format!(
            "no command name can be made from {}",
            py_repr(model_id)
        )));
    }
    let mut name = base.clone();
    let mut n = 1;
    while taken.contains(&name) {
        n += 1;
        name = format!("{base}-{n}");
    }
    Ok(name)
}

/// A listing entry's name, made safe for a command's YAML description line.
pub fn display_name(listed: Option<&Value>, model_id: &str) -> String {
    static UNSAFE: OnceLock<Regex> = OnceLock::new();
    let rx = UNSAFE.get_or_init(|| Regex::new(r"[^\w .+()/,-]").expect("static regex"));
    let Some(name) = listed.and_then(|l| l.get("name")).and_then(Value::as_str) else {
        return model_id.to_string();
    };
    // Listing names read "Lab: Model"; the lab has a field of its own, and ": "
    // inside a command's YAML description line would break its frontmatter.
    let name = name.split_once(": ").map_or(name, |(_, rest)| rest);
    let name = rx.replace_all(name, " ");
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        model_id.to_string()
    } else {
        name
    }
}

/// Splits a comma-separated panel, dropping blanks.
pub fn split_panel(panel: &str) -> Vec<String> {
    panel
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// The panel as aliases, and an entry for each member that is no favourite.
///
/// An entry is a favourite's alias or an OpenRouter id, and a favourite's own id counts
/// as that favourite. Any other id must be in the live listing when there is one.
/// Offline only its name can be checked, and a router's name need not give it away.
pub fn pick_panel(
    cat: &Catalog,
    entries: &[String],
    live: &IndexMap<String, Value>,
) -> Result<(Vec<String>, IndexMap<String, CuratedModel>), GenerateError> {
    let models = &cat.models;
    if entries.is_empty() {
        return Err(GenerateError::invalid("the panel is empty"));
    }
    let by_id: IndexMap<&str, &str> = models
        .iter()
        .map(|(a, m)| (m.id.as_str(), a.as_str()))
        .collect();
    // Every favourite is registered, on the panel or not, and the server finds a model
    // by alias or by command, so a derived name must avoid them all.
    let mut taken: HashSet<String> = RESERVED_COMMANDS.iter().map(|c| c.to_string()).collect();
    taken.extend(models.keys().cloned());
    taken.extend(models.values().map(|m| m.command.clone()));
    let mut panel: Vec<String> = Vec::new();
    let mut outside: IndexMap<String, CuratedModel> = IndexMap::new();
    let mut unknown: Vec<&str> = Vec::new();
    for entry in entries {
        if models.contains_key(entry) {
            panel.push(entry.clone());
            continue;
        }
        if !entry.contains('/') {
            unknown.push(entry);
            continue;
        }
        if let Some(why) = refusal(entry) {
            return Err(GenerateError::invalid(format!("{entry} {why}")));
        }
        if let Some(alias) = by_id.get(entry.as_str()) {
            panel.push(alias.to_string());
            continue;
        }
        if !model_id().is_match(entry) {
            return Err(GenerateError::invalid(format!(
                "{} is not an OpenRouter model id",
                py_repr(entry)
            )));
        }
        if !live.is_empty() && !live.contains_key(entry) {
            return Err(GenerateError::invalid(format!(
                "{entry} is not a tool-capable model in OpenRouter's listing"
            )));
        }
        if outside.values().any(|m| m.id == *entry) {
            return Err(GenerateError::invalid("the panel names a model twice"));
        }
        let command = derive_command(entry, &taken)?;
        taken.insert(command.clone());
        outside.insert(
            command.clone(),
            CuratedModel {
                id: entry.clone(),
                display: display_name(live.get(entry), entry),
                lab: entry.split_once('/').map_or("", |(lab, _)| lab).to_string(),
                tier: None,
                command: command.clone(),
                tagline: entry.clone(),
                plays_to: FALLBACK_PLAYS_TO.to_string(),
                pitch: FALLBACK_PITCH.to_string(),
                note: FALLBACK_NOTE.to_string(),
            },
        );
        panel.push(command);
    }
    if !unknown.is_empty() {
        return Err(GenerateError::invalid(format!(
            "not in the catalog: {}",
            unknown.join(", ")
        )));
    }
    let unique: HashSet<&String> = panel.iter().collect();
    if unique.len() != panel.len() {
        return Err(GenerateError::invalid("the panel names a model twice"));
    }
    Ok((panel, outside))
}

/// One registered reviewer with its live values.
#[derive(Clone, Debug, PartialEq)]
pub struct RosterEntry {
    /// The curated fields (or the fallbacks of a model from the listing).
    pub model: CuratedModel,
    /// Whether it is a favourite.
    pub curated: bool,
    /// Live context.
    pub context: Option<u64>,
    /// Live input price per million tokens.
    pub price_in: Option<f64>,
    /// Live output price per million tokens.
    pub price_out: Option<f64>,
}

/// Every favourite in catalog order, then the panel members from outside them, each
/// with its live context and prices.
pub fn build_roster(
    cat: &Catalog,
    outside: &IndexMap<String, CuratedModel>,
    live: &IndexMap<String, Value>,
) -> IndexMap<String, RosterEntry> {
    let mut roster = IndexMap::new();
    for (curated, group) in [(true, &cat.models), (false, outside)] {
        for (alias, m) in group {
            // Live values replace any a catalog still carries: numbers written
            // into the repo are stale within hours.
            let (context, price_in, price_out) = live_values(live.get(&m.id));
            roster.insert(
                alias.clone(),
                RosterEntry {
                    model: m.clone(),
                    curated,
                    context,
                    price_in,
                    price_out,
                },
            );
        }
    }
    roster
}

// ---- rendering -------------------------------------------------------------------

/// Fills a template's placeholders in order, then refuses one with a placeholder left
/// or a `$` before a digit (Claude Code's argument substitution).
pub fn render(
    template: &str,
    values: &[(String, String)],
    name: &str,
) -> Result<String, GenerateError> {
    let mut out = template.to_string();
    for (k, v) in values {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    let left: BTreeSet<&str> = placeholder().find_iter(&out).map(|m| m.as_str()).collect();
    if !left.is_empty() {
        return Err(GenerateError::invalid(format!(
            "{name}: unfilled placeholders {}",
            left.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    for line in splitlines(&out) {
        if arg_substitution().is_match(line) {
            return Err(GenerateError::invalid(format!(
                "{name}: '$' before a digit is replaced by Claude Code's argument substitution; write the amount as 'N USD': {}",
                crate::util::clip_chars(line.trim(), 120)
            )));
        }
    }
    Ok(out)
}

/// USD, never reading as free when paid.
pub fn usd(x: f64) -> String {
    crate::util::usd(x)
}

fn priced(m: &RosterEntry) -> Option<(f64, f64)> {
    Some((m.price_in?, m.price_out?))
}

/// A model's price for prose.
pub fn price(m: &RosterEntry) -> String {
    match priced(m) {
        None => PRICE_UNKNOWN.to_string(),
        Some((i, o)) => format!("{}/{} USD per M tokens in/out", usd(i), usd(o)),
    }
}

/// The placeholder values every template shares, in the order they are filled.
pub fn shared_values(
    roster: &IndexMap<String, RosterEntry>,
    panel: &[String],
    today: &str,
    as_of: Option<&str>,
) -> Vec<(String, String)> {
    let extras: Vec<&String> = roster
        .iter()
        .filter(|(a, m)| m.model.tier.as_deref() == Some("premium") && !panel.contains(a))
        .map(|(a, _)| a)
        .collect();
    let extras_text = if extras.is_empty() {
        "any alias from `list_reviewers`".to_string()
    } else {
        let mut t = extras
            .iter()
            .map(|a| format!("`{a}` ({}, {})", roster[*a].model.lab, price(&roster[*a])))
            .collect::<Vec<_>>()
            .join(", ");
        if extras.iter().any(|a| priced(&roster[*a]).is_some()) {
            t.push_str(&format!("; prices as of {}", as_of.unwrap_or("None")));
        }
        t
    };
    let rows = panel
        .iter()
        .map(|a| {
            let m = &roster[a];
            let cost = match priced(m) {
                Some((i, o)) => format!("{} / {}", usd(i), usd(o)),
                None => PRICE_UNKNOWN.to_string(),
            };
            format!(
                "| `{a}` | {} | {} | {cost} |",
                m.model.lab, m.model.plays_to
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut plus: Vec<Value> = panel.iter().map(|a| json!(a)).collect();
    plus.extend(extras.iter().map(|a| json!(a)));
    let size = NUMBER_WORDS
        .get(panel.len().wrapping_sub(1))
        .map_or_else(|| panel.len().to_string(), |w| w.to_string());
    vec![
        ("PANEL_SIZE".into(), size),
        (
            "PANEL_NAMES".into(),
            panel
                .iter()
                .map(|a| format!("**{}**", roster[a].model.display))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "PANEL_ALIASES".into(),
            panel
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        (
            "QUICK_COMMANDS".into(),
            panel
                .iter()
                .map(|a| format!("`/{}`", roster[a].model.command))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        ("CLEAN_ROOM_EXTRAS".into(), extras_text),
        (
            "PANEL_PLUS_EXTRAS_JSON".into(),
            py_dumps(&Value::Array(plus), true),
        ),
        ("PANEL_TABLE".into(), rows),
        (
            "PRICES_AS_OF".into(),
            match as_of {
                Some(at) => format!("as of {at}"),
                None => PRICE_UNKNOWN.to_string(),
            },
        ),
        ("GENERATED_ON".into(), today.to_string()),
    ]
}

/// Every file destined for the Claude config dir, keyed by absolute path, in the order
/// they are written.
pub fn build_files(
    roster: &IndexMap<String, RosterEntry>,
    panel: &[String],
    templates: &Templates,
    claude_dir: &Path,
    today: &str,
    as_of: Option<&str>,
) -> Result<IndexMap<PathBuf, String>, GenerateError> {
    let shared = shared_values(roster, panel, today, as_of);
    let cmd_dir = claude_dir.join("commands");
    let mut out = IndexMap::new();
    for (name, text) in [
        ("consult.md", &templates.consult),
        ("cleanroom.md", &templates.cleanroom),
    ] {
        out.insert(cmd_dir.join(name), render(text, &shared, name)?);
    }
    for alias in panel {
        let m = &roster[alias];
        let mut values = shared.clone();
        values.extend([
            ("DISPLAY".to_string(), m.model.display.clone()),
            ("ALIAS".to_string(), alias.clone()),
            ("TAGLINE".to_string(), m.model.tagline.clone()),
            ("PITCH".to_string(), m.model.pitch.trim().to_string()),
            (
                "PRICE".to_string(),
                price(m)
                    + &match (priced(m), as_of) {
                        (Some(_), Some(at)) => format!(" as of {at}"),
                        (Some(_), None) => " as of None".to_string(),
                        _ => String::new(),
                    },
            ),
        ]);
        out.insert(
            cmd_dir.join(format!("{}.md", m.model.command)),
            render(&templates.quick, &values, &format!("quick.md for {alias}"))?,
        );
    }
    out.insert(
        claude_dir
            .join("skills")
            .join("openrouter-workflow")
            .join("SKILL.md"),
        render(&templates.skill, &shared, "SKILL.md")?,
    );
    let voices: Vec<Value> = panel.iter().map(|a| json!(a)).collect();
    out.insert(
        claude_dir.join("workflows").join("verify-claims.js"),
        render(
            &templates.workflow,
            &[(
                "WORKFLOW_VOICES".into(),
                py_dumps(&Value::Array(voices), false),
            )],
            "verify-claims.js",
        )?,
    );
    Ok(out)
}

/// models.json: every roster entry with the keys the server reads.
pub fn models_json(
    roster: &IndexMap<String, RosterEntry>,
    panel: &[String],
    today: &str,
    priced_at: Option<&str>,
) -> String {
    let mut models = Map::new();
    for (alias, m) in roster {
        models.insert(
            alias.clone(),
            json!({
                "id": m.model.id,
                "lab": m.model.lab,
                "command": m.model.command,
                "note": m.model.note,
                "curated": m.curated,
                "context": m.context,
                "price_in": m.price_in,
                "price_out": m.price_out,
            }),
        );
    }
    let doc = json!({
        "_comment": format!(
            "Generated by the installer on {today} from catalog.json and OpenRouter's model listing; re-run the installer rather than editing by hand. Context and prices (USD per million tokens) are the listing's at priced_at, null when it could not be read; they drift, and the cost printed with each review comes from OpenRouter's own accounting. curated is false for a panel member that is not one of the favourites. The id of any other model in OpenRouter's tool-capable listing works too; Anthropic models, routers and presets are refused."
        ),
        "priced_at": priced_at,
        "default_panel": panel,
        "models": models,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
}

/// The progress and summary styles an install writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Display {
    /// Progress style.
    pub progress: ProgressStyle,
    /// Summary style.
    pub summary: SummaryStyle,
}

impl Serialize for Display {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(2))?;
        m.serialize_entry("progress", self.progress.as_str())?;
        m.serialize_entry("summary", self.summary.as_str())?;
        m.end()
    }
}

/// A style given on this run wins; otherwise the one already installed stays.
pub fn pick_display(
    install_dir: &Path,
    progress: Option<ProgressStyle>,
    summary: Option<SummaryStyle>,
) -> Display {
    // Notepad and PowerShell 5.1 save a hand edit with a BOM, and failing on it would
    // reset both styles on the next re-install.
    let old = crate::display::read_display(install_dir).unwrap_or_default();
    let text = |k: &str| old.get(k).and_then(Value::as_str);
    Display {
        progress: progress
            .or_else(|| text("progress").and_then(ProgressStyle::parse))
            .unwrap_or_default(),
        summary: summary
            .or_else(|| text("summary").and_then(SummaryStyle::parse))
            .unwrap_or_default(),
    }
}

/// display.json.
pub fn display_json(display: Display) -> String {
    let progress: Vec<&str> = ProgressStyle::ALL.iter().map(|s| s.as_str()).collect();
    let summary: Vec<&str> = SummaryStyle::ALL.iter().map(|s| s.as_str()).collect();
    let doc = json!({
        "_comment": format!(
            "Read on every consult by the server and the hooks, so an edit here applies to the next call without a restart. progress: {}. summary: {}. The summary needs a hook that the installer registers only while summary is not off, so switch it off or back on with the installer's --summary-style rather than here.",
            progress.join(", "),
            summary.join(", ")
        ),
        "progress": display.progress.as_str(),
        "summary": display.summary.as_str(),
    });
    json_ascii(&serde_json::to_string_pretty(&doc).unwrap_or_default()) + "\n"
}

// ---- filesystem --------------------------------------------------------------------

/// manifest.json of an install, or an empty map when there is none or it is unreadable.
pub fn load_manifest(install_dir: &Path) -> Map<String, Value> {
    let Ok(bytes) = std::fs::read(install_dir.join(MANIFEST_FILE)) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(strip_bom(&String::from_utf8_lossy(&bytes))) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> GenerateError + '_ {
    move |e| GenerateError::io(path, e)
}

/// Puts back the user's own file that an earlier install displaced, if any.
///
/// The oldest backup is the one taken before we ever touched the file; later ones can
/// only be of files we did not generate either, so oldest wins.
pub fn restore_original(
    install_dir: &Path,
    claude_dir: &Path,
    target: &Path,
) -> Result<bool, GenerateError> {
    if target.exists() {
        return Ok(false);
    }
    let Ok(rel) = target.strip_prefix(claude_dir) else {
        return Ok(false);
    };
    let Ok(entries) = std::fs::read_dir(install_dir.join("backup")) else {
        return Ok(false);
    };
    let mut runs: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    runs.sort();
    for run in runs {
        let src = run.join(rel);
        if src.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(io(parent))?;
            }
            std::fs::copy(&src, target).map_err(io(target))?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_as_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    // As Python's text mode read it: undecodable bytes replaced, newlines universal.
    Some(
        String::from_utf8_lossy(&bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n"),
    )
}

// ---- install and uninstall --------------------------------------------------------

/// What an install is given. It fetches nothing itself.
#[derive(Clone, Debug)]
pub struct InstallInputs<'a> {
    /// The validated catalog.
    pub catalog: &'a Catalog,
    /// The raw body of the tool-capable listing and when it was fetched; `None` offline.
    pub listing: Option<(&'a str, DateTime<Utc>)>,
    /// Favourites' aliases or OpenRouter ids.
    pub panel: Vec<String>,
    /// The install dir (holds `bin/`, gets models.json, display.json, manifest.json).
    pub install_dir: &'a Path,
    /// Claude Code's config dir.
    pub claude_dir: &'a Path,
    /// The key to store in settings.json, if any.
    pub key: Option<String>,
    /// A progress style to set; `None` keeps the installed one.
    pub progress_style: Option<ProgressStyle>,
    /// A summary style to set; `None` keeps the installed one.
    pub summary_style: Option<SummaryStyle>,
    /// Whether ours may take the status line.
    pub status_line: StatusLineMode,
    /// The templates to render.
    pub templates: &'a Templates,
    /// The install's clock; `None` is now.
    pub now: Option<DateTime<Local>>,
}

/// What an install did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InstallReport {
    /// The panel as aliases.
    pub panel: Vec<String>,
    /// When the prices held, `YYYY-MM-DDTHH:MM:SSZ`; `None` offline.
    pub priced_at: Option<String>,
    /// Every file written into the Claude dir.
    pub written: Vec<String>,
    /// The user's files copied to the backup dir first.
    pub backed_up: Vec<String>,
    /// Where they went, when any did.
    pub backup_dir: Option<String>,
    /// Files of an earlier install that are no longer wanted.
    pub removed: Vec<String>,
    /// User files restored in their place.
    pub restored: Vec<String>,
    /// Settings entries changed.
    pub settings_changed: Vec<String>,
    /// The styles now installed.
    pub display: Display,
    /// What happened to the status line.
    pub status_line: StatusLineOutcome,
}

/// Renders and writes everything an install puts in place: commands, skill, workflow,
/// models.json, display.json, settings.json entries and the manifest.
pub fn install_files(inputs: &InstallInputs<'_>) -> Result<InstallReport, GenerateError> {
    let install_dir = resolve_lenient(inputs.install_dir);
    let claude_dir = resolve_lenient(inputs.claude_dir);
    let (live, saved) = match inputs.listing {
        Some((body, at)) => load_listing(Some(body), Some(at)),
        None => load_listing(None, None),
    };
    let entries: Vec<String> = inputs
        .panel
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let (panel, outside) = pick_panel(inputs.catalog, &entries, &live)?;
    let roster = build_roster(inputs.catalog, &outside, &live);
    let priced_at = saved.map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    let as_of = saved.map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string());

    let key = match inputs.key.as_deref() {
        None => None,
        Some(raw) => {
            // A pipe or an editor may leave a BOM on the key, which a plain trim keeps:
            // the saved key would then fail every request with a 401.
            let key = clean_key(raw);
            validate_key_format(key).map_err(GenerateError::invalid)?;
            Some(key.to_string())
        }
    };

    // Settings would point at it. A hook whose program is missing fails, which Claude
    // Code treats as a blocking hook error on every call.
    let binary = binary_path(&install_dir);
    if !binary.is_file() {
        return Err(GenerateError::invalid(format!(
            "the claude-consult binary is missing from {}: {}",
            install_dir.join("bin").display(),
            crate::paths::exe_name()
        )));
    }

    let now = inputs.now.unwrap_or_else(Local::now);
    let today = now.format("%Y-%m-%d").to_string();
    let files = build_files(
        &roster,
        &panel,
        inputs.templates,
        &claude_dir,
        &today,
        as_of.as_deref(),
    )?;
    let manifest = load_manifest(&install_dir);
    let display = pick_display(&install_dir, inputs.progress_style, inputs.summary_style);

    // Every settings change is worked out before anything is written, so settings we
    // cannot merge stop the install with nothing half-done.
    let settings_file = settings_path(&claude_dir);
    let mut settings = load_settings(&settings_file)?;
    let original = settings.clone();
    let mut changed_settings = merge_env(&mut settings, &settings_file, key.as_deref())?;
    merge_hooks(&mut settings, &settings_file, &install_dir, display.summary)?;
    let displaced_before = manifest
        .get("status_line_displaced")
        .filter(|v| !v.is_null())
        .cloned();
    let (status_line, displaced) = merge_status_line(
        &mut settings,
        &install_dir,
        inputs.status_line,
        displaced_before,
    );
    changed_settings.extend(settings_diff(&original, &settings));

    let previous: BTreeSet<PathBuf> = manifest
        .get("files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    let backup_root = install_dir
        .join("backup")
        .join(now.format("%Y%m%d-%H%M%S").to_string());
    let (mut backed_up, mut removed, mut restored) = (Vec::new(), Vec::new(), Vec::new());

    for (path, text) in &files {
        // A file we did not generate last time is someone's own work: keep a copy
        // before replacing it. Our own previous output is replaced silently.
        if path.exists()
            && !previous.contains(path)
            && read_as_text(path).as_deref() != Some(text.as_str())
        {
            let rel = path.strip_prefix(&claude_dir).unwrap_or(path);
            let dest = backup_root.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(io(parent))?;
            }
            std::fs::copy(path, &dest).map_err(io(&dest))?;
            backed_up.push(path.display().to_string());
        }
        write_atomic(path, text).map_err(io(path))?;
    }

    // A model dropped from the panel must not leave its slash command behind, and
    // whatever it displaced comes back.
    let mut stale: Vec<&PathBuf> = previous
        .iter()
        .filter(|p| !files.contains_key(*p))
        .collect();
    stale.sort_by_key(|p| p.display().to_string());
    for old in stale {
        if old.is_file() {
            std::fs::remove_file(old).map_err(io(old))?;
            removed.push(old.display().to_string());
        }
        if old.starts_with(&claude_dir) && restore_original(&install_dir, &claude_dir, old)? {
            restored.push(old.display().to_string());
        }
    }

    let models_file = install_dir.join(MODELS_FILE);
    write_atomic(
        &models_file,
        &models_json(&roster, &panel, &today, priced_at.as_deref()),
    )
    .map_err(io(&models_file))?;
    let display_file = install_dir.join(DISPLAY_FILE);
    write_atomic(&display_file, &display_json(display)).map_err(io(&display_file))?;
    if settings != original {
        save_settings(&settings_file, &settings)?;
    }

    let mut hooks = Map::new();
    hooks.insert(
        "PostToolUse".into(),
        json!(hook_command(&install_dir, HookKind::Summary)),
    );
    if display.summary != SummaryStyle::Off {
        hooks.insert(
            "MessageDisplay".into(),
            json!(hook_command(&install_dir, HookKind::Display)),
        );
    }
    let status_command = matches!(
        status_line,
        StatusLineOutcome::Ours | StatusLineOutcome::Set | StatusLineOutcome::Replaced
    )
    .then(|| {
        settings
            .get("statusLine")
            .and_then(|s| s.get("command"))
            .cloned()
    })
    .flatten()
    .unwrap_or(Value::Null);
    let mut written: Vec<String> = files.keys().map(|p| p.display().to_string()).collect();
    written.sort();
    let manifest = json!({
        "generated_on": now.format("%Y-%m-%dT%H:%M:%S").to_string(),
        "panel": panel,
        "claude_dir": claude_dir.display().to_string(),
        "files": written,
        // What this install put into settings.json. Uninstall removes every command in
        // our form that runs from the install dir, which covers all of these.
        "settings": {"hooks": hooks, "statusLine": status_command},
        "status_line_displaced": displaced.clone().unwrap_or(Value::Null),
    });
    let manifest_file = install_dir.join(MANIFEST_FILE);
    write_atomic(
        &manifest_file,
        &(serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n"),
    )
    .map_err(io(&manifest_file))?;

    Ok(InstallReport {
        panel,
        priced_at,
        written,
        backup_dir: (!backed_up.is_empty()).then(|| backup_root.display().to_string()),
        backed_up,
        removed,
        restored,
        settings_changed: changed_settings,
        display,
        status_line,
    })
}

/// What an uninstall did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UninstallReport {
    /// Files removed from the Claude dir.
    pub removed: Vec<String>,
    /// User files restored from backup.
    pub restored: Vec<String>,
    /// Whether the key was taken out of settings.json.
    pub key_removed: bool,
    /// Settings entries removed.
    pub settings_removed: Vec<String>,
    /// Whether a displaced status line was put back.
    pub status_line_restored: bool,
}

/// Removes what an install wrote into the Claude dir and its settings.json entries,
/// restoring the user's displaced files. The install dir itself is left for the caller
/// to delete (the backups live there until then).
pub fn uninstall_files(
    install_dir: &Path,
    claude_dir: &Path,
    remove_key: bool,
) -> Result<UninstallReport, GenerateError> {
    let install_dir = resolve_lenient(install_dir);
    let claude_dir = resolve_lenient(claude_dir);
    let manifest = load_manifest(&install_dir);

    // Settings first: if they cannot be read, stop before any file is touched.
    let mut report = UninstallReport {
        removed: Vec::new(),
        restored: Vec::new(),
        key_removed: false,
        settings_removed: Vec::new(),
        status_line_restored: false,
    };
    let settings_file = settings_path(&claude_dir);
    if settings_file.is_file() {
        let mut data = load_settings(&settings_file)?;
        let original = data.clone();
        let displaced = manifest
            .get("status_line_displaced")
            .filter(|v| !v.is_null())
            .cloned();
        (report.settings_removed, report.status_line_restored) =
            remove_ours(&mut data, &install_dir, displaced);
        if remove_key
            && let Some(env) = data.get_mut("env").and_then(Value::as_object_mut)
            && env.shift_remove(KEY_VAR).is_some()
        {
            report.key_removed = true;
        }
        if data != original {
            save_settings(&settings_file, &data)?;
        }
    }

    let files: Vec<PathBuf> = manifest
        .get("files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    for p in files {
        if p.is_file() {
            std::fs::remove_file(&p).map_err(io(&p))?;
            report.removed.push(p.display().to_string());
        }
        // The backups live in the install dir, which the caller deletes next.
        if p.starts_with(&claude_dir) && restore_original(&install_dir, &claude_dir, &p)? {
            report.restored.push(p.display().to_string());
        }
    }
    let skill_dir = claude_dir.join("skills").join("openrouter-workflow");
    if skill_dir.is_dir() && std::fs::read_dir(&skill_dir).is_ok_and(|mut d| d.next().is_none()) {
        std::fs::remove_dir(&skill_dir).map_err(io(&skill_dir))?;
    }
    Ok(report)
}

// ---- the panel check the installer runs before it installs --------------------------

/// The verdict on a chosen panel, as the installer's picker and `--panel` check it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelChoice {
    /// Fine as it is.
    Ok,
    /// Two seats from one lab: allowed only if the user confirms. The message says why.
    SameLab {
        /// The labs with more than one seat.
        labs: Vec<String>,
        /// What to tell the user.
        message: String,
    },
    /// Refused, with the reason.
    Refused(String),
}

/// A favourite given by its id becomes its alias, so it keeps its curated text and
/// cannot sit on the panel twice.
pub fn panel_keys(cat: &Catalog, entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .map(|e| {
            cat.models
                .iter()
                .find(|(_, m)| m.id == *e)
                .map_or_else(|| e.clone(), |(a, _)| a.clone())
        })
        .collect()
}

/// Checks a panel as the installer does before installing: no Claude, no `:batch`, a
/// non-favourite must be an id the listing carries (and cannot be checked offline), a
/// listed favourite must still be listed, nothing twice; two seats from one lab needs
/// the user's say-so. `listing` is the reviewable models, `None` offline.
pub fn check_panel_choice(
    cat: &Catalog,
    chosen: &[String],
    listing: Option<&IndexMap<String, Value>>,
) -> PanelChoice {
    let mut labs: Vec<String> = Vec::new();
    for a in chosen {
        let fav = cat.models.get(a);
        let id = fav.map_or(a.as_str(), |f| f.id.as_str());
        let low = id.to_lowercase();
        if low
            .strip_prefix('~')
            .unwrap_or(&low)
            .starts_with("anthropic/")
        {
            return PanelChoice::Refused(format!("'{a}': consult asks models other than Claude."));
        }
        if low.ends_with(":batch") {
            return PanelChoice::Refused(format!(
                "'{a}' is for OpenRouter's batch processing; a review needs a live model."
            ));
        }
        if fav.is_none() && !id.contains('/') {
            return PanelChoice::Refused(format!(
                "'{a}' is neither a favourite nor an OpenRouter id (provider/model)."
            ));
        }
        if fav.is_none() && listing.is_none() {
            return PanelChoice::Refused(format!(
                "'{a}' cannot be checked while OpenRouter's listing is unreachable. Pick favourites, or retry later."
            ));
        }
        if let Some(listing) = listing
            && !listing.contains_key(id)
        {
            return PanelChoice::Refused(format!(
                "'{a}' ({id}) is not offered on OpenRouter with tool calling."
            ));
        }
        labs.push(id.split('/').next().unwrap_or("").to_lowercase());
    }
    let unique: HashSet<&String> = chosen.iter().collect();
    if unique.len() != chosen.len() {
        return PanelChoice::Refused("A model is listed twice.".to_string());
    }
    let mut dupes: Vec<String> = Vec::new();
    for lab in &labs {
        if labs.iter().filter(|l| *l == lab).count() > 1 && !dupes.contains(lab) {
            dupes.push(lab.clone());
        }
    }
    if dupes.is_empty() {
        return PanelChoice::Ok;
    }
    let message = format!(
        "Two panel seats from one lab ({}): their agreement says less than two labs agreeing.",
        dupes.join(", ")
    );
    PanelChoice::SameLab {
        labs: dupes,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_command_names() {
        let taken: HashSet<String> = ["consult", "deepseek", "deepseek-2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            derive_command("mistralai/Devstral_2:free", &taken).expect("ok"),
            "devstral-2-free"
        );
        assert_eq!(derive_command("x/--A..b--", &taken).expect("ok"), "a-b");
        assert_eq!(
            derive_command("x/consult", &taken).expect("ok"),
            "consult-2"
        );
        assert_eq!(
            derive_command("x/deepseek", &taken).expect("ok"),
            "deepseek-3"
        );
        assert!(derive_command("x/-.-", &taken).is_err());
    }

    #[test]
    fn render_refuses_argument_substitution_and_unfilled_placeholders() {
        let v = |k: &str, x: &str| vec![(k.to_string(), x.to_string())];
        assert_eq!(
            render("{{A}} costs 0.06 USD", &v("A", "it"), "t").expect("ok"),
            "it costs 0.06 USD"
        );
        assert!(
            render("about ~$0.06 a call", &[], "t")
                .expect_err("refused")
                .to_string()
                .contains("argument")
        );
        assert!(
            render("{{A}}", &v("A", "$1 each"), "t")
                .expect_err("refused")
                .to_string()
                .contains("argument")
        );
        let e = render("{{A}} {{MISSING}}", &v("A", "x"), "t")
            .expect_err("refused")
            .to_string();
        assert_eq!(e, "t: unfilled placeholders {{MISSING}}");
    }

    #[test]
    fn display_names_are_safe() {
        let listed = json!({"name": "Acme: Weird: $5 {{NAME}} #1 \"model\""});
        assert_eq!(
            display_name(Some(&listed), "acme/weird"),
            "Weird 5 NAME 1 model"
        );
        assert_eq!(display_name(None, "a/b"), "a/b");
        assert_eq!(display_name(Some(&json!({"name": ": "})), "a/b"), "a/b");
    }

    #[test]
    fn live_values_parse_leniently() {
        let v = json!({"context_length": 262_144, "pricing": {"prompt": "0.0000004", "completion": "0.000002"}});
        assert_eq!(live_values(Some(&v)), (Some(262_144), Some(0.4), Some(2.0)));
        let v = json!({"context_length": null, "pricing": {"prompt": "", "completion": "x"}});
        assert_eq!(live_values(Some(&v)), (None, None, None));
        let v = json!({"context_length": 1.5, "pricing": {"prompt": "0", "completion": "-1"}});
        assert_eq!(live_values(Some(&v)), (None, Some(0.0), None));
    }

    #[test]
    fn embedded_templates_have_their_placeholders() {
        let t = Templates::embedded();
        assert!(t.quick.contains("{{ALIAS}}"));
        assert!(
            t.workflow
                .contains("const DEFAULT_VOICES = {{WORKFLOW_VOICES}}\n")
        );
        assert!(!t.cleanroom.contains('\r'));
    }
}
