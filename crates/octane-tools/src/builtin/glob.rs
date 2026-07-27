//! `glob` — find files by name pattern.

use async_trait::async_trait;
use camino::Utf8PathBuf;
use serde::Deserialize;

use crate::paths;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};
use crate::walk::{self, WalkOptions};

/// Results returned before truncating.
///
/// Past this the model is not learning more about the shape of the codebase, it
/// is spending context. A truncated list plus a note to narrow the pattern is
/// more useful than 2000 paths.
pub const MAX_RESULTS: usize = 200;

const DESCRIPTION: &str = "\
Finds files whose paths match a glob pattern.

Usage:
- `pattern` is a glob, e.g. `**/*.rs`, `src/**/test_*.py`, `Cargo.toml`.
- Matching is against the path relative to the search root, so `**/` is only
  needed when you mean 'at any depth'.
- Results are sorted by modification time, most recent first — the file
  someone touched recently is usually the one in question.
- Files ignored by .gitignore are skipped, so build output and vendored
  dependencies do not appear. Set `include_ignored` to search them anyway.
- Returns paths only. Use `grep` to search contents, `read` to see a file.
- Prefer this over `bash` with find or ls: it is faster and already excludes
  the noise.";

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    /// Directory to search. Defaults to the working directory.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include_ignored: bool,
    #[serde(default)]
    include_hidden: bool,
}

#[derive(Debug, Default)]
pub struct GlobTool;

impl GlobTool {
    pub fn new() -> Self {
        Self
    }

