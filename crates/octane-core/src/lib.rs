//! The ReAct loop and session orchestration.
//!
//! This crate coordinates and holds **no domain logic**. Policy lives in
//! `octane-permission`, containment in `octane-sandbox`, token accounting in
//! `octane-context`, tool behaviour in `octane-tools`. The loop's only job is to
//! call them in the right order and honour what they return.
//!
//! That line is the SRP boundary, and it is also what makes the loop testable
//! without a model: every collaborator is a trait, so a test can assert
//! "a denied permission ends the turn" without a network call.
//!
//! ```text
//! 1. assemble prompt      static-first, order-stable        [octane-context]
//! 2. stream inference     normalized StreamEvents           [octane-provider]
//! 3. per tool call:
//!      a. registry lookup                                   [octane-tools]
//!      b. policy decision (allow / ask / deny)               [octane-permission]
//!      c. wrap in sandbox, execute                           [octane-sandbox]
//!      d. persist, publish event
//! 4. evaluate stop conditions                                [this crate]
//! ```

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod events;
pub mod model_source;
pub mod loop_guard;
pub mod prompt;
pub mod step;
pub mod task;
pub mod turn;

pub use agent::{Agent, AgentKind};
pub use events::EventSink;
pub use model_source::ModelStepSource;
pub use loop_guard::LoopGuard;
pub use prompt::{BASE_INSTRUCTIONS, PromptAssembler, mode_switch_notice};
pub use step::{StepDecision, StopReason};
pub use task::{Delegate, TaskTool};
pub use turn::{Approver, TurnRunner};

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Provider(#[from] octane_provider::ProviderError),

    #[error(transparent)]
    Permission(#[from] octane_permission::PermissionError),

    #[error(transparent)]
    Context(#[from] octane_context::ContextError),

    #[error("no tool named {0:?} is registered")]
    UnknownTool(String),

    #[error("turn cancelled")]
    Cancelled,
}
