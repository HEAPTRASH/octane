//! Pruning: drop old tool output, keep everything else.
//!
//! The cheapest lever available and usually enough. Tool output dominates a coding
//! session's token count — file reads, build logs, test output — while the
//! reasoning about it lives in assistant messages that are small and worth
//! keeping.
//!
//! What is *never* pruned:
//!
//! - anything inside the protection window (the last [`crate::PRUNE_PROTECT_TOKENS`])
//! - user-typed text, ever — that is the instructions
//! - the `ToolCall` half of a pair, so nothing is orphaned
//! - `skill` output, because it carries project rules and losing it mid-session
//!   degrades quality in a way that looks like the model got worse

use octane_protocol::{Message, Part};

/// Tools whose output survives pruning.
///
/// `skill` for the reason above. `todo_read`/`todo_write` because the todo list is
/// the model's own plan and dropping it makes it re-derive one.
const PROTECTED_TOOLS: &[&str] = &["skill", "todo_read", "todo_write"];

/// Placeholder left behind, so the model knows content was removed rather than
/// concluding it never read the file.
const CLEARED: &str = "[Old tool output cleared to reclaim context. Re-read if needed.]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneOutcome {
    pub messages: Vec<Message>,
    pub tokens_reclaimed: usize,
    pub outputs_cleared: usize,
}

/// Estimate tokens. Crude by design (~4 bytes/token): exact counts come back from
/// the provider, and this only has to be good enough to pick a strategy.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .map(|part| match part {
            Part::Text { text } | Part::Reasoning { text } | Part::Synthetic { text } => text.len(),
            Part::ToolCall(call) => call.name.len() + call.input.len(),
            Part::ToolResult(result) => result.output.len(),
            Part::File { path, content } => path.len() + content.len(),
            // Images are billed per-tile, not per-byte; a base64 length here
            // would wildly overstate them. Flat nominal estimate instead.
            Part::Image { .. } => 1_000,
        })
        .sum::<usize>()
        .div_ceil(4)
}

/// How many tokens pruning would reclaim, without doing it.
///
/// [`crate::budget::Budget::pressure`] needs this to choose between pruning and
/// compaction, so it must not have side effects.
pub fn reclaimable_tokens(messages: &[Message], protect_tokens: usize) -> usize {
    let cutoff = protection_cutoff(messages, protect_tokens);
    // The same exemptions `prune` applies. Sharing the set is what stops the
    // two from drifting: an estimate that promises tokens the prune will not
    // touch keeps `Budget::pressure` answering `ShouldPrune` long after
    // pruning has stopped doing anything, and the caller has no way to tell.
    let protected = protected_call_ids(messages);

    messages[..cutoff]
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::ToolResult(result)
                if !result.output.is_empty()
                    && result.output != CLEARED
                    && !protected.contains(&result.call_id) =>
            {
                Some(result.output.len())
            }
            _ => None,
        })
        .sum::<usize>()
        .div_ceil(4)
}

/// Clear prunable tool outputs before the protection window.
///
/// Returns the history unchanged when there is nothing worth reclaiming, so the
/// caller can skip the cache invalidation entirely.
pub fn prune(messages: Vec<Message>, protect_tokens: usize, minimum: usize) -> PruneOutcome {
    if reclaimable_tokens(&messages, protect_tokens) < minimum {
        return PruneOutcome { messages, tokens_reclaimed: 0, outputs_cleared: 0 };
    }

    let cutoff = protection_cutoff(&messages, protect_tokens);
    let protected_calls = protected_call_ids(&messages);

    let mut tokens_reclaimed = 0;
    let mut outputs_cleared = 0;
    let mut out = messages;

    for message in out.iter_mut().take(cutoff) {
        // No role guard here, deliberately. Tool results travel in `User` messages
        // (that is how Anthropic transports them), so skipping that role would
        // mean nothing is ever pruned. User-*typed* content is already safe
        // because only `ToolResult` parts are mutated below — text parts, which is
        // what an instruction is, are never touched.
        for part in &mut message.parts {
            let Part::ToolResult(result) = part else { continue };

            if result.output.is_empty() || result.output == CLEARED {
                continue;
            }
            if protected_calls.contains(&result.call_id) {
                continue;
            }

            tokens_reclaimed += result.output.len().div_ceil(4);
            outputs_cleared += 1;

            // The result is replaced, not removed: deleting it would orphan the
            // matching ToolCall and the provider would reject the history.
            result.output = CLEARED.to_string();
            result.metadata = None;
        }
    }

    PruneOutcome { messages: out, tokens_reclaimed, outputs_cleared }
}

