//! Agent definitions — which model, which tools, which prompt.
//!
//! Two tiers, matching every harness surveyed. Primary agents are what the user
//! drives; subagents are delegation targets invoked through the `task` tool with
//! their own context window.

use octane_permission::PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// User-facing. Cycled with Shift+Tab.
    Primary,
    /// Invoked via `task`, runs in its own context.
    Subagent,
    /// Internal, never user-selectable: compaction, title generation.
    Hidden,
}

#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    /// Shown to the model in the `task` tool's description, and it is the only
    /// thing the model uses to pick a subagent — so it must say what this agent
    /// is *for*, not what it is.
    pub description: String,
    pub kind: AgentKind,
    /// Overrides the base system prompt when set.
    pub prompt: Option<String>,
    /// Overrides the session model when set — a cheap model for mechanical work,
    /// a stronger one for review.
    pub model: Option<String>,
    /// Tool names this agent may use. Empty means all.
    ///
    /// Enforced by *omitting the schema from the prompt*, not by refusing at call
    /// time: a tool the model can see is a tool it will try, and the resulting
    /// refusal is context spent on nothing.
    pub allowed_tools: Vec<String>,
    /// Baseline mode. `plan` agents pin themselves read-only here.
    pub mode: PermissionMode,
}

impl Agent {
    /// Full tool access, ask before mutating. The default.
    pub fn build() -> Self {
        Self {
            name: "build".into(),
            description: "Implements changes. Full tool access.".into(),
            kind: AgentKind::Primary,
            prompt: None,
            model: None,
            allowed_tools: Vec::new(),
            mode: PermissionMode::Default,
        }
    }

    /// Read-only investigation and planning.
    ///
    /// Restricted by mode rather than by tool list, so the restriction holds even
    /// for tools added later — a new mutating tool is covered automatically
    /// instead of needing to be remembered.
    pub fn plan() -> Self {
        Self {
            name: "plan".into(),
            description: "Investigates and plans without changing anything.".into(),
            kind: AgentKind::Primary,
            prompt: None,
            model: None,
            allowed_tools: Vec::new(),
            mode: PermissionMode::Plan,
        }
    }

    /// Read-only codebase search, for delegation.
    ///
    /// The highest-value subagent: a sweep that would cost 40k tokens in the main
    /// context returns as a short summary instead.
    pub fn explore() -> Self {
        Self {
            name: "explore".into(),
            description: "Searches the codebase and reports findings. Read-only. \
                          Use when locating code across many files would fill the \
                          main context."
                .into(),
            kind: AgentKind::Subagent,
            prompt: None,
            model: None,
            allowed_tools: vec![
                "read".into(),
                "glob".into(),
                "grep".into(),
                "list".into(),
                "lsp_definition".into(),
                "lsp_references".into(),
            ],
            mode: PermissionMode::Plan,
        }
    }

    pub fn permits_tool(&self, tool: &str) -> bool {
        self.allowed_tools.is_empty() || self.allowed_tools.iter().any(|name| name == tool)
    }

    /// Whether this agent may delegate.
    ///
    /// Subagents may not: unbounded recursion is a spend risk with no upside, and
    /// every harness surveyed draws the line here.
    pub fn can_delegate(&self) -> bool {
        matches!(self.kind, AgentKind::Primary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allow_list_means_everything() {
        assert!(Agent::build().permits_tool("bash"));
        assert!(Agent::build().permits_tool("anything"));
    }

    #[test]
    fn explore_is_restricted_to_read_only_tools() {
        let explore = Agent::explore();
        assert!(explore.permits_tool("grep"));
        assert!(!explore.permits_tool("write"));
        assert!(!explore.permits_tool("bash"));
    }

    #[test]
    fn plan_restricts_by_mode_so_new_tools_are_covered() {
        let plan = Agent::plan();
        // The tool list is open...
        assert!(plan.permits_tool("some_future_mutating_tool"));
        // ...but the mode denies mutation regardless.
        use octane_permission::Resource;
        assert_eq!(
            plan.mode.force(&Resource::write_file("/p/f")),
            Some(octane_permission::Decision::Deny)
        );
    }

    #[test]
    fn subagents_cannot_delegate_further() {
        assert!(Agent::build().can_delegate());
        assert!(Agent::plan().can_delegate());
        assert!(!Agent::explore().can_delegate());
    }
}
