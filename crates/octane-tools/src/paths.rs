//! Path resolution for the file tools.
//!
//! Every file tool resolves an argument to an absolute path the same way, and
//! that resolution has to be right for two independent reasons:
//!
//! 1. The resolved path is what the permission engine matches rules against. A
//!    path that normalizes differently from what the user wrote in their config
//!    means their rules silently do not apply.
//! 2. It is the first line of defence against traversal. It is *not* the last —
//!    the sandbox is — but a tool that hands `../../../../etc/passwd` to the
//!    policy engine as a workspace-relative path has already lost.
//!
//! **Symlinks are resolved where the file exists.** Lexical normalization alone
//! would let a symlink inside the workspace point anywhere, and the policy check
//! would pass on the lexical path while the OS opened something else. For files
//! that do not exist yet — the `write` case — the *parent* is canonicalized and
//! the filename appended, which closes the same hole without requiring the target
//! to exist.

use camino::{Utf8Path, Utf8PathBuf};

use crate::tool::ToolError;

/// A path argument resolved to an absolute, symlink-free location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// Absolute, canonical where possible. This is what permission rules match.
    pub absolute: Utf8PathBuf,
    /// Relative to the workspace when inside it, for display. Absolute otherwise.
    pub display: String,
    pub inside_workspace: bool,
}

impl ResolvedPath {
    /// The `read_file(...)` resource this path implies.
    pub fn read_resource(&self) -> String {
        format!("read_file({})", self.absolute)
    }

    /// The `write_file(...)` resource this path implies.
    pub fn write_resource(&self) -> String {
        format!("write_file({})", self.absolute)
    }
}

/// Resolve `input` against `cwd`, canonicalizing symlinks.
///
/// `must_exist` distinguishes read from write: a read of a missing file is an
/// error the model should hear about, whereas a write to a missing file is the
/// normal case and only its parent can be canonicalized.
pub fn resolve(
    input: &str,
    cwd: &Utf8Path,
    workspace: &Utf8Path,
    must_exist: bool,
) -> Result<ResolvedPath, ToolError> {
    if input.trim().is_empty() {
        return Err(ToolError::Recoverable("path must not be empty".into()));
    }

    let candidate = Utf8Path::new(input);
    let joined =
        if candidate.is_absolute() { candidate.to_owned() } else { cwd.join(candidate) };

    let absolute = if must_exist {
        canonicalize(&joined).ok_or_else(|| {
            ToolError::Recoverable(format!("{input}: no such file or directory"))
        })?
    } else {
        // Canonicalize the parent so a symlinked directory cannot be used to
        // escape, then re-attach the filename.
        let parent = joined.parent().unwrap_or(Utf8Path::new("/"));
        let file_name = joined.file_name().ok_or_else(|| {
            ToolError::Recoverable(format!("{input}: path has no file name"))
        })?;

        match canonicalize(parent) {
            Some(real_parent) => real_parent.join(file_name),
            // The parent does not exist either. Fall back to lexical
            // normalization so the caller gets a coherent error about the
            // directory rather than a confusing one about symlinks.
            None => normalize(&joined),
        }
    };

    let workspace_real = canonicalize(workspace).unwrap_or_else(|| normalize(workspace));
    let inside_workspace =
        absolute == workspace_real || absolute.starts_with(&workspace_real);

    let display = if inside_workspace {
        absolute
            .strip_prefix(&workspace_real)
            .map(|relative| relative.to_string())
            .unwrap_or_else(|_| absolute.to_string())
    } else {
        absolute.to_string()
    };

    Ok(ResolvedPath { absolute, display, inside_workspace })
}

fn canonicalize(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    Utf8PathBuf::from_path_buf(real).ok()
}

/// Lexical `.`/`..` resolution, without touching the filesystem.
pub fn normalize(path: &Utf8Path) -> Utf8PathBuf {
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

    fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(
            std::fs::canonicalize(dir.path()).expect("canonicalize"),
        )
        .expect("utf8");
        (dir, path)
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "hi").unwrap();

        let resolved = resolve("a.txt", &root, &root, true).unwrap();
        assert_eq!(resolved.absolute, root.join("a.txt"));
        assert_eq!(resolved.display, "a.txt");
        assert!(resolved.inside_workspace);
    }

    #[test]
    fn absolute_paths_are_taken_as_given() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "hi").unwrap();

        let resolved = resolve(root.join("a.txt").as_str(), &root, &root, true).unwrap();
        assert_eq!(resolved.absolute, root.join("a.txt"));
    }

    #[test]
    fn traversal_is_reported_as_outside_the_workspace() {
        let (_guard, root) = workspace();
        let nested = root.join("sub");
        std::fs::create_dir(&nested).unwrap();

        let resolved = resolve("../../..", &nested, &root, true).unwrap();
        assert!(!resolved.inside_workspace, "traversal must not be reported as inside");
        // And the display form is absolute, so a prompt cannot show a
        // reassuring-looking relative path for a file outside the project.
        assert!(resolved.display.starts_with('/'));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_detected() {
        let (_guard, root) = workspace();
        let (_outside_guard, outside) = workspace();
        std::fs::write(outside.join("secret.txt"), "s3cret").unwrap();

        // The classic escape: a link inside the project pointing out of it.
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("innocent.txt"))
            .unwrap();

        let resolved = resolve("innocent.txt", &root, &root, true).unwrap();

        assert!(
            !resolved.inside_workspace,
            "the symlink target is outside the workspace and must be treated as such"
        );
        assert_eq!(resolved.absolute, outside.join("secret.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlinked_directory_is_detected() {
        let (_guard, root) = workspace();
        let (_outside_guard, outside) = workspace();

        // Directory symlink, and the target file does not exist yet — the case
        // lexical normalization would miss.
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let resolved = resolve("link/new.txt", &root, &root, false).unwrap();

        assert!(!resolved.inside_workspace);
        assert_eq!(resolved.absolute, outside.join("new.txt"));
    }

    #[test]
    fn a_missing_file_is_an_error_for_reads_but_not_writes() {
        let (_guard, root) = workspace();

        assert!(resolve("nope.txt", &root, &root, true).is_err());

        let resolved = resolve("nope.txt", &root, &root, false).unwrap();
        assert_eq!(resolved.absolute, root.join("nope.txt"));
        assert!(resolved.inside_workspace);
    }

    #[test]
    fn empty_paths_are_rejected() {
        let (_guard, root) = workspace();
        assert!(resolve("   ", &root, &root, false).is_err());
    }

    #[test]
    fn resources_use_the_resolved_absolute_path() {
        let (_guard, root) = workspace();
        let resolved = resolve("a.txt", &root, &root, false).unwrap();

        // Not the input string: rules must match what will actually be opened.
        assert_eq!(resolved.read_resource(), format!("read_file({})", root.join("a.txt")));
        assert_eq!(resolved.write_resource(), format!("write_file({})", root.join("a.txt")));
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize(Utf8Path::new("/a/b/../c")), Utf8PathBuf::from("/a/c"));
        assert_eq!(normalize(Utf8Path::new("/a/./b/../../../x")), Utf8PathBuf::from("/x"));
    }
}