/// Index of the first message inside the protection window.
///
/// Walks backwards accumulating tokens; everything from the returned index
/// onwards is protected.
fn protection_cutoff(messages: &[Message], protect_tokens: usize) -> usize {
    let mut accumulated = 0;
    for (index, message) in messages.iter().enumerate().rev() {
        accumulated += estimate_tokens(std::slice::from_ref(message));
        if accumulated >= protect_tokens {
            return index;
        }
    }
    0
}

/// Call ids whose output is exempt from pruning.
fn protected_call_ids(messages: &[Message]) -> std::collections::HashSet<octane_protocol::ToolCallId> {
    messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            Part::ToolCall(call) if PROTECTED_TOOLS.contains(&call.name.as_str()) => {
                Some(call.id.clone())
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_protocol::{Role, ToolCall, ToolCallId, ToolResult};

    fn tool_exchange(name: &str, output_len: usize) -> Vec<Message> {
        let id = ToolCallId::new();
        vec![
            Message::new(
                Role::Assistant,
                vec![Part::ToolCall(ToolCall {
                    id: id.clone(),
                    name: name.into(),
                    input: "{}".into(),
                })],
            ),
            Message::new(
                Role::User,
                vec![Part::ToolResult(ToolResult {
                    call_id: id,
                    output: "x".repeat(output_len),
                    metadata: None,
                    is_error: false,
                })],
            ),
        ]
    }

    fn tool_exchange_with(name: &str, output: &str) -> Vec<Message> {
        let id = ToolCallId::new();
        vec![
            Message::new(
                Role::Assistant,
                vec![Part::ToolCall(ToolCall { id: id.clone(), name: name.into(), input: "{}".into() })],
            ),
            Message::new(
                Role::User,
                vec![Part::ToolResult(ToolResult {
                    call_id: id,
                    output: output.to_string(),
                    metadata: None,
                    is_error: false,
                })],
            ),
        ]
    }

    fn long_history(exchanges: usize, name: &str, output_len: usize) -> Vec<Message> {
        (0..exchanges).flat_map(|_| tool_exchange(name, output_len)).collect()
    }

    #[test]
    fn does_nothing_below_the_minimum() {
        let history = long_history(2, "read", 100);
        let outcome = prune(history.clone(), 1_000, crate::PRUNE_MINIMUM_TOKENS);
        assert_eq!(outcome.tokens_reclaimed, 0);
        // Untouched, so the cache is preserved.
        assert_eq!(outcome.messages, history);
    }

    #[test]
    fn clears_old_output_past_the_protection_window() {
        // 40 exchanges x 40k chars ≈ 400k tokens of tool output.
        let history = long_history(40, "read", 40_000);
        let outcome = prune(history, PRUNE_PROTECT_TOKENS_TEST, 1_000);

        assert!(outcome.outputs_cleared > 0);
        assert!(outcome.tokens_reclaimed > 1_000);
        assert!(outcome.messages.iter().any(|m| m
            .parts
            .iter()
            .any(|p| matches!(p, Part::ToolResult(r) if r.output == CLEARED))));
    }

    const PRUNE_PROTECT_TOKENS_TEST: usize = 10_000;

    #[test]
    fn recent_output_is_never_cleared() {
        let history = long_history(40, "read", 40_000);
        let outcome = prune(history, PRUNE_PROTECT_TOKENS_TEST, 1_000);

        // The last exchange is well inside the protection window.
        let last = outcome.messages.last().unwrap();
        let intact = last
            .parts
            .iter()
            .any(|p| matches!(p, Part::ToolResult(r) if r.output != CLEARED));
        assert!(intact, "the most recent tool output must survive");
    }

    #[test]
    fn skill_output_survives_pruning() {
        let mut history = long_history(20, "skill", 40_000);
        history.extend(long_history(20, "read", 40_000));

        let outcome = prune(history, PRUNE_PROTECT_TOKENS_TEST, 1_000);

        // No skill result was cleared, however old.
        let skill_ids = protected_call_ids(&outcome.messages);
        for message in &outcome.messages {
            for part in &message.parts {
                if let Part::ToolResult(result) = part {
                    if skill_ids.contains(&result.call_id) {
                        assert_ne!(result.output, CLEARED, "skill output must be protected");
                    }
                }
            }
        }
    }

    #[test]
    fn tool_calls_are_never_orphaned() {
        let history = long_history(40, "read", 40_000);
        let call_count = history
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, Part::ToolCall(_)))
            .count();

        let outcome = prune(history, PRUNE_PROTECT_TOKENS_TEST, 1_000);

        let calls_after = outcome
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, Part::ToolCall(_)))
            .count();
        let results_after = outcome
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, Part::ToolResult(_)))
            .count();

        assert_eq!(calls_after, call_count);
        assert_eq!(results_after, call_count, "every call must still have a result");
    }

    #[test]
    fn reclaimable_is_side_effect_free() {
        let history = long_history(40, "read", 40_000);
        let before = history.clone();
        let _ = reclaimable_tokens(&history, PRUNE_PROTECT_TOKENS_TEST);
        assert_eq!(history, before);
    }

    #[test]
    fn empty_history_is_handled() {
        let outcome = prune(Vec::new(), 40_000, 20_000);
        assert!(outcome.messages.is_empty());
        assert_eq!(outcome.tokens_reclaimed, 0);
        assert_eq!(reclaimable_tokens(&[], 40_000), 0);
    }

    #[test]
    fn pruning_is_idempotent() {
        let history = long_history(40, "read", 40_000);
        let first = prune(history, PRUNE_PROTECT_TOKENS_TEST, 1_000);
        let second = prune(first.messages.clone(), PRUNE_PROTECT_TOKENS_TEST, 1_000);
        // Nothing left to reclaim, so the second pass is a no-op.
        assert_eq!(second.tokens_reclaimed, 0);
        assert_eq!(second.messages, first.messages);
    }

    #[test]
    fn a_cleared_placeholder_is_not_counted_as_reclaimable() {
        // `prune` skips placeholders, so counting them here promises tokens it
        // cannot deliver. `Budget::pressure` then keeps answering ShouldPrune
        // after pruning has stopped doing anything, and the caller's prune
        // branch loops without progress.
        fn result_then_tail(output: &str) -> Vec<Message> {
            vec![
                Message::new(
                    Role::User,
                    vec![Part::ToolResult(ToolResult {
                        call_id: ToolCallId::new(),
                        output: output.to_string(),
                        metadata: None,
                        is_error: false,
                    })],
                ),
                // A trailing message, or the protection cutoff leaves nothing
                // ahead of it to measure.
                Message::text(Role::Assistant, "done"),
            ]
        }

        let live = result_then_tail(&"x".repeat(200_000));
        let cleared = result_then_tail(CLEARED);

        assert!(reclaimable_tokens(&live, 0) > 0, "a real output is reclaimable");
        assert_eq!(reclaimable_tokens(&cleared, 0), 0, "a placeholder is not");
    }

    #[test]
    fn the_estimate_never_promises_more_than_the_prune_delivers() {
        // These two walk the same messages under the same exemptions, and the
        // caller uses the first to decide whether to call the second. If the
        // estimate is the larger, `Budget::pressure` keeps answering
        // ShouldPrune while `prune` clears nothing, and the turn loop spins
        // without taking a step.
        let mut history = Vec::new();
        for (name, output) in [
            ("read", "a".repeat(80_000)),   // prunable
            ("skill", "b".repeat(80_000)),  // protected by tool name
            ("grep", CLEARED.to_string()),  // already pruned
            ("read", String::new()),        // empty
            ("todo_read", "c".repeat(40_000)),
        ] {
            history.extend(tool_exchange_with(name, &output));
        }
        history.push(Message::text(Role::Assistant, "tail"));

        let estimate = reclaimable_tokens(&history, 0);
        let delivered = prune(history, 0, 0).tokens_reclaimed;

        assert_eq!(estimate, delivered, "the estimate and the prune must agree");
        assert!(delivered > 0, "the fixture must actually prune something");
    }
}
