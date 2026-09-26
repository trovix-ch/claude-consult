//! The MCP server: the `consult`, `consult_clean` and `list_reviewers` tools.
//!
//! Every result is plain text. A structured result would reach the client as JSON with
//! every newline escaped, which is what it would then show and forward.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use consult_core::display::load_progress_style;
use consult_core::panel::{
    ConsultRequest, Consultant, MAX_REVIEWER_COST_USD, Progress, clamp_steps, failed,
    normalize_mode, split_list,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::Value;

use crate::heartbeat::{self, PROGRESS_INTERVAL, PeerSink, Ticker};

/// The server's name, which is also the prefix of the tool names Claude Code sees
/// (`mcp__openrouter__consult`), so hooks and templates depend on it.
pub const SERVER_NAME: &str = "openrouter";

/// The server's instructions to the client, verbatim.
pub const INSTRUCTIONS: &str = "Second opinions from non-Claude models via OpenRouter. Each reviewer \
investigates the project itself with read-only tools and cannot modify \
anything. Use it on plans, designs, and proposed solutions before \
committing to them.";

/// The `consult` tool's description, verbatim.
pub const CONSULT_DESCRIPTION: &str = "Get an independent opinion from non-Claude models running on OpenRouter. \
Two modes: `review` (default) assesses a plan, design, proposal or idea; \
`diagnose` works out what is causing a problem whose cause is not yet \
established, ranking competing explanations and naming the cheapest check \
that would settle it. Each reviewer gets a clean context plus read-only \
access to the project (glob, grep, read_file, read-only git) and \
investigates on its own before answering; none can modify anything. Three \
reviewers from different labs run concurrently, so disagreement between \
them is a useful signal. Put everything that matters in `question` — they \
cannot see the conversation.";

/// The `consult_clean` tool's description, verbatim.
pub const CONSULT_CLEAN_DESCRIPTION: &str = "Ask a design or architecture question with NO project context at all — no file \
tree, no code, no git, no tools. Use this when the existing implementation would \
anchor the answer: showing a model the current design reliably produces a patch \
to what exists rather than what a competent engineer would actually build. Ask \
for the design you want judged on its merits, describing requirements and \
constraints only. Defaults to the full panel, each responder pointed at a \
different aspect of the problem (simplicity / scale-and-failure / \
question-the-frame / data-and-state) so the answers cover more ground; with no \
repo to read this costs only a few cents. Name a single model for a quick \
one-off. The answer is unanchored and therefore also unverified against the \
real codebase — check it before acting.";

/// The `list_reviewers` tool's description, verbatim.
pub const LIST_REVIEWERS_DESCRIPTION: &str = "Show the registered reviewer models, the names they answer to, their context \
windows and per-million-token prices, live from OpenRouter when it answers.";

// A list that also arrives as one comma-separated string; models send either. Plain
// comments here, not doc comments: a doc comment would be copied into the schema.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

fn split(value: Option<StringOrList>) -> Option<Vec<String>> {
    let v = match value? {
        StringOrList::One(s) => Value::String(s),
        StringOrList::Many(v) => Value::Array(v.into_iter().map(Value::String).collect()),
    };
    split_list(&v)
}

fn default_max_steps() -> i64 {
    consult_core::panel::DEFAULT_MAX_STEPS as i64
}

fn default_mode() -> String {
    "review".to_string()
}

// The field descriptions are the Python docstring's `Args:` entries, each unwrapped
// onto one line (schemars joins doc lines with "\n", so a wrapped doc comment would
// put newlines into the description).
#[derive(Debug, Deserialize, JsonSchema)]
struct ConsultArgs {
    #[doc = "The plan, design, problem, or question. Be generous with \
context: state the goal, the constraints, and what you have already \
ruled out. Reviewers see only this plus the project itself."]
    question: String,
    #[doc = "Absolute path to the project the reviewers may read. Defaults to \
the server's working directory; pass it explicitly when in doubt."]
    #[serde(default)]
    root: String,
    #[doc = "Reviewer aliases, command names such as `deepseek`, or ids of \
models in OpenRouter's tool-capable listing (routers and presets are \
refused). Omit for the default panel. Call list_reviewers to see what \
is registered."]
    #[serde(default)]
    models: Option<StringOrList>,
    #[doc = "Project-relative file paths to place in front of every \
reviewer up front, for files you already know are central."]
    #[serde(default)]
    attachments: Option<StringOrList>,
    #[doc = "Cap on investigation rounds per reviewer. Default 24."]
    #[serde(default = "default_max_steps")]
    max_steps: i64,
    #[doc = "\"review\" (default) to assess a plan, design, proposal or idea — \
is it sound, what does it get wrong, what does it omit. Or \
\"diagnose\" when something is going wrong and the cause is not yet \
established: reviewers then work from evidence, rank competing \
explanations, and name the cheapest check that would settle it. \
Using \"review\" for an unsolved problem asks the wrong question."]
    #[serde(default = "default_mode")]
    mode: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CleanArgs {
    #[doc = "The problem, stated as requirements and constraints. Do NOT describe \
the current implementation or name the files involved — that is the whole \
point of this tool. Include what must be true, what the load looks like, \
and any hard constraints."]
    question: String,
    #[doc = "Optional reviewer aliases, command names or OpenRouter model ids. Defaults to \
the full panel, which is what you usually want here. Pass one alias for a \
quick single answer."]
    #[serde(default)]
    models: Option<StringOrList>,
}

/// The MCP server. Cheap to clone: the streamable HTTP transport builds one per request.
///
/// It holds no copy of `models.json` or `display.json`: both are read on every call,
/// so a re-install takes effect in a long-lived shared server without a restart.
#[derive(Clone, Debug)]
pub struct ConsultServer {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    consultant: Consultant,
    interval: Duration,
}

impl ConsultServer {
    /// A server against the real OpenRouter, reading the registry from `install_dir`
    /// and the key from the environment or `claude_dir`'s settings.json.
    pub fn new(install_dir: PathBuf, claude_dir: PathBuf) -> Self {
        Self::from_consultant(Consultant::new(install_dir, claude_dir))
    }

    /// A server around this consultant (tests inject a mock base URL and key here).
    pub fn from_consultant(consultant: Consultant) -> Self {
        Self {
            inner: Arc::new(Inner {
                consultant,
                interval: PROGRESS_INTERVAL,
            }),
        }
    }

    /// The same server with another progress interval (tests shorten it).
    pub fn with_progress_interval(self, interval: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                consultant: self.inner.consultant.clone(),
                interval,
            }),
        }
    }

    /// The install dir the server reads `models.json` and `display.json` from.
    pub fn install_dir(&self) -> &Path {
        &self.inner.consultant.install_dir
    }

    /// The consultant every call goes through.
    pub fn consultant(&self) -> &Consultant {
        &self.inner.consultant
    }

    fn ticker(&self, ctx: &RequestContext<RoleServer>, progress: &Arc<Progress>) -> Option<Ticker> {
        // No token means the client asked for no progress: send nothing.
        let token = ctx.meta.get_progress_token()?;
        let style = load_progress_style(self.install_dir());
        Some(Ticker::start(
            PeerSink::new(ctx.peer.clone(), token),
            Arc::clone(progress),
            style,
            self.inner.interval,
        ))
    }

    /// Runs `work` with a heartbeat, turning a panic into the tool's failure text and
    /// a client's cancellation into an abandoned call.
    async fn reporting<F>(
        &self,
        ctx: &RequestContext<RoleServer>,
        label: &str,
        failure: &str,
        work: impl FnOnce(Consultant, Arc<Progress>) -> F,
    ) -> Result<CallToolResult, McpError>
    where
        F: Future<Output = String> + Send + 'static,
    {
        let progress = Arc::new(Progress::new(label));
        let ticker = self.ticker(ctx, &progress);
        // Spawned so a panic becomes text rather than a dead connection; aborted on
        // every early exit so a cancelled call stops spending.
        let mut task = AbortOnDrop(tokio::spawn(work(
            self.inner.consultant.clone(),
            Arc::clone(&progress),
        )));
        let joined = tokio::select! {
            joined = &mut task.0 => Some(joined),
            _ = ctx.ct.cancelled() => None,
        };
        // Awaited so no progress notification can follow the result.
        heartbeat::stop(ticker).await;
        match joined {
            Some(Ok(text)) => Ok(text_result(text)),
            Some(Err(e)) => Ok(text_result(failed(&format!("{failure}RuntimeError: {e}")))),
            None => Err(McpError::internal_error("request cancelled", None)),
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<String>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn text_result(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

#[tool_router]
impl ConsultServer {
    #[tool(
        name = "consult",
        title = "Consult a review panel",
        description = CONSULT_DESCRIPTION
    )]
    async fn consult(
        &self,
        Parameters(args): Parameters<ConsultArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let req = ConsultRequest {
            question: args.question,
            root: if args.root.is_empty() {
                cwd()
            } else {
                args.root
            },
            models: split(args.models),
            attachments: split(args.attachments),
            max_steps: clamp_steps(Some(args.max_steps)),
            max_cost_usd: MAX_REVIEWER_COST_USD,
            mode: normalize_mode(Some(&args.mode)),
        };
        self.reporting(&ctx, "consult", "Consult failed: ", |c, p| async move {
            c.consult_text(&req, Some(&p)).await
        })
        .await
    }

    #[tool(
        name = "consult_clean",
        title = "Clean-room question (no project context)",
        description = CONSULT_CLEAN_DESCRIPTION
    )]
    async fn consult_clean(
        &self,
        Parameters(args): Parameters<CleanArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let question = args.question;
        let models = split(args.models);
        self.reporting(
            &ctx,
            "cleanroom",
            "Clean-room consult failed: ",
            |c, p| async move {
                c.consult_clean_text(&question, models.as_deref(), Some(&p))
                    .await
            },
        )
        .await
    }

    #[tool(
        name = "list_reviewers",
        title = "List reviewer models",
        description = LIST_REVIEWERS_DESCRIPTION
    )]
    async fn list_reviewers(&self) -> Result<CallToolResult, McpError> {
        Ok(text_result(self.inner.consultant.list_reviewers().await))
    }
}

#[tool_handler]
impl ServerHandler for ConsultServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> Option<Vec<String>> {
        split(Some(serde_json::from_value(v).expect("string or list")))
    }

    #[test]
    fn split_takes_a_string_or_a_list() {
        let ab = Some(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(parse(Value::from("a, b,,")), ab);
        assert_eq!(parse(serde_json::json!(["a", " b ", ""])), ab);
        assert_eq!(parse(Value::from(" , ")), None);
        assert_eq!(parse(serde_json::json!([])), None);
        assert_eq!(split(None), None);
    }

    #[test]
    fn argument_defaults_follow_the_python_signature() {
        let a: ConsultArgs =
            serde_json::from_value(serde_json::json!({"question": "q"})).expect("args");
        assert_eq!(
            (a.root.as_str(), a.max_steps, a.mode.as_str()),
            ("", 24, "review")
        );
        assert!(a.models.is_none() && a.attachments.is_none());
    }
}
