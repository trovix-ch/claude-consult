//! Ported from tests/test_picker.py: the picker's rows, state and lines, driven with
//! fake keys, a fixture catalog and a fixture listing. Nothing touches the network or
//! the console.

use super::*;
use consult_core::catalog::load_catalog;
use consult_core::listing::reviewable_models;
use ratatui::style::Color;
use serde_json::json;

fn fav(id: &str, display: &str, lab: &str, tier: &str) -> Value {
    let command = display
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    json!({"id": id, "display": display, "lab": lab, "tier": tier, "command": command,
           "tagline": format!("{display} tagline"), "plays_to": "", "pitch": "", "note": ""})
}

fn catalog() -> Catalog {
    let doc = json!({
        "default_panel": ["alpha", "beta", "gamma"],
        "budget_panel": ["delta", "beta"],
        "models": {
            "alpha": fav("deepseek/alpha-1", "Alpha One", "DeepSeek", "panel"),
            "beta": fav("z-ai/beta-2", "Beta Two", "Z.ai", "panel"),
            "gamma": fav("openai/gamma-3", "Gamma Three", "OpenAI", "panel"),
            "delta": fav("minimax/delta-4", "Delta Four", "MiniMax", "budget"),
            "retired": fav("x-ai/retired-5", "Retired Five", "xAI", "alternate"),
        }
    });
    load_catalog(&doc.to_string()).expect("fixture catalog")
}

fn live(id: &str, name: &str, prompt: &str, completion: &str, context: u64, tools: bool) -> Value {
    let mut params = vec!["max_tokens", "temperature"];
    if tools {
        params.extend(["tool_choice", "tools"]);
    }
    json!({"id": id, "name": name, "context_length": context,
           "pricing": {"prompt": prompt, "completion": completion},
           "supported_parameters": params})
}

fn l(id: &str, name: &str) -> Value {
    live(id, name, "0.000001", "0.000002", 131_072, true)
}

/// Listing order stands for popularity. retired-5 is missing: OpenRouter dropped it.
fn listing() -> Value {
    json!({"data": [
        l("z-ai/glm-other", "Z.ai: GLM Other"),
        l("anthropic/claude-x", "Anthropic: Claude X"),
        live("deepseek/alpha-1", "DeepSeek: Alpha One", "0.0000005", "0.0000011", 1_048_576, true),
        live("mistralai/no-tools", "Mistral: No Tools", "0.000001", "0.000002", 131_072, false),
        l("openai/gamma-3", "OpenAI: Gamma Three"),
        l("openai/gamma-3:batch", "OpenAI: Gamma Three (batch)"),
        l("qwen/q-one", "Qwen: Q One"),
        l("z-ai/beta-2", "Z.ai: Beta Two"),
        l("minimax/delta-4", "MiniMax: Delta Four"),
        l("qwen/q-two", "Qwen: Q Two"),
        live("nvidia/n-three:free", "NVIDIA: N Three (free)", "0", "0", 131_072, true),
        // A router: it lists a price of -1 and picks the model per request.
        live("openrouter/auto", "Auto Router", "-1", "-1", 131_072, true),
    ]})
}

fn models() -> IndexMap<String, Value> {
    reviewable_models(&listing())
}

