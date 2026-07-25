//! Layered instruction files — the agent's standing knowledge of a project.
//!
//! Loaded once at session start, most general first, so a specific layer refines
//! a general one rather than fighting it:
//!
//! ```text
//! managed policy            org-wide, not user-editable
//! ~/.octane/OCTANE.md       user, every project
//! <repo root>/OCTANE.md     project, committed
//! <subdirs>/OCTANE.md       every level from repo root down to cwd
//! OCTANE.local.md           personal, gitignored
//! ```
//!
//! Two decisions worth stating:
//!
//! **Both `OCTANE.md` and `AGENTS.md` are read.** `AGENTS.md` is the emerging
//! cross-tool convention and nobody should maintain the same file twice under two
//! names. Where both exist at one level, both load, `AGENTS.md` first.
//!
//! **Memory is snapshotted, not re-read.** It lands in the cached prompt prefix,
//! so re-reading a file that changed mid-session would silently invalidate the
//! cache for every later turn. The prompt says it is a snapshot, matching what
//! Claude Code does with its directory tree and git status.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod imports;
pub mod layer;
pub mod loader;

pub use imports::resolve_imports;
pub use layer::{Layer, MemoryFile, Origin};
pub use loader::{MemorySnapshot, discover};

/// Filenames recognized at each level, in load order.
///
/// `AGENTS.md` first so a project's octane-specific file can refine the shared
/// one rather than being overridden by it.
pub const MEMORY_FILENAMES: &[&str] = &["AGENTS.md", "OCTANE.md"];

/// Personal overrides. Gitignored by convention, so this is where a developer
/// puts notes they do not want to commit.
pub const LOCAL_MEMORY_FILENAME: &str = "OCTANE.local.md";

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("reading {path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },

    #[error("import cycle detected: {0}")]
    ImportCycle(String),

    #[error("import depth limit ({limit}) exceeded at {path}")]
    ImportTooDeep { path: String, limit: usize },
}
