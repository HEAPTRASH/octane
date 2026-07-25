//! Permission modes: presets over the same engine, not a parallel code path.
//!
//! Every harness surveyed ships roughly these four, cycled with Shift+Tab. The
//! important design property is that a mode can only *narrow* or *skip prompts* —
//! it can never override a deny rule. A "bypass" that also bypasses denies is
//! indistinguishable from having no policy.

use serde::{Deserialize, Serialize};

use crate::policy::Decision;
use crate::resource::{Action, Resource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Ask before anything mutating. The safe default.
    #[default]
    Default,
    /// Read-only. Investigate and propose; change nothing.
    Plan,
    /// Auto-approve file edits, still ask for commands. The mode people actually
    /// live in once they trust the agent's edits but not its shell.
    AcceptEdits,
    /// Skip prompts. Deny rules and the sandbox still apply.
    Bypass,
}

impl PermissionMode {
    /// What the next Shift+Tab produces.
    pub fn cycle(self) -> Self {
        match self {
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Default,
            // Bypass is opt-in explicitly and never entered by cycling.
            Self::Bypass => Self::Default,
        }
    }

    /// Short label for the status line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::AcceptEdits => "accept-edits",
            Self::Bypass => "bypass",
        }
    }

    /// A decision this mode imposes regardless of the rule lists.
    ///
    /// Called *after* deny rules are evaluated, so returning `Allow` here can
    /// never resurrect a denied operation.
    pub fn force(self, resource: &Resource) -> Option<Decision> {
        match self {
            Self::Default => None,

            // Read-only means read-only: refuse rather than ask, so the model
            // gets an unambiguous signal instead of pestering the user for a
            // write it was told not to attempt.
            Self::Plan => match resource.action {
                Action::WriteFile | Action::Command | Action::ExecuteUrl | Action::Unsandboxed => {
                    Some(Decision::Deny)
                }
                Action::ReadFile | Action::ReadUrl | Action::Mcp => None,
            },

            // Edits stop prompting; commands still do. `unsandboxed` is
            // deliberately excluded — escaping containment is never an "edit".
            Self::AcceptEdits => match resource.action {
                Action::WriteFile | Action::ReadFile => Some(Decision::Allow),
                _ => None,
            },

            Self::Bypass => Some(Decision::Allow),
        }
    }

    /// Whether subagents inherit this mode.
    ///
    /// They must, for `accept-edits`: Antigravity found that background agents
    /// which do not inherit it silently queue writes for approval the user never
    /// sees. `plan` inherits for the obvious reason.
    pub fn inherited_by_subagents(self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_visits_the_three_safe_modes() {
        let mut mode = PermissionMode::Default;
        let mut seen = vec![mode];
        for _ in 0..2 {
            mode = mode.cycle();
            seen.push(mode);
        }
        assert_eq!(seen, vec![PermissionMode::Default, PermissionMode::AcceptEdits, PermissionMode::Plan]);
        assert_eq!(mode.cycle(), PermissionMode::Default);
    }

    #[test]
    fn cycling_never_reaches_bypass() {
        let mut mode = PermissionMode::Default;
        for _ in 0..10 {
            mode = mode.cycle();
            assert_ne!(mode, PermissionMode::Bypass);
        }
    }

    #[test]
    fn plan_denies_mutation_but_permits_reads() {
        let plan = PermissionMode::Plan;
        assert_eq!(plan.force(&Resource::write_file("/p/f")), Some(Decision::Deny));
        assert_eq!(plan.force(&Resource::command("ls")), Some(Decision::Deny));
        assert_eq!(plan.force(&Resource::read_file("/p/f")), None);
    }

    #[test]
    fn accept_edits_does_not_cover_commands_or_escapes() {
        let mode = PermissionMode::AcceptEdits;
        assert_eq!(mode.force(&Resource::write_file("/p/f")), Some(Decision::Allow));
        assert_eq!(mode.force(&Resource::command("cargo test")), None);
        assert_eq!(mode.force(&Resource::new(Action::Unsandboxed, "git push")), None);
    }
}
