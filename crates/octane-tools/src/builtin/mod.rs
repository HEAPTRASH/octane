//! The built-in tool set.
//!
//! A small set of primitives plus a shell, which every harness surveyed
//! independently converged on (`RESEARCH.md` §3). `bash` is the universal
//! adapter: it supplies git, cargo, docker, and everything else for free, so
//! there is no reason to wrap any of them.
//!
//! What takes the work is each tool's edges — binary detection, line limits,
//! staleness checks, output truncation, and error messages phrased so the model
//! can act on them. Those are what these modules are mostly made of.

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod list;
pub mod read;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list::ListTool;
pub use read::ReadTool;
pub use write::WriteTool;

/// Register every built-in tool against a shared tracker and sandbox policy.
///
/// The tracker must be shared: `read` records into it and `write`/`edit` check it,
/// so handing each tool its own would silently disable the read-before-write
/// guarantee.
pub fn register_all(
    registry: &mut crate::ToolRegistry,
    tracker: std::sync::Arc<crate::tracker::FileTracker>,
    sandbox: octane_sandbox::SandboxPolicy,
) {
    registry.register(std::sync::Arc::new(ReadTool::new(tracker.clone())));
    registry.register(std::sync::Arc::new(WriteTool::new(tracker.clone())));
    registry.register(std::sync::Arc::new(EditTool::new(tracker)));
    registry.register(std::sync::Arc::new(BashTool::new(sandbox)));

    // Search tools are stateless: they read the filesystem directly and hold
    // nothing between calls.
    registry.register(std::sync::Arc::new(GlobTool::new()));
    registry.register(std::sync::Arc::new(GrepTool::new()));
    registry.register(std::sync::Arc::new(ListTool::new()));
}
