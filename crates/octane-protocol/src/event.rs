//! The core's only output channel.
//!
//! Everything observable is an event: the TUI subscribes, and so could an HTTP
//! client, a log sink, or a test. Nothing in the loop is allowed to write to a
//! terminal directly.
//!
//! Events are published *after* the corresponding state is committed. Emitting
//! early is the classic race where a client sees the notification and then
//! cannot find the row it refers to.

use serde::{Deserialize, Serialize};

use crate::id::{ItemId, ThreadId, TurnId};
use crate::item::{Item, ItemStatus};
use crate::thread::TurnStatus;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Thread { id: ThreadId, kind: ThreadEventKind },
    Turn(TurnEvent),
    Item(ItemEvent),
    /// Token and cost accounting for one inference step.
    Usage(Usage),
    /// Context management fired. Surfaced because silent context loss is the
    /// most confusing failure mode a user can hit.
    Compaction { before_tokens: u64, after_tokens: u64, strategy: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadEventKind {
    Created,
    Resumed,
    Forked,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEvent {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub status: TurnStatus,
}

/// `Started` carries the whole item so a client can render immediately;
/// `Delta` carries only the increment; `Completed` carries the terminal payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ItemEvent {
    Started { turn_id: TurnId, item: Item },
    Delta { turn_id: TurnId, item_id: ItemId, text: String },
    Completed { turn_id: TurnId, item: Item },
    StatusChanged { turn_id: TurnId, item_id: ItemId, status: ItemStatus },
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Split out because cached input is priced differently, and because a
    /// collapsing cache-hit rate is the first symptom of a poisoned prefix.
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// USD, derived from a pricing table rather than guessed.
    pub cost: f64,
}
