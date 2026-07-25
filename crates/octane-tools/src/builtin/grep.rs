//! `grep` — search file contents by regex.

use async_trait::async_trait;
use camino::Utf8PathBuf;
use serde::Deserialize;

use crate::content::looks_binary;
use crate::paths;
use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};
use crate::walk::{self, WalkOptions};

/// Matching lines returned before truncating.
pub const MAX_MATCHES: usize = 200;

/// Files opened before giving up, independent of how many match.
///
/// Bounds the work for a pattern that matches nothing across a huge tree, which
/// would otherwise read every file in the repository to report zero hits.
pub const MAX_FILES_SCANNED: usize = 5_000;

/// Longer matching lines are cut. A minified bundle with one 2MB line would
/// otherwise fill the context from a single hit.
pub const MAX_LINE_LENGTH: usize = 500;

const DESCRIPTION: &str = "\
Searches file contents with a regular expression.

Usage:
- `pattern` is a Rust regex, e.g. `fn \\\\w+_handler`, `TODO|FIXME`,
  `impl\\\\s+Display\\\\s+for`.
- `glob` narrows which files are searched, e.g. `**/*.rs`.
- `mode` selects the output shape:
    `content` (default) — matching lines as `path:line: text`
    `files`             — just the paths that contain a match
    `count`             — match counts per file
- `context` adds surrounding lines in `content` mode, like grep -C.
- Files ignored by .gitignore and binary files are skipped.
- Searching is line-based; a pattern cannot match across a newline.
- Prefer this over `bash` with grep or rg: it is already scoped to the
  project and excludes build output.
- Start with `files` mode when locating where something lives, then `read`
  the file. That costs far less context than dumping every matching line.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Content,
    Files,
    Count,
}

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    mode: Mode,
    #[serde(default)]
    case_insensitive: bool,
    /// Lines of context either side, `content` mode only.
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    include_ignored: bool,
}

#[derive(Debug, Default)]
pub struct GrepTool;

impl GrepTool {
    pub fn new() -> Self {
        Self
    }

    fn root(&self, parsed: &Input, ctx: &ToolContext) -> Result<Utf8PathBuf, ToolError> {
        match parsed.path.as_deref() {
            Some(path) => Ok(paths::resolve(path, &ctx.cwd, &ctx.workspace, true)?.absolute),
            None => Ok(ctx.cwd.clone()),
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regular expression to search for." },
                "path": { "type": "string", "description": "File or directory to search. Defaults to the working directory." },
                "glob": { "type": "string", "description": "Only search files matching this glob, e.g. `**/*.rs`." },
                "mode": {
                    "type": "string",
                    "enum": ["content", "files", "count"],
                    "description": "Output shape. `content` returns matching lines, `files` returns paths only, `count` returns per-file counts."
                },
                "case_insensitive": { "type": "boolean", "description": "Case-insensitive matching. Defaults to false." },
                "context": { "type": "integer", "description": "Lines of context either side of a match, in `content` mode." },
                "include_ignored": { "type": "boolean", "description": "Also search paths excluded by .gitignore. Defaults to false." }
            },
            "required": ["pattern"],
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

        let regex = regex::RegexBuilder::new(&parsed.pattern)
            .case_insensitive(parsed.case_insensitive)
            .build()
            .map_err(|error| {
                ToolError::Recoverable(format!("invalid regex {:?}: {error}", parsed.pattern))
            })?;

        let root = self.root(&parsed, ctx)?;

        let file_filter = parsed
            .glob
            .as_deref()
            .map(build_glob)
            .transpose()?;

        // A single file target is searched directly rather than walked, so
        // `grep(pattern, path=src/main.rs)` works and does not silently return
        // nothing because a file is not a directory.
        let targets: Vec<Utf8PathBuf> = if root.is_file() {
            vec![root.clone()]
        } else {
            let options = WalkOptions {
                limit: MAX_FILES_SCANNED,
                respect_ignore_files: !parsed.include_ignored,
                ..Default::default()
            };
            let root_for_filter = root.clone();
            walk::walk(&root, &options, |path, is_dir| {
                if is_dir {
                    return false;
                }
                match &file_filter {
                    Some(set) => {
                        set.is_match(walk::display_path(path, &root_for_filter).as_str())
                    }
                    None => true,
                }
            })
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect()
        };

        let mut hits: Vec<FileHits> = Vec::new();
        let mut total_matches = 0usize;
        let mut truncated = false;

        for path in &targets {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            if total_matches >= MAX_MATCHES {
                truncated = true;
                break;
            }

            let Ok(bytes) = std::fs::read(path) else { continue };
            if looks_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);

            let matched: Vec<(usize, String)> = text
                .lines()
                .enumerate()
                .filter(|(_, line)| regex.is_match(line))
                .map(|(index, line)| (index + 1, line.to_string()))
                .collect();

            if matched.is_empty() {
                continue;
            }

            total_matches += matched.len();
            hits.push(FileHits {
                display: walk::display_path(path, &ctx.workspace),
                lines: text.lines().map(ToString::to_string).collect(),
                matched,
            });
        }

