//! Normalized streaming events.
//!
//! Providers disagree on chunk shapes, event names, and ordering. Everything is
//! flattened into this one enum at the provider boundary so the loop has exactly
//! one thing to consume.

use std::pin::Pin;

use futures::Stream;
use octane_protocol::{ToolCallId, Usage};

use crate::ProviderError;

pub type ModelStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A new inference step began. One step is one round trip.
    StepStart,

    TextStart,
    TextDelta(String),
    TextEnd,

    ReasoningStart,
    ReasoningDelta(String),
    ReasoningEnd,

    /// Tool name and id are known before the arguments finish streaming, so the
    /// UI can show "Reading src/main.rs…" while the JSON is still arriving.
    ToolCallStart { id: ToolCallId, name: String },
    ToolCallInputDelta { id: ToolCallId, delta: String },
    ToolCallEnd { id: ToolCallId, input: String },

    StepFinish { reason: FinishReason, usage: Usage },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// The model asked for tools. The loop continues.
    ToolCalls,
    /// The model produced a message instead. The turn ends.
    Stop,
    /// Output limit hit mid-answer — truncated, not finished.
    Length,
    /// Provider-side content filtering.
    ContentFilter,
}

impl FinishReason {
    /// Whether the loop should run another step.
    pub fn continues(self) -> bool {
        matches!(self, Self::ToolCalls)
    }
}
