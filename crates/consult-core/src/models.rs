//! The reviewer registry (`models.json`) and resolving the names a caller gives.

use std::path::Path;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;

use crate::error::ConsultError;
use crate::listing::{ListingCache, LiveListing};
use crate::openrouter::Client;
use crate::paths::models_path;
use crate::util::{number, positive_int, py_repr, py_str};

/// One registered reviewer, read leniently from a `models.json` of any vintage.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RegisteredModel {
    /// OpenRouter model id.
    pub id: Option<String>,
    /// The lab.
    pub lab: Option<String>,
    /// Its quick command; installs from before `command` existed have none.
    pub command: Option<String>,
    /// A note.
    pub note: Option<String>,
    /// `false` for a panel member that is not one of the favourites.
    pub curated: Option<bool>,
    /// Context window recorded at install.
    pub context: Option<u64>,
    /// Input price per million tokens recorded at install.
    pub price_in: Option<f64>,
    /// Output price per million tokens recorded at install.
    pub price_out: Option<f64>,
}

/// The registry as the server reads it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Registry {
    /// When the install-time prices held.
    pub priced_at: Option<String>,
    /// Who answers when no reviewer is named.
    pub default_panel: Vec<String>,
    /// Every reviewer by alias, in file order.
    pub models: IndexMap<String, RegisteredModel>,
}

fn opt_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        other => Some(py_str(other)),
    }
}

impl Registry {
    /// Reads any JSON value as a registry, never failing: entries that are not objects
    /// are skipped and unusable numbers read as unknown.
    pub fn from_value(cfg: &Value) -> Self {
        let mut models = IndexMap::new();
        if let Some(map) = cfg.get("models").and_then(Value::as_object) {
            for (alias, m) in map {
                if !m.is_object() {
                    continue;
                }
                models.insert(
                    alias.clone(),
                    RegisteredModel {
                        id: opt_string(m.get("id")),
                        lab: opt_string(m.get("lab")),
                        command: opt_string(m.get("command")),
                        note: opt_string(m.get("note")),
                        curated: m.get("curated").and_then(Value::as_bool),
                        context: positive_int(m.get("context")),
                        price_in: number(m.get("price_in")),
                        price_out: number(m.get("price_out")),
                    },
                );
            }
        }
        let default_panel = cfg
            .get("default_panel")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(py_str).collect())
            .unwrap_or_default();
        Self {
            priced_at: cfg
                .get("priced_at")
                .and_then(Value::as_str)
                .map(str::to_string),
            default_panel,
            models,
        }
    }
}

/// Why `models.json` could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ModelsError {
    /// The file could not be read.
    #[error("FileNotFoundError: {path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// It is not JSON.
    #[error("JSONDecodeError: {0}")]
    Json(String),
}

impl From<ModelsError> for ConsultError {
    fn from(e: ModelsError) -> Self {
        let (kind, message) = match e {
            ModelsError::Io { path, source } => ("FileNotFoundError", format!("{path}: {source}")),
            ModelsError::Json(m) => ("JSONDecodeError", m),
        };
        ConsultError::Other { kind, message }
    }
}

/// `models.json` from the install dir as raw JSON, for a reader that must tolerate anything.
pub fn load_models_value(install_dir: &Path) -> Result<Value, ModelsError> {
    let path = models_path(install_dir);
    let text = std::fs::read(&path).map_err(|source| ModelsError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let text = String::from_utf8_lossy(&text);
    serde_json::from_str(crate::util::strip_bom(&text))
        .map_err(|e| ModelsError::Json(e.to_string()))
}

/// `models.json` from the install dir.
pub fn load_models(install_dir: &Path) -> Result<Registry, ModelsError> {
    load_models_value(install_dir).map(|v| Registry::from_value(&v))
}

/// A reviewer a request resolved to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResolvedModel {
    /// The alias, or the raw name for one that is not registered.
    pub alias: String,
    /// The OpenRouter id sent.
    pub id: String,
    /// Its quick command, if any.
    pub command: Option<String>,
    /// A note; "not in local registry" for a raw id.
    pub note: Option<String>,
    /// `Some(false)` when nobody curated it, which holds it to the live listing.
    pub curated: Option<bool>,
}

impl ResolvedModel {
    /// The short name progress and summaries use: its command, else its alias.
    pub fn short(&self) -> &str {
        // Installs from before models.json carried `command` fall back to the alias.
        match self.command.as_deref() {
            Some(c) if !c.is_empty() => c,
            _ => &self.alias,
        }
    }
}

/// Whether the id reaches an Anthropic model.
pub fn is_anthropic(model_id: &str) -> bool {
    // Case-blind, and "~anthropic/..." router aliases reach Claude as well.
    model_id
        .trim()
        .to_lowercase()
        .trim_start_matches('~')
        .starts_with("anthropic/")
}

