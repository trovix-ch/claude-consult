//! The curated favourites (`catalog.json`, embedded at build time) and the checks on them.

use std::collections::HashSet;
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::GenerateError;
use crate::openrouter::{Client, FetchError};
use crate::util::{py_repr, strip_bom};

/// The catalog as shipped, byte for byte.
pub const CATALOG_JSON: &str = include_str!("../../../catalog.json");

/// Every field a favourite must carry.
pub const CURATED_FIELDS: [&str; 9] = [
    "id", "display", "lab", "tier", "command", "tagline", "plays_to", "pitch", "note",
];

/// Commands the panel itself owns; no reviewer may take one.
pub const RESERVED_COMMANDS: [&str; 2] = ["consult", "cleanroom"];

/// OpenRouter's whole model listing. The whole listing rather than the tool-capable
/// one, so a favourite that lost tool calling is told apart from one that was withdrawn.
pub const FULL_LISTING_PATH: &str = "/models";

/// One favourite: what OpenRouter's listing cannot say about a model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CuratedModel {
    /// OpenRouter model id.
    pub id: String,
    /// Display name.
    pub display: String,
    /// The lab that trained it.
    pub lab: String,
    /// `panel`, `budget`, `alternate` or `premium`; `None` for a model from the listing.
    pub tier: Option<String>,
    /// Its quick command, `/<command>`.
    pub command: String,
    /// A few words on what it is.
    pub tagline: String,
    /// What it plays to, for the panel table.
    pub plays_to: String,
    /// The paragraph its quick command shows.
    pub pitch: String,
    /// Notes kept in models.json.
    pub note: String,
}

/// The curated favourites, in file order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    /// The recommended panel, as aliases.
    #[serde(default)]
    pub default_panel: Vec<String>,
    /// The budget panel, as aliases.
    #[serde(default)]
    pub budget_panel: Vec<String>,
    /// Every favourite by alias, in file order.
    pub models: IndexMap<String, CuratedModel>,
}

/// Why this id can never be a reviewer, or `None` if its name allows it.
pub fn refusal(model_id: &str) -> Option<&'static str> {
    let low = model_id.to_lowercase();
    // The tool exists to get a view from outside Claude's lineage; a Claude
    // reviewer defeats that and bills the same model twice. OpenRouter's
    // "~anthropic/..." router aliases reach Claude as well.
    if low.trim_start_matches('~').starts_with("anthropic/") {
        return Some("is an Anthropic model; consult is for non-Claude reviewers only");
    }
    // These pick the model per request, from the id alone or from a preset
    // saved in the key's account, and the pick can be Claude.
    if low.starts_with("openrouter/") {
        return Some("lets OpenRouter choose the model, and it can choose Claude");
    }
    if low.contains('@') {
        return Some(
            "runs an OpenRouter preset, which can send the request to any model, Claude included",
        );
    }
    // A ":batch" variant serves OpenRouter's batch processing, and a review
    // needs a live tool-calling loop.
    if low.ends_with(":batch") {
        return Some(
            "is a :batch variant, for OpenRouter's batch processing; a review needs a live tool-calling loop",
        );
    }
    None
}

/// Whether a listing entry is priced per request, which is how a router shows itself.
pub fn priced_per_request(listed: &Value) -> bool {
    // A router is listed at -1: it costs whatever the model it picks for each
    // request costs, and it can pick Claude. No id pattern gives one away.
    let pricing = listed.get("pricing").filter(|p| p.is_object());
    ["prompt", "completion"]
        .iter()
        .any(|k| crate::util::negative(pricing.and_then(|p| p.get(*k))))
}

