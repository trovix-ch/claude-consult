//! The panel checks the installer runs on `--panel` and on the picker's choice.
//! Ported from the panel-validation parts of tests/test_picker.py; the picker's own
//! state machine belongs to the TUI crate.

use consult_core::catalog::{Catalog, load_catalog};
use consult_core::generate::{PanelChoice, check_panel_choice, panel_keys};
use consult_core::listing::reviewable_models;
use indexmap::IndexMap;
use serde_json::{Value, json};

fn fav(id: &str, display: &str, lab: &str, tier: &str) -> Value {
    json!({"id": id, "display": display, "lab": lab, "tier": tier,
           "command": display.split_whitespace().next().unwrap_or("").to_lowercase(),
           "tagline": format!("{display} tagline"), "plays_to": "", "pitch": "", "note": ""})
}

fn catalog() -> Catalog {
    let raw = json!({
        "default_panel": ["alpha", "beta", "gamma"],
        "budget_panel": ["delta", "beta"],
        "models": {
            "alpha": fav("deepseek/alpha-1", "Alpha One", "DeepSeek", "panel"),
            "beta": fav("z-ai/beta-2", "Beta Two", "Z.ai", "panel"),
            "gamma": fav("openai/gamma-3", "Gamma Three", "OpenAI", "panel"),
            "delta": fav("minimax/delta-4", "Delta Four", "MiniMax", "budget"),
            "retired": fav("x-ai/retired-5", "Retired Five", "xAI", "alternate"),
        },
    });
    load_catalog(&raw.to_string()).expect("catalog")
}

fn live(id: &str, name: &str, prompt: &str, completion: &str, tools: bool) -> Value {
    let mut params = vec!["max_tokens", "temperature"];
    if tools {
        params.extend(["tool_choice", "tools"]);
    }
    json!({"id": id, "name": name, "context_length": 131_072,
           "pricing": {"prompt": prompt, "completion": completion},
           "supported_parameters": params})
}

/// Listing order stands for popularity. retired-5 is missing: OpenRouter dropped it.
fn listing() -> IndexMap<String, Value> {
    let body = json!({"data": [
        live("z-ai/glm-other", "Z.ai: GLM Other", "0.000001", "0.000002", true),
        live("anthropic/claude-x", "Anthropic: Claude X", "0.000001", "0.000002", true),
        live("deepseek/alpha-1", "DeepSeek: Alpha One", "0.0000005", "0.0000011", true),
        live("mistralai/no-tools", "Mistral: No Tools", "0.000001", "0.000002", false),
        live("openai/gamma-3", "OpenAI: Gamma Three", "0.000001", "0.000002", true),
        live("openai/gamma-3:batch", "OpenAI: Gamma Three (batch)", "0.000001", "0.000002", true),
        live("qwen/q-one", "Qwen: Q One", "0.000001", "0.000002", true),
        live("z-ai/beta-2", "Z.ai: Beta Two", "0.000001", "0.000002", true),
        live("minimax/delta-4", "MiniMax: Delta Four", "0.000001", "0.000002", true),
        live("qwen/q-two", "Qwen: Q Two", "0.000001", "0.000002", true),
        live("nvidia/n-three:free", "NVIDIA: N Three (free)", "0", "0", true),
        // A router: it lists a price of -1 and picks the model per request.
        live("openrouter/auto", "Auto Router", "-1", "-1", true),
    ]});
    reviewable_models(&body)
}

#[test]
fn only_reviewable_models_are_offered() {
    let ids: Vec<String> = listing().keys().cloned().collect();
    assert_eq!(
        ids,
        [
            "z-ai/glm-other",
            "deepseek/alpha-1",
            "openai/gamma-3",
            "qwen/q-one",
            "z-ai/beta-2",
            "minimax/delta-4",
            "qwen/q-two",
            "nvidia/n-three:free"
        ]
    );
}

#[test]
fn a_favourites_id_becomes_its_alias() {
    let entries: Vec<String> = ["z-ai/beta-2", "qwen/q-one", "alpha"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        panel_keys(&catalog(), &entries),
        ["beta", "qwen/q-one", "alpha"]
    );
}

#[test]
fn panel_entries_are_checked_against_the_listing() {
    // A retired favourite is refused while the listing says so, and allowed offline; an
    // outside id needs the listing to vouch for it. Two seats from one lab need a yes,
    // which an unattended run never gives.
    let cat = catalog();
    let listing = listing();
    let checks: [(&str, bool, &[&str], bool); 14] = [
        ("favourites", true, &["alpha", "gamma"], true),
        ("outside_id", true, &["alpha", "qwen/q-one"], true),
        ("retired_online", true, &["retired"], false),
        ("retired_offline", false, &["retired"], true),
        ("outside_offline", false, &["qwen/q-one"], false),
        ("unlisted_id", true, &["qwen/nope"], false),
        ("not_an_id", true, &["nope"], false),
        ("anthropic", true, &["anthropic/claude-x"], false),
        ("tilde_anthropic", true, &["~anthropic/claude-x"], false),
        ("batch", true, &["alpha", "openai/gamma-3:batch"], false),
        ("batch_offline", false, &["openai/gamma-3:batch"], false),
        ("router", true, &["alpha", "openrouter/auto"], false),
        ("same_lab", true, &["beta", "z-ai/glm-other"], false),
        ("twice", true, &["alpha", "alpha"], false),
    ];
    for (name, online, entries, accepted) in checks {
        let entries: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        let verdict = check_panel_choice(&cat, &entries, online.then_some(&listing));
        assert_eq!(verdict == PanelChoice::Ok, accepted, "{name}: {verdict:?}");
    }
}

#[test]
fn verdicts_say_why() {
    let cat = catalog();
    let listing = listing();
    let check = |e: &[&str], online: bool| {
        let e: Vec<String> = e.iter().map(|s| s.to_string()).collect();
        check_panel_choice(&cat, &e, online.then_some(&listing))
    };
    assert_eq!(
        check(&["beta", "z-ai/glm-other"], true),
        PanelChoice::SameLab {
            labs: vec!["z-ai".into()],
            message: "Two panel seats from one lab (z-ai): their agreement says less than two labs agreeing.".into(),
        }
    );
    assert_eq!(
        check(&["alpha", "alpha"], true),
        PanelChoice::Refused("A model is listed twice.".into())
    );
    assert_eq!(
        check(&["anthropic/claude-x"], true),
        PanelChoice::Refused("'anthropic/claude-x': consult asks models other than Claude.".into())
    );
    assert_eq!(
        check(&["nope"], true),
        PanelChoice::Refused(
            "'nope' is neither a favourite nor an OpenRouter id (provider/model).".into()
        )
    );
    assert_eq!(
        check(&["qwen/q-one"], false),
        PanelChoice::Refused("'qwen/q-one' cannot be checked while OpenRouter's listing is unreachable. Pick favourites, or retry later.".into())
    );
    assert_eq!(
        check(&["retired"], true),
        PanelChoice::Refused(
            "'retired' (x-ai/retired-5) is not offered on OpenRouter with tool calling.".into()
        )
    );
    assert_eq!(
        check(&["openai/gamma-3:batch"], false),
        PanelChoice::Refused("'openai/gamma-3:batch' is for OpenRouter's batch processing; a review needs a live model.".into())
    );
}
