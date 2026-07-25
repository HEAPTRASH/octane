//! `list` — see the shape of a directory.

use async_trait::async_trait;
use serde::Deserialize;

use crate::paths;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};
use crate::walk::{self, WalkOptions};

/// Entries shown before truncating.
pub const MAX_ENTRIES: usize = 400;

/// Depth shown when the caller does not ask for one.
///
/// Two levels is enough to see how a project is organized without paying for a
/// full recursive listing, which is almost never what was wanted.
pub const DEFAULT_DEPTH: usize = 2;

const DESCRIPTION: &str = "\
Lists the contents of a directory as a tree.

Usage:
- `path` defaults to the working directory.
- `depth` defaults to 2. Increase it deliberately; a deep listing of a large
  project is mostly noise.
- Directories are shown with a trailing `/`.
- Entries ignored by .gitignore are skipped, so build output and vendored
  dependencies do not appear.
- Use `glob` when looking for files by name and `grep` when looking for
  contents. This tool is for orienting yourself in an unfamiliar directory.";

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    depth: Option<usize>,
    #[serde(default)]
    include_ignored: bool,
    #[serde(default)]
    include_hidden: bool,
}

#[derive(Debug, Default)]
pub struct ListTool;

impl ListTool {
    pub fn new() -> Self {
        Self
    }

    fn root(
        &self,
        parsed: &Input,
        ctx: &ToolContext,
    ) -> Result<camino::Utf8PathBuf, ToolError> {
        match parsed.path.as_deref() {
            Some(path) => {
                let resolved = paths::resolve(path, &ctx.cwd, &ctx.workspace, true)?;
                if !resolved.absolute.is_dir() {
                    return Err(ToolError::Recoverable(format!(
                        "{} is a file, not a directory. Use `read` to see its contents.",
                        resolved.display
                    )));
                }
                Ok(resolved.absolute)
            }
            None => Ok(ctx.cwd.clone()),
        }
    }
}

