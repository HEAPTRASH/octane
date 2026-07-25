//! Shared test fixtures.

use camino::Utf8PathBuf;
use octane_protocol::{SessionId, ToolCallId};

use crate::tool::ToolContext;

/// A real temporary directory, canonicalized so it compares equal to resolved
/// paths (on macOS `/tmp` is a symlink to `/private/tmp`, which otherwise makes
/// every containment assertion fail for the wrong reason).
pub fn workspace() -> (tempfile::TempDir, Utf8PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path =
        Utf8PathBuf::from_path_buf(std::fs::canonicalize(dir.path()).expect("canonicalize"))
            .expect("utf8 path");
    (dir, path)
}

pub fn context(root: &Utf8PathBuf) -> ToolContext {
    ToolContext {
        session_id: SessionId::new(),
        call_id: ToolCallId::new(),
        agent: "build".into(),
        workspace: root.clone(),
        cwd: root.clone(),
        cancel: Default::default(),
    }
}
