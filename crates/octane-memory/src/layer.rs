//! Where a memory file came from, and how that orders it.

use camino::Utf8PathBuf;

/// Precedence tier. Lower loads first, so later tiers refine earlier ones.
///
/// Ordering is by `#[derive(Ord)]` over declaration order — do not reorder these
/// variants without meaning to change precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    /// Org-managed. Present so an enterprise can state non-negotiables.
    Managed,
    /// `~/.octane/` — user preferences across all projects.
    User,
    /// Repository root.
    Project,
    /// A directory between the repo root and cwd. Ordered by depth so a
    /// subpackage's file refines the root's instead of replacing it.
    Directory { depth: usize },
    /// `OCTANE.local.md`. Last, so a developer's personal note wins.
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub layer: Layer,
    pub path: Utf8PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFile {
    pub origin: Origin,
    /// File contents with `@imports` already inlined.
    pub content: String,
    /// Files pulled in via `@path` references, for display and for explaining
    /// where an instruction came from.
    pub imported: Vec<Utf8PathBuf>,
}

impl MemoryFile {
    /// Rough token estimate, for the context budget.
    ///
    /// Deliberately crude — ~4 bytes per token. Memory is a small, fixed cost
    /// measured once at startup; paying for a real tokenizer here would buy
    /// precision that changes no decision.
    pub fn estimated_tokens(&self) -> usize {
        self.content.len().div_ceil(4)
    }
}
