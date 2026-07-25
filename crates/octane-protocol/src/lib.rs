//! Shared vocabulary for octane, and the wire format clients speak.
//!
//! Three nested primitives, each with an explicit lifecycle (the shape Codex
//! settled on for its App Server, and the reason its CLI, web, and IDE surfaces
//! can share one core):
//!
//! - [`Thread`] — durable conversation container. Persists, resumes, forks.
//! - [`Turn`] — one unit of agent work: begins on user input, ends when the
//!   agent has produced every output for it. Contains many [`Item`]s.
//! - [`Item`] — atomic unit of I/O. Emits `Started`, then zero or more `Delta`s,
//!   then `Completed`. Clients render on `Started` rather than waiting for
//!   content, which is what makes streaming feel immediate.
//!
//! Everything the core does is published as an [`Event`]. The TUI is just one
//! subscriber; keeping this the *only* channel out of the core is what leaves
//! room for a second client later without refactoring the loop.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod event;
pub mod id;
pub mod item;
pub mod message;
pub mod thread;

pub use event::{Event, ItemEvent, ThreadEventKind, TurnEvent, Usage};
pub use id::{ItemId, SessionId, ThreadId, ToolCallId, TurnId};
pub use item::{ApprovalRequest, Item, ItemKind, ItemStatus};
pub use message::{Message, Part, Role, ToolCall, ToolResult};
pub use thread::{Thread, Turn, TurnStatus};
