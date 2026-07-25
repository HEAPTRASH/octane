//! Messages as arrays of polymorphic parts.
//!
//! A message is not a string. Modelling it as a list of typed [`Part`]s is what
//! lets text, reasoning traces, tool calls, and attachments share one container
//! while each keeps its own schema, persistence, and rendering. Adding a part
//! kind touches nothing existing.

use serde::{Deserialize, Serialize};

use crate::id::ToolCallId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Immutable instructions. Must come first in the prompt so that a prefix
    /// cache can cover it, and so injected content cannot outrank it.
    System,
    /// Harness-supplied context: sandbox policy, environment, project memory.
    Developer,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn new(role: Role, parts: Vec<Part>) -> Self {
        Self { role, parts }
    }

    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![Part::Text { text: text.into() }])
    }

    /// Concatenation of every text part, ignoring reasoning and tool traffic.
    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    /// Model reasoning. Rendered collapsed, and replayed to the provider on the
    /// next turn when the provider supports it.
    Reasoning {
        text: String,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    /// A file pulled into context, tracked separately from prose so compaction
    /// can restore recently-read files after a summarization pass.
    File {
        path: String,
        content: String,
    },
    Image {
        media_type: String,
        /// Base64. Kept out of the text path so token accounting stays honest.
        data: String,
    },
    /// Harness-injected note that reads as content to the model but was never
    /// typed by the user — mode switches, truncation notices, reminders.
    Synthetic {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    /// Raw JSON as the model emitted it. Kept unparsed so a schema mismatch is
    /// reported to the model as a tool error rather than crashing the turn.
    pub input: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    /// What the model sees.
    pub output: String,
    /// What the UI sees: diffs, exit codes, previews. Never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub is_error: bool,
}
