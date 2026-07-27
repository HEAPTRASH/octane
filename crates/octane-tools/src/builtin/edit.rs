//! `edit` — exact string replacement.
//!
//! The highest-traffic mutating tool and the one that fails most often, so its
//! error messages are part of its design rather than an afterthought. Every
//! failure here tells the model specifically what went wrong and what to do:
//! a near-miss reports how many similar lines exist, an ambiguous match reports
//! the count, an unchanged edit is refused rather than silently succeeding.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::paths::{self, ResolvedPath};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};
use crate::tracker::FileTracker;

/// Lines of context shown around a change in the result.
const CONTEXT_LINES: usize = 3;

const DESCRIPTION: &str = "\
Replaces an exact string in a file.

Usage:
- The file MUST have been read first in this session.
- `old` must match the file exactly, including all whitespace and indentation.
- `old` must be UNIQUE in the file. If it is not, include more surrounding
  lines until it is, or set `replace_all` to change every occurrence.
- `old` and `new` must differ.
- To delete text, pass an empty `new`.
- Prefer this over `write` for changing part of a file: it cannot accidentally
  discard the rest of the file.";

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Debug)]
pub struct EditTool {
    tracker: Arc<FileTracker>,
}

impl EditTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }

    fn resolve(&self, input: &str, ctx: &ToolContext) -> Result<(Input, ResolvedPath), ToolError> {
        let parsed: Input = serde_json::from_str(input)
            .map_err(|error| ToolError::Recoverable(format!("invalid arguments: {error}")))?;
        let resolved = paths::resolve(&parsed.path, &ctx.cwd, &ctx.workspace, true)?;
        Ok((parsed, resolved))
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit." },
                "old": { "type": "string", "description": "Exact text to replace, including indentation." },
                "new": { "type": "string", "description": "Replacement text. Empty to delete." },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring a unique match. Defaults to false." }
            },
            "required": ["path", "old", "new"],
            "additionalProperties": false
        })
    }

    fn required_permissions(&self, input: &str, ctx: &ToolContext) -> Vec<String> {
        match self.resolve(input, ctx) {
            Ok((_, resolved)) => vec![resolved.write_resource()],
            Err(_) => Vec::new(),
        }
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let (parsed, resolved) = self.resolve(input, ctx)?;

        if parsed.old == parsed.new {
            return Err(ToolError::Recoverable(
                "`old` and `new` are identical, so this edit would do nothing.".into(),
            ));
        }
        if parsed.old.is_empty() {
            return Err(ToolError::Recoverable(
                "`old` must not be empty. Use `write` to create a file.".into(),
            ));
        }

        let check = self.tracker.check_write(&resolved.absolute);
        if !check.is_ok() {
            return Err(ToolError::Recoverable(check.message(&resolved.display)));
        }

        let original = tokio::fs::read_to_string(&resolved.absolute)
            .await
            .map_err(|error| ToolError::Recoverable(format!("{}: {error}", resolved.display)))?;

        let occurrences = original.matches(&parsed.old).count();

        if occurrences == 0 {
            return Err(ToolError::Recoverable(no_match_message(&original, &parsed.old, &resolved.display)));
        }
        if occurrences > 1 && !parsed.replace_all {
            return Err(ToolError::Recoverable(format!(
                "`old` matches {occurrences} places in {}. Include more surrounding \
                 lines to make it unique, or set replace_all to change all {occurrences}.",
                resolved.display
            )));
        }

        let updated = if parsed.replace_all {
            original.replace(&parsed.old, &parsed.new)
        } else {
            original.replacen(&parsed.old, &parsed.new, 1)
        };

        tokio::fs::write(&resolved.absolute, &updated)
            .await
            .map_err(|error| ToolError::Recoverable(format!("{}: {error}", resolved.display)))?;

        self.tracker.record_write(&resolved.absolute);

        let snippet = changed_region(&updated, &parsed.new);
        let summary = format!(
            "Edited {} ({} replacement{})\n\n{snippet}",
            resolved.display,
            if parsed.replace_all { occurrences } else { 1 },
            if parsed.replace_all && occurrences != 1 { "s" } else { "" }
        );

        // The replaced pair is the diff, exactly, with no algorithm needed:
        // this tool's whole contract is "`old` became `new`". Capped because a
        // replacement can be a whole file and the UI is not the model's copy.
        Ok(ToolOutcome::new(resolved.display.clone(), summary).with_metadata(serde_json::json!({
            "path": resolved.absolute,
            "replacements": if parsed.replace_all { occurrences } else { 1 },
            "removed": clip(&parsed.old, DIFF_LINE_CAP),
            "added": clip(&parsed.new, DIFF_LINE_CAP),
        })))
    }
}

