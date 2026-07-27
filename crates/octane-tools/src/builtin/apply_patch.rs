//! Many edits to many files, as one reviewable patch.
//!
//! # Provenance
//!
//! The patch **format** and the decreasing-strictness matching strategy are
//! taken from OpenAI Codex (`codex-rs/apply-patch`), Copyright 2025 OpenAI,
//! licensed under the Apache License 2.0 — see `LICENSE-APACHE` and `NOTICE`.
//! Speaking the same dialect is the entire point: models have seen this format,
//! and inventing a private one buys nothing but a worse hit rate.
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/main.rs
//! @@ fn main() {
//!      let x = 1;
//! -    println!("old");
//! +    println!("new");
//! *** Add File: src/new.rs
//! +fn added() {}
//! *** Delete File: src/gone.rs
//! *** End Patch
//! ```
//!
//! # Why this exists beside `edit`
//!
//! `edit` replaces one string in one file and is the right tool for one change.
//! A refactor that touches six files costs six calls, six round trips, and six
//! chances to be interrupted halfway. This is one call, and it either applies
//! whole or changes nothing.
//!
//! # All-or-nothing
//!
//! Every hunk is resolved against the current file contents *before* anything
//! is written. A patch whose third hunk does not match leaves the first two
//! unapplied, because a half-applied refactor is worse than a rejected one: the
//! tree does not compile and neither the model nor the user knows how far it
//! got.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::paths::{self, ResolvedPath};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};
use crate::tracker::FileTracker;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";

const DESCRIPTION: &str = "\
Apply a patch touching one or more files in a single call. Use this instead of \
repeated `edit` calls when a change spans several files, or several places in \
one file. The patch either applies whole or changes nothing.

Format:
*** Begin Patch
*** Update File: <path>
@@ <optional context line naming the enclosing scope>
 <unchanged line, one leading space>
-<line to remove>
+<line to add>
*** Add File: <path>
+<line of the new file>
*** Delete File: <path>
*** End Patch

Context lines must reproduce the file exactly, including indentation.";

/// One file's worth of change.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hunk {
    Add { path: String, contents: String },
    Delete { path: String },
    Update { path: String, move_to: Option<String>, chunks: Vec<Chunk> },
}

impl Hunk {
    fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

/// A contiguous run of changes within a file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Chunk {
    /// Lines expected to be present, in order.
    old: Vec<String>,
    /// What replaces them.
    new: Vec<String>,
    /// The chunk is anchored to the end of the file.
    at_eof: bool,
}

#[derive(Debug)]
pub struct ApplyPatchTool {
    tracker: Arc<FileTracker>,
}

