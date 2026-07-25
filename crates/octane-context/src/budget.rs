//! Token budgeting: how close to the edge are we, and what should happen next.

use octane_provider::model::ModelInfo;

/// How much room is left, as a decision rather than a number.
///
/// The loop should branch on this, not on raw token counts — the thresholds are
/// this module's business, and scattering `if tokens > 167_000` through the loop
/// is how they drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// Plenty of room.
    Comfortable,
    /// Approaching the limit; warn the user but change nothing.
    Warning,
    /// Prune tool outputs before the next step.
    ShouldPrune,
    /// Pruning will not be enough; summarize.
    ShouldCompact,
    /// Over the limit. The next call will fail.
    Exceeded,
}

impl Pressure {
    pub fn needs_action(self) -> bool {
        matches!(self, Self::ShouldPrune | Self::ShouldCompact | Self::Exceeded)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Window minus reserved output space.
    effective_window: usize,
    compact_threshold: usize,
    warn_threshold: usize,
}

impl Budget {
    /// Derive thresholds from a model's advertised limits.
    ///
    /// Starting points are Claude Code's reported values (`RESEARCH.md` §4). On a
    /// 200k model with a 20k output reservation this compacts at ~167k and warns
    /// at ~160k:
    ///
    /// ```text
    /// effective = context_window - min(max_output, 20_000)
    /// compact   = effective - 13_000
    /// warn      = effective - 20_000
    /// ```
    ///
    /// The 13k gap exists because compaction itself needs room to run: it makes
    /// a model call, and a "compact now" that cannot fit its own request is
    /// useless.
    pub fn for_model(info: &ModelInfo) -> Self {
        let effective_window = info.effective_context_window() as usize;
        Self {
            effective_window,
            compact_threshold: effective_window.saturating_sub(13_000),
            warn_threshold: effective_window.saturating_sub(20_000),
        }
    }

    /// Explicit thresholds, for tests and for models with unusual shapes.
    pub fn new(effective_window: usize, compact_threshold: usize, warn_threshold: usize) -> Self {
        Self { effective_window, compact_threshold, warn_threshold }
    }

    pub fn effective_window(&self) -> usize {
        self.effective_window
    }

    /// Classify a token count.
    ///
    /// `reclaimable` is how many tokens pruning could recover — the caller gets
    /// it from [`crate::prune::reclaimable_tokens`]. It is what distinguishes
    /// "prune, it will be enough" from "summarize, it won't".
    pub fn pressure(&self, used: usize, reclaimable: usize) -> Pressure {
        if used >= self.effective_window {
            return Pressure::Exceeded;
        }
        if used >= self.compact_threshold {
            // Prefer pruning when it alone would drop us back out of the
            // compaction band: it is free, whereas compaction costs a model call
            // and discards detail no summary recovers.
            //
            // Note that with the default thresholds this reduces to "is there at
            // least `PRUNE_MINIMUM_TOKENS` to reclaim?". The band between
            // `compact_threshold` and `effective_window` is 13k wide, so any prune
            // meeting the 20k minimum necessarily clears it. The second condition
            // only starts to bite for models configured with a wider band via
            // [`Budget::new`]. It is kept because the *intent* — escalate only
            // when the cheap lever is insufficient — should not depend on two
            // constants happening to be ordered a particular way.
            let would_be = used.saturating_sub(reclaimable);
            if reclaimable >= crate::PRUNE_MINIMUM_TOKENS && would_be < self.compact_threshold {
                return Pressure::ShouldPrune;
            }
            return Pressure::ShouldCompact;
        }
        if used >= self.warn_threshold {
            return Pressure::Warning;
        }
        Pressure::Comfortable
    }

    /// Fraction of the effective window in use, for a status line.
    pub fn utilization(&self, used: usize) -> f64 {
        if self.effective_window == 0 {
            return 1.0;
        }
        (used as f64 / self.effective_window as f64).min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_provider::model::ProviderId;
    use octane_provider::pricing::Pricing;

    fn model(context_window: u64, max_output: u64) -> ModelInfo {
        ModelInfo {
            provider: ProviderId("test".into()),
            id: "m".into(),
            display_name: "M".into(),
            context_window,
            max_output_tokens: max_output,
            supports_tools: true,
            supports_images: false,
            supports_reasoning: false,
            needs_explicit_cache_control: false,
            pricing: Pricing::default(),
        }
    }

    #[test]
    fn thresholds_match_the_documented_formula() {
        let budget = Budget::for_model(&model(200_000, 64_000));
        // min(64_000, 20_000) reserved.
        assert_eq!(budget.effective_window(), 180_000);
        assert_eq!(budget.compact_threshold, 167_000);
        assert_eq!(budget.warn_threshold, 160_000);
    }

    #[test]
    fn low_usage_is_comfortable() {
        let budget = Budget::for_model(&model(200_000, 64_000));
        assert_eq!(budget.pressure(10_000, 0), Pressure::Comfortable);
    }

    #[test]
    fn warns_before_acting() {
        let budget = Budget::for_model(&model(200_000, 64_000));
        assert_eq!(budget.pressure(161_000, 0), Pressure::Warning);
    }

    #[test]
    fn prefers_pruning_when_it_would_suffice() {
        let budget = Budget::for_model(&model(200_000, 64_000));
        // 168k used, 30k reclaimable → 138k, under the 160k warn line.
        assert_eq!(budget.pressure(168_000, 30_000), Pressure::ShouldPrune);
    }

    #[test]
    fn compacts_when_there_is_too_little_to_reclaim() {
        let budget = Budget::for_model(&model(200_000, 64_000));
        // Only 5k reclaimable: below the minimum, so pruning is not worth the
        // cache invalidation and compaction is the only lever left.
        assert_eq!(budget.pressure(168_000, 5_000), Pressure::ShouldCompact);
        assert_eq!(budget.pressure(179_000, 0), Pressure::ShouldCompact);
    }

    #[test]
    fn compacts_when_pruning_would_not_clear_the_band() {
        // Requires a band wider than PRUNE_MINIMUM_TOKENS, which the default
        // thresholds do not produce — see the note in `pressure`.
        let budget = Budget::new(500_000, 300_000, 250_000);
        // 30k reclaimable clears the minimum but leaves us at 370k, still deep
        // inside the compaction band.
        assert_eq!(budget.pressure(400_000, 30_000), Pressure::ShouldCompact);
        // 150k reclaimable would drop us to 250k, out of the band.
        assert_eq!(budget.pressure(400_000, 150_000), Pressure::ShouldPrune);
    }

    #[test]
    fn over_the_window_is_exceeded() {
        let budget = Budget::for_model(&model(200_000, 64_000));
        assert_eq!(budget.pressure(180_000, 100_000), Pressure::Exceeded);
    }

    #[test]
    fn small_windows_do_not_underflow() {
        // A 4k model: saturating_sub must not wrap into a huge threshold.
        let budget = Budget::for_model(&model(4_096, 1_024));
        assert_eq!(budget.compact_threshold, 0);
        assert_eq!(budget.pressure(1, 0), Pressure::ShouldCompact);
    }
}