/// Parses and validates a catalog: required fields, refused ids, no `/` in an alias,
/// unique commands clear of the reserved ones, and panels that name known aliases.
pub fn load_catalog(text: &str) -> Result<Catalog, GenerateError> {
    let raw: Value = serde_json::from_str(strip_bom(text))
        .map_err(|e| GenerateError::invalid(format!("the catalog is not valid JSON ({e})")))?;
    let models = raw.get("models").and_then(Value::as_object);
    if models.is_none_or(|m| m.is_empty()) {
        return Err(GenerateError::invalid("the catalog lists no models"));
    }
    let mut commands: HashSet<String> = HashSet::new();
    for (alias, m) in models.into_iter().flatten() {
        let missing: Vec<&str> = CURATED_FIELDS
            .iter()
            .copied()
            .filter(|k| m.get(*k).is_none())
            .collect();
        if !missing.is_empty() {
            return Err(GenerateError::invalid(format!(
                "catalog model {} is missing {}",
                py_repr(alias),
                missing.join(", ")
            )));
        }
        let id = m.get("id").and_then(Value::as_str).unwrap_or_default();
        if let Some(why) = refusal(id) {
            return Err(GenerateError::invalid(format!(
                "catalog model {} {why}",
                py_repr(alias)
            )));
        }
        // A panel entry with a slash is read as an OpenRouter id.
        if alias.contains('/') {
            return Err(GenerateError::invalid(format!(
                "catalog alias {} contains '/'",
                py_repr(alias)
            )));
        }
        // Unique here, so a name derived for a model from the listing only has
        // to steer clear of these, and never has to displace one.
        let command = m.get("command").and_then(Value::as_str).unwrap_or_default();
        if RESERVED_COMMANDS.contains(&command) || !commands.insert(command.to_string()) {
            return Err(GenerateError::invalid(format!(
                "catalog model {} reuses the command {}",
                py_repr(alias),
                py_repr(command)
            )));
        }
    }
    let cat: Catalog = serde_json::from_value(raw)
        .map_err(|e| GenerateError::invalid(format!("the catalog is malformed ({e})")))?;
    for (key, panel) in [
        ("default_panel", &cat.default_panel),
        ("budget_panel", &cat.budget_panel),
    ] {
        for alias in panel {
            if !cat.models.contains_key(alias) {
                return Err(GenerateError::invalid(format!(
                    "{key} names {}, which is not in the catalog",
                    py_repr(alias)
                )));
            }
        }
    }
    Ok(cat)
}

/// The embedded catalog, validated.
pub fn embedded() -> Result<Catalog, GenerateError> {
    load_catalog(CATALOG_JSON)
}

/// The outcome of checking the favourites against the live listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CatalogCheck {
    /// Aliases that are listed and tool-capable.
    pub ok: Vec<String>,
    /// One line per favourite that is not.
    pub problems: Vec<String>,
}

/// The favourites that check out, and the problems found, from a parsed listing body.
///
/// Fails only when the body has no `data` list, in which case nothing was checked.
pub fn check(
    favourites: &IndexMap<String, CuratedModel>,
    listing: &Value,
) -> Result<CatalogCheck, String> {
    let data = listing
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| "the listing has no 'data' list".to_string())?;
    let live: IndexMap<&str, &Value> = data
        .iter()
        .filter_map(|m| Some((m.get("id")?.as_str()?, m)))
        .collect();
    let mut out = CatalogCheck {
        ok: Vec::new(),
        problems: Vec::new(),
    };
    for (alias, m) in favourites {
        match live.get(m.id.as_str()) {
            None => out
                .problems
                .push(format!("{alias}: {} is no longer listed", m.id)),
            Some(lm) => {
                let tools = lm
                    .get("supported_parameters")
                    .and_then(Value::as_array)
                    .is_some_and(|p| p.iter().any(|x| x.as_str() == Some("tools")));
                if tools {
                    out.ok.push(alias.clone());
                } else {
                    out.problems
                        .push(format!("{alias}: {} no longer supports tool calling", m.id));
                }
            }
        }
    }
    Ok(out)
}

/// [`check`] on a raw listing body, as fetched from the full `/models` listing.
pub fn check_catalog(catalog: &Catalog, listing_body: &str) -> Result<CatalogCheck, String> {
    let body: Value = serde_json::from_str(strip_bom(listing_body)).map_err(|e| e.to_string())?;
    check(&catalog.models, &body)
}

/// Fetches OpenRouter's whole model listing (public, no key, no cost), within 30 s.
pub async fn fetch_full_listing(client: &Client) -> Result<String, FetchError> {
    client
        .get_public(FULL_LISTING_PATH, Duration::from_secs(30))
        .await
}

