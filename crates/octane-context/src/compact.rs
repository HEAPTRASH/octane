//! Compaction: replace an old span of the conversation with a summary.
//!
//! The lever after pruning. Pruning drops tool output and keeps the reasoning;
//! compaction gives up the reasoning too, in exchange for a paragraph. It costs
//! a model call and it loses detail no summary recovers, so it runs only when
//! pruning cannot free enough.
//!
//! Three rules shape the span this module chooses:
//!
//! 1. **The cached prefix is never touched.** System instructions, the sandbox
//!    description and project memory sit at the front in a fixed order so every
//!    turn shares them. Summarizing them would invalidate the prefix on every
//!    subsequent turn, which costs more than the compaction saves.
//! 2. **Recent turns are never touched.** The model needs the immediate
//!    conversation intact to keep working; a summary of the last exchange is
//!    the one thing that makes it repeat itself.
//! 3. **The span is cut on a message boundary that leaves no orphan.** A
//!    `ToolCall` whose `ToolResult` was summarized away is rejected by every
//!    provider.

use octane_protocol::{Message, Part, Role};

use crate::ContextError;

/// Recent conversation kept out of the summary, as a share of the window.
///
/// A fraction rather than a constant: a fixed 20k keep-window is larger than
/// the entire context of a small local model, which would make compaction
/// impossible for exactly the models most likely to need it.
const KEEP_RECENT_SHARE: f64 = 0.25;

/// Ceiling on the keep window. Past this, holding more recent context stops
/// buying accuracy and just delays the next compaction.
const KEEP_RECENT_CEILING: usize = 20_000;

/// What a summarizer produced, and what it cost.
///
/// Usage travels with the text because compaction is a paid model call that
/// the session would otherwise not see: `/stats` and the status line both
/// accumulate from reported usage, and a silent call understates both.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub text: String,
    pub usage: octane_protocol::Usage,
}

/// Turns a span of conversation into a paragraph.
///
/// A trait so `octane-context` states the requirement without owning a provider
/// handle, and so the turn loop can be tested against a canned summary.
#[async_trait::async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, span: &[Message]) -> Result<Summary, ContextError>;
}

/// Which messages to replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// First message of the span, immediately after the preserved prefix.
    pub start: usize,
    /// One past the last message of the span.
    pub end: usize,
}

/// Choose the span to summarize.
///
/// `preserved_prefix` is how many leading messages are the cached prompt
/// prefix; the caller knows this because it assembled them.
pub fn plan(
    messages: &[Message],
    preserved_prefix: usize,
    effective_window: usize,
) -> Result<Plan, ContextError> {
    let keep = keep_recent_tokens(effective_window);
    let start = preserved_prefix.min(messages.len());

    // Walk back from the end until `keep` tokens are accounted for. Everything
    // before that is a candidate.
    let mut accumulated = 0;
    let mut cut = messages.len();
    while cut > start {
        let next = cut - 1;
        accumulated += crate::prune::estimate_tokens(std::slice::from_ref(&messages[next]));
        if accumulated >= keep {
            break;
        }
        cut = next;
    }

    let cut = orphan_safe_cut(messages, start, cut);
    if cut <= start {
        return Err(ContextError::Irreducible);
    }
    Ok(Plan { start, end: cut })
}

/// Recent tokens held out of any summary.
pub fn keep_recent_tokens(effective_window: usize) -> usize {
    (((effective_window as f64) * KEEP_RECENT_SHARE) as usize).clamp(1, KEEP_RECENT_CEILING)
}

/// Pull the cut back so the span never ends between a call and its result.
///
/// Splitting a pair leaves a `ToolCall` in the summarized region whose
/// `ToolResult` survives, or the reverse. Providers reject both.
fn orphan_safe_cut(messages: &[Message], start: usize, mut cut: usize) -> usize {
    while cut > start {
        let unresolved = messages[start..cut]
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                Part::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .any(|id| {
                !messages[start..cut].iter().flat_map(|m| &m.parts).any(|part| {
                    matches!(part, Part::ToolResult(result) if result.call_id == id)
                })
            });

        if !unresolved {
            return cut;
        }
        cut -= 1;
    }
    start
}

