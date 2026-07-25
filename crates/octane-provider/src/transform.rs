//! Where provider quirks are quarantined.
//!
//! Real examples this exists to absorb: Anthropic rejects empty content blocks;
//! Mistral requires tool call ids to be exactly nine alphanumeric characters;
//! Anthropic, Bedrock, and OpenRouter need explicit `cache_control` breakpoints
//! while OpenAI caches prefixes on its own.
//!
//! One rule dominates all of them: **the prompt is append-only.** Editing or
//! reordering anything already sent invalidates the prefix cache from that point
//! on, which turns a cheap turn into a full re-read of the conversation. When
//! configuration changes mid-session, append a new developer message rather than
//! rewriting the old one.

use octane_protocol::Message;

use crate::model::{ModelInfo, ModelRequest, ToolSchema};

pub trait ProviderTransform: Send + Sync {
    /// Reshape history to satisfy the provider's validation rules.
    fn messages(&self, messages: Vec<Message>) -> Vec<Message> {
        messages
    }

    /// Place cache breakpoints. No-op where the provider caches implicitly.
    fn apply_caching(&self, request: &mut ModelRequest) {
        let _ = request;
    }

    /// Clamp or adjust sampling parameters the provider rejects.
    fn options(&self, request: &mut ModelRequest, info: &ModelInfo) {
        request.max_output_tokens = request.max_output_tokens.min(info.max_output_tokens);
    }

    /// Normalize tool schemas, and — critically — impose a **stable order**.
    ///
    /// Tool definitions sit in the cached prefix. A registry that yields them in
    /// hash-map order re-orders them between turns and silently destroys every
    /// cache hit. Codex shipped exactly this bug with MCP tools.
    fn tools(&self, mut tools: Vec<ToolSchema>) -> Vec<ToolSchema> {
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }
}

/// Applies to providers that need nothing special.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassthroughTransform;

impl ProviderTransform for PassthroughTransform {}
