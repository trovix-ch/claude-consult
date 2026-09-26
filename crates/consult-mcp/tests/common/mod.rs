//! Shared offline harness: a wiremock OpenRouter replaying canned replies per model id,
//! a temp install dir with models.json and display.json, and a consultant wired to both.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use consult_core::listing::ListingCache;
use consult_core::openrouter::Client;
use consult_core::panel::{Consultant, KeySource};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub const A: &str = "lab-a/alpha";
pub const B: &str = "lab-b/beta";
pub const C: &str = "lab-c/gamma";

pub fn models() -> Value {
    json!({
        "default_panel": ["alpha", "beta", "gamma"],
        "models": {
            "alpha": {"id": A, "context": 100_000, "price_in": 1.0, "price_out": 2.0, "command": "al"},
            "beta": {"id": B, "context": 100_000, "price_in": 1.0, "price_out": 2.0, "command": "be"},
            "gamma": {"id": C, "context": 100_000, "price_in": 1.0, "price_out": 2.0},
        }
    })
}

/// A final answer from the model.
pub fn reply(content: &str) -> Value {
    json!({
        "choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"cost": 0.01, "prompt_tokens": 1000, "completion_tokens": 100},
    })
}

/// A turn that reads `a.txt`.
pub fn read() -> Value {
    json!({
        "choices": [{"message": {"role": "assistant", "content": "", "tool_calls": [
            {"id": "t1", "type": "function",
             "function": {"name": "read_file", "arguments": "{\"path\": \"a.txt\"}"}}
        ]}, "finish_reason": "tool_calls"}],
        "usage": {"cost": 0.01, "prompt_tokens": 1000, "completion_tokens": 100},
    })
}

struct Script {
    queues: Mutex<HashMap<String, VecDeque<Value>>>,
    delay: Duration,
}

impl Respond for Script {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let model = body["model"].as_str().unwrap_or("").to_string();
        let step = self
            .queues
            .lock()
            .expect("lock")
            .get_mut(&model)
            .and_then(VecDeque::pop_front);
        match step {
            Some(v) => ResponseTemplate::new(200).set_body_json(v),
            None => {
                ResponseTemplate::new(599).set_body_string(format!("script ran out for {model}"))
            }
        }
        .set_delay(self.delay)
    }
}

pub struct Harness {
    pub server: MockServer,
    pub tmp: tempfile::TempDir,
    pub project: PathBuf,
    pub install: PathBuf,
    pub claude: PathBuf,
}

impl Harness {
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        std::fs::write(project.join("a.txt"), "hello\n").expect("write");
        let install = tmp.path().join("install");
        std::fs::create_dir_all(&install).expect("mkdir");
        std::fs::write(install.join("models.json"), models().to_string()).expect("write");
        std::fs::write(install.join("display.json"), r#"{"progress": "full"}"#).expect("write");
        let claude = tmp.path().join("claude");
        std::fs::create_dir_all(&claude).expect("mkdir");
        // The listing is down: registered aliases resolve without it.
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Self {
            server,
            tmp,
            project,
            install,
            claude,
        }
    }

    fn consultant_with(&self, key: KeySource) -> Consultant {
        Consultant {
            client: Client::with_base_url(format!("{}/api/v1", self.server.uri()))
                .with_backoff_unit(Duration::ZERO),
            key,
            install_dir: self.install.clone(),
            listing: Arc::new(ListingCache::new()),
        }
    }

    /// A consultant with a dummy key.
    pub fn consultant(&self) -> Consultant {
        self.consultant_with(KeySource::Fixed("dummy-not-a-key".into()))
    }

    /// A consultant whose only key source is an empty Claude dir: never the real env.
    pub fn keyless(&self) -> Consultant {
        self.consultant_with(KeySource::SettingsOnly(self.claude.clone()))
    }

    pub async fn script(&self, by_model: Vec<(&str, Vec<Value>)>, delay: Duration) {
        let queues = by_model
            .into_iter()
            .map(|(m, steps)| (m.to_string(), steps.into_iter().collect()))
            .collect();
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(Script {
                queues: Mutex::new(queues),
                delay,
            })
            .mount(&self.server)
            .await;
    }

    pub async fn chat_requests(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.path() == "/api/v1/chat/completions")
            .count()
    }
}
