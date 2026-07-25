//! `read` — the most-used tool, and the one whose edges matter most.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::content::{image_kind, looks_binary};
use crate::paths::{self, ResolvedPath};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};
use crate::tracker::FileTracker;

/// Lines returned when the model does not ask for a range.
pub const DEFAULT_LINE_LIMIT: usize = 2_000;

/// Longer lines are cut. A minified bundle on one line would otherwise consume
/// the whole context window in a single call.
pub const MAX_LINE_LENGTH: usize = 2_000;

const DESCRIPTION: &str = "\
Reads a file from the filesystem and returns its contents with line numbers.

Usage:
- `path` may be absolute or relative to the current working directory.
- By default the first 2000 lines are returned. Use `offset` (1-based) and
  `limit` to read a specific range of a longer file.
- Output is line-numbered, `<line number>\\t<content>`. The numbers are for
  reference; do not reproduce them when quoting the file.
- Lines longer than 2000 characters are truncated.
- Binary files and images are rejected — use `bash` for those.
- If more lines remain beyond what was returned, the output says so.
- It is fine to read a file that may not exist; the error is informative.
- Prefer reading several relevant files in one batch of parallel calls over
  reading them one at a time.";

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct ReadTool {
    tracker: Arc<FileTracker>,
}

impl std::fmt::Debug for ReadTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadTool").finish_non_exhaustive()
    }
}

impl ReadTool {
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
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to read. Absolute, or relative to the working directory." },
                "offset": { "type": "integer", "description": "1-based line to start from. Only for files too long to read at once." },
                "limit": { "type": "integer", "description": "Number of lines to read." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    fn required_permissions(&self, input: &str, ctx: &ToolContext) -> Vec<String> {
        // Resolution can fail on a missing file. That is not this method's problem
        // — returning no resource lets `execute` produce the real error rather
        // than a misleading permission failure.
        match self.resolve(input, ctx) {
            Ok((_, resolved)) => vec![resolved.read_resource()],
            Err(_) => Vec::new(),
        }
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let (parsed, resolved) = self.resolve(input, ctx)?;

        if resolved.absolute.is_dir() {
            return Err(ToolError::Recoverable(format!(
                "{} is a directory, not a file. Use `list` or `glob` to see what is in it.",
                resolved.display
            )));
        }

        let bytes = tokio::fs::read(&resolved.absolute)
            .await
            .map_err(|error| ToolError::Recoverable(format!("{}: {error}", resolved.display)))?;

        if let Some(kind) = image_kind(&resolved.absolute) {
            return Err(ToolError::Recoverable(format!(
                "{} is a {kind} image. This tool cannot read images.",
                resolved.display
            )));
        }
        if looks_binary(&bytes) {
            return Err(ToolError::Recoverable(format!(
                "{} appears to be a binary file. Use `bash` with a tool that understands it.",
                resolved.display
            )));
        }

        let text = String::from_utf8_lossy(&bytes);

        // Recorded before the empty check: the model *did* read it, and requiring
        // a second read before it may write to a file it found empty is nonsense.
        self.tracker.record_read(&resolved.absolute);

        if text.is_empty() {
            return Ok(ToolOutcome::new(
                resolved.display.clone(),
                format!("<file path=\"{}\" note=\"file exists but is empty\" />", resolved.display),
            ));
        }

        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        // 1-based to match what the numbers in the output say. An off-by-one here
        // means the model asks for line 500 and silently gets 501.
        let offset = parsed.offset.unwrap_or(1).max(1);
        let start = offset - 1;

        if start >= total {
            return Err(ToolError::Recoverable(format!(
                "{} has {total} lines; offset {offset} is past the end.",
                resolved.display
            )));
        }

        let limit = parsed.limit.unwrap_or(DEFAULT_LINE_LIMIT).max(1);
        let end = (start + limit).min(total);

        let mut out = String::new();
        out.push_str(&format!("<file path=\"{}\">\n", resolved.display));

        for (index, line) in lines[start..end].iter().enumerate() {
            let number = start + index + 1;
            let rendered = if line.chars().count() > MAX_LINE_LENGTH {
                let cut: String = line.chars().take(MAX_LINE_LENGTH).collect();
                format!("{cut}… [line truncated]")
            } else {
                (*line).to_string()
            };
            out.push_str(&format!("{number:>6}\t{rendered}\n"));
        }

        if end < total {
            // Stated explicitly, with the exact next offset. Left implicit, models
            // routinely assume they have seen the whole file.
            out.push_str(&format!(
                "\n[showing lines {}-{} of {total}. Use offset={} to continue.]\n",
                start + 1,
                end,
                end + 1
            ));
        }
        out.push_str("</file>");

        let preview: String = lines[start..end.min(start + 20)].join("\n");

        Ok(ToolOutcome::new(resolved.display, out).with_metadata(serde_json::json!({
            "path": resolved.absolute,
            "lines_total": total,
            "lines_shown": end - start,
            "preview": preview,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, workspace};

    fn tool() -> (ReadTool, Arc<FileTracker>) {
        let tracker = Arc::new(FileTracker::new());
        (ReadTool::new(tracker.clone()), tracker)
    }

    #[tokio::test]
    async fn numbers_lines_from_one() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "first\nsecond\nthird\n").unwrap();
        let (tool, _) = tool();

        let outcome = tool
            .execute(r#"{"path":"a.txt"}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("     1\tfirst"));
        assert!(outcome.output.contains("     3\tthird"));
    }

    #[tokio::test]
    async fn offset_is_one_based() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "l1\nl2\nl3\nl4\n").unwrap();
        let (tool, _) = tool();

        let outcome = tool
            .execute(r#"{"path":"a.txt","offset":2,"limit":2}"#, &context(&root))
            .await
            .unwrap();

        // offset=2 must yield line 2, not line 3.
        assert!(outcome.output.contains("     2\tl2"));
        assert!(outcome.output.contains("     3\tl3"));
        assert!(!outcome.output.contains("\tl1"));
        assert!(!outcome.output.contains("\tl4"));
    }

    #[tokio::test]
    async fn truncation_states_the_next_offset() {
        let (_guard, root) = workspace();
        let body: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("a.txt"), body).unwrap();
        let (tool, _) = tool();

        let outcome = tool
            .execute(r#"{"path":"a.txt","limit":10}"#, &context(&root))
            .await
            .unwrap();

        assert!(outcome.output.contains("of 50"));
        assert!(outcome.output.contains("offset=11"), "must say exactly where to resume");
    }

    #[tokio::test]
    async fn a_complete_read_does_not_claim_there_is_more() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let (tool, _) = tool();

        let outcome = tool.execute(r#"{"path":"a.txt"}"#, &context(&root)).await.unwrap();
        assert!(!outcome.output.contains("Use offset"));
    }

    #[tokio::test]
    async fn long_lines_are_truncated() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("bundle.js"), "x".repeat(10_000)).unwrap();
        let (tool, _) = tool();

        let outcome = tool.execute(r#"{"path":"bundle.js"}"#, &context(&root)).await.unwrap();

        assert!(outcome.output.contains("[line truncated]"));
        assert!(outcome.output.len() < 6_000, "a minified file must not flood the context");
    }

    #[tokio::test]
    async fn binary_files_are_refused_with_a_useful_message() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.bin"), [0x00, 0x01, 0x02, 0x00]).unwrap();
        let (tool, _) = tool();

        let error = tool.execute(r#"{"path":"a.bin"}"#, &context(&root)).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("binary"));
        assert!(message.contains("bash"), "the message should point at what to use instead");
    }

    #[tokio::test]
    async fn images_are_named_by_format() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("logo.png"), [0x89, 0x50, 0x4e, 0x47]).unwrap();
        let (tool, _) = tool();

        let error = tool.execute(r#"{"path":"logo.png"}"#, &context(&root)).await.unwrap_err();
        assert!(error.to_string().contains("PNG"));
    }

    #[tokio::test]
    async fn svg_is_readable_because_it_is_text() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("i.svg"), "<svg></svg>\n").unwrap();
        let (tool, _) = tool();