/// Lines of each side of a replacement kept for display.
const DIFF_LINE_CAP: usize = 200;

/// First `cap` lines, with a marker when there are more.
fn clip(text: &str, cap: usize) -> String {
    let mut out: Vec<&str> = text.lines().take(cap).collect();
    let total = text.lines().count();
    if total > cap {
        out.push("...");
    }
    out.join("\n")
}

/// Explain a failed match in terms the model can act on.
///
/// A bare "not found" leaves it guessing between a typo, wrong indentation, and
/// the wrong file. Naming the likely cause turns three retries into one.
fn no_match_message(original: &str, old: &str, display: &str) -> String {
    let trimmed_matches = original
        .lines()
        .filter(|line| !old.trim().is_empty() && line.trim() == old.trim())
        .count();

    if trimmed_matches > 0 {
        return format!(
            "`old` was not found in {display}, but {trimmed_matches} line(s) match it \
             apart from leading or trailing whitespace. The indentation in `old` must \
             match the file exactly — re-read the file and copy the text verbatim."
        );
    }

    if old.contains('\r') != original.contains('\r') {
        return format!(
            "`old` was not found in {display}. The line endings differ: the file uses \
             {}, and `old` uses {}.",
            if original.contains('\r') { "CRLF" } else { "LF" },
            if old.contains('\r') { "CRLF" } else { "LF" }
        );
    }

    let first_line = old.lines().next().unwrap_or("").trim();
    if !first_line.is_empty() && original.contains(first_line) {
        return format!(
            "`old` was not found in {display}, though its first line does appear. \
             The later lines differ — re-read the file and copy the block verbatim."
        );
    }

    format!("`old` was not found in {display}. Re-read the file to see its current contents.")
}

