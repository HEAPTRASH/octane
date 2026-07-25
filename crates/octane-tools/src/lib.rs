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
pub mod content;
pub mod paths;
pub mod registry;
pub mod tool;
pub mod tracker;
pub mod walk;

#[cfg(test)]
mod tests_support;

pub use builtin::{
    BashTool, EditTool, GlobTool, GrepTool, ListTool, ReadTool, WriteTool, register_all,
};
pub use paths::{ResolvedPath, resolve};
pub use registry::ToolRegistry;
pub use tool::{Tool, ToolContext, ToolError, ToolOutcome};
pub use tracker::{FileTracker, WriteCheck};
