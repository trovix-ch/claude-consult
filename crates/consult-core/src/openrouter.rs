//! The HTTP client for OpenRouter: chat completions with retries, and the public GETs.

use std::time::Duration;

use serde_json::Value;

use crate::error::ConsultError;
use crate::util::{clip_chars, py_dumps};

/// OpenRouter's API root.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// TEST ONLY: replaces [`DEFAULT_BASE_URL`] in [`Client::new`], so the end-to-end tests
/// can run the real binary against a mock or a closed port instead of OpenRouter. Not a
/// user setting: the key is sent to whatever root this names.
pub const BASE_URL_ENV: &str = "CLAUDE_CONSULT_OPENROUTER_BASE_URL";
/// Per-read timeout on a chat request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Attempts per chat request before giving up.
pub const ATTEMPTS: u32 = 4;
/// Sent as `HTTP-Referer`, which OpenRouter shows as the app.
pub const REFERER: &str = "https://github.com/trovix-ch/claude-consult";
/// Sent as `X-Title`.
pub const TITLE: &str = "claude-consult";

/// Statuses worth retrying: rate limits and transient upstream failures.
const RETRY_STATUSES: [u16; 6] = [408, 429, 500, 502, 503, 504];

/// A GET that did not produce a body.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// No answer within the bound.
    #[error("no answer within {}s", .0.as_secs_f64())]
    Timeout(Duration),
    /// A non-success status.
    #[error("HTTP {0}")]
    Status(u16),
    /// Connection or protocol failure, named as Python's httpx would name it.
    #[error("{0}")]
    Transport(String),
}

/// An OpenRouter client whose base URL can be pointed anywhere, so tests use a mock.
#[derive(Clone, Debug)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    backoff_unit: Duration,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// A client for the real OpenRouter, or for [`BASE_URL_ENV`]'s root when that is set.
    pub fn new() -> Self {
        match std::env::var(BASE_URL_ENV) {
            Ok(url) if !url.trim().is_empty() => Self::with_base_url(url.trim()),
            _ => Self::with_base_url(DEFAULT_BASE_URL),
        }
    }

    /// A client for another API root, e.g. a mock server's `.../api/v1`.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        // reqwest is built without a default crypto provider (see Cargo.toml), so
        // one has to be installed before the first TLS handshake. Installing
        // twice is refused, harmlessly.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .read_timeout(REQUEST_TIMEOUT)
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            backoff_unit: Duration::from_secs(1),
        }
    }

    /// The same client with every retry sleep scaled to `unit` (one second by default).
    /// Tests pass `Duration::ZERO`.
    pub fn with_backoff_unit(mut self, unit: Duration) -> Self {
        self.backoff_unit = unit;
        self
    }

    /// The API root this client talks to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `base_url` + `path`.
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// The underlying reqwest client.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// POST to `/chat/completions` with backoff on rate limits and transient upstream
    /// failures: four attempts, sleeping 2^n units after a transport error and 1.5*2^n
    /// after 408/429/5xx. A 200 body carrying `error` and no `choices` is an error.
    pub async fn post_chat(&self, key: &str, payload: &Value) -> Result<Value, ConsultError> {
        let url = self.url("/chat/completions");
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            let sent = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {key}"))
                .header("Content-Type", "application/json")
                .header("HTTP-Referer", REFERER)
                .header("X-Title", TITLE)
                .timeout(REQUEST_TIMEOUT)
                .json(payload)
                .send()
                .await;
            let response = match sent {
                Ok(r) => r,
                Err(e) => {
                    last = transport_name(&e);
                    self.sleep(2f64.powi(attempt as i32)).await;
                    continue;
                }
            };
            let status = response.status().as_u16();
            let text = match response.text().await {
                Ok(t) => t,
                Err(e) => {
                    last = transport_name(&e);
                    self.sleep(2f64.powi(attempt as i32)).await;
                    continue;
                }
            };
            if status == 200 {
                let data: Value = serde_json::from_str(&text).map_err(|e| ConsultError::Other {
                    kind: "JSONDecodeError",
                    message: e.to_string(),
                })?;
                let no_choices = data
                    .get("choices")
                    .is_none_or(|c| c.is_null() || c.as_array().is_some_and(Vec::is_empty));
                if let Some(error) = data.get("error")
                    && no_choices
                {
                    return Err(ConsultError::msg(format!(
                        "OpenRouter error: {}",
                        clip_chars(&py_dumps(error, false), 400)
                    )));
                }
                return Ok(data);
            }
            if RETRY_STATUSES.contains(&status) {
                last = format!("HTTP {status}: {}", clip_chars(&text, 300));
                self.sleep(2f64.powi(attempt as i32) * 1.5).await;
                continue;
            }
            return Err(ConsultError::msg(format!(
                "HTTP {status}: {}",
                clip_chars(&text, 500)
            )));
        }
        Err(ConsultError::msg(format!("gave up after retries — {last}")))
    }

    /// A GET on the public API that carries no key, bounded as a whole by `timeout`.
    pub async fn get_public(&self, path: &str, timeout: Duration) -> Result<String, FetchError> {
        // Deliberately without the key: the listing is public, and a request
        // that needs no key should never carry one.
        let fetch = async {
            let r = self
                .http
                .get(self.url(path))
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| fetch_error(&e, timeout))?;
            let status = r.status();
            if !status.is_success() {
                return Err(FetchError::Status(status.as_u16()));
            }
            r.text().await.map_err(|e| fetch_error(&e, timeout))
        };
        // A bound on the whole fetch, not only a per-read one: a body that trickles
        // in would otherwise keep the caller waiting.
        match tokio::time::timeout(timeout, fetch).await {
            Ok(result) => result,
            Err(_) => Err(FetchError::Timeout(timeout)),
        }
    }

    async fn sleep(&self, units: f64) {
        let d = self.backoff_unit.mul_f64(units);
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
    }
}

fn fetch_error(e: &reqwest::Error, timeout: Duration) -> FetchError {
    if e.is_timeout() {
        FetchError::Timeout(timeout)
    } else {
        FetchError::Transport(transport_name(e))
    }
}

/// A transport error as `Name: message`, the way the Python server reported httpx's.
pub fn transport_name(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "TimeoutException"
    } else if e.is_connect() {
        "ConnectError"
    } else if e.is_decode() {
        "DecodingError"
    } else {
        "TransportError"
    };
    format!("{kind}: {e}")
}
