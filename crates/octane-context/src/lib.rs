//! Context management — the hardest constraint in the system.
//!
//! History is append-only and resent in full on every step, so a conversation is
//! O(n²) in bytes transmitted. Prefix caching makes that affordable; running out
//! of window makes it fatal. This crate owns both concerns.
//!
//! Four strategies, applied in escalating order (`RESEARCH.md` §4):
//!
//! 1. **Prune** old tool outputs behind a protection window. Free, no API call.
//! 2. **Cache-preserving removal**, where the provider supports it.
//! 3. **Summarize** the conversation and replace the history.
//! 4. **Model-native compaction** — needs a provider endpoint we do not own.
//!
//! Two invariants the whole crate exists to protect:
//!
//! **Never orphan a tool call.** Every `ToolCall` must keep its `ToolResult` and
//! vice versa. Providers reject a history with a call and no result, so a
//! truncation that splits a pair converts a recoverable size problem into a hard
//! API error.
//!
//! **Never edit the prefix.** Pruning rewrites *old* entries, which invalidates
//! the cache from that point on. That is why pruning is worth doing only in bulk
//! and only past a minimum threshold — otherwise the cache loss costs more than
//! the tokens saved.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod budget;
pub mod prune;

pub use budget::{Budget, Pressure};
pub use prune::{PruneOutcome, prune};

/// Tokens of recent history never touched by pruning.
///
/// Too small and the agent loses what it just learned and starts repeating tool
/// calls — the failure looks like the model got stupid. opencode uses 40k.
pub const PRUNE_PROTECT_TOKENS: usize = 40_000;

/// Minimum reclaimable tokens before pruning runs at all.
///
/// Below this the cache invalidation costs more than the tokens saved, and
/// frequent small prunes thrash. opencode uses 20k.
pub const PRUNE_MINIMUM_TOKENS: usize = 20_000;

/// Consecutive compaction failures after which compaction is abandoned.
///
/// Claude Code's circuit breaker. Accepting an over-limit context is better than
/// looping forever on a compaction that cannot succeed.
pub const COMPACTION_FAILURE_LIMIT: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("compaction failed {attempts} times; giving up")]
    CircuitOpen { attempts: u32 },

    #[error("history cannot be reduced further without orphaning a tool call")]
    Irreducible,
}
