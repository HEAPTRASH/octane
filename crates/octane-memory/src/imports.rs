//! `@path` imports.
//!
//! Lets a memory file point at documentation instead of inlining it, so
//! `OCTANE.md` stays a short index over `@docs/architecture.md` rather than
//! growing into a copy of the docs.
//!
//! Bounded on three axes because this is recursive file inclusion driven by
//! repository contents: depth, cycles, and escaping the root. All three are
//! reachable by accident, and the third is reachable on purpose.

use std::collections::HashSet;

use camino::{Utf8Path, Utf8PathBuf};

use crate::MemoryError;

/// Nesting limit. Deep import chains make it impossible for a user to reason
/// about what the agent was actually told.
pub const MAX_IMPORT_DEPTH: usize = 3;

/// Inline every `@path` reference in `content`.
///
/// Resolution is relative to the importing file's directory. Imports that escape
/// `root`, do not exist, or are not files are left as literal text: a broken
/// import should be visible in the prompt, not silently dropped, or the user will
/// wonder why their instructions had no effect.
pub fn resolve_imports(
    content: &str,
    base_dir: &Utf8Path,
    root: &Utf8Path,
    imported: &mut Vec<Utf8PathBuf>,
) -> Result<String, MemoryError> {
    let mut seen = HashSet::new();
    resolve_recursive(content, base_dir, root, 0, &mut seen, imported)
}

fn resolve_recursive(
    content: &str,
    base_dir: &Utf8Path,
    root: &Utf8Path,
    depth: usize,
    seen: &mut HashSet<Utf8PathBuf>,
    imported: &mut Vec<Utf8PathBuf>,
) -> Result<String, MemoryError> {
    let mut out = String::with_capacity(content.len());

    for line in content.lines() {
        match parse_import(line) {
            Some(target) => {
                let resolved = normalize(&base_dir.join(target));

                // Escaping the root would let a repository read arbitrary files
                // from the developer's machine into the model's context.
                let escapes = !resolved.starts_with(root);
                if escapes || depth >= MAX_IMPORT_DEPTH || seen.contains(&resolved) {
                    // Preserved verbatim so the failure is visible.
                    out.push_str(line);
                    out.push('\n');
                    continue;
                }

                let Ok(nested) = std::fs::read_to_string(&resolved) else {
                    out.push_str(line);
                    out.push('\n');
                    continue;
                };

                seen.insert(resolved.clone());
                imported.push(resolved.clone());

                let nested_dir = resolved.parent().unwrap_or(root).to_owned();
                let expanded =
                    resolve_recursive(&nested, &nested_dir, root, depth + 1, seen, imported)?;

                // Fenced with provenance so the model can tell imported material
                // from the importing file's own instructions.
                out.push_str(&format!(
                    "<imported path=\"{resolved}\">\n{}\n</imported>\n",
                    expanded.trim_end()
                ));
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    Ok(out)
}

/// Recognize a line that is *only* an `@path` reference.
///
/// Requiring the whole line keeps `@` usable in prose — an instruction that
/// mentions `@octane` or an email address must not trigger a file read.
fn parse_import(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let target = trimmed.strip_prefix('@')?;

    if target.is_empty() || target.contains(char::is_whitespace) {
        return None;
    }
    // Absolute paths are not imports; they are almost always prose about a path.
    if target.starts_with('/') {
        return None;
    }
    Some(target)
}

/// Lexically resolve `.` and `..` without touching the filesystem.
///
/// Must be lexical: `std::fs::canonicalize` follows symlinks, so a symlinked
/// `docs` directory could resolve outside the root *after* the containment check
/// passed.
fn normalize(path: &Utf8Path) -> Utf8PathBuf {
    let mut out = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            camino::Utf8Component::ParentDir => {
                out.pop();
            }
            camino::Utf8Component::CurDir => {}
            other => out.push(other.as_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_whole_line_references_are_imports() {
        assert_eq!(parse_import("@docs/arch.md"), Some("docs/arch.md"));
        assert_eq!(parse_import("  @docs/arch.md  "), Some("docs/arch.md"));
        // Prose must not trigger file reads.
        assert_eq!(parse_import("email me @ foo@bar.com"), None);
        assert_eq!(parse_import("see @docs/arch.md for details"), None);
        assert_eq!(parse_import("regular line"), None);
        assert_eq!(parse_import("@"), None);
        assert_eq!(parse_import("@/etc/passwd"), None);
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize(Utf8Path::new("/a/b/../c")), Utf8PathBuf::from("/a/c"));
        assert_eq!(normalize(Utf8Path::new("/a/./b")), Utf8PathBuf::from("/a/b"));
        assert_eq!(normalize(Utf8Path::new("/a/b/../../../etc")), Utf8PathBuf::from("/etc"));
    }

    #[test]
    fn traversal_out_of_root_is_left_inert() {
        let mut imported = Vec::new();
        let content = "@../../../../etc/passwd\n";
        let result = resolve_imports(
            content,
            Utf8Path::new("/proj/sub"),
            Utf8Path::new("/proj"),
            &mut imported,
        )
        .unwrap();

        // Left as literal text, and nothing was read.
        assert!(result.contains("@../../../../etc/passwd"));
        assert!(imported.is_empty());
    }

    #[test]
    fn missing_imports_stay_visible() {
        let mut imported = Vec::new();
        let result = resolve_imports(
            "@docs/nope.md\n",
            Utf8Path::new("/nonexistent"),
            Utf8Path::new("/nonexistent"),
            &mut imported,
        )
        .unwrap();
        assert!(result.contains("@docs/nope.md"));
        assert!(imported.is_empty());
    }

    #[test]
    fn prose_passes_through_unchanged() {
        let mut imported = Vec::new();
        let content = "# Project\n\nUse tabs.\n";
        let result =
            resolve_imports(content, Utf8Path::new("/p"), Utf8Path::new("/p"), &mut imported).unwrap();
        assert_eq!(result, content);
    }
}