/// The `check-catalog` report and its exit code: 0 every favourite checks out, 1 at
/// least one problem, 2 the listing could not be fetched or read, so nothing was checked.
/// `quiet` prints problems only.
pub fn check_report(
    catalog: &Catalog,
    outcome: Result<CatalogCheck, String>,
    quiet: bool,
) -> (String, i32) {
    let checked = match outcome {
        Ok(c) => c,
        Err(e) => {
            return (
                format!("!!  could not read https://openrouter.ai/api/v1/models: {e}"),
                2,
            );
        }
    };
    let mut lines = Vec::new();
    if !quiet {
        for alias in &checked.ok {
            let id = catalog.models.get(alias).map_or("", |m| m.id.as_str());
            lines.push(format!("ok  {alias:<22} {id}"));
        }
    }
    lines.extend(checked.problems.iter().map(|p| format!("!!  {p}")));
    if !quiet {
        lines.push(format!(
            "{} favourites checked, {} problem(s)",
            catalog.models.len(),
            checked.problems.len()
        ));
    }
    let code = if checked.problems.is_empty() { 0 } else { 1 };
    (lines.join("\n"), code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn modified(edit: impl FnOnce(&mut Value)) -> String {
        let mut cat: Value = serde_json::from_str(CATALOG_JSON).expect("catalog");
        edit(&mut cat);
        cat.to_string()
    }

    fn error(text: &str) -> String {
        load_catalog(text).expect_err("refused").to_string()
    }

    #[test]
    fn embedded_catalog_is_valid() {
        let cat = embedded().expect("valid");
        assert_eq!(
            cat.default_panel,
            ["deepseek-v4-pro", "glm-5.2", "gpt-6-luna-pro"]
        );
        assert_eq!(
            cat.models.keys().next().map(String::as_str),
            Some("deepseek-v4-pro")
        );
    }

    #[test]
    fn catalog_holds_curated_fields_only() {
        // K1: prices and context sizes drift within hours; they come live.
        let raw: Value = serde_json::from_str(CATALOG_JSON).expect("catalog");
        let allowed = [
            "_comment",
            "_sources",
            "default_panel",
            "budget_panel",
            "models",
        ];
        for key in raw.as_object().expect("object").keys() {
            assert!(allowed.contains(&key.as_str()), "{key}");
        }
        let money = regex::Regex::new(r"\$|USD|/M\b").expect("regex");
        for (alias, m) in raw["models"].as_object().expect("models") {
            let mut keys: Vec<&str> = m
                .as_object()
                .expect("model")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            let mut want = CURATED_FIELDS.to_vec();
            want.sort_unstable();
            assert_eq!(keys, want, "{alias}");
            // Nor in the curated text, where they would go stale just the same.
            for field in ["tagline", "plays_to", "pitch", "note"] {
                assert!(
                    !money.is_match(m[field].as_str().expect("str")),
                    "{alias}.{field}"
                );
            }
        }
        assert!(!money.is_match(raw["_comment"].as_str().expect("str")));
    }

    #[test]
    fn refused_ids_in_the_catalog() {
        for (id, why) in [
            ("anthropic/claude-haiku-4.5", "Anthropic"),
            ("z-ai/glm-5.2:BATCH", ":batch variant"),
            ("openrouter/auto", "catalog model 'glm-5.2'"),
            ("z-ai/glm-5.2@preset/x", "catalog model 'glm-5.2'"),
        ] {
            let text = modified(|c| c["models"]["glm-5.2"]["id"] = json!(id));
            let e = error(&text);
            assert!(e.contains(why), "{id}: {e}");
        }
    }

    #[test]
    fn catalog_commands_must_be_unique() {
        let text = modified(|c| c["models"]["glm-5.2"]["command"] = json!("deepseek"));
        assert!(error(&text).contains("reuses the command 'deepseek'"));
        let text = modified(|c| c["models"]["glm-5.2"]["command"] = json!("consult"));
        assert!(error(&text).contains("reuses the command 'consult'"));
    }

    #[test]
    fn catalog_structure_is_checked() {
        let text = modified(|c| {
            c["models"]["glm-5.2"]
                .as_object_mut()
                .expect("obj")
                .remove("pitch");
        });
        assert_eq!(error(&text), "catalog model 'glm-5.2' is missing pitch");
        let text = modified(|c| c["default_panel"] = json!(["nope"]));
        assert_eq!(
            error(&text),
            "default_panel names 'nope', which is not in the catalog"
        );
        let text = modified(|c| {
            let m = c["models"]["glm-5.2"].clone();
            c["models"]
                .as_object_mut()
                .expect("obj")
                .insert("a/b".into(), m);
        });
        assert!(error(&text).contains("contains '/'") || error(&text).contains("reuses"));
        assert_eq!(error(r#"{"models": {}}"#), "the catalog lists no models");
    }

    #[test]
    fn refusals() {
        assert!(refusal("Anthropic/claude").is_some());
        assert!(refusal("~anthropic/claude-opus-latest").is_some());
        assert!(refusal("lab-z/anthropic-distill").is_none());
        assert!(refusal("OpenRouter/Auto").is_some());
        assert!(refusal("openai/gpt-4o@preset/x").is_some());
        assert!(refusal("x/y:batch").is_some());
        assert!(priced_per_request(
            &json!({"pricing": {"prompt": "0.1", "completion": "-1"}})
        ));
        assert!(!priced_per_request(&json!({"pricing": {"prompt": "x"}})));
        assert!(!priced_per_request(&json!({"pricing": "free"})));
    }

    #[test]
    fn check_catalog_checks_listing_and_tools_only() {
        let fav = |id: &str| CuratedModel {
            id: id.into(),
            display: String::new(),
            lab: String::new(),
            tier: None,
            command: String::new(),
            tagline: String::new(),
            plays_to: String::new(),
            pitch: String::new(),
            note: String::new(),
        };
        let favourites: IndexMap<String, CuratedModel> = [
            ("a".to_string(), fav("deepseek/deepseek-v4-pro")),
            ("b".to_string(), fav("acme/no-tools")),
            ("c".to_string(), fav("openai/gpt-6-sol")),
        ]
        .into_iter()
        .collect();
        let listing = json!({"data": [
            {"id": "deepseek/deepseek-v4-pro", "supported_parameters": ["max_tokens", "tools"]},
            {"id": "acme/no-tools", "supported_parameters": ["max_tokens"]},
            "not a model", {"name": "no id"}
        ]});
        let res = check(&favourites, &listing).expect("checked");
        assert_eq!(res.ok, ["a"]);
        assert_eq!(
            res.problems,
            [
                "b: acme/no-tools no longer supports tool calling",
                "c: openai/gpt-6-sol is no longer listed"
            ]
        );
        assert!(check(&favourites, &json!({"error": 1})).is_err());
    }

    #[test]
    fn check_report_lines_and_exit_codes() {
        let cat = embedded().expect("catalog");
        let n = cat.models.len();
        let ok: Vec<String> = cat.models.keys().cloned().collect();
        let all_ok = CatalogCheck {
            ok: ok.clone(),
            problems: Vec::new(),
        };
        let (text, code) = check_report(&cat, Ok(all_ok.clone()), false);
        assert_eq!(code, 0);
        assert!(text.starts_with("ok  deepseek-v4-pro        deepseek/deepseek-v4-pro\n"));
        assert!(text.ends_with(&format!("{n} favourites checked, 0 problem(s)")));
        assert_eq!(check_report(&cat, Ok(all_ok), true), (String::new(), 0));
        let bad = CatalogCheck {
            ok,
            problems: vec!["x: y is no longer listed".into()],
        };
        assert_eq!(
            check_report(&cat, Ok(bad), true),
            ("!!  x: y is no longer listed".to_string(), 1)
        );
        let (text, code) = check_report(&cat, Err("HTTP 503".into()), false);
        assert_eq!(code, 2);
        assert_eq!(
            text,
            "!!  could not read https://openrouter.ai/api/v1/models: HTTP 503"
        );
    }
}