/// Whether the id lets OpenRouter choose the model.
pub fn is_router(model_id: &str) -> bool {
    // openrouter/auto and its kin pick a model per request, and the pick can be
    // Claude, so an Anthropic check on the id alone would not catch it.
    model_id.trim().to_lowercase().starts_with("openrouter/")
}

/// Whether the id applies an OpenRouter preset.
pub fn is_preset(model_id: &str) -> bool {
    // "@preset/<slug>", alone or after a model id, applies a preset saved in
    // the key's account, and a preset can name its own models.
    model_id.contains('@')
}

/// The short name of a resolved reviewer.
pub fn short(model: &ResolvedModel) -> &str {
    model.short()
}

/// Resolves requested names (or the default panel) to reviewers: alias, then id, then
/// command, each in file order; an unknown name goes out as a raw id. Refuses the whole
/// request, before anything is spent, on an Anthropic model, a router or a preset.
pub fn resolve_models(
    registry: &Registry,
    requested: Option<&[String]>,
) -> Result<Vec<ResolvedModel>, ConsultError> {
    let names: Vec<String> = match requested {
        Some(r) if !r.is_empty() => r.to_vec(),
        _ => registry.default_panel.clone(),
    };
    let mut out = Vec::new();
    for n in names {
        let n = n.trim();
        // Alias, then id, then command, each in file order, so a name two
        // entries could claim always means the same one. The alias goes first
        // because it is the name the panel lists and the records report.
        let found = if registry.models.contains_key(n) {
            Some(n.to_string())
        } else {
            registry
                .models
                .iter()
                .find(|(_, m)| m.id.as_deref() == Some(n))
                .or_else(|| {
                    registry
                        .models
                        .iter()
                        .find(|(_, m)| m.command.as_deref() == Some(n))
                })
                .map(|(a, _)| a.clone())
        };
        let model = match found.and_then(|a| registry.models.get(&a).map(|m| (a, m))) {
            Some((alias, m)) => ResolvedModel {
                alias,
                id: m.id.clone().unwrap_or_default(),
                command: m.command.clone(),
                note: m.note.clone(),
                curated: m.curated,
            },
            // Any model OpenRouter lists, catalogued or not. Not curated, so
            // resolve_listed holds it to the listing like a non-favourite.
            None => ResolvedModel {
                alias: n.to_string(),
                id: n.to_string(),
                command: None,
                note: Some("not in local registry".to_string()),
                curated: Some(false),
            },
        };
        // Checked on every call rather than trusted to the install: an unknown
        // name goes out as a raw id and models.json can be edited by hand. The
        // whole call is refused, so nothing is spent on the rest of the panel.
        if is_anthropic(&model.id) {
            return Err(ConsultError::msg(format!(
                "{} is an Anthropic model ({}); consult asks only models other than Claude, so it never sends one to OpenRouter.",
                py_repr(n),
                model.id
            )));
        }
        if is_router(&model.id) {
            return Err(ConsultError::msg(format!(
                "{} lets OpenRouter choose the model ({}), and it can choose Claude; name the reviewer instead.",
                py_repr(n),
                model.id
            )));
        }
        if is_preset(&model.id) {
            return Err(ConsultError::msg(format!(
                "{} runs an OpenRouter preset ({}), which can send the request to any model, Claude included; name the reviewer's model id instead.",
                py_repr(n),
                model.id
            )));
        }
        out.push(model);
    }
    Ok(out)
}

