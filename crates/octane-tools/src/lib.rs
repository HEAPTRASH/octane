//! Tools: the agent's hands.
//!
//! Every harness surveyed in `RESEARCH.md` converged on roughly the same dozen
//! primitives, and each independently concluded that a small primitive set plus
//! a shell beats a large set of specialized wrappers. `bash` is the universal
//! adapter — it supplies git, cargo, docker, and everything else for free.
//!
//! What is *not* trivial is each tool's edges. `read` must reject binaries with a
//! useful message, cap line length, number lines, say that more remain, and
//! attach diagnostics. Those edges are the actual work.
//!
//! This crate owns the *interface and registry only*. Policy lives in
//! `octane-permission`, containment in `octane-sandbox`. A tool asks; it does not
//! decide.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod builtin;
pub mod paths;
pub mod registry;
pub mod tool;
pub mod tracker;

#[cfg(test)]
mod tests_support;

pub use builtin::{BashTool, EditTool, ReadTool, WriteTool, register_all};
pub use paths::{ResolvedPath, resolve};
pub use registry::ToolRegistry;
pub use tool::{Tool, ToolContext, ToolError, ToolOutcome};
pub use tracker::{FileTracker, WriteCheck};

/// The built-in surface, named up front so its shape is visible before it exists.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    // files
    "read", "write", "edit", "multi_edit",
    // search
    "glob", "grep", "list",
    // execution
    "bash",
    // planning: a per-session scratchpad. Trivial to build, and the single
    // highest-leverage tool against context rot.
    "todo_write", "todo_read",
    // delegation: description is the generated list of available agents.
    "task",
    // knowledge
    "web_fetch", "web_search",
    // code intelligence
    "lsp_diagnostics", "lsp_definition", "lsp_references",
    // interaction: lets the model stop guessing and ask.
    "question",
];
