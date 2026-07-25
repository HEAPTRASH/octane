//! One interface over many model APIs.
//!
//! The loop must never contain a `match provider`. Every provider difference —
//! empty-content filtering, tool-id format rules, where cache breakpoints go,
//! whether reasoning replays — lives in [`transform`] and nowhere else. The loop
//! only knows: send messages, receive a stream.
//!
//! [`LanguageModel`] is obtained through a *closure* rather than held as a
//! value, so the model can be swapped mid-session with context intact. That is
//! painful to retrofit and nearly free to design in.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod api;
pub mod config;
pub mod model;
pub mod registry;
pub mod pricing;
pub mod stream;
pub mod transform;

pub use api::ApiType;
pub use config::{
    Auth, ConfigError, Defaults, ModelEntry, ProviderConfig, ResolvedModel, Role,
};
pub use registry::Registry;
pub use model::{LanguageModel, ModelInfo, ModelRequest, ModelSelector, ProviderId};
pub use stream::{FinishReason, StreamEvent};
pub use transform::ProviderTransform;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The prompt exceeded the model's window. Recoverable: compact and retry.
    #[error("context window exceeded: {used} of {limit} tokens")]
    ContextWindowExceeded { used: u64, limit: u64 },

    /// Retryable with backoff.
    #[error("rate limited{}", .retry_after_secs.map(|s| format!(", retry after {s}s")).unwrap_or_default())]
    RateLimited { retry_after_secs: Option<u64> },

    /// Credentials are missing or stale; the caller may be able to refresh.
    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("provider returned {status}: {message}")]
    Api { status: u16, message: String },
}

impl ProviderError {
    /// Whether a bare retry could plausibly succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Transport(_) | Self::Api { status: 500..=599, .. }
        )
    }
}
