//! OpenRouter's public listing of tool-capable models: live prices and context sizes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;

use crate::catalog::{priced_per_request, refusal};
use crate::openrouter::{Client, FetchError};
use crate::util::{monotonic, negative, number, positive_int, round_to, strip_bom};

/// OpenRouter's public model listing, filtered to the models that can call tools, which
/// grounded review needs. It needs no key. Served with max-age=120, so asking more
/// often only gets the same answer back.
pub const LISTING_PATH: &str = "/models?supported_parameters=tools";
/// The installer's variant: most-popular order puts the models people use near the top.
pub const INSTALLER_LISTING_PATH: &str = "/models?supported_parameters=tools&sort=most-popular";
/// The server gives up on the listing after this, in total: someone is waiting on the
/// answer, and install-time values will do.
pub const LISTING_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a successful listing is reused.
pub const LISTING_TTL: Duration = Duration::from_secs(120);

/// One model's live values, USD per million tokens.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveModel {
    /// Context window in tokens.
    pub context: Option<u64>,
    /// Input price per million tokens.
    pub price_in: Option<f64>,
    /// Output price per million tokens.
    pub price_out: Option<f64>,
}

/// The server's view of the listing: live values by model id, or why there are none.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveListing {
    /// The listing answered.
    Live {
        /// ISO UTC time of the fetch, `YYYY-MM-DDTHH:MM:SSZ`.
        at: String,
        /// Live values by model id.
        models: HashMap<String, LiveModel>,
    },
    /// It did not, and this is why.
    Unavailable {
        /// E.g. `HTTP 503`, `no answer within 10s`.
        error: String,
    },
}

impl LiveListing {
    /// The models, when the listing answered.
    pub fn models(&self) -> Option<&HashMap<String, LiveModel>> {
        match self {
            Self::Live { models, .. } => Some(models),
            Self::Unavailable { .. } => None,
        }
    }
}

/// Why a body is not a usable listing.
pub const UNEXPECTED_SHAPE: &str = "ValueError: unexpected listing shape";

/// Live values by id from a listing body. Routers (priced at -1) are left out, and so
/// they are refused by name like anything else the listing does not carry.
pub fn parse_listing(body: &Value) -> Result<HashMap<String, LiveModel>, String> {
    let data = body.get("data").and_then(Value::as_array);
    // An empty listing would report every registered model as withdrawn.
    let data = match data {
        Some(d) if !d.is_empty() => d,
        _ => return Err(UNEXPECTED_SHAPE.to_string()),
    };
    let mut out = HashMap::new();
    for m in data {
        let Some(id) = m.get("id").and_then(Value::as_str) else {
            continue;
        };
        let pricing = m.get("pricing").filter(|p| p.is_object());
        let field = |k: &str| pricing.and_then(|p| p.get(k));
        // A router is listed at -1: its price is that of whichever model it
        // picks per request, and it can pick Claude. Left out, it is refused
        // by name like anything else OpenRouter does not list.
        if negative(field("prompt")) || negative(field("completion")) {
            continue;
        }
        // Priced per token, as strings; the table speaks per million.
        let per_m = |k: &str| number(field(k)).map(|x| round_to(x * 1e6, 6));
        out.insert(
            id.to_string(),
            LiveModel {
                context: positive_int(m.get("context_length")),
                price_in: per_m("prompt"),
                price_out: per_m("completion"),
            },
        );
    }
    Ok(out)
}

/// Fetches a listing body: the server's tool-capable listing, or with `installer` the
/// installer's most-popular-first one. No key, bounded by `timeout` as a whole.
pub async fn fetch_listing(
    client: &Client,
    timeout: Duration,
    installer: bool,
) -> Result<String, FetchError> {
    let path = if installer {
        INSTALLER_LISTING_PATH
    } else {
        LISTING_PATH
    };
    client.get_public(path, timeout).await
}

/// The listing, cached for [`LISTING_TTL`] after a success only.
#[derive(Debug)]
pub struct ListingCache {
    ttl: Duration,
    timeout: Duration,
    cached: Mutex<Option<(f64, Arc<LiveListing>)>>,
}

impl Default for ListingCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ListingCache {
    /// A cache with the server's TTL and timeout.
    pub fn new() -> Self {
        Self::with_limits(LISTING_TTL, LISTING_TIMEOUT)
    }

    /// A cache with its own TTL and fetch bound.
    pub fn with_limits(ttl: Duration, timeout: Duration) -> Self {
        Self {
            ttl,
            timeout,
            cached: Mutex::new(None),
        }
    }

    /// Current price and context per model id, or why there are none. Never fails.
    pub async fn get(&self, client: &Client) -> Arc<LiveListing> {
        if let Some((at, live)) = self.lock().as_ref()
            && monotonic() - at < self.ttl.as_secs_f64()
        {
            return Arc::clone(live);
        }
        let fetched = match fetch_listing(client, self.timeout, false).await {
            Err(e) => Err(e.to_string()),
            Ok(text) => match serde_json::from_str::<Value>(strip_bom(&text)) {
                Err(e) => Err(format!("JSONDecodeError: {e}")),
                Ok(body) => parse_listing(&body),
            },
        };
        let models = match fetched {
            Ok(models) => models,
            Err(error) => {
                return Arc::new(LiveListing::Unavailable {
                    error: crate::util::clip_chars(&error, 200).to_string(),
                });
            }
        };
        // Only a success is cached: after a failure the next call may find the
        // network back, and should not be told old news for two minutes.
        let live = Arc::new(LiveListing::Live {
            at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            models,
        });
        *self.lock() = Some((monotonic(), Arc::clone(&live)));
        live
    }