/// A few lines of context around the inserted text, so the result shows what the
/// file now looks like rather than just asserting success.
fn changed_region(updated: &str, new: &str) -> String {
    let Some(byte_index) = updated.find(new) else {
        return String::new();
    };

    let line_index = updated[..byte_index].lines().count().saturating_sub(1);
    let inserted_lines = new.lines().count().max(1);

    let lines: Vec<&str> = updated.lines().collect();
    let start = line_index.saturating_sub(CONTEXT_LINES);
    let end = (line_index + inserted_lines + CONTEXT_LINES).min(lines.len());

    lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("{:>6}\t{line}", start + offset + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, workspace};

    fn tool_with(root: &camino::Utf8Path, contents: &str) -> (EditTool, Arc<FileTracker>) {
        let tracker = Arc::new(FileTracker::new());
        let path = root.join("a.rs");
        std::fs::write(&path, contents).unwrap();
        tracker.record_read(&path);
        (EditTool::new(tracker.clone()), tracker)
    }

    #[tokio::test]
    async fn replaces_a_unique_match() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "let x = 1;\nlet y = 2;\n");

        tool.execute(
            r#"{"path":"a.rs","old":"let x = 1;","new":"let x = 42;"}"#,
            &context(&root),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "let x = 42;\nlet y = 2;\n"
        );
    }

    #[tokio::test]
    async fn an_ambiguous_match_is_refused_with_a_count() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "a();\nb();\na();\n");

        let error = tool
            .execute(r#"{"path":"a.rs","old":"a();","new":"c();"}"#, &context(&root))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("2 places"));
        assert!(error.to_string().contains("replace_all"));
        // Nothing changed.
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "a();\nb();\na();\n");
    }

    #[tokio::test]
    async fn replace_all_changes_every_occurrence() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "a();\nb();\na();\n");

        let outcome = tool
            .execute(
                r#"{"path":"a.rs","old":"a();","new":"c();","replace_all":true}"#,
                &context(&root),
            )
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "c();\nb();\nc();\n");
        assert!(outcome.output.contains("2 replacements"));
    }

    #[tokio::test]
    async fn an_indentation_mismatch_says_so_specifically() {
        let (_guard, root) = workspace();
        // File has no indentation; the model assumes some.
        let (tool, _) = tool_with(&root, "fn f() {\nlet x = 1;\n}\n");

        let error = tool
            .execute(
                r#"{"path":"a.rs","old":"    let x = 1;","new":"    let x = 2;"}"#,
                &context(&root),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("indentation"));
    }

    #[tokio::test]
    async fn a_less_indented_old_still_matches_and_keeps_the_indentation() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "fn f() {\n    let x = 1;\n}\n");

        // Matching is by substring, so omitting the leading whitespace is not an
        // error — it matches the text portion and the file's own indentation is
        // left untouched. Worth pinning: the alternative (requiring byte-exact
        // lines) would reject a very common and perfectly safe call.
        tool.execute(r#"{"path":"a.rs","old":"let x = 1;","new":"let x = 2;"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("a.rs")).unwrap(),
            "fn f() {\n    let x = 2;\n}\n"
        );
    }

    #[tokio::test]
    async fn a_partial_block_match_says_the_later_lines_differ() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "fn f() {\n    real();\n}\n");

        let error = tool
            .execute(
                r#"{"path":"a.rs","old":"fn f() {\n    imagined();\n}","new":"fn f() {}"}"#,
                &context(&root),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("first line does appear"));
    }

    #[tokio::test]
    async fn editing_an_unread_file_is_refused() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.rs"), "x\n").unwrap();
        let tool = EditTool::new(Arc::new(FileTracker::new()));

        let error = tool
            .execute(r#"{"path":"a.rs","old":"x","new":"y"}"#, &context(&root))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Read it first"));
    }

    #[tokio::test]
    async fn a_stale_edit_is_refused_so_the_users_change_survives() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "v1\n");

        // The user edits the file after the model read it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("a.rs"), "the user's work\n").unwrap();

        let error = tool
            .execute(r#"{"path":"a.rs","old":"v1","new":"v2"}"#, &context(&root))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Read it again"));
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "the user's work\n");
    }

    #[tokio::test]
    async fn a_no_op_edit_is_refused_rather_than_silently_succeeding() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "x\n");

        let error = tool
            .execute(r#"{"path":"a.rs","old":"x","new":"x"}"#, &context(&root))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("do nothing"));
    }

    #[tokio::test]
    async fn empty_new_deletes() {
        let (_guard, root) = workspace();
        let (tool, _) = tool_with(&root, "keep\ndelete me\nkeep\n");

        tool.execute(r#"{"path":"a.rs","old":"delete me\n","new":""}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "keep\nkeep\n");
    }

    #[tokio::test]
    async fn the_result_shows_the_changed_region() {
        let (_guard, root) = workspace();
        let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let (tool, _) = tool_with(&root, &body);

        let outcome = tool
            .execute(r#"{"path":"a.rs","old":"line 10","new":"CHANGED"}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("CHANGED"));
        // Context around it, not the whole file.
        assert!(outcome.output.contains("line 8"));
        assert!(!outcome.output.contains("line 1\n"));
    }
}
