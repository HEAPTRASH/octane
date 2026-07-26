//! `write` — create a file, or replace one wholesale.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::paths::{self, ResolvedPath};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};
use crate::tracker::FileTracker;

const DESCRIPTION: &str = "\
Writes a file, creating it or replacing its entire contents.

Usage:
- `path` may be absolute or relative to the current working directory.
- If the file already exists it MUST have been read first in this session.
  This prevents overwriting content that was never seen.
- Prefer `edit` for changing part of an existing file. Use `write` for new
  files, or when replacing a file completely.
- The parent directory must already exist; create it with `bash` if needed.
- Do not add comments explaining that the file was generated.";

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    content: String,
}

pub struct WriteTool {
    tracker: Arc<FileTracker>,
}

impl std::fmt::Debug for WriteTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteTool").finish_non_exhaustive()
    }
}

impl WriteTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }

    fn resolve(&self, input: &str, ctx: &ToolContext) -> Result<(Input, ResolvedPath), ToolError> {
        let parsed: Input = serde_json::from_str(input)
            .map_err(|error| ToolError::Recoverable(format!("invalid arguments: {error}")))?;
        let resolved = paths::resolve(&parsed.path, &ctx.cwd, &ctx.workspace, false)?;
        Ok((parsed, resolved))
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to write. Absolute, or relative to the working directory." },
                "content": { "type": "string", "description": "Complete new contents of the file." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn required_permissions(&self, input: &str, ctx: &ToolContext) -> Vec<String> {
        match self.resolve(input, ctx) {
            // `write_file` implies `read_file` in the policy engine, so asking for
            // both here would prompt twice for one operation.
            Ok((_, resolved)) => vec![resolved.write_resource()],
            Err(_) => Vec::new(),
        }
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let (parsed, resolved) = self.resolve(input, ctx)?;

        if resolved.absolute.is_dir() {
            return Err(ToolError::Recoverable(format!(
                "{} is a directory.",
                resolved.display
            )));
        }

        let check = self.tracker.check_write(&resolved.absolute);
        if !check.is_ok() {
            return Err(ToolError::Recoverable(check.message(&resolved.display)));
        }

        let parent = resolved
            .absolute
            .parent()
            .ok_or_else(|| ToolError::Recoverable("path has no parent directory".into()))?;
        if !parent.is_dir() {
            // Not created implicitly: a typo'd path would otherwise silently
            // produce a new tree instead of an error.
            return Err(ToolError::Recoverable(format!(
                "directory {parent} does not exist. Create it first if it is intended."
            )));
        }

        let existed = resolved.absolute.exists();
        let previous_len = if existed {
            tokio::fs::read_to_string(&resolved.absolute).await.ok().map(|s| s.lines().count())
        } else {
            None
        };

        tokio::fs::write(&resolved.absolute, &parsed.content)
            .await
            .map_err(|error| ToolError::Recoverable(format!("{}: {error}", resolved.display)))?;

        // Recorded so a follow-up `edit` in the same turn is not called stale
        // against our own write.
        self.tracker.record_write(&resolved.absolute);

        let new_lines = parsed.content.lines().count();
        let summary = match previous_len {
            Some(old) => format!("Replaced {} ({old} lines -> {new_lines} lines)", resolved.display),
            None => format!("Created {} ({new_lines} lines)", resolved.display),
        };

        Ok(ToolOutcome::new(resolved.display.clone(), summary).with_metadata(serde_json::json!({
            "path": resolved.absolute,
            "created": !existed,
            "lines": new_lines,
            "content": parsed.content.clone(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, workspace};

    fn tool() -> (WriteTool, Arc<FileTracker>) {
        let tracker = Arc::new(FileTracker::new());
        (WriteTool::new(tracker.clone()), tracker)
    }

    #[tokio::test]
    async fn creates_a_new_file_without_a_prior_read() {
        let (_guard, root) = workspace();
        let (tool, _) = tool();

        let outcome = tool
            .execute(r#"{"path":"new.txt","content":"hello\n"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "hello\n");
        assert!(outcome.output.contains("Created"));
    }

    #[tokio::test]
    async fn refuses_to_clobber_a_file_that_was_never_read() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("existing.txt"), "important work\n").unwrap();
        let (tool, _) = tool();

        let error = tool
            .execute(r#"{"path":"existing.txt","content":"gone"}"#, &context(&root))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("Read it first"));
        // Critically: the original survives.
        assert_eq!(
            std::fs::read_to_string(root.join("existing.txt")).unwrap(),
            "important work\n"
        );
    }

    #[tokio::test]
    async fn overwrites_once_the_file_has_been_read() {
        let (_guard, root) = workspace();
        let path = root.join("existing.txt");
        std::fs::write(&path, "v1\n").unwrap();

        let (tool, tracker) = tool();
        tracker.record_read(&path);

        let outcome = tool
            .execute(r#"{"path":"existing.txt","content":"v2\n"}"#, &context(&root))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2\n");
        assert!(outcome.output.contains("Replaced"));
    }

    #[tokio::test]
    async fn a_missing_parent_directory_is_an_error_not_a_new_tree() {
        let (_guard, root) = workspace();
        let (tool, _) = tool();

        let error = tool
            .execute(r#"{"path":"a/b/c.txt","content":"x"}"#, &context(&root))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("does not exist"));
        assert!(!root.join("a").exists(), "a typo must not create a directory tree");
    }

    #[tokio::test]
    async fn writing_permits_an_immediate_follow_up_write() {
        let (_guard, root) = workspace();
        let (tool, _) = tool();
        let ctx = context(&root);

        tool.execute(r#"{"path":"f.txt","content":"one"}"#, &ctx).await.unwrap();
        // Our own write must not make the next one look stale.
        tool.execute(r#"{"path":"f.txt","content":"two"}"#, &ctx).await.unwrap();

        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "two");
    }

    #[test]
    fn permission_is_write_only_since_write_implies_read() {
        let (_guard, root) = workspace();
        let (tool, _) = tool();

        let permissions =
            tool.required_permissions(r#"{"path":"a.txt","content":"x"}"#, &context(&root));
        assert_eq!(permissions, vec![format!("write_file({})", root.join("a.txt"))]);
    }
}
