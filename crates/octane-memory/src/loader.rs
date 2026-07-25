//! Discovery: which files exist, in what order.

use camino::{Utf8Path, Utf8PathBuf};

use crate::imports::resolve_imports;
use crate::layer::{Layer, MemoryFile, Origin};
use crate::{LOCAL_MEMORY_FILENAME, MEMORY_FILENAMES, MemoryError};

/// Everything loaded at session start, ordered for prompt assembly.
#[derive(Debug, Clone, Default)]
pub struct MemorySnapshot {
    pub files: Vec<MemoryFile>,
}

impl MemorySnapshot {
    /// Render for the prompt.
    ///
    /// Each file is fenced with its path so the model can attribute an
    /// instruction to a source, and so a user asking "why did it do that?" can be
    /// pointed at a line in a file.
    pub fn render(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }

        let mut out = String::from(
            "The following instruction files were found for this project. \
             They are a snapshot taken at session start and will not update if \
             the files change.\n\n",
        );

        for file in &self.files {
            out.push_str(&format!(
                "<instructions path=\"{}\">\n{}\n</instructions>\n\n",
                file.origin.path,
                file.content.trim_end()
            ));
        }
        out
    }

    pub fn estimated_tokens(&self) -> usize {
        self.files.iter().map(MemoryFile::estimated_tokens).sum()
    }
}

/// Walk the layers and load what exists.
///
/// `repo_root` and `cwd` are passed in rather than discovered here: locating the
/// git root is the workspace crate's job, and this crate stays testable against
/// plain directories.
pub fn discover(
    user_config_dir: Option<&Utf8Path>,
    repo_root: &Utf8Path,
    cwd: &Utf8Path,
) -> Result<MemorySnapshot, MemoryError> {
    let mut files = Vec::new();

    // User layer.
    if let Some(dir) = user_config_dir {
        collect_from(dir, Layer::User, repo_root, &mut files)?;
    }

    // Project root.
    collect_from(repo_root, Layer::Project, repo_root, &mut files)?;

    // Every directory between the root and cwd, shallowest first.
    //
    // Codex aggregates this way and it is the right call for monorepos: a
    // subpackage adds its own context without having to restate the root's.
    for (depth, dir) in intermediate_dirs(repo_root, cwd).into_iter().enumerate() {
        collect_from(&dir, Layer::Directory { depth }, repo_root, &mut files)?;
    }

    // Local overrides last, so they win.
    if let Some(file) = load_file(&repo_root.join(LOCAL_MEMORY_FILENAME), Layer::Local, repo_root)? {
        files.push(file);
    }

    files.sort_by_key(|file| file.origin.layer);
    Ok(MemorySnapshot { files })
}

fn collect_from(
    dir: &Utf8Path,
    layer: Layer,
    root: &Utf8Path,
    out: &mut Vec<MemoryFile>,
) -> Result<(), MemoryError> {
    for name in MEMORY_FILENAMES {
        if let Some(file) = load_file(&dir.join(name), layer, root)? {
            out.push(file);
        }
    }
    Ok(())
}

fn load_file(
    path: &Utf8Path,
    layer: Layer,
    root: &Utf8Path,
) -> Result<Option<MemoryFile>, MemoryError> {
    if !path.is_file() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(path)
        .map_err(|source| MemoryError::Io { path: path.to_string(), source })?;

    let base_dir = path.parent().unwrap_or(root).to_owned();
    let mut imported = Vec::new();
    let content = resolve_imports(&raw, &base_dir, root, &mut imported)?;

    Ok(Some(MemoryFile {
        origin: Origin { layer, path: path.to_owned() },
        content,
        imported,
    }))
}

/// Directories strictly between `root` and `cwd`, plus `cwd` itself.
///
/// Returns nothing when `cwd` is outside `root` — a caller in an unrelated
/// directory should not pull in that directory's instruction files.
fn intermediate_dirs(root: &Utf8Path, cwd: &Utf8Path) -> Vec<Utf8PathBuf> {
    let Ok(relative) = cwd.strip_prefix(root) else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    let mut current = root.to_owned();
    for component in relative.components() {
        current = current.join(component.as_str());
        dirs.push(current.clone());
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_order_general_before_specific() {
        let mut layers = vec![
            Layer::Local,
            Layer::Directory { depth: 1 },
            Layer::Project,
            Layer::User,
            Layer::Managed,
            Layer::Directory { depth: 0 },
        ];
        layers.sort();
        assert_eq!(
            layers,
            vec![
                Layer::Managed,
                Layer::User,
                Layer::Project,
                Layer::Directory { depth: 0 },
                Layer::Directory { depth: 1 },
                Layer::Local,
            ]
        );
    }

    #[test]
    fn intermediate_dirs_walk_root_to_cwd() {
        let dirs = intermediate_dirs(Utf8Path::new("/repo"), Utf8Path::new("/repo/a/b"));
        assert_eq!(dirs, vec![Utf8PathBuf::from("/repo/a"), Utf8PathBuf::from("/repo/a/b")]);
    }

    #[test]
    fn cwd_at_root_contributes_no_extra_dirs() {
        assert!(intermediate_dirs(Utf8Path::new("/repo"), Utf8Path::new("/repo")).is_empty());
    }

    #[test]
    fn cwd_outside_root_contributes_nothing() {
        assert!(intermediate_dirs(Utf8Path::new("/repo"), Utf8Path::new("/elsewhere")).is_empty());
    }

    #[test]
    fn empty_snapshot_renders_to_nothing() {
        assert!(MemorySnapshot::default().render().is_empty());
    }

    #[test]
    fn render_fences_each_file_with_its_path() {
        let snapshot = MemorySnapshot {
            files: vec![MemoryFile {
                origin: Origin { layer: Layer::Project, path: "/repo/OCTANE.md".into() },
                content: "Use tabs.".into(),
                imported: Vec::new(),
            }],
        };
        let rendered = snapshot.render();
        assert!(rendered.contains(r#"<instructions path="/repo/OCTANE.md">"#));
        assert!(rendered.contains("Use tabs."));
        // The snapshot caveat must be stated, or the model will assume it is live.
        assert!(rendered.contains("snapshot"));
    }
}