/// [`resolve_models`], and then every reviewer nobody curated must be a model OpenRouter
/// lists as tool-capable, whenever the listing answers.
///
/// A raw id and a non-favourite picked at install are held to the listing because a
/// router is only recognisable there: its id can look like any other model's.
/// Unreachable, the checks on the name are all there are, and the consult goes ahead on
/// them rather than failing on a flaky listing.
pub async fn resolve_listed(
    registry: &Registry,
    requested: Option<&[String]>,
    client: &Client,
    listing: &ListingCache,
) -> Result<Vec<ResolvedModel>, ConsultError> {
    let chosen = resolve_models(registry, requested)?;
    let unvetted: Vec<&ResolvedModel> =
        chosen.iter().filter(|m| m.curated == Some(false)).collect();
    if unvetted.is_empty() {
        // Only fetched when needed, so a panel of favourites never waits on it.
        return Ok(chosen);
    }
    let live = listing.get(client).await;
    if let LiveListing::Live { models, .. } = live.as_ref() {
        for m in unvetted {
            if !models.contains_key(&m.id) {
                return Err(ConsultError::msg(format!(
                    "OpenRouter does not list {} as a tool-capable model, so {} is not sent; name a model from its listing, or a registered reviewer (list_reviewers shows them).",
                    py_repr(&m.id),
                    py_repr(&m.alias)
                )));
            }
        }
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn models() -> Registry {
        Registry::from_value(&json!({
            "default_panel": ["alpha", "beta", "gamma"],
            "models": {
                "alpha": {"id": "lab-a/alpha", "context": 100_000, "price_in": 1.0, "price_out": 2.0, "command": "al"},
                "beta": {"id": "lab-b/beta", "context": 100_000, "price_in": 1.0, "price_out": 2.0, "command": "be"},
                "gamma": {"id": "lab-c/gamma", "context": 100_000, "price_in": 1.0, "price_out": 2.0},
            }
        }))
    }

    fn resolve(names: Option<&[&str]>, reg: &Registry) -> Vec<(String, String)> {
        let names: Option<Vec<String>> = names.map(|n| n.iter().map(|s| s.to_string()).collect());
        resolve_models(reg, names.as_deref())
            .expect("resolved")
            .into_iter()
            .map(|m| (m.alias, m.id))
            .collect()
    }

    #[test]
    fn alias_id_and_command_all_name_the_entry() {
        let got = resolve(Some(&["alpha", "lab-a/alpha", "al", " al "]), &models());
        assert_eq!(
            got,
            vec![("alpha".to_string(), "lab-a/alpha".to_string()); 4]
        );
    }

    #[test]
    fn no_names_means_the_default_panel() {
        let got = resolve(None, &models());
        let ids: Vec<&str> = got.iter().map(|(_, i)| i.as_str()).collect();
        assert_eq!(ids, ["lab-a/alpha", "lab-b/beta", "lab-c/gamma"]);
        assert_eq!(resolve(Some(&[]), &models()).len(), 3);
    }

    #[test]
    fn an_unknown_name_goes_to_openrouter_as_an_id() {
        let reg = models();
        let names = vec!["lab-z/new-model".to_string()];
        let m = &resolve_models(&reg, Some(&names)).expect("ok")[0];
        assert_eq!(
            (m.alias.as_str(), m.id.as_str(), m.note.as_deref()),
            (
                "lab-z/new-model",
                "lab-z/new-model",
                Some("not in local registry")
            )
        );
        assert_eq!(m.curated, Some(false));
    }

    #[test]
    fn ambiguity_resolves_alias_then_id_then_command_then_file_order() {
        // Each pair lists the entry that must lose first, so file order alone would pick it.
        let reg = Registry::from_value(&json!({"default_panel": [], "models": {
            "deepseek-v4-pro": {"id": "x/v4", "command": "deepseek"},
            "deepseek": {"id": "x/plain", "command": "ds"},
            "shadow": {"id": "x/named"},
            "x/named": {"id": "x/other"},
            "cmd-owner": {"id": "x/c", "command": "x/clash"},
            "id-owner": {"id": "x/clash"},
            "dup-1": {"id": "x/d1", "command": "dup"},
            "dup-2": {"id": "x/d2", "command": "dup"},
        }}));
        let got: Vec<String> = resolve(Some(&["deepseek", "x/named", "x/clash", "dup"]), &reg)
            .into_iter()
            .map(|(a, _)| a)
            .collect();
        assert_eq!(got, ["deepseek", "x/named", "id-owner", "dup-1"]);
    }

    #[test]
    fn anthropic_only_as_a_prefix() {
        let names = vec!["lab-z/anthropic-distill".to_string()];
        let m = &resolve_models(&models(), Some(&names)).expect("ok")[0];
        assert_eq!(m.id, "lab-z/anthropic-distill");
    }

    #[test]
    fn refusals_name_the_reason() {
        let reg = models();
        for (name, why) in [
            ("anthropic/claude-opus", "is an Anthropic model"),
            ("~anthropic/claude-opus-latest", "is an Anthropic model"),
            ("Anthropic/Claude-Opus", "is an Anthropic model"),
            ("openrouter/auto", "lets OpenRouter choose the model"),
            ("OpenRouter/Auto", "lets OpenRouter choose the model"),
            ("@preset/x", "runs an OpenRouter preset"),
            ("openai/gpt-4o@preset/x", "runs an OpenRouter preset"),
        ] {
            let names = vec!["alpha".to_string(), name.to_string()];
            let e = resolve_models(&reg, Some(&names))
                .expect_err("refused")
                .to_string();
            assert!(e.contains(why), "{name}: {e}");
            assert!(e.starts_with(&format!("'{name}' ")), "{e}");
        }
    }

    #[test]
    fn short_falls_back_to_the_alias() {
        let names = vec!["gamma".to_string(), "alpha".to_string()];
        let got = resolve_models(&models(), Some(&names)).expect("ok");
        assert_eq!(got[0].short(), "gamma");
        assert_eq!(got[1].short(), "al");
    }

    #[test]
    fn a_registry_of_any_vintage_reads() {
        let reg = Registry::from_value(&json!({"default_panel": null, "priced_at": 7, "models": {
            "odd": {"context": "big", "price_in": "x", "note": null},
            "junk": "not an entry"}}));
        assert_eq!(reg.models.len(), 1);
        assert_eq!(reg.models["odd"].context, None);
        assert_eq!(reg.priced_at, None);
        assert!(Registry::from_value(&json!([])).models.is_empty());
    }
}