    /// Forgets the cached listing.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(f64, Arc<LiveListing>)>> {
        self.cached.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The process-wide cache the server uses.
pub fn global_cache() -> &'static ListingCache {
    static CACHE: OnceLock<ListingCache> = OnceLock::new();
    CACHE.get_or_init(ListingCache::new)
}

/// [`ListingCache::get`] on the process-wide cache.
pub async fn live_listing(client: &Client) -> Arc<LiveListing> {
    global_cache().get(client).await
}

/// The listing's models that could be reviewers, by id in listing order: tool-capable,
/// not refused by name ([`refusal`]) and not priced per request. Entries are kept whole.
pub fn reviewable_models(body: &Value) -> IndexMap<String, Value> {
    let mut live = IndexMap::new();
    for m in body
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = m.get("id").and_then(Value::as_str) else {
            continue;
        };
        if refusal(id).is_some() || priced_per_request(m) {
            continue;
        }
        let tools = m
            .get("supported_parameters")
            .and_then(Value::as_array)
            .is_some_and(|p| p.iter().any(|x| x.as_str() == Some("tools")));
        if tools {
            live.insert(id.to_string(), m.clone());
        }
    }
    live
}

/// [`reviewable_models`] from a raw body; an unreadable body is an empty listing.
pub fn reviewable_models_text(body: &str) -> IndexMap<String, Value> {
    serde_json::from_str::<Value>(strip_bom(body))
        .map(|v| reviewable_models(&v))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listing(rows: &[(&str, Value, Value, Value)]) -> Value {
        let data: Vec<Value> = rows
            .iter()
            .map(|(id, p, c, ctx)| {
                json!({"id": id, "name": id, "context_length": ctx,
                       "pricing": {"prompt": p, "completion": c}})
            })
            .collect();
        json!({"data": data, "total_count": rows.len()})
    }

    #[test]
    fn prices_are_per_million_tokens() {
        let body = listing(&[(
            "lab-a/alpha",
            json!("0.00000055071"),
            json!("0.00000110142"),
            json!(1_048_576),
        )]);
        let models = parse_listing(&body).expect("parsed");
        assert_eq!(
            models["lab-a/alpha"],
            LiveModel {
                context: Some(1_048_576),
                price_in: Some(0.55071),
                price_out: Some(1.10142)
            }
        );
    }

    #[test]
    fn unusable_values_are_unknown_and_routers_are_left_out() {
        let body = listing(&[
            ("a/a", json!(""), json!("abc"), Value::Null),
            ("b/b", Value::Null, json!("0.000000001"), json!("big")),
            ("r/r", json!("-1"), json!("-1"), json!(2_000_000)),
            ("h/h", json!("0.000001"), json!("-1"), json!(100_000)),
        ]);
        let models = parse_listing(&body).expect("parsed");
        assert_eq!(
            models["a/a"],
            LiveModel {
                context: None,
                price_in: None,
                price_out: None
            }
        );
        assert_eq!(models["b/b"].price_out, Some(0.001));
        assert!(!models.contains_key("r/r"));
        assert!(!models.contains_key("h/h"));
    }

    #[test]
    fn unexpected_bodies() {
        for body in [
            json!({"data": "nope"}),
            json!({"data": []}),
            json!([]),
            json!("text"),
            json!({"error": {"code": 500}}),
        ] {
            assert_eq!(
                parse_listing(&body),
                Err(UNEXPECTED_SHAPE.to_string()),
                "{body}"
            );
        }
    }

    #[test]
    fn reviewable_filters() {
        let body = json!({"data": [
            {"id": "z-ai/glm", "supported_parameters": ["tools"], "pricing": {"prompt": "0", "completion": "0"}},
            {"id": "anthropic/claude-x", "supported_parameters": ["tools"]},
            {"id": "~anthropic/claude-x", "supported_parameters": ["tools"]},
            {"id": "mistralai/no-tools", "supported_parameters": ["max_tokens"]},
            {"id": "openai/gamma:batch", "supported_parameters": ["tools"]},
            {"id": "openrouter/auto", "supported_parameters": ["tools"], "pricing": {"prompt": "-1", "completion": "-1"}},
            {"id": "typesafe/router", "supported_parameters": ["tools"], "pricing": {"prompt": "-1", "completion": "-1"}},
            {"id": "qwen/q", "supported_parameters": ["tools"]},
            "not a model", {"name": "no id"}
        ]});
        let models = reviewable_models(&body);
        let got: Vec<&String> = models.keys().collect();
        assert_eq!(got, ["z-ai/glm", "qwen/q"]);
        assert!(reviewable_models_text("<html>").is_empty());
    }
}