#[async_trait]
impl Tool for ListTool {
    fn name(&self) -> &str {
        "list"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list. Defaults to the working directory." },
                "depth": { "type": "integer", "description": "How many levels to descend. Defaults to 2." },
                "include_ignored": { "type": "boolean", "description": "Also show paths excluded by .gitignore. Defaults to false." },
                "include_hidden": { "type": "boolean", "description": "Also show dotfiles. Defaults to false." }
            },
            "additionalProperties": false
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    fn required_permissions(&self, input: &str, ctx: &ToolContext) -> Vec<String> {
        match serde_json::from_str::<Input>(input) {
            Ok(parsed) => match self.root(&parsed, ctx) {
                Ok(root) => vec![format!("read_file({root})")],
                Err(_) => Vec::new(),
            },
            Err(_) => Vec::new(),
        }
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let parsed: Input = serde_json::from_str(input)
            .map_err(|error| ToolError::Recoverable(format!("invalid arguments: {error}")))?;

        let root = self.root(&parsed, ctx)?;
        let depth = parsed.depth.unwrap_or(DEFAULT_DEPTH).max(1);

        let options = WalkOptions {
            limit: MAX_ENTRIES,
            max_depth: Some(depth),
            respect_ignore_files: !parsed.include_ignored,
            include_hidden: parsed.include_hidden,
            include_dirs: true,
        };

        let result = walk::walk(&root, &options, |_, _| true);

        if result.entries.is_empty() {
            return Ok(ToolOutcome::new(
                walk::display_path(&root, &ctx.workspace),
                format!(
                    "{} is empty, or everything in it is excluded by .gitignore \
                     (set include_ignored) or hidden (set include_hidden).",
                    walk::display_path(&root, &ctx.workspace)
                ),
            ));
        }

        // Sorted by path so the tree reads in a stable, predictable order.
        // Recency sorting would be actively confusing here: a tree whose branches
        // move between calls is harder to reason about than an alphabetical one.
        let mut entries = result.entries;
        entries.sort_by(|a, b| a.path.cmp(&b.path));

        let root_display = walk::display_path(&root, &ctx.workspace);
        let mut output = format!("{}/\n", if root_display.is_empty() { "." } else { &root_display });

        let mut files = 0usize;
        let mut dirs = 0usize;

        for entry in &entries {
            let name = entry.path.file_name().unwrap_or_default();
            let indent = "  ".repeat(entry.depth);
            if entry.is_dir {
                dirs += 1;
                output.push_str(&format!("{indent}{name}/\n"));
            } else {
                files += 1;
                output.push_str(&format!("{indent}{name}\n"));
            }
        }

        output.push_str(&format!(
            "\n{files} file{}, {dirs} director{}",
            if files == 1 { "" } else { "s" },
            if dirs == 1 { "y" } else { "ies" }
        ));

        if result.truncated {
            output.push_str(&format!(
                "\n\n[stopped at {MAX_ENTRIES} entries. List a subdirectory, or reduce \
                 `depth`, to see the rest.]"
            ));
        }

        Ok(ToolOutcome::new(
            format!("{root_display} ({files} files, {dirs} dirs)"),
            output,
        )
        .with_metadata(serde_json::json!({
            "root": root,
            "files": files,
            "directories": dirs,
            "truncated": result.truncated,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, workspace};

    fn touch(root: &camino::Utf8Path, relative: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "x").unwrap();
    }

    #[tokio::test]
    async fn shows_a_tree_with_directories_marked() {
        let (_guard, root) = workspace();
        touch(&root, "Cargo.toml");
        touch(&root, "src/main.rs");

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();

        assert!(outcome.output.contains("Cargo.toml"));
        assert!(outcome.output.contains("src/"), "directories need a trailing slash");
        assert!(outcome.output.contains("main.rs"));
    }

    #[tokio::test]
    async fn nesting_is_shown_by_indentation() {
        let (_guard, root) = workspace();
        touch(&root, "src/main.rs");

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();

        let src_line = outcome.output.lines().find(|l| l.contains("src/")).unwrap();
        let main_line = outcome.output.lines().find(|l| l.contains("main.rs")).unwrap();

        let indent = |line: &str| line.len() - line.trim_start().len();
        assert!(indent(main_line) > indent(src_line));
    }

    #[tokio::test]
    async fn depth_defaults_to_two_levels() {
        let (_guard, root) = workspace();
        touch(&root, "a/b/c/deep.rs");

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();

        assert!(outcome.output.contains("a/"));
        assert!(outcome.output.contains("b/"));
        // Level 3 is past the default.
        assert!(!outcome.output.contains("deep.rs"));
    }

    #[tokio::test]
    async fn depth_can_be_raised() {
        let (_guard, root) = workspace();
        touch(&root, "a/b/c/deep.rs");

        let outcome =
            ListTool::new().execute(r#"{"depth":4}"#, &context(&root)).await.unwrap();
        assert!(outcome.output.contains("deep.rs"));
    }

    #[tokio::test]
    async fn gitignored_entries_are_hidden_by_default() {
        let (_guard, root) = workspace();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        touch(&root, "src/a.rs");
        touch(&root, "target/gen.rs");

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();
        assert!(!outcome.output.contains("target"));

        let including = ListTool::new()
            .execute(r#"{"include_ignored":true}"#, &context(&root))
            .await
            .unwrap();
        assert!(including.output.contains("target/"));
    }

    #[tokio::test]
    async fn counts_are_reported() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs");
        touch(&root, "b.rs");
        touch(&root, "sub/c.rs");

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();
        assert!(outcome.output.contains("3 files, 1 directory"));
    }

    #[tokio::test]
    async fn a_file_path_points_at_the_right_tool() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs");

        let error = ListTool::new()
            .execute(r#"{"path":"a.rs"}"#, &context(&root))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not a directory"));
        assert!(error.to_string().contains("read"));
    }

    #[tokio::test]
    async fn an_empty_directory_explains_the_possibilities() {
        let (_guard, root) = workspace();

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();

        assert!(outcome.output.contains("empty"));
        assert!(outcome.output.contains("include_ignored"));
    }

    #[tokio::test]
    async fn truncation_is_reported() {
        let (_guard, root) = workspace();
        for i in 0..(MAX_ENTRIES + 20) {
            touch(&root, &format!("f{i}.rs"));
        }

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();
        assert!(outcome.output.contains("stopped at"));
        assert_eq!(outcome.metadata.unwrap()["truncated"], true);
    }

    #[tokio::test]
    async fn ordering_is_alphabetical_and_stable() {
        let (_guard, root) = workspace();
        touch(&root, "zebra.rs");
        std::thread::sleep(std::time::Duration::from_millis(20));
        touch(&root, "alpha.rs");

        let outcome = ListTool::new().execute("{}", &context(&root)).await.unwrap();
        let alpha = outcome.output.find("alpha.rs").unwrap();
        let zebra = outcome.output.find("zebra.rs").unwrap();

        // Not recency: a tree whose branches move between calls is harder to
        // reason about than an alphabetical one.
        assert!(alpha < zebra);
    }

    #[test]
    fn list_is_not_mutating() {
        assert!(!ListTool::new().is_mutating());
    }
}
