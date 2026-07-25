//! Directory walking, shared by `glob`, `grep`, and `list`.
//!
//! Built on `ignore`, the crate ripgrep itself uses, so `.gitignore` semantics are
//! the ones developers already expect rather than an approximation. That matters
//! more than it sounds: a search that returns hits from `target/` or
//! `node_modules/` buries the real answer and burns the context window on build
//! artifacts.
//!
//! Every walk is bounded. An agent pointed at `/` should get a truncated answer
//! and a clear note, not an unbounded traversal.

use camino::{Utf8Path, Utf8PathBuf};

/// How to traverse.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    /// Stop after this many entries. Truncation is always reported.
    pub limit: usize,
    /// Maximum directory depth; `None` for unlimited.
    pub max_depth: Option<usize>,
    /// Honour `.gitignore`, `.ignore`, and the global gitignore.
    pub respect_ignore_files: bool,
    /// Include dotfiles.
    pub include_hidden: bool,
    /// Yield directories as well as files.
    pub include_dirs: bool,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            limit: 1_000,
            max_depth: None,
            // On by default: build output and vendored dependencies are noise in
            // every case these tools are used for.
            respect_ignore_files: true,
            include_hidden: false,
            include_dirs: false,
        }
    }
}

/// One entry found by a walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: Utf8PathBuf,
    pub is_dir: bool,
    /// Modification time, used to sort by recency.
    pub modified: Option<std::time::SystemTime>,
    pub depth: usize,
}

/// Result of a bounded walk.
#[derive(Debug, Clone)]
pub struct WalkResult {
    pub entries: Vec<Entry>,
    /// True when the limit stopped the walk early, so callers can say so rather
    /// than presenting a partial list as complete.
    pub truncated: bool,
}

/// Walk `root`, applying `options` and an optional per-entry filter.
///
/// `.git` is always skipped regardless of ignore settings: it is never what
/// anyone is searching for, and it is large.
pub fn walk(
    root: &Utf8Path,
    options: &WalkOptions,
    mut accept: impl FnMut(&Utf8Path, bool) -> bool,
) -> WalkResult {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!options.include_hidden)
        .git_ignore(options.respect_ignore_files)
        .git_global(options.respect_ignore_files)
        .git_exclude(options.respect_ignore_files)
        .ignore(options.respect_ignore_files)
        .parents(options.respect_ignore_files)
        // Apply `.gitignore` even when the directory is not a git repository.
        //
        // `ignore` defaults this to `true` to match ripgrep's CLI behaviour, but
        // the reasoning does not carry over: an agent gets pointed at extracted
        // archives, vendored subtrees, and worktrees that have a `.gitignore` and
        // no `.git`. Honouring the file only *sometimes* means search results
        // quietly fill with build artifacts in exactly those cases, which reads as
        // the tool being bad rather than as a configuration subtlety.
        .require_git(false)
        // Symlinked directories are not followed: a link pointing at `/` or back
        // into an ancestor turns a bounded walk into an unbounded one.
        .follow_links(false);

    if let Some(depth) = options.max_depth {
        builder.max_depth(Some(depth));
    }
    builder.filter_entry(|entry| entry.file_name() != ".git");

    let mut entries = Vec::new();
    let mut truncated = false;

    for result in builder.build() {
        if entries.len() >= options.limit {
            truncated = true;
            break;
        }

        // Unreadable directories are skipped rather than failing the walk: one
        // permission-denied subdirectory should not void an otherwise good answer.
        let Ok(entry) = result else { continue };
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path().to_path_buf()) else {
            continue;
        };

        let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        let depth = entry.depth();

        // The root itself is not a result.
        if depth == 0 {
            continue;
        }
        if is_dir && !options.include_dirs {
            continue;
        }
        if !accept(&path, is_dir) {
            continue;
        }

        let modified = entry.metadata().ok().and_then(|meta| meta.modified().ok());
        entries.push(Entry { path, is_dir, modified, depth });
    }

    WalkResult { entries, truncated }
}

/// Sort newest first.
///
/// Recency is the cheapest available relevance signal: the file someone touched
/// an hour ago is far likelier to be the one in question than one untouched for
/// two years. Files without an mtime sort last.
pub fn sort_by_recency(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            // Deterministic tiebreak, so identical mtimes do not reorder between
            // runs and make output non-reproducible.
            .then_with(|| a.path.cmp(&b.path))
    });
}

