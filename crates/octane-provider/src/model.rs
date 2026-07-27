//! The model interface, and how one gets selected.

use async_trait::async_trait;
use octane_protocol::Message;
use serde::{Deserialize, Serialize};

use crate::ProviderError;
use crate::stream::ModelStream;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

/// What a model can do and what it costs. Capabilities are data, not `if`
/// statements scattered through the loop — a model that cannot call tools or
/// accept images should degrade by configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: ProviderId,
    pub id: String,
    pub display_name: String,

    pub context_window: u64,
    pub max_output_tokens: u64,

    pub supports_tools: bool,
    pub supports_images: bool,
    pub supports_reasoning: bool,
    /// Whether the provider needs explicit cache breakpoints (Anthropic-style)
    /// rather than caching prefixes automatically (OpenAI-style).
    pub needs_explicit_cache_control: bool,

    pub pricing: crate::pricing::Pricing,
}

impl ModelInfo {
    /// Usable input budget: the window minus room for the reply.
    ///
    /// Compaction triggers against this, not the raw window — a prompt that
    /// technically fits but leaves no room to answer is still a failed turn.
    pub fn effective_context_window(&self) -> u64 {
        self.context_window
            .saturating_sub(self.max_output_tokens.min(20_000))
    }
}

/// A single inference request. Deliberately flat: assembling it is the caller's
/// job, because prompt *order* is a cache concern the provider cannot see.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub max_output_tokens: u64,
    pub temperature: Option<f32>,
    /// How much the model should think. Each format spells this differently,
    /// and not every endpoint honours `Off` — see [`crate::thinking`].
    pub thinking: crate::thinking::Thinking,
}

/// A tool as the model sees it: name, prose contract, JSON Schema.
///
/// The description is the real interface. It carries the behavioral contract
/// ("absolute paths only", "batch independent reads") and is where most tool
/// reliability work actually happens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[async_trait]
pub trait LanguageModel: Send + Sync {
    fn info(&self) -> &ModelInfo;

    /// Start an inference call. Returns immediately with a stream; the caller
    /// drives it and decides what to persist and publish.
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError>;
}

/// Resolves a model on demand.
///
/// A closure and not a value: `/model` mid-session must swap the backing model
/// without discarding the conversation.
pub type ModelSelector =
    std::sync::Arc<dyn Fn() -> std::sync::Arc<dyn LanguageModel> + Send + Sync>;