impl ApplyPatchTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        Self { tracker }
    }

    fn parse_input(input: &str) -> Result<String, ToolError> {
        #[derive(serde::Deserialize)]
        struct Input {
            patch: String,
        }
        serde_json::from_str::<Input>(input)
            .map(|parsed| parsed.patch)
            .map_err(|error| ToolError::Recoverable(format!("invalid arguments: {error}")))
    }

    /// Every path the patch touches, resolved and checked against the workspace.
    fn resolve_all(
        hunks: &[Hunk],
        ctx: &ToolContext,
    ) -> Result<Vec<(ResolvedPath, Option<ResolvedPath>)>, ToolError> {
        hunks
            .iter()
            .map(|hunk| {
                // Never `must_exist`. This resolves paths in order to *name*
                // them; whether the file should already be there is a property
                // of the hunk, checked in `execute` where the error can say
                // what was expected. Coupling the two made a delete of a
                // missing file return no permissions at all — including the
                // paths of every other hunk in the same patch.
                //
                // Safe to relax: `resolve` canonicalizes the parent either way,
                // so a symlinked directory still cannot escape the workspace,
                // and `inside_workspace` is computed regardless.
                let target = paths::resolve(hunk.path(), &ctx.cwd, &ctx.workspace, false)?;
                let moved = match hunk {
                    Hunk::Update { move_to: Some(to), .. } => {
                        Some(paths::resolve(to, &ctx.cwd, &ctx.workspace, false)?)
                    }
                    _ => None,
                };
                Ok((target, moved))
            })
            .collect()
    }
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "The patch, from `*** Begin Patch` to `*** End Patch`.",
                }
            },
            "required": ["patch"],
            "additionalProperties": false
        })
    }

    /// Every path the patch writes, so the policy sees the whole blast radius
    /// before any of it happens rather than one file at a time.
    fn required_permissions(&self, input: &str, ctx: &ToolContext) -> Vec<String> {
        let Ok(patch) = Self::parse_input(input) else { return Vec::new() };
        let Ok(hunks) = parse(&patch) else { return Vec::new() };
        let Ok(resolved) = Self::resolve_all(&hunks, ctx) else { return Vec::new() };

        let mut resources: Vec<String> = resolved
            .iter()
            .flat_map(|(target, moved)| {
                std::iter::once(target.write_resource())
                    .chain(moved.iter().map(|path| path.write_resource()))
            })
            .collect();
        resources.sort();
        resources.dedup();
        resources
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let patch = Self::parse_input(input)?;
        let hunks = parse(&patch).map_err(ToolError::Recoverable)?;
        if hunks.is_empty() {
            return Err(ToolError::Recoverable("the patch contains no changes.".into()));
        }
        let resolved = Self::resolve_all(&hunks, ctx)?;

        // Resolve everything first. Nothing below this point may fail for a
        // reason the patch could have been checked for, or the tree is left
        // half-changed.
        let mut planned: Vec<(ResolvedPath, Option<ResolvedPath>, Option<String>)> = Vec::new();
        for (hunk, (target, moved)) in hunks.iter().zip(resolved) {
            let contents = match hunk {
                Hunk::Delete { .. } => {
                    if !target.absolute.exists() {
                        return Err(ToolError::Recoverable(format!(
                            "{}: cannot delete a file that does not exist.",
                            target.display
                        )));
                    }
                    None
                }
                Hunk::Add { contents, .. } => {
                    if target.absolute.exists() {
                        return Err(ToolError::Recoverable(format!(
                            "{}: already exists. Use *** Update File to change it.",
                            target.display
                        )));
                    }
                    Some(contents.clone())
                }
                Hunk::Update { chunks, .. } => {
                    // Read-before-write, as `edit` requires: a patch written
                    // against a file the model has not seen is a guess.
                    let check = self.tracker.check_write(&target.absolute);
                    if !check.is_ok() {
                        return Err(ToolError::Recoverable(check.message(&target.display)));
                    }
                    let original = std::fs::read_to_string(&target.absolute).map_err(|error| {
                        ToolError::Recoverable(format!("{}: {error}", target.display))
                    })?;
                    Some(
                        apply_chunks(&original, chunks)
                            .map_err(|reason| {
                                ToolError::Recoverable(format!("{}: {reason}", target.display))
                            })?,
                    )
                }
            };
            planned.push((target, moved, contents));
        }

        // Everything resolved. Now write.
        let mut touched = BTreeMap::new();
        for (hunk, (target, moved, contents)) in hunks.iter().zip(planned) {
            match contents {
                None => {
                    std::fs::remove_file(&target.absolute).map_err(|error| {
                        ToolError::Internal(format!("{}: {error}", target.display))
                    })?;
                    touched.insert(target.display.clone(), "deleted");
                }
                Some(contents) => {
                    let destination = moved.as_ref().unwrap_or(&target);
                    if let Some(parent) = destination.absolute.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| {
                            ToolError::Internal(format!("{}: {error}", destination.display))
                        })?;
                    }
                    std::fs::write(&destination.absolute, &contents).map_err(|error| {
                        ToolError::Internal(format!("{}: {error}", destination.display))
                    })?;
                    self.tracker.record_write(&destination.absolute);

                    if moved.is_some() {
                        let _ = std::fs::remove_file(&target.absolute);
                        touched.insert(target.display.clone(), "moved");
                    } else {
                        touched.insert(
                            target.display.clone(),
                            match hunk {
                                Hunk::Add { .. } => "added",
                                _ => "updated",
                            },
                        );
                    }
                }
            }
        }

        let summary = touched
            .iter()
            .map(|(path, what)| format!("{what} {path}"))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutcome::new("apply_patch", format!("Applied to {} file(s):\n{summary}", touched.len())))
    }
}

