//! The OpenRouter API key: where it is read from, how it is shown, how it is checked.
//!
//! The key never goes on a command line, into a log or into output unmasked.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::error::ConsultError;
use crate::openrouter::Client;
use crate::paths::settings_path;

/// The environment variable, and the key in settings.json's `env` block.
pub const KEY_VAR: &str = "OPENROUTER_API_KEY";

/// Trims what an editor or a pipe can leave around a pasted key: a BOM and whitespace.
pub fn clean_key(raw: &str) -> &str {
    raw.trim_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
}

/// The key in the settings.json `env` block of this Claude dir, if any.
pub fn settings_key(claude_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(settings_path(claude_dir)).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let data: Value = serde_json::from_str(crate::util::strip_bom(&text)).ok()?;
    let key = clean_key(data.get("env")?.get(KEY_VAR)?.as_str()?);
    (!key.is_empty()).then(|| key.to_string())
}

/// The key: the environment first, then Claude Code's settings.
pub fn load_api_key(claude_dir: &Path) -> Result<String, ConsultError> {
    load_api_key_from(std::env::var(KEY_VAR).ok().as_deref(), claude_dir)
}

/// [`load_api_key`] with the environment's value given.
pub fn load_api_key_from(
    env_value: Option<&str>,
    claude_dir: &Path,
) -> Result<String, ConsultError> {
    if let Some(key) = env_value.map(clean_key).filter(|k| !k.is_empty()) {
        return Ok(key.to_string());
    }
    settings_key(claude_dir).ok_or_else(|| {
        ConsultError::msg(format!(
            "No OpenRouter API key. Set {KEY_VAR}, or put it in the 'env' block of {}.",
            settings_path(claude_dir).display()
        ))
    })
}

/// The key as it may be shown: its first 9 and last 4 characters, or only stars for a
/// short one.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 16 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..9].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

/// Whether it has the shape of an OpenRouter key.
pub fn looks_like_key(key: &str) -> bool {
    key.starts_with("sk-or-")
}

/// Refuses an empty key and one with spaces or anything outside printable ASCII, which
/// would fail every request with a 401 while the service looked healthy.
pub fn validate_key_format(key: &str) -> Result<(), &'static str> {
    if key.is_empty() {
        return Err("no key was given");
    }
    if !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(
            "the key contains spaces or non-ASCII characters; paste it again exactly as OpenRouter shows it",
        );
    }
    Ok(())
}

/// What OpenRouter said about a key.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyCheck {
    /// Accepted.
    Valid {
        /// Spent so far, USD.
        usage: Option<f64>,
        /// Credit limit, USD; `None` when none is set.
        limit: Option<f64>,
        /// The whole `data` object.
        data: Value,
    },
    /// Refused with 401 or 403.
    Rejected {
        /// The status.
        status: u16,
    },
    /// No verdict: the check itself failed.
    Unreachable {
        /// Why.
        reason: String,
    },
}

impl KeyCheck {
    /// Why a key was not accepted, as a sentence tail: "was rejected by OpenRouter (HTTP 401)".
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Valid { .. } => None,
            Self::Rejected { status } => {
                Some(format!("was rejected by OpenRouter (HTTP {status})"))
            }
            Self::Unreachable { reason } => Some(reason.clone()),
        }
    }
}

/// Asks `GET /key` about the key: free, and touches no model.
pub async fn check_key(client: &Client, key: &str) -> KeyCheck {
    let sent = client
        .http()
        .get(client.url("/key"))
        .header("Authorization", format!("Bearer {key}"))
        .timeout(Duration::from_secs(20))
        .send()
        .await;
    let response = match sent {
        Ok(r) => r,
        Err(e) => {
            return KeyCheck::Unreachable {
                reason: crate::openrouter::transport_name(&e),
            };
        }
    };
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return KeyCheck::Rejected { status };
    }
    if !response.status().is_success() {
        return KeyCheck::Unreachable {
            reason: format!("HTTP {status}"),
        };
    }
    match response.json::<Value>().await {
        Ok(body) => {
            let data = body.get("data").cloned().unwrap_or(Value::Null);
            KeyCheck::Valid {
                usage: data.get("usage").and_then(Value::as_f64),
                limit: data.get("limit").and_then(Value::as_f64),
                data,
            }
        }
        Err(e) => KeyCheck::Unreachable {
            reason: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking() {
        assert_eq!(mask_key("short"), "*****");
        assert_eq!(mask_key("abcdefghijklmnop"), "****************");
        assert_eq!(mask_key("sk-or-v1-0123456789abcdef"), "sk-or-v1-...cdef");
    }

    #[test]
    fn format_checks() {
        assert!(validate_key_format("sk-or-v1-abc").is_ok());
        assert!(validate_key_format("").is_err());
        assert!(validate_key_format("has space").is_err());
        assert!(validate_key_format("clé").is_err());
        assert!(looks_like_key("sk-or-x"));
        assert!(!looks_like_key("x"));
    }

    #[test]
    fn environment_then_settings() {
        let tmp = tempfile::tempdir().expect("tmp");
        let e = load_api_key_from(None, tmp.path()).expect_err("no key");
        assert!(
            e.to_string()
                .starts_with("No OpenRouter API key. Set OPENROUTER_API_KEY")
        );
        std::fs::write(
            settings_path(tmp.path()),
            "\u{feff}{\"env\": {\"OPENROUTER_API_KEY\": \"\u{feff} from-settings \\n\"}}",
        )
        .expect("write");
        assert_eq!(
            load_api_key_from(None, tmp.path()).expect("key"),
            "from-settings"
        );
        assert_eq!(
            load_api_key_from(Some("  "), tmp.path()).expect("key"),
            "from-settings"
        );
        assert_eq!(
            load_api_key_from(Some(" env "), tmp.path()).expect("key"),
            "env"
        );
    }
}
