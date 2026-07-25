//! The policy engine: *should this be allowed to happen?*
//!
//! Strictly separate from `octane-sandbox`, which answers a different question:
//! *what can the process reach if it does something other than what it said?*
//! Policy is consent; the sandbox is containment. Conflating them produces a
//! system that is either annoying or insecure, usually both.
//!
//! Every sensitive operation is named as a [`Resource`] — `action(target)` — and
//! evaluated against three ordered rule lists.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod matcher;
pub mod mode;
pub mod policy;
pub mod resource;

pub use matcher::RuleMatcher;
pub use mode::PermissionMode;
pub use policy::{Decision, Policy, PolicyBuilder, Rule, Scope};
pub use resource::{Action, Resource};

#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error("malformed permission rule {rule:?}: {reason}")]
    MalformedRule { rule: String, reason: String },

    #[error("unknown permission action {0:?}")]
    UnknownAction(String),
}
