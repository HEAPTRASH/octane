//! Stop conditions.
//!
//! The single most important property of the loop: **the model decides when the
//! task is done, not us.** We only decide when to *stop letting it try*. Every
//! variant below is a safety rail, not a completion criterion.

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a message instead of a tool call. The normal ending,
    /// and the only one that means "finished".
    Completed,

    /// Step cap hit. Prevents unbounded spend when the model cannot converge.
    StepLimit { limit: u32 },

    /// The same (tool, input, output) triple kept recurring — the model is stuck
    /// re-reading the same thing rather than making progress.
    LoopDetected { signature: String, repeats: usize },

    /// The user refused a tool call.
    ///
    /// Ends the turn rather than reporting the refusal to the model. Letting the
    /// model continue invites it to route around the denial, which converts a
    /// clear "no" into a negotiation.
    Denied { resource: String },

    /// Ctrl-C.
    Interrupted,

    /// The model was cut off mid-output by its own token limit. Distinct from
    /// `Completed` because the answer is truncated, not finished.
    OutputTruncated,

    Failed { message: String },
}

impl StopReason {
    /// Whether the turn produced a usable result.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// One line for the UI.
    pub fn summary(&self) -> String {
        match self {
            Self::Completed => "done".into(),
            Self::StepLimit { limit } => format!("stopped after {limit} steps"),
            Self::LoopDetected { repeats, .. } => {
                format!("stopped: same action repeated {repeats} times without progress")
            }
            Self::Denied { resource } => format!("stopped: you denied {resource}"),
            Self::Interrupted => "interrupted".into(),
            Self::OutputTruncated => "stopped: response hit the output limit".into(),
            Self::Failed { message } => format!("failed: {message}"),
        }
    }
}

/// Default step cap.
///
/// High enough never to interrupt legitimate work — long refactors genuinely take
/// hundreds of steps — and low enough to bound a runaway. opencode uses 1000.
pub const DEFAULT_STEP_LIMIT: u32 = 1_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_completion_counts_as_success() {
        assert!(StopReason::Completed.is_success());
        assert!(!StopReason::StepLimit { limit: 10 }.is_success());
        assert!(!StopReason::Interrupted.is_success());
        assert!(!StopReason::OutputTruncated.is_success());
    }

    #[test]
    fn summaries_name_the_cause() {
        assert!(StopReason::Denied { resource: "command(rm)".into() }.summary().contains("command(rm)"));
        assert!(
            StopReason::LoopDetected { signature: "s".into(), repeats: 6 }
                .summary()
                .contains('6')
        );
    }
}