/// Parse a patch into hunks. Lenient about whitespace around the markers,
/// because models reproduce them imperfectly and rejecting a patch over a
/// trailing space costs a whole turn.
fn parse(patch: &str) -> Result<Vec<Hunk>, String> {
    let lines: Vec<&str> = patch.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim() == BEGIN)
        .ok_or_else(|| format!("the patch must start with `{BEGIN}`."))?;
    let end = lines
        .iter()
        .rposition(|line| line.trim() == END)
        .ok_or_else(|| format!("the patch must end with `{END}`."))?;
    if end <= start {
        return Err(format!("`{END}` appears before `{BEGIN}`."));
    }

    let mut hunks = Vec::new();
    let mut index = start + 1;

    while index < end {
        let line = lines[index];
        let trimmed = line.trim_end();

        if let Some(path) = marker(trimmed, ADD) {
            let mut contents = String::new();
            index += 1;
            while index < end && !is_marker(lines[index]) {
                // An added line carries a leading `+`; anything else in an Add
                // block is the model forgetting it, so the raw line is taken.
                let body = lines[index].strip_prefix('+').unwrap_or(lines[index]);
                contents.push_str(body);
                contents.push('\n');
                index += 1;
            }
            hunks.push(Hunk::Add { path, contents });
            continue;
        }

        if let Some(path) = marker(trimmed, DELETE) {
            hunks.push(Hunk::Delete { path });
            index += 1;
            continue;
        }

        if let Some(path) = marker(trimmed, UPDATE) {
            index += 1;
            let move_to = match lines.get(index).map(|line| line.trim_end()) {
                Some(line) => match marker(line, MOVE_TO) {
                    Some(to) => {
                        index += 1;
                        Some(to)
                    }
                    None => None,
                },
                None => None,
            };

            let mut chunks: Vec<Chunk> = Vec::new();
            let mut current = Chunk::default();
            let mut started = false;

            while index < end && !is_marker(lines[index]) {
                let line = lines[index];
                if line.trim_end() == END_OF_FILE {
                    current.at_eof = true;
                    index += 1;
                    continue;
                }
                // `@@` starts a new chunk. Its text is a hint about the
                // enclosing scope and is deliberately not matched against the
                // file: models paraphrase it, and treating it as required
                // turns a good patch into a rejected one.
                if line.trim_end() == "@@" || line.starts_with("@@ ") {
                    if started && !(current.old.is_empty() && current.new.is_empty()) {
                        chunks.push(std::mem::take(&mut current));
                    }
                    started = true;
                    index += 1;
                    continue;
                }

                started = true;
                match line.chars().next() {
                    Some('+') => current.new.push(line[1..].to_string()),
                    Some('-') => current.old.push(line[1..].to_string()),
                    Some(' ') => {
                        current.old.push(line[1..].to_string());
                        current.new.push(line[1..].to_string());
                    }
                    // A bare empty line is context for an empty line.
                    None => {
                        current.old.push(String::new());
                        current.new.push(String::new());
                    }
                    Some(_) => {
                        return Err(format!(
                            "line {}: {line:?} must start with `+`, `-`, or a space.",
                            index + 1
                        ));
                    }
                }
                index += 1;
            }

            if !(current.old.is_empty() && current.new.is_empty()) {
                chunks.push(current);
            }
            if chunks.is_empty() {
                return Err(format!("`{UPDATE}{path}` has no changes under it."));
            }
            hunks.push(Hunk::Update { path, move_to, chunks });
            continue;
        }

        if trimmed.is_empty() {
            index += 1;
            continue;
        }
        return Err(format!("line {}: {line:?} is not a patch marker.", index + 1));
    }

    Ok(hunks)
}

fn marker(line: &str, prefix: &str) -> Option<String> {
    line.strip_prefix(prefix).map(|rest| rest.trim().to_string())
}

fn is_marker(line: &str) -> bool {
    let trimmed = line.trim_end();
    [ADD, DELETE, UPDATE, MOVE_TO].iter().any(|m| trimmed.starts_with(m))
        || trimmed == BEGIN
        || trimmed == END
}