        if hits.is_empty() {
            return Ok(ToolOutcome::new(
                parsed.pattern.clone(),
                format!(
                    "No matches for {:?} in {}.{}",
                    parsed.pattern,
                    walk::display_path(&root, &ctx.workspace),
                    if parsed.glob.is_some() {
                        " The `glob` filter may be excluding the files you meant."
                    } else {
                        ""
                    }
                ),
            ));
        }

        let output = match parsed.mode {
            Mode::Files => render_files(&hits),
            Mode::Count => render_counts(&hits, total_matches),
            Mode::Content => render_content(&hits, parsed.context.unwrap_or(0)),
        };

        let mut output = output;
        if truncated {
            output.push_str(&format!(
                "\n\n[stopped at {MAX_MATCHES} matches. Narrow the pattern, add a `glob` \
                 filter, or use mode=\"files\" to see where the matches are.]"
            ));
        }

        Ok(ToolOutcome::new(
            format!(
                "{} ({total_matches} match{} in {} file{})",
                parsed.pattern,
                if total_matches == 1 { "" } else { "es" },
                hits.len(),
                if hits.len() == 1 { "" } else { "s" }
            ),
            output,
        )
        .with_metadata(serde_json::json!({
            "pattern": parsed.pattern,
            "matches": total_matches,
            "files": hits.len(),
            "files_scanned": targets.len(),
            "truncated": truncated,
        })))
    }
}

struct FileHits {
    display: String,
    /// Whole file, kept so context lines can be rendered without re-reading.
    lines: Vec<String>,
    /// `(1-based line number, line text)`.
    matched: Vec<(usize, String)>,
}

fn render_files(hits: &[FileHits]) -> String {
    hits.iter().map(|hit| hit.display.clone()).collect::<Vec<_>>().join("\n")
}