/// Every entry of the listing, unfiltered.
fn raw_models() -> IndexMap<String, Value> {
    listing()["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|m| (m["id"].as_str().expect("id").to_string(), m.clone()))
        .collect()
}

const ONLINE_ROWS: [&str; 8] = [
    "alpha",
    "beta",
    "gamma",
    "delta",
    "z-ai/glm-other",
    "qwen/q-one",
    "qwen/q-two",
    "nvidia/n-three:free",
];

fn recommended() -> Vec<String> {
    vec!["alpha".into(), "beta".into(), "gamma".into()]
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn keys_of(rows: &[&PickerRow]) -> Vec<String> {
    rows.iter().map(|r| r.key.clone()).collect()
}

fn chars(s: &str) -> Vec<Key> {
    s.chars().map(Key::Char).collect()
}

/// Runs a scenario: (online, viewport height, keys).
fn run(online: bool, height: usize, keys: Vec<Key>) -> Picker {
    let cat = catalog();
    let live = models();
    let rows = rows_from(&cat, online.then_some(&live));
    let mut p = Picker::new(rows, &cat.default_panel, &cat.budget_panel, height, !online);
    for k in keys {
        p.step(k);
    }
    p
}

fn footer(p: &Picker) -> Vec<String> {
    let lines = p.lines(200);
    lines[lines.len() - 3..].to_vec()
}

fn down4() -> Vec<Key> {
    vec![Key::Down; 4]
}

#[test]
fn favourites_first_then_listing_order() {
    let p = run(true, 20, vec![]);
    let keys: Vec<&str> = p.rows().iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ONLINE_ROWS);
    let favs: Vec<bool> = p.rows().iter().map(|r| r.fav).collect();
    assert_eq!(favs, [true, true, true, true, false, false, false, false]);
    let (alpha, other) = (&p.rows()[0], &p.rows()[4]);
    assert!((alpha.price_in.expect("priced") - 0.5).abs() < 1e-9);
    assert!((alpha.price_out.expect("priced") - 1.1).abs() < 1e-9);
    assert_eq!(alpha.context, Some(1_048_576));
    assert_eq!(other.lab, "z-ai");
    assert_eq!(other.text, "Z.ai: GLM Other");
    assert_eq!(alpha.vendor, "deepseek");
}

#[test]
fn recommended_is_preselected_and_marked() {
    let p = run(true, 20, vec![]);
    assert_eq!(p.selected(), recommended());
    let lines = p.lines(200);
    let rows = &lines[2..2 + ONLINE_ROWS.len()];
    assert!(rows[0].starts_with(">[x]* alpha"));
    assert!(rows[2].starts_with(" [x]* gamma"));
    assert!(rows[3].starts_with(" [ ]  delta"));
    assert!(rows[0].contains("0.50"));
    assert!(rows[0].contains("1024K"));
    assert!(rows[0].contains("Alpha One tagline"));
    assert!(footer(&p)[0].contains("3 selected: alpha, beta, gamma"));
    assert_eq!(footer(&p)[1], "");
}

#[test]
fn anthropic_toolless_batch_and_routers_are_never_offered() {
    let listed = models();
    for id in [
        "anthropic/claude-x",
        "mistralai/no-tools",
        "openai/gamma-3:batch",
        "openrouter/auto",
    ] {
        assert!(!listed.contains_key(id), "{id}");
    }
    assert!(listed.contains_key("openai/gamma-3"));
    // Even a listing that was not filtered on the way in.
    let raw = raw_models();
    let raw_rows: Vec<String> = rows_from(&catalog(), Some(&raw))
        .into_iter()
        .map(|r| r.key)
        .collect();
    for id in [
        "anthropic/claude-x",
        "openai/gamma-3:batch",
        "openrouter/auto",
    ] {
        assert!(!raw_rows.contains(&id.to_string()), "{id}");
    }
    let mut tilde = raw.clone();
    tilde.insert("~anthropic/claude-y".into(), l("~anthropic/claude-y", "x"));
    assert!(
        !rows_from(&catalog(), Some(&tilde))
            .iter()
            .any(|r| r.key.contains("anthropic"))
    );
}

#[test]
fn a_negative_price_is_unknown_and_zero_is_free() {
    assert_eq!(format_price(per_million(Some(&json!("-1")))), "?");
    assert_eq!(format_price(per_million(Some(&json!("0")))), "0.00");
    assert_eq!(format_price(Some(0.55071)), "0.551");
    assert_eq!(format_price(Some(15.0)), "15.00");
    assert_eq!(format_price(Some(1.1)), "1.10");
    assert_eq!(format_price(None), "?");
    assert_eq!(format_context(Some(131_072)), "128K");
    assert_eq!(format_context(Some(1_050_000)), "1025K");
    // Half to even, as .NET's Math.Round.
    assert_eq!(format_context(Some(512 + 1024 * 2)), "2K");
    assert_eq!(format_context(Some(0)), "?");
    assert_eq!(format_context(None), "?");
}

#[test]
fn offline_shows_favourites_only_without_prices() {
    let p = run(false, 20, vec![]);
    let keys: Vec<&str> = p.rows().iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["alpha", "beta", "gamma", "delta", "retired"]);
    assert!(
        p.rows()
            .iter()
            .all(|r| r.price_in.is_none() && r.context.is_none())
    );
    let lines = p.lines(200);
    assert!(lines[0].contains("OFFLINE"));
    assert!(
        lines[2].contains("       ?       ?       ?  "),
        "{}",
        lines[2]
    );
    assert_eq!(p.selected(), recommended());
}

