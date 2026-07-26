//! Items: the atomic unit of I/O, with an explicit lifecycle.

use serde::{Deserialize, Serialize};

use crate::id::{ItemId, ToolCallId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub kind: ItemKind,
    pub status: ItemStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage { text: String },
    AgentMessage { text: String },
    Reasoning { text: String },
    ToolExecution { call_id: ToolCallId, name: String, input: String },
    /// What a tool produced, as the tool describes it.
    ///
    /// Carries the summary rather than the output. The full text goes to the
    /// model in `Part::ToolResult`; a client showing it verbatim buries the
    /// conversation under file bodies and build logs, and would be printing
    /// wire framing like `<file path=...>` that exists for the model.
    ToolResult {
        call_id: ToolCallId,
        name: String,
        /// The tool's own one-line description of what it did.
        title: String,
        /// Structured detail: line counts, exit codes, match counts, and for
        /// `edit` and `write` the content a client may want to show.
        metadata: Option<serde_json::Value>,
        /// The tool's own output, for clients that show some of it.
        ///
        /// Carried rather than rendered wholesale: a client decides how much of
        /// it a reader wants, which differs per tool. Publishing it verbatim is
        /// what buried the transcript under file bodies before.
        body: String,
        is_error: bool,
    },
    /// A file write the agent wants to make, held for review.
    Diff { path: String, unified: String },
    /// The turn is parked until the client answers.
    Approval(ApprovalRequest),
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Started,
    Streaming,
    Completed,
    Failed,
    /// The user interrupted, or a queued item was dropped by a cancellation.
    Canceled,
}

/// A pause in the middle of a turn. The loop blocks here until answered, which
/// is what makes an autonomous agent safely interruptible rather than merely
/// killable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Permission resource being requested, as `action(target)`.
    pub resource: String,
    /// Short human-readable intent, e.g. "Runs the test suite".
    pub summary: String,
    /// Present for edits: what the user is being asked to accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}