        assert!(tool.execute(r#"{"path":"i.svg"}"#, &context(&root)).await.is_ok());
    }

    #[tokio::test]
    async fn an_empty_file_says_so_rather_than_returning_nothing() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("empty.txt"), "").unwrap();
        let (tool, _) = tool();

        let outcome = tool.execute(r#"{"path":"empty.txt"}"#, &context(&root)).await.unwrap();
        assert!(outcome.output.contains("empty"));
    }

    #[tokio::test]
    async fn reading_records_the_file_for_write_checks() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "hi\n").unwrap();
        let (tool, tracker) = tool();

        tool.execute(r#"{"path":"a.txt"}"#, &context(&root)).await.unwrap();
        assert!(tracker.has_read(&root.join("a.txt")));
    }

    #[tokio::test]
    async fn a_directory_is_refused_with_a_pointer_to_the_right_tool() {
        let (_guard, root) = workspace();
        std::fs::create_dir(root.join("sub")).unwrap();
        let (tool, _) = tool();

        let error = tool.execute(r#"{"path":"sub"}"#, &context(&root)).await.unwrap_err();
        assert!(error.to_string().contains("directory"));
    }

    #[tokio::test]
    async fn an_offset_past_the_end_reports_the_real_length() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let (tool, _) = tool();

        let error = tool
            .execute(r#"{"path":"a.txt","offset":99}"#, &context(&root))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("2 lines"));
    }

    #[test]
    fn permissions_name_the_resolved_path() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("a.txt"), "hi").unwrap();
        let (tool, _) = tool();

        let permissions = tool.required_permissions(r#"{"path":"a.txt"}"#, &context(&root));
        assert_eq!(permissions, vec![format!("read_file({})", root.join("a.txt"))]);
    }

    #[test]
    fn read_is_not_mutating() {
        assert!(!tool().0.is_mutating());
    }
}