#[test]
fn typing_filters_case_insensitively() {
    let p = run(true, 20, chars("GLM"));
    assert_eq!(keys_of(&p.filtered()), ["z-ai/glm-other"]);
    assert!(p.lines(200)[0].contains("Filter: GLM_"));
    // By lab and by live name: beta's lab, glm-other's name.
    let p = run(true, 20, chars("z.Ai"));
    assert_eq!(keys_of(&p.filtered()), ["beta", "z-ai/glm-other"]);
    let mut keys = chars("GLM");
    keys.extend([Key::Backspace, Key::Backspace]);
    let p = run(true, 20, keys);
    assert_eq!(p.filter(), "G");
    assert_eq!(keys_of(&p.filtered()), ["gamma", "z-ai/glm-other"]);
    let mut keys = chars("GLM");
    keys.push(Key::Esc);
    let p = run(true, 20, keys);
    assert_eq!(keys_of(&p.filtered()), ONLINE_ROWS);
    let mut keys = chars("glm");
    keys.push(Key::Space);
    let p = run(true, 20, keys);
    let mut want = recommended();
    want.push("z-ai/glm-other".into());
    assert_eq!(p.selected(), want);
}

#[test]
fn space_toggles_the_row_under_the_cursor() {
    assert_eq!(
        run(true, 20, vec![Key::Space]).selected(),
        strings(&["beta", "gamma"])
    );
    assert_eq!(
        run(true, 20, vec![Key::Space, Key::Space]).selected(),
        strings(&["beta", "gamma", "alpha"])
    );
}

#[test]
fn same_lab_warning() {
    let mut keys = down4();
    keys.push(Key::Space);
    let p = run(true, 20, keys);
    let mut want = recommended();
    want.push("z-ai/glm-other".into());
    assert_eq!(p.selected(), want);
    assert!(footer(&p)[1].contains("one lab (Z.ai)"), "{:?}", footer(&p));
    assert_eq!(p.same_lab(), ["Z.ai"]);
}

#[test]
fn presets() {
    assert_eq!(
        run(true, 20, vec![Key::CtrlB]).selected(),
        strings(&["delta", "beta"])
    );
    assert_eq!(
        run(true, 20, vec![Key::CtrlB, Key::CtrlR]).selected(),
        recommended()
    );
}

#[test]
fn viewport_scrolls_with_the_cursor() {
    let mut page_up = down4();
    page_up.push(Key::PageUp);
    let cases: Vec<(&str, Vec<Key>, usize, usize)> = vec![
        ("scroll_down", down4(), 4, 2),
        ("scroll_page_up", page_up, 1, 1),
        ("scroll_page_down", vec![Key::PageDown], 3, 1),
        ("scroll_end", vec![Key::End, Key::PageDown, Key::Down], 7, 5),
        ("scroll_home", vec![Key::End, Key::Home, Key::Up], 0, 0),
    ];
    for (name, keys, cursor, top) in cases {
        let p = run(true, 3, keys);
        assert_eq!((p.cursor(), p.top()), (cursor, top), "{name}");
        let lines = p.lines(200);
        assert_eq!(lines.len(), 3 + 5, "{name}");
        let shown = &lines[2..5];
        let marks: Vec<bool> = shown.iter().map(|l| l.starts_with('>')).collect();
        let want: Vec<bool> = (top..top + 3).map(|i| i == cursor).collect();
        assert_eq!(marks, want, "{name}");
        assert!(shown[0].contains(ONLINE_ROWS[top]), "{name}");
    }
}

#[test]
fn enter_needs_a_selection() {
    let p = run(
        true,
        20,
        vec![
            Key::Space,
            Key::Down,
            Key::Space,
            Key::Down,
            Key::Space,
            Key::Enter,
        ],
    );
    assert!(p.selected().is_empty());
    assert!(!p.done());
    assert_eq!(p.note(), NOTHING_SELECTED);
    assert!(footer(&p)[1].contains(p.note()));
    assert!(run(true, 20, vec![Key::Enter]).done());
    assert!(run(true, 20, vec![Key::CtrlC]).aborted());
    // The note lasts one key.
    let mut p = p;
    p.step(Key::Down);
    assert_eq!(p.note(), "");
}

#[test]
fn keys_on_an_empty_picker_are_harmless() {
    let mut p = Picker::new(Vec::new(), &[], &[], 3, false);
    for k in [
        Key::Up,
        Key::End,
        Key::PageDown,
        Key::Space,
        Key::Enter,
        Key::Char('x'),
    ] {
        p.step(k);
    }
    assert_eq!((p.cursor(), p.top()), (0, 0));
    assert!(!p.done() && !p.aborted());
    let mut p = Picker::new(Vec::new(), &[], &[], 3, false);
    p.step(Key::CtrlC);
    assert!(p.aborted());
}

