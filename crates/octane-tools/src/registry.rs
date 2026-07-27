//! Tool registry.
//!
//! Built-in tools, project-local tools, and MCP tools all land here. The model
//! cannot tell them apart, and — more importantly — they all pass through the
//! same permission engine. That uniformity is what makes adding a third-party
//! MCP server a bounded decision rather than a leap of faith.

use std::collections::BTreeMap;
use std::sync::Arc;

use octane_provider::model::ToolSchema;

use crate::tool::Tool;

/// `BTreeMap`, not `HashMap`, and the choice is load-bearing.
///
/// Tool schemas sit inside the cached prompt prefix. Hash-map iteration order
/// varies between runs, which reorders the schemas and invalidates the cache on
/// every turn — Codex shipped exactly this bug with MCP tools. Sorted order makes
/// that failure impossible by construction rather than by discipline.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Later registrations win, which is what lets a project-local tool override
    /// a built-in of the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Schemas for the tools this agent may use, in stable order.
    ///
    /// Filtering happens here rather than at call time on purpose: a tool the
    /// model can see is a tool it will eventually try, and the refusal it then
    /// has to reason around is wasted context. A `plan` agent should never be
    /// *shown* `write`.
    pub fn schemas_where(&self, allowed: impl Fn(&dyn Tool) -> bool) -> Vec<ToolSchema> {
        self.tools
            .values()
            .filter(|tool| allowed(tool.as_ref()))
            .map(|tool| tool.schema())
            .collect()
    }
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}