    fn root(&self, parsed: &Input, ctx: &ToolContext) -> Result<Utf8PathBuf, ToolError> {
        let hint = "is not a directory";
        paths::resolve_dir_or_cwd(parsed.path.as_deref(), &ctx.cwd, &ctx.workspace, hint)
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. `**/*.rs`." },
                "path": { "type": "string", "description": "Directory to search. Defaults to the working directory." },
                "include_ignored": { "type": "boolean", "description": "Also search paths excluded by .gitignore. Defaults to false." },
                "include_hidden": { "type": "boolean", "description": "Also search dotfiles. Defaults to false." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    fn required_permissions(&self, input: &str, ctx: &ToolContext) -> Vec<String> {
        // The search root, not each hit: asking about a directory once is the
        // decision the user is actually making.
        match serde_json::from_str::<Input>(input).map(|parsed| self.root(&parsed, ctx)) {
            Ok(Ok(root)) => vec![format!("read_file({root})")],
            _ => Vec::new(),
        }
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let parsed: Input = serde_json::from_str(input)
            .map_err(|error| ToolError::Recoverable(format!("invalid arguments: {error}")))?;

        let root = self.root(&parsed, ctx)?;
        let matcher = walk::glob_matcher(&parsed.pattern)?;

        let options = WalkOptions {
            limit: MAX_RESULTS,
            respect_ignore_files: !parsed.include_ignored,
            include_hidden: parsed.include_hidden,
            ..Default::default()
        };

        let root_for_filter = root.clone();
        let mut result = walk::walk(&root, &options, |path, is_dir| {
            if is_dir {
                return false;
            }
            let relative = walk::display_path(path, &root_for_filter);
            matcher.is_match(relative.as_str())
        });

        walk::sort_by_recency(&mut result.entries);

        if result.entries.is_empty() {
            return Ok(ToolOutcome::new(
                parsed.pattern.clone(),
                format!(
                    "No files match {:?} under {}.\n\n\
                     If files were expected, they may be excluded by .gitignore \
                     (set include_ignored) or be dotfiles (set include_hidden).",
                    parsed.pattern,
                    walk::display_path(&root, &ctx.workspace)
                ),
            ));
        }

        let listed: Vec<String> = result
            .entries
            .iter()
            .map(|entry| walk::display_path(&entry.path, &ctx.workspace))
            .collect();

        let mut output = listed.join("\n");
        if result.truncated {
            output.push_str(&format!(
                "\n\n[stopped at {MAX_RESULTS} results. Narrow the pattern or search a \
                 subdirectory to see the rest.]"
            ));
        }

        Ok(ToolOutcome::new(
            format!("{} ({} match{})", parsed.pattern, listed.len(), if listed.len() == 1 { "" } else { "es" }),
            output,
        )
        .with_metadata(serde_json::json!({
            "pattern": parsed.pattern,
            "root": root,
            "count": listed.len(),
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

    fn lines(output: &str) -> Vec<&str> {
        output.lines().filter(|line| !line.is_empty() && !line.starts_with('[')).collect()
    }

    #[tokio::test]
    async fn matches_at_any_depth_including_the_root() {
        let (_guard, root) = workspace();
        touch(&root, "main.rs");
        touch(&root, "src/lib.rs");
        touch(&root, "src/deep/mod.rs");
        touch(&root, "notes.txt");

        let outcome = GlobTool::new()
            .execute(r#"{"pattern":"**/*.rs"}"#, &context(&root))
            .await
            .unwrap();

        let found = lines(&outcome.output);
        // The root-level file is the case a naive `**/` would miss.
        assert!(found.contains(&"main.rs"), "got {found:?}");
        assert!(found.contains(&"src/lib.rs"));
        assert!(found.contains(&"src/deep/mod.rs"));
        assert!(!found.contains(&"notes.txt"));
    }

    #[tokio::test]
    async fn a_directory_scoped_pattern_does_not_leak_outside_it() {
        let (_guard, root) = workspace();
        touch(&root, "src/a.rs");
        touch(&root, "tests/b.rs");

        let outcome = GlobTool::new()
            .execute(r#"{"pattern":"src/**/*.rs"}"#, &context(&root))
            .await
            .unwrap();

        let found = lines(&outcome.output);
        assert_eq!(found, vec!["src/a.rs"]);
    }

    #[tokio::test]
    async fn a_single_star_does_not_cross_directories() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs");
        touch(&root, "src/b.rs");

        let outcome =
            GlobTool::new().execute(r#"{"pattern":"*.rs"}"#, &context(&root)).await.unwrap();

        assert_eq!(lines(&outcome.output), vec!["a.rs"]);
    }

    #[tokio::test]
    async fn results_are_newest_first() {
        let (_guard, root) = workspace();
        touch(&root, "old.rs");
        std::thread::sleep(std::time::Duration::from_millis(20));
        touch(&root, "new.rs");

        let outcome =
            GlobTool::new().execute(r#"{"pattern":"*.rs"}"#, &context(&root)).await.unwrap();

        assert_eq!(lines(&outcome.output), vec!["new.rs", "old.rs"]);
    }

    #[tokio::test]
    async fn gitignored_files_are_excluded_by_default() {
        let (_guard, root) = workspace();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        touch(&root, "src/a.rs");
        touch(&root, "target/generated.rs");

        let outcome = GlobTool::new()
            .execute(r#"{"pattern":"**/*.rs"}"#, &context(&root))
            .await
            .unwrap();
        assert_eq!(lines(&outcome.output), vec!["src/a.rs"]);

        let with_ignored = GlobTool::new()
            .execute(r#"{"pattern":"**/*.rs","include_ignored":true}"#, &context(&root))
            .await
            .unwrap();
        assert!(lines(&with_ignored.output).contains(&"target/generated.rs"));
    }

    #[tokio::test]
    async fn no_matches_explains_why_that_might_be() {
        let (_guard, root) = workspace();
        touch(&root, "a.txt");

        let outcome = GlobTool::new()
            .execute(r#"{"pattern":"**/*.rs"}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("No files match"));
        // A dead end should suggest the two reasons a file might be invisible.
        assert!(outcome.output.contains("include_ignored"));
        assert!(outcome.output.contains("include_hidden"));
    }

    #[tokio::test]
    async fn truncation_is_reported() {
        let (_guard, root) = workspace();
        for i in 0..(MAX_RESULTS + 20) {
            touch(&root, &format!("f{i}.rs"));
        }

        let outcome =
            GlobTool::new().execute(r#"{"pattern":"*.rs"}"#, &context(&root)).await.unwrap();

        assert!(outcome.output.contains("stopped at"));
        assert_eq!(outcome.metadata.unwrap()["truncated"], true);
    }

    #[tokio::test]
    async fn an_invalid_glob_is_recoverable() {
        let (_guard, root) = workspace();
        let error = GlobTool::new()
            .execute(r#"{"pattern":"[unclosed"}"#, &context(&root))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Recoverable(_)));
        assert!(error.to_string().contains("invalid glob"));
    }

    #[tokio::test]
    async fn searching_a_subdirectory_scopes_the_walk() {
        let (_guard, root) = workspace();
        touch(&root, "src/a.rs");
        touch(&root, "other/b.rs");

        let outcome = GlobTool::new()
            .execute(r#"{"pattern":"**/*.rs","path":"src"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(lines(&outcome.output), vec!["src/a.rs"]);
    }

    #[test]
    fn the_permission_target_is_the_search_root() {
        let (_guard, root) = workspace();
        let permissions =
            GlobTool::new().required_permissions(r#"{"pattern":"*.rs"}"#, &context(&root));
        assert_eq!(permissions, vec![format!("read_file({root})")]);
    }

    #[test]
    fn glob_is_not_mutating() {
        assert!(!GlobTool::new().is_mutating());
    }
}
