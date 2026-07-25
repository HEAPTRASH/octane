//! Controlling how much a model thinks.
//!
//! Four formats, four spellings, and one caveat that matters more than any of
//! them: **not every endpoint lets you turn it off.** Measured against
//! OpenRouter, `google/gemini-3.6-flash` and `openai/gpt-oss-120b` both reject
//! `reasoning.enabled = false` with "Reasoning is mandatory for this endpoint
//! and cannot be disabled", so [`Thinking::Off`] is a request, not a guarantee,
//! and the caller has to survive it being refused.
//!
//! Effort does work where thinking is mandatory. Same model, same question:
//!
//! | Setting | Reasoning tokens |
//! |---|---|
//! | none sent | 149 |
//! | `low` | 89–117 |
//! | `high` | 208–219 |
//!
//! Which is the useful control in practice — the cost lever is how *much* it
//! thinks, not whether.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    /// Send nothing and let the endpoint decide. The default, for the same
    /// reason temperature is unset by default: a value we invent overrides
    /// whatever the provider tuned.
    #[default]
    Auto,
    /// Ask for no thinking. Refused by some endpoints — see the module note.
    Off,
    Low,
    Medium,
    High,
    /// An explicit token budget, where the format takes one.
    Budget(u32),
}

impl Thinking {
    /// The `low`/`medium`/`high` spelling, where one applies.
    pub fn effort(self) -> Option<&'static str> {
        match self {
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            // OpenAI's spelling for "as little as possible", which is as close
            // to off as the reasoning models get.
            Self::Off => Some("minimal"),
            Self::Auto | Self::Budget(_) => None,
        }
    }

    /// A token budget, for formats that take one rather than a level.
    ///
    /// The level-to-budget mapping is a judgement call: these are the round
    /// numbers that behave sensibly against a 64k output limit.
    pub fn budget(self) -> Option<u32> {
        match self {
            Self::Budget(tokens) => Some(tokens),
            Self::Off => Some(0),
            Self::Low => Some(2_048),
            Self::Medium => Some(8_192),
            Self::High => Some(24_576),
            Self::Auto => None,
        }
    }

    pub fn is_auto(self) -> bool {
        self == Self::Auto
    }

    pub fn is_off(self) -> bool {
        matches!(self, Self::Off | Self::Budget(0))
    }

    /// Cycle for a keybinding or a bare `/thinking` with no argument.
    pub fn cycle(self) -> Self {
        match self {
            Self::Auto => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
            Self::Off | Self::Budget(_) => Self::Auto,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Auto => "auto".into(),
            Self::Off => "off".into(),
            Self::Low => "low".into(),
            Self::Medium => "medium".into(),
            Self::High => "high".into(),
            Self::Budget(tokens) => format!("{tokens} tokens"),
        }
    }
}

impl std::str::FromStr for Thinking {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim().to_ascii_lowercase();
        Ok(match value.as_str() {
            "auto" | "default" | "" => Self::Auto,
            "off" | "none" | "no" | "disabled" | "minimal" => Self::Off,
            "low" | "min" => Self::Low,
            "medium" | "med" | "mid" => Self::Medium,
            "high" | "max" => Self::High,
            other => {
                let tokens: u32 = other
                    .parse()
                    .map_err(|_| format!("expected auto, off, low, medium, high, or a token budget, got {other:?}"))?;
                Self::Budget(tokens)
            }
        })
    }
}

/// Whether an error says the endpoint refuses to stop thinking.
///
/// Worth recognizing rather than passing through: "reasoning is mandatory" in
/// response to a request the *harness* added is confusing, because the user did
/// not ask for anything that obviously relates to it.
pub fn is_mandatory_reasoning(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("reasoning is mandatory")
        || (lower.contains("reasoning") && lower.contains("cannot be disabled"))
        || (lower.contains("thinking") && lower.contains("cannot be disabled"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_sends_nothing() {
        // Same reasoning as temperature: an invented value overrides a tuned one.
        assert!(Thinking::Auto.is_auto());
        assert_eq!(Thinking::Auto.effort(), None);
        assert_eq!(Thinking::Auto.budget(), None);
    }

    #[test]
    fn off_maps_to_the_lowest_each_format_offers() {
        // Not every endpoint has a true off, so this is the closest thing.
        assert_eq!(Thinking::Off.effort(), Some("minimal"));
        assert_eq!(Thinking::Off.budget(), Some(0));
        assert!(Thinking::Off.is_off());
    }

    #[test]
    fn levels_map_to_efforts_and_budgets() {
        assert_eq!(Thinking::Low.effort(), Some("low"));
        assert_eq!(Thinking::High.effort(), Some("high"));
        assert!(Thinking::Low.budget() < Thinking::High.budget());
    }

    #[test]
    fn an_explicit_budget_overrides_the_level_mapping() {
        assert_eq!(Thinking::Budget(5_000).budget(), Some(5_000));
        // A budget has no effort spelling, so nothing is guessed.
        assert_eq!(Thinking::Budget(5_000).effort(), None);
    }

    #[test]
    fn a_zero_budget_counts_as_off() {
        assert!(Thinking::Budget(0).is_off());
    }

    #[test]
    fn cycling_visits_every_level_and_returns() {
        let mut level = Thinking::Auto;
        let mut seen = vec![level];
        for _ in 0..4 {
            level = level.cycle();
            seen.push(level);
        }
        assert_eq!(
            seen,
            vec![
                Thinking::Auto,
                Thinking::Low,
                Thinking::Medium,
                Thinking::High,
                Thinking::Off
            ]
        );
        assert_eq!(level.cycle(), Thinking::Auto);
    }

    #[test]
    fn levels_parse_from_what_people_type() {
        for (text, expected) in [
            ("auto", Thinking::Auto),
            ("", Thinking::Auto),
            ("off", Thinking::Off),
            ("none", Thinking::Off),
            ("LOW", Thinking::Low),
            ("med", Thinking::Medium),
            ("max", Thinking::High),
            ("4096", Thinking::Budget(4_096)),
        ] {
            assert_eq!(text.parse::<Thinking>().unwrap(), expected, "{text:?}");
        }
    }

    #[test]
    fn an_unparseable_level_explains_the_options() {
        let error = "sideways".parse::<Thinking>().unwrap_err();
        assert!(error.contains("low"));
        assert!(error.contains("sideways"));
    }

    #[test]
    fn mandatory_reasoning_refusals_are_recognized() {
        // Measured verbatim from OpenRouter.
        assert!(is_mandatory_reasoning(
            "Reasoning is mandatory for this endpoint and cannot be disabled."
        ));
        assert!(is_mandatory_reasoning("thinking cannot be disabled for this model"));
        assert!(!is_mandatory_reasoning("invalid api key"));
        assert!(!is_mandatory_reasoning("context length exceeded"));
    }
}
