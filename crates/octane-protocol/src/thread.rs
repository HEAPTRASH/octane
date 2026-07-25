//! Threads and turns: the durable containers.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::id::{ItemId, ThreadId, TurnId};

/// A conversation. Persisted, resumable after a disconnect, and forkable —
/// coding is exploratory, so "go back to here and try the other way" is worth
/// designing for rather than bolting on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    /// Set when this thread was forked from another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<ThreadId>,
    /// Generated from the first user message by a cheap model.
    pub title: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub archived: bool,
}

/// One unit of agent work. Spans however many inference/tool cycles the model
/// needs; ends when it emits a message instead of another tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub thread_id: ThreadId,
    pub status: TurnStatus,
    pub items: Vec<ItemId>,
    /// Inference round trips consumed. The real cost driver, and what the
    /// step cap and loop detector are measured against.
    pub steps: u32,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    /// Parked on an approval request.
    AwaitingApproval,
    Completed,
    Canceled,
    Failed,
}