fn render_counts(hits: &[FileHits], total: usize) -> String {
    let mut rows: Vec<(usize, &str)> =
        hits.iter().map(|hit| (hit.matched.len(), hit.display.as_str())).collect();
    // Most hits first: that is the file to look at.
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    let body = rows
        .iter()
        .map(|(count, path)| format!("{count:>6}  {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{body}\n\n{total} matches across {} files", hits.len())
}

fn render_content(hits: &[FileHits], context: usize) -> String {
    let mut out = String::new();

    for hit in hits {
        out.push_str(&format!("{}\n", hit.display));

        let mut last_rendered: Option<usize> = None;
        for (number, line) in &hit.matched {
            let start = number.saturating_sub(context).max(1);
            let end = (number + context).min(hit.lines.len());

            // A gap marker keeps the model from reading two distant hits as
            // adjacent code.
            if let Some(previous) = last_rendered {
                if start > previous + 1 {
                    out.push_str("  --\n");
                }
            }

            for current in start..=end {
                if last_rendered.is_some_and(|previous| current <= previous) {
                    continue;
                }
                let text = if current == *number {
                    line.as_str()
                } else {
                    hit.lines.get(current - 1).map(String::as_str).unwrap_or_default()
                };
                out.push_str(&format!("{current:>6}: {}\n", clip(text)));
                last_rendered = Some(current);
            }
        }
        out.push('\n');
    }

    out.trim_end().to_string()
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LENGTH {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_LINE_LENGTH).collect();
    format!("{cut}… [line truncated]")
}

fn build_glob(pattern: &str) -> Result<globset::GlobSet, ToolError> {
    let compile = |text: &str| {
        globset::GlobBuilder::new(text)
            .literal_separator(true)
            .build()
            .map_err(|error| ToolError::Recoverable(format!("invalid glob {text:?}: {error}")))
    };

    let mut builder = globset::GlobSetBuilder::new();
    builder.add(compile(pattern)?);
    // Same leading-`**/` accommodation as `glob`; see that tool for why.
    if let Some(rest) = pattern.strip_prefix("**/") {
        builder.add(compile(rest)?);
    }
    builder
        .build()
        .map_err(|error| ToolError::Recoverable(format!("invalid glob {pattern:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, workspace};
    use camino::Utf8Path;

    fn touch(root: &Utf8Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn finds_matching_lines_with_numbers() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs", "fn one() {}\nfn two() {}\nstruct S;\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"^fn "}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("     1: fn one() {}"));
        assert!(outcome.output.contains("     2: fn two() {}"));
        assert!(!outcome.output.contains("struct S"));
    }

    #[tokio::test]
    async fn files_mode_returns_paths_only() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs", "TODO: x\n");
        touch(&root, "b.rs", "nothing\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"TODO","mode":"files"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(outcome.output, "a.rs");
    }

    #[tokio::test]
    async fn count_mode_ranks_by_hit_count() {
        let (_guard, root) = workspace();
        touch(&root, "few.rs", "x\n");
        touch(&root, "many.rs", "x\nx\nx\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"x","mode":"count"}"#, &context(&root))
            .await
            .unwrap();

        let first_line = outcome.output.lines().next().unwrap();
        assert!(first_line.contains("many.rs"), "busiest file should lead: {first_line}");
        assert!(outcome.output.contains("4 matches across 2 files"));
    }

    #[tokio::test]
    async fn context_lines_are_included_and_gaps_marked() {
        let (_guard, root) = workspace();
        let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        touch(&root, "a.txt", &body);

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"line (5|25)$","context":1}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("     4: line 4"));
        assert!(outcome.output.contains("     5: line 5"));
        assert!(outcome.output.contains("     6: line 6"));
        assert!(outcome.output.contains("    24: line 24"));
        // Distant hits must not read as adjacent code.
        assert!(outcome.output.contains("--"));
        assert!(!outcome.output.contains("line 15"));
    }

    #[tokio::test]
    async fn overlapping_context_does_not_duplicate_lines() {
        let (_guard, root) = workspace();
        touch(&root, "a.txt", "a\nhit\nhit\nb\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"hit","context":2}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(outcome.output.matches("     2: hit").count(), 1);
        assert_eq!(outcome.output.matches("     3: hit").count(), 1);
    }

    #[tokio::test]
    async fn the_glob_filter_scopes_the_search() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs", "target\n");
        touch(&root, "b.txt", "target\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"target","glob":"**/*.rs","mode":"files"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(outcome.output, "a.rs");
    }

    #[tokio::test]
    async fn case_insensitive_matching_is_opt_in() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs", "Hello\n");

        let sensitive = GrepTool::new()
            .execute(r#"{"pattern":"hello","mode":"files"}"#, &context(&root))
            .await
            .unwrap();
        assert!(sensitive.output.contains("No matches"));

        let insensitive = GrepTool::new()
            .execute(
                r#"{"pattern":"hello","mode":"files","case_insensitive":true}"#,
                &context(&root),
            )
            .await
            .unwrap();
        assert_eq!(insensitive.output, "a.rs");
    }

    #[tokio::test]
    async fn binary_files_are_skipped_silently() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.bin"), [b'f', b'i', b'n', b'd', 0x00, b'm', b'e']).unwrap();
        touch(&root, "b.rs", "find me\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"find","mode":"files"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(outcome.output, "b.rs");
    }

    #[tokio::test]
    async fn gitignored_files_are_skipped_by_default() {
        let (_guard, root) = workspace();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        touch(&root, "src/a.rs", "needle\n");
        touch(&root, "target/gen.rs", "needle\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"needle","mode":"files"}"#, &context(&root))
            .await
            .unwrap();
        assert_eq!(outcome.output, "src/a.rs");

        let including = GrepTool::new()
            .execute(
                r#"{"pattern":"needle","mode":"files","include_ignored":true}"#,
                &context(&root),
            )
            .await
            .unwrap();
        assert!(including.output.contains("target/gen.rs"));
    }

    #[tokio::test]
    async fn a_single_file_path_is_searched_directly() {
        let (_guard, root) = workspace();
        touch(&root, "a.rs", "needle\n");
        touch(&root, "b.rs", "needle\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"needle","path":"a.rs","mode":"files"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(outcome.output, "a.rs");
    }

    #[tokio::test]
    async fn very_long_lines_are_clipped() {
        let (_guard, root) = workspace();
        touch(&root, "bundle.js", &format!("var x=\"{}\";needle\n", "y".repeat(5_000)));

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"needle"}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("[line truncated]"));
        assert!(outcome.output.len() < 2_000);
    }

    #[tokio::test]
    async fn an_invalid_regex_is_recoverable_and_names_the_pattern() {
        let (_guard, root) = workspace();
        let error = GrepTool::new()
            .execute(r#"{"pattern":"([unclosed"}"#, &context(&root))
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::Recoverable(_)));
        assert!(error.to_string().contains("invalid regex"));
    }

    #[tokio::test]
    async fn no_matches_mentions_the_glob_when_one_was_given() {
        let (_guard, root) = workspace();
        touch(&root, "a.txt", "needle\n");

        let outcome = GrepTool::new()
            .execute(r#"{"pattern":"needle","glob":"**/*.rs"}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("No matches"));
        assert!(outcome.output.contains("glob"), "the likely cause should be named");
    }

    #[tokio::test]
    async fn cancellation_is_honoured_mid_search() {
        let (_guard, root) = workspace();
        for i in 0..50 {
            touch(&root, &format!("f{i}.rs"), "needle\n");
        }
        let ctx = context(&root);
        ctx.cancel.cancel();

        let error = GrepTool::new()
            .execute(r#"{"pattern":"needle"}"#, &ctx)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Cancelled));
    }

    #[test]
    fn grep_is_not_mutating() {
        assert!(!GrepTool::new().is_mutating());
    }
}
