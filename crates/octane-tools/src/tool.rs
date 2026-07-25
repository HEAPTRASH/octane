//! The tool interface.

use async_trait::async_trait;
use camino::Utf8PathBuf;
use octane_protocol::{SessionId, ToolCallId};
use octane_provider::model::ToolSchema;
use tokio_util::sync::CancellationToken;

/// Everything a tool may know about its invocation.
///
/// Deliberately does *not* include a permission handle. Tools describe what they
/// intend to do; the loop decides whether it happens. Letting a tool grant itself
/// permission is how a permission system becomes decorative.
pub struct ToolContext {
    pub session_id: SessionId,
    pub call_id: ToolCallId,
    /// Calling agent. `plan` gets a narrower tool set than `build`.
    pub agent: String,
    /// Project root. Anything outside needs an explicit grant.
    pub workspace: Utf8PathBuf,
    /// Where relative paths resolve from.
    pub cwd: Utf8PathBuf,
    /// Ctrl-C must abort in-flight work, not merely stop rendering it.
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("session_id", &self.session_id)
            .field("call_id", &self.call_id)
            .field("agent", &self.agent)
            .field("workspace", &self.workspace)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

/// A tool result, split by audience.
///
/// `output` goes to the model and is the only field that costs tokens. `title`
/// and `metadata` go to the UI. Conflating them is how transcripts fill with
/// rendering detail the model never needed.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub title: String,
    pub output: String,
    pub metadata: Option<serde_json::Value>,
}

impl ToolOutcome {
    pub fn new(title: impl Into<String>, output: impl Into<String>) -> Self {
        Self { title: title.into(), output: output.into(), metadata: None }
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Reported back to the model as a tool result so it can correct itself.
    /// Most failures belong here — a wrong path is a conversation, not a crash.
    #[error("{0}")]
    Recoverable(String),

    /// The user said no. Ends the turn rather than letting the model retry or
    /// route around the refusal.
    #[error("denied: {0}")]
    Denied(String),

    /// The sandbox blocked it. Distinct from `Recoverable` because the model
    /// cannot fix it by trying harder — the user has to widen the policy.
    #[error("blocked by sandbox: {0}")]
    SandboxDenied(String),

    #[error("cancelled")]
    Cancelled,

    /// The harness is broken. Not the model's problem, and not its business.
    #[error("internal error: {0}")]
    Internal(String),
}

impl ToolError {
    /// Whether this failure should be reported to the model and the loop
    /// continued, as opposed to ending the turn.
    pub fn is_reportable_to_model(&self) -> bool {
        matches!(self, Self::Recoverable(_) | Self::SandboxDenied(_))
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// The prose contract shown to the model. Long and prescriptive on purpose:
    /// this is where tool reliability is actually won.
    fn description(&self) -> &str;

    fn input_schema(&self) -> serde_json::Value;

    /// Permission resources this call would need, derived from the concrete
    /// arguments.
    ///
    /// This is why policy cannot be decided from the tool name alone: `bash`
    /// needs the parsed command, `write` needs the resolved path. The loop calls
    /// this, evaluates the result, and only then calls [`Tool::execute`].
    fn required_permissions(&self, input: &str, ctx: &ToolContext) -> Vec<String>;

    /// Raw JSON, not a parsed struct, so a schema mismatch surfaces as a
    /// recoverable tool error the model can correct rather than a panic.
    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError>;

    /// Whether this tool mutates anything. Read-only tools are what a `plan`
    /// agent is restricted to.
    fn is_mutating(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}