#[test]
fn the_filter_never_holds_a_space() {
    let p = run(
        true,
        20,
        vec![Key::Char('q'), Key::Char(' '), Key::Char('o')],
    );
    assert_eq!(p.filter(), "qo");
}

#[test]
fn lines_snapshot() {
    let p = run(true, 20, vec![]);
    let lines = p.lines(200);
    assert_eq!(lines.len(), 20 + 5);
    assert_eq!(lines[0], "  Filter: _   8 of 8 models");
    assert_eq!(
        lines[1],
        format!(
            "      model (* recommended){}lab{}USD/M in     out context  ",
            " ".repeat(16),
            " ".repeat(9)
        )
    );
    assert_eq!(
        lines[2],
        format!(
            ">[x]* alpha{}DeepSeek{}    0.50    1.10   1024K  Alpha One tagline",
            " ".repeat(32),
            " ".repeat(4)
        )
    );
    assert_eq!(
        lines[6],
        format!(
            " [ ]  z-ai/glm-other{}z-ai{}    1.00    2.00    128K  Z.ai: GLM Other",
            " ".repeat(23),
            " ".repeat(8)
        )
    );
    assert_eq!(
        lines[9],
        format!(
            " [ ]  nvidia/n-three:free{}nvidia{}    0.00    0.00    128K  NVIDIA: N Three (free)",
            " ".repeat(18),
            " ".repeat(6)
        )
    );
    assert_eq!(lines[10], "");
    assert_eq!(lines[22], "  3 selected: alpha, beta, gamma");
    assert_eq!(lines[23], "");
    assert_eq!(lines[24], LEGEND);
}

#[test]
fn long_keys_and_labs_are_clipped_and_lines_fit_the_width() {
    let long_id = format!("acme/{}", "x".repeat(40));
    let mut m = IndexMap::new();
    m.insert(
        long_id.clone(),
        json!({"id": long_id, "name": "Acme: Long", "context_length": 1000,
               "pricing": {"prompt": "0", "completion": "0"}}),
    );
    let cat = catalog();
    let mut rows = rows_from(&cat, Some(&m));
    rows.iter_mut()
        .for_each(|r| r.lab = "a-very-long-lab-name".into());
    let p = Picker::new(rows, &[], &[], 3, false);
    let line = &p.lines(200)[2];
    let want_key = format!("acme/{}~", "x".repeat(30));
    assert!(
        line.starts_with(&format!(">[ ]  {want_key} a-very-lon~ ")),
        "{line}"
    );
    for l in p.lines(30) {
        assert!(l.chars().count() <= 30, "{l}");
    }
    assert_eq!(clip("abcdef", 4), "abc~");
    assert_eq!(clip("abc", 3), "abc");
    assert_eq!(clip("abc", 0), "");
}

#[test]
fn with_selected_ticks_the_known_keys_only() {
    let p = run(true, 5, vec![]).with_selected(&[
        "qwen/q-one".to_string(),
        "nope".into(),
        "beta".into(),
    ]);
    assert_eq!(p.selected(), ["qwen/q-one", "beta"]);
    // The recommended panel stays what Ctrl+R restores and what the stars mark.
    assert_eq!(p.recommended(), ["alpha", "beta", "gamma"]);
    let p = run(true, 5, vec![]).with_selected(&["nope".to_string()]);
    assert!(p.selected().is_empty());
}

#[test]
fn height_follows_the_terminal() {
    assert_eq!(Picker::height_for(8, 50), 8);
    assert_eq!(Picker::height_for(100, 30), 22);
    assert_eq!(Picker::height_for(100, 5), 3);
    let mut p = run(true, 3, down4());
    p.set_height(2);
    assert_eq!((p.cursor(), p.top()), (4, 3));
}

#[test]
fn render_paints_the_lines_with_the_cursor_row_highlighted() {
    let p = run(true, 5, vec![Key::Down]);
    let area = Rect::new(0, 0, 100, 10);
    let mut buf = Buffer::empty(area);
    render(&p, area, &mut buf);
    let row = |y: u16| -> String { (0..area.width).map(|x| buf[(x, y)].symbol()).collect() };
    assert!(row(0).starts_with("  Filter: _   8 of 8 models"));
    assert!(row(3).starts_with(">[x]* beta"));
    assert_eq!(buf[(0, 3)].fg, Color::Cyan);
    assert_ne!(buf[(0, 2)].fg, Color::Cyan);
    assert_eq!(buf[(2, 1)].fg, Color::DarkGray);
}
