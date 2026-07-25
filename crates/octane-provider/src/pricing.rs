//! Per-token pricing, so cost can be reported rather than guessed.

use serde::{Deserialize, Serialize};

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    /// Reads against a warm cache. Usually a large discount, which is why
    /// cache-hit rate belongs in the usage display.
    pub cached_input: f64,
    /// Charged by some providers for establishing the cache entry.
    pub cache_write: f64,
}

impl Pricing {
    pub fn cost(&self, usage: &octane_protocol::Usage) -> f64 {
        let fresh_input = usage.input_tokens.saturating_sub(usage.cached_input_tokens);
        let per_million = 1_000_000.0;

        (fresh_input as f64 * self.input
            + usage.cached_input_tokens as f64 * self.cached_input
            + usage.output_tokens as f64 * self.output)
            / per_million
    }
}