/// Apply every chunk to `original`, or explain which one did not fit.
fn apply_chunks(original: &str, chunks: &[Chunk]) -> Result<String, String> {
    let ends_with_newline = original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let mut search_from = 0;

    for chunk in chunks {
        let at = seek(&lines, &chunk.old, search_from, chunk.at_eof).ok_or_else(|| {
            let first = chunk.old.first().map(String::as_str).unwrap_or("");
            format!(
                "no match for the context starting {first:?}. \
                 Context lines must reproduce the file exactly, including indentation."
            )
        })?;

        lines.splice(at..at + chunk.old.len(), chunk.new.iter().cloned());
        // Chunks are ordered, so the next one starts after this one's result.
        // Without this a repeated block matches the same place twice.
        search_from = at + chunk.new.len();
    }

    let mut out = lines.join("\n");
    if ends_with_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Find `pattern` in `lines` at or after `start`, trying decreasing strictness.
///
/// Exact first, then ignoring trailing whitespace, then ignoring leading and
/// trailing. The looser passes exist because a model reproducing context by
/// hand gets indentation subtly wrong often enough to matter, and refusing on
/// that costs a turn to fix something that was never ambiguous.
fn seek(lines: &[String], pattern: &[String], start: usize, at_eof: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(if at_eof { lines.len() } else { start });
    }
    if pattern.len() > lines.len() {
        return None;
    }

    let last = lines.len() - pattern.len();
    // An end-anchored chunk is looked for at the end first, so a pattern meant
    // to match the file's tail is not captured by an identical run earlier.
    let first_try = if at_eof { last } else { start };

    for compare in [
        |a: &str, b: &str| a == b,
        |a: &str, b: &str| a.trim_end() == b.trim_end(),
        |a: &str, b: &str| a.trim() == b.trim(),
    ] {
        for offset in first_try..=last {
            if pattern
                .iter()
                .enumerate()
                .all(|(index, want)| compare(&lines[offset + index], want))
            {
                return Some(offset);
            }
        }
        // The end-anchored attempt starts late; fall back to a full scan before
        // loosening further, or a tail pattern that also appears earlier is missed.
        if at_eof {
            for offset in start..=last {
                if pattern
                    .iter()
                    .enumerate()
                    .all(|(index, want)| compare(&lines[offset + index], want))
                {
                    return Some(offset);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, workspace};

    async fn apply(root: &camino::Utf8PathBuf, patch: &str) -> Result<ToolOutcome, ToolError> {
        let tracker = Arc::new(FileTracker::new());
        let tool = ApplyPatchTool::new(tracker);
        let input = serde_json::json!({ "patch": patch }).to_string();
        tool.execute(&input, &context(root)).await
    }

    /// The property the whole tool is built around. A patch that fails partway
    /// must leave the tree exactly as it was — a half-applied refactor does not
    /// compile and nobody knows how far it got.
    #[tokio::test]
    async fn a_patch_that_fails_partway_changes_nothing_at_all() {
        let (_guard, root) = workspace();
        let error = apply(
            &root,
            "*** Begin Patch\n\
             *** Add File: first.rs\n\
             +fn first() {}\n\
             *** Delete File: absent.rs\n\
             *** End Patch\n",
        )
        .await
        .expect_err("the delete cannot resolve");

        assert!(matches!(error, ToolError::Recoverable(_)), "the model can fix this");
        assert!(
            !root.join("first.rs").exists(),
            "the earlier hunk must not have been written",
        );
    }

    #[tokio::test]
    async fn one_patch_can_add_several_files() {
        let (_guard, root) = workspace();
        apply(
            &root,
            "*** Begin Patch\n\
             *** Add File: a.rs\n\
             +fn a() {}\n\
             *** Add File: nested/b.rs\n\
             +fn b() {}\n\
             *** End Patch\n",
        )
        .await
        .expect("applies");

        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "fn a() {}\n");
        // Parent directories are created, or a patch adding a module fails on
        // the directory rather than the file.
        assert_eq!(std::fs::read_to_string(root.join("nested/b.rs")).unwrap(), "fn b() {}\n");
    }

    /// Read-before-write, as `edit` enforces: a patch written against a file
    /// the model never read is a guess about its contents.
    #[tokio::test]
    async fn updating_a_file_nobody_read_is_refused() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("seen.rs"), "one\n").unwrap();

        let error = apply(
            &root,
            "*** Begin Patch\n\
             *** Update File: seen.rs\n\
             @@\n\
             -one\n\
             +two\n\
             *** End Patch\n",
        )
        .await
        .expect_err("the tracker has no record of a read");
        assert!(matches!(error, ToolError::Recoverable(_)));
        assert_eq!(std::fs::read_to_string(root.join("seen.rs")).unwrap(), "one\n");
    }

    /// Every written path is declared before anything runs, so the policy sees
    /// the whole blast radius rather than approving it one file at a time.
    #[test]
    fn every_path_the_patch_writes_is_declared_up_front() {
        let (_guard, root) = workspace();
        let tool = ApplyPatchTool::new(Arc::new(FileTracker::new()));
        let input = serde_json::json!({
            "patch": "*** Begin Patch\n\
                      *** Add File: a.rs\n\
                      +x\n\
                      *** Delete File: b.rs\n\
                      *** End Patch\n"
        })
        .to_string();

        let resources = tool.required_permissions(&input, &context(&root));
        assert_eq!(resources.len(), 2, "{resources:?}");
        assert!(resources.iter().any(|r| r.contains("a.rs")));
        assert!(resources.iter().any(|r| r.contains("b.rs")));
    }


    fn update(body: &str) -> Vec<Hunk> {
        parse(&format!("{BEGIN}\n{body}{END}\n")).expect("parses")
    }

    #[test]
    fn an_update_becomes_context_removals_and_additions() {
        let hunks = update("*** Update File: a.rs\n@@ fn main\n keep\n-gone\n+added\n");
        assert_eq!(
            hunks,
            vec![Hunk::Update {
                path: "a.rs".into(),
                move_to: None,
                chunks: vec![Chunk {
                    old: vec!["keep".into(), "gone".into()],
                    new: vec!["keep".into(), "added".into()],
                    at_eof: false,
                }],
            }]
        );
    }

    #[test]
    fn add_and_delete_and_move_are_recognised() {
        let hunks = update("*** Add File: new.rs\n+fn x() {}\n*** Delete File: old.rs\n");
        assert_eq!(hunks[0], Hunk::Add { path: "new.rs".into(), contents: "fn x() {}\n".into() });
        assert_eq!(hunks[1], Hunk::Delete { path: "old.rs".into() });

        let moved = update("*** Update File: a.rs\n*** Move to: b.rs\n@@\n-x\n+y\n");
        assert!(matches!(&moved[0], Hunk::Update { move_to: Some(to), .. } if to == "b.rs"));
    }

    #[test]
    fn a_patch_without_its_markers_is_refused() {
        assert!(parse("*** Update File: a.rs\n-x\n+y\n").is_err());
        assert!(parse(&format!("{BEGIN}\n*** Update File: a.rs\nnonsense\n{END}\n")).is_err());
    }

    #[test]
    fn a_chunk_applies_where_its_context_matches() {
        let original = "one\ntwo\nthree\n";
        let chunks = vec![Chunk {
            old: vec!["two".into()],
            new: vec!["TWO".into()],
            at_eof: false,
        }];
        assert_eq!(apply_chunks(original, &chunks).unwrap(), "one\nTWO\nthree\n");
    }

    /// The looser passes are the point: a model reproducing context by hand
    /// gets indentation subtly wrong often enough that refusing costs a turn.
    #[test]
    fn context_still_matches_when_the_model_mangles_indentation() {
        let original = "fn main() {\n    let x = 1;\n}\n";
        let chunks = vec![Chunk {
            old: vec!["let x = 1;".into()], // the model dropped the indent
            new: vec!["    let x = 2;".into()],
            at_eof: false,
        }];
        assert_eq!(apply_chunks(original, &chunks).unwrap(), "fn main() {\n    let x = 2;\n}\n");
    }

    /// Two chunks touching identical text must land in different places, or the
    /// second silently rewrites the first.
    #[test]
    fn a_later_chunk_cannot_match_where_an_earlier_one_already_applied() {
        let original = "dup\nmiddle\ndup\n";
        let chunks = vec![
            Chunk { old: vec!["dup".into()], new: vec!["first".into()], at_eof: false },
            Chunk { old: vec!["dup".into()], new: vec!["second".into()], at_eof: false },
        ];
        assert_eq!(apply_chunks(original, &chunks).unwrap(), "first\nmiddle\nsecond\n");
    }

    #[test]
    fn context_that_matches_nothing_is_an_error_rather_than_a_guess() {
        let chunks = vec![Chunk {
            old: vec!["absent".into()],
            new: vec!["x".into()],
            at_eof: false,
        }];
        assert!(apply_chunks("one\ntwo\n", &chunks).is_err());
    }

    /// A file with no trailing newline must not gain one, and one with a
    /// trailing newline must not lose it — both show up as spurious diff noise.
    #[test]
    fn the_files_trailing_newline_is_preserved_either_way() {
        let chunks = vec![Chunk { old: vec!["a".into()], new: vec!["b".into()], at_eof: false }];
        assert_eq!(apply_chunks("a\n", &chunks).unwrap(), "b\n");
        assert_eq!(apply_chunks("a", &chunks).unwrap(), "b");
    }

    #[test]
    fn a_pattern_longer_than_the_file_does_not_panic() {
        let lines = vec!["only".to_string()];
        assert_eq!(seek(&lines, &["a".into(), "b".into(), "c".into()], 0, false), None);
    }
}