/// Render a path relative to `root` when it is inside it.
pub fn display_path(path: &Utf8Path, root: &Utf8Path) -> String {
    path.strip_prefix(root).map(ToString::to_string).unwrap_or_else(|_| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::workspace;

    fn touch(root: &Utf8Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn names(result: &WalkResult, root: &Utf8Path) -> Vec<String> {
        let mut out: Vec<String> =
            result.entries.iter().map(|entry| display_path(&entry.path, root)).collect();
        out.sort();
        out
    }

    #[test]
    fn finds_files_recursively() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs", "");
        touch(&root, "src/b.rs", "");
        touch(&root, "src/deep/c.rs", "");

        let result = walk(&root, &WalkOptions::default(), |_, _| true);
        assert_eq!(names(&result, &root), vec!["a.rs", "src/b.rs", "src/deep/c.rs"]);
        assert!(!result.truncated);
    }

    #[test]
    fn gitignored_paths_are_skipped() {
        let (_guard, root) = workspace();
        touch(&root, ".gitignore", "target/\n*.log\n");
        touch(&root, "keep.rs", "");
        touch(&root, "noisy.log", "");
        touch(&root, "target/build-artifact.rs", "");

        let result = walk(&root, &WalkOptions::default(), |_, _| true);
        let found = names(&result, &root);

        assert!(found.contains(&"keep.rs".to_string()));
        assert!(!found.contains(&"noisy.log".to_string()));
        assert!(!found.iter().any(|p| p.starts_with("target/")));
    }

    #[test]
    fn ignore_files_can_be_bypassed() {
        let (_guard, root) = workspace();
        touch(&root, ".gitignore", "*.log\n");
        touch(&root, "noisy.log", "");

        let options = WalkOptions { respect_ignore_files: false, ..Default::default() };
        let result = walk(&root, &options, |_, _| true);
        assert!(names(&result, &root).contains(&"noisy.log".to_string()));
    }

    #[test]
    fn git_internals_are_never_walked() {
        let (_guard, root) = workspace();
        touch(&root, ".git/config", "[core]\n");
        touch(&root, "a.rs", "");

        // Even with hidden files enabled and ignore files off.
        let options = WalkOptions {
            include_hidden: true,
            respect_ignore_files: false,
            ..Default::default()
        };
        let result = walk(&root, &options, |_, _| true);
        assert!(!names(&result, &root).iter().any(|p| p.starts_with(".git/")));
    }

    #[test]
    fn hidden_files_are_excluded_by_default_and_includable() {
        let (_guard, root) = workspace();
        touch(&root, ".env", "SECRET=1");
        touch(&root, "a.rs", "");

        let hidden_off = walk(&root, &WalkOptions::default(), |_, _| true);
        assert!(!names(&hidden_off, &root).contains(&".env".to_string()));

        let options = WalkOptions { include_hidden: true, ..Default::default() };
        let hidden_on = walk(&root, &options, |_, _| true);
        assert!(names(&hidden_on, &root).contains(&".env".to_string()));
    }

    #[test]
    fn the_limit_is_enforced_and_reported() {
        let (_guard, root) = workspace();
        for i in 0..20 {
            touch(&root, &format!("f{i}.rs"), "");
        }

        let options = WalkOptions { limit: 5, ..Default::default() };
        let result = walk(&root, &options, |_, _| true);

        assert_eq!(result.entries.len(), 5);
        assert!(result.truncated, "a partial list must not be presented as complete");
    }

    #[test]
    fn depth_can_be_bounded() {
        let (_guard, root) = workspace();
        touch(&root, "top.rs", "");
        touch(&root, "a/mid.rs", "");
        touch(&root, "a/b/deep.rs", "");

        let options = WalkOptions { max_depth: Some(1), ..Default::default() };
        let result = walk(&root, &options, |_, _| true);
        assert_eq!(names(&result, &root), vec!["top.rs"]);
    }

    #[test]
    fn directories_are_excluded_unless_asked_for() {
        let (_guard, root) = workspace();
        touch(&root, "sub/a.rs", "");

        let files_only = walk(&root, &WalkOptions::default(), |_, _| true);
        assert!(files_only.entries.iter().all(|entry| !entry.is_dir));

        let options = WalkOptions { include_dirs: true, ..Default::default() };
        let with_dirs = walk(&root, &options, |_, _| true);
        assert!(with_dirs.entries.iter().any(|entry| entry.is_dir));
    }

    #[test]
    fn the_filter_is_applied() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs", "");
        touch(&root, "b.txt", "");

        let result = walk(&root, &WalkOptions::default(), |path, _| {
            path.extension() == Some("rs")
        });
        assert_eq!(names(&result, &root), vec!["a.rs"]);
    }

    #[test]
    fn recency_sorting_is_newest_first_and_deterministic() {
        let now = std::time::SystemTime::now();
        let older = now - std::time::Duration::from_secs(3_600);

        let entry = |name: &str, modified| Entry {
            path: Utf8PathBuf::from(name),
            is_dir: false,
            modified,
            depth: 1,
        };

        let mut entries = vec![
            entry("old.rs", Some(older)),
            entry("unknown.rs", None),
            entry("new.rs", Some(now)),
            // Same mtime as new.rs: the tiebreak must be stable.
            entry("also-new.rs", Some(now)),
        ];
        sort_by_recency(&mut entries);

        let order: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(order, vec!["also-new.rs", "new.rs", "old.rs", "unknown.rs"]);
    }

    #[test]
    fn display_paths_are_relative_when_inside_the_root() {
        let root = Utf8Path::new("/repo");
        assert_eq!(display_path(Utf8Path::new("/repo/src/a.rs"), root), "src/a.rs");
        assert_eq!(display_path(Utf8Path::new("/elsewhere/a.rs"), root), "/elsewhere/a.rs");
    }
}