/// Build the compacted history. Does not modify the input.
///
/// Returned rather than applied in place so the caller can measure it and
/// discard it if it did not shrink. Installing first and checking after means
/// a summary larger than what it replaced is already in the history when the
/// caller notices, and the retry then summarizes the summary.
pub fn apply(messages: &[Message], plan: &Plan, summary: &Summary) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len() - (plan.end - plan.start) + 1);
    out.extend_from_slice(&messages[..plan.start]);

    // Developer, not System. The Anthropic codec lifts every `Role::System`
    // message out of the conversation and into the top-level `system` field
    // regardless of where it sits, so a System summary would be teleported
    // into the cached prefix it was written to stay out of.
    out.push(Message::new(
        Role::Developer,
        vec![Part::Synthetic {
            text: format!(
                "Summary of earlier conversation, which was removed to reclaim context:\n\n{}",
                summary.text
            ),
        }],
    ));

    out.extend_from_slice(&messages[plan.end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_protocol::{ToolCall, ToolCallId, ToolResult};

    fn text(role: Role, body: &str) -> Message {
        Message::text(role, body)
    }

    fn pair(name: &str, output: &str) -> Vec<Message> {
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
                    output: output.into(),
                    metadata: None,
                    is_error: false,
                })],
            ),
        ]
    }

    fn summary() -> Summary {
        Summary { text: "they discussed the parser".into(), ..Summary::default() }
    }

    #[test]
    fn the_cached_prefix_is_never_summarized() {
        // Summarizing it would invalidate the prefix on every later turn, which
        // costs more than the compaction saves.
        let mut messages = vec![
            text(Role::System, "instructions"),
            text(Role::Developer, "sandbox"),
        ];
        for index in 0..40 {
            messages.push(text(Role::User, &format!("message {index} {}", "x".repeat(500))));
        }

        let plan = plan(&messages, 2, 8_000).unwrap();
        assert_eq!(plan.start, 2, "the span begins after the preserved prefix");

        let compacted = apply(&messages, &plan, &summary());
        assert_eq!(compacted[0], messages[0]);
        assert_eq!(compacted[1], messages[1]);
    }

    #[test]
    fn a_tool_call_is_never_separated_from_its_result() {
        // A call whose result was summarized away, or the reverse, is rejected
        // by every provider.
        let mut messages = vec![text(Role::System, "instructions")];
        for _ in 0..30 {
            messages.extend(pair("read", &"x".repeat(2_000)));
        }

        let plan = plan(&messages, 1, 8_000).unwrap();
        let compacted = apply(&messages, &plan, &summary());

        let calls: Vec<_> = compacted
            .iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                Part::ToolCall(call) => Some(call.id.clone()),
                _ => None,
            })
            .collect();
        for id in calls {
            assert!(
                compacted.iter().flat_map(|m| &m.parts).any(
                    |p| matches!(p, Part::ToolResult(r) if r.call_id == id)
                ),
                "every surviving call keeps its result"
            );
        }
    }

    #[test]
    fn the_summary_is_a_developer_message_not_a_system_one() {
        // The Anthropic codec hoists every System message into the top-level
        // `system` field wherever it appears, which would move the summary into
        // the cached prefix it exists to stay out of.
        let messages: Vec<Message> =
            (0..40).map(|i| text(Role::User, &format!("{i} {}", "x".repeat(500)))).collect();

        let plan = plan(&messages, 0, 8_000).unwrap();
        let compacted = apply(&messages, &plan, &summary());

        let inserted = &compacted[plan.start];
        assert_eq!(inserted.role, Role::Developer);
        assert!(matches!(inserted.parts.first(), Some(Part::Synthetic { .. })));
    }

    #[test]
    fn a_small_window_can_still_compact() {
        // A fixed keep-window larger than the whole context would make
        // compaction impossible for exactly the local models most likely to
        // hit it.
        assert!(keep_recent_tokens(4_000) < 4_000, "a 4k model keeps less than its window");
        assert_eq!(keep_recent_tokens(1_000_000), KEEP_RECENT_CEILING, "and it is capped");

        let messages: Vec<Message> =
            (0..40).map(|i| text(Role::User, &format!("{i} {}", "x".repeat(200)))).collect();
        assert!(plan(&messages, 0, 4_000).is_ok(), "a 4k model can compact");
    }

    #[test]
    fn a_history_that_is_all_prefix_and_recency_is_irreducible() {
        let messages = vec![text(Role::System, "instructions"), text(Role::User, "hello")];
        assert!(matches!(plan(&messages, 1, 200_000), Err(ContextError::Irreducible)));
    }

    #[test]
    fn compacting_removes_the_span_and_leaves_the_tail() {
        let messages: Vec<Message> =
            (0..40).map(|i| text(Role::User, &format!("{i} {}", "x".repeat(500)))).collect();

        let plan = plan(&messages, 0, 8_000).unwrap();
        let compacted = apply(&messages, &plan, &summary());

        assert_eq!(compacted.len(), messages.len() - (plan.end - plan.start) + 1);
        assert_eq!(compacted.last(), messages.last(), "the newest message survives verbatim");
        assert!(
            crate::prune::estimate_tokens(&compacted)
                < crate::prune::estimate_tokens(&messages),
            "compaction must shrink the history"
        );
    }
}
