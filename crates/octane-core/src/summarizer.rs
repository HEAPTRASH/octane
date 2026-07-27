//! Turning an old span of conversation into a paragraph, with a model.
//!
//! The adapter between [`octane_context::compact::Summarizer`], which states
//! the requirement without owning a provider handle, and a real endpoint.
//!
//! Nothing here publishes items. A summarization is machinery, not part of the
//! conversation, and streaming it into the transcript would show the user a
//! paragraph about their own session appearing for no reason they asked for.
//! Usage *is* reported, because it is a paid call the session would otherwise
//! not see.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use octane_context::ContextError;
use octane_context::compact::{Summarizer, Summary};
use octane_protocol::{Message, Part, Role};
use octane_provider::model::{LanguageModel, ModelRequest};
use octane_provider::stream::StreamEvent;

/// What the summarizer is asked to produce.
///
/// Written for the reader that comes after it: the next turn's model, which
/// has lost the detail and needs enough to continue without asking the user to
/// repeat themselves.
const INSTRUCTIONS: &str = "\
You are compacting a coding session that has run out of context.

Summarize the conversation below so that work can continue without it. Keep:
what the user asked for and any constraints they gave, decisions taken and the
reasons, files and symbols touched, what is done, and what is still outstanding.

Drop tool output, exploration that led nowhere, and anything already superseded.

Write plain prose. Do not address the user, do not offer to help, and do not
speculate about what was not said.";

pub struct ModelSummarizer {
    model: Arc<dyn LanguageModel>,
}

impl ModelSummarizer {
    pub fn new(model: Arc<dyn LanguageModel>) -> Self {
        Self { model }
    }
}

impl std::fmt::Debug for ModelSummarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelSummarizer")
            .field("model", &self.model.info().display_name)
            .finish()
    }
}

#[async_trait]
impl Summarizer for ModelSummarizer {
    async fn summarize(&self, span: &[Message]) -> Result<Summary, ContextError> {
        let info = self.model.info();

        // The span travels as one transcript inside a single user message
        // rather than as the messages themselves. Replaying them would put
        // orphaned tool calls in front of the model, and the span is cut on a
        // message boundary that need not be a valid conversation on its own.
        let mut transcript = String::new();
        for message in span {
            let body = render(message);
            if body.is_empty() {
                continue;
            }
            transcript.push_str(&format!("[{:?}] {}\n\n", message.role, body));
        }

        let request = ModelRequest {
            messages: vec![
                Message::text(Role::System, INSTRUCTIONS),
                Message::text(Role::User, transcript),
            ],
            tools: Vec::new(),
            max_output_tokens: info.max_output_tokens.min(2_000),
            temperature: None,
            thinking: octane_provider::Thinking::Off,
        };

        let mut stream = self
            .model
            .stream(request)
            .await
            .map_err(|error| ContextError::Summarization(error.to_string()))?;

        let mut text = String::new();
        let mut usage = octane_protocol::Usage::default();

        while let Some(event) = stream.next().await {
            match event.map_err(|error| ContextError::Summarization(error.to_string()))? {
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                StreamEvent::StepFinish { usage: reported, .. } => usage = reported,
                _ => {}
            }
        }

        if text.trim().is_empty() {
            // An empty summary would splice a header with nothing under it and
            // still count as a successful compaction.
            return Err(ContextError::Summarization("the model returned nothing".into()));
        }

        Ok(Summary { text: text.trim().to_string(), usage })
    }
}

/// One message as plain text for the transcript.
fn render(message: &Message) -> String {
    let mut out = String::new();
    for part in &message.parts {
        match part {
            Part::Text { text } | Part::Synthetic { text } => out.push_str(text),
            Part::Reasoning { .. } => {}
            Part::ToolCall(call) => {
                out.push_str(&format!("called {} with {}", call.name, truncate(&call.input, 200)));
            }
            Part::ToolResult(result) => {
                out.push_str(&format!("result: {}", truncate(&result.output, 400)));
            }
            Part::File { path, .. } => out.push_str(&format!("attached {path}")),
            Part::Image { .. } => out.push_str("[image]"),
        }
        out.push(' ');
    }
    out.trim().to_string()
}

/// Clip on a character boundary, since the input is arbitrary UTF-8.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "..."
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_protocol::{ToolCall, ToolCallId, ToolResult};

    #[test]
    fn a_tool_exchange_survives_as_readable_text() {
        // The span is flattened into one message, so what the summarizer sees
        // has to carry the shape of the work without the raw output.
        let id = ToolCallId::new();
        let call = Message::new(
            Role::Assistant,
            vec![Part::ToolCall(ToolCall {
                id: id.clone(),
                name: "read".into(),
                input: "{\"path\":\"src/main.rs\"}".into(),
            })],
        );
        let result = Message::new(
            Role::User,
            vec![Part::ToolResult(ToolResult {
                call_id: id,
                output: "fn main() {}".into(),
                metadata: None,
                is_error: false,
            })],
        );

        assert!(render(&call).contains("called read"));
        assert!(render(&call).contains("src/main.rs"));
        assert!(render(&result).contains("fn main()"));
    }

    #[test]
    fn a_long_output_is_clipped_on_a_character_boundary() {
        // Tool output is arbitrary UTF-8; slicing by byte would panic.
        let text = "é".repeat(1_000);
        let clipped = truncate(&text, 400);
        assert!(clipped.chars().count() <= 404);
        assert!(clipped.ends_with("..."));
    }

    #[test]
    fn reasoning_is_left_out_of_the_transcript() {
        // It is the model's own scratch work, already superseded by whatever it
        // concluded, and it is the largest thing in a reasoning-heavy session.
        let message = Message::new(
            Role::Assistant,
            vec![
                Part::Reasoning { text: "let me think about this at length".into() },
                Part::Text { text: "the parser is recursive descent".into() },
            ],
        );
        let rendered = render(&message);
        assert!(!rendered.contains("at length"));
        assert!(rendered.contains("recursive descent"));
    }
}
