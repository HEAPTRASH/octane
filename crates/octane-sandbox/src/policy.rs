//! What the sandbox is asked to enforce, as data.
//!
//! Kept serializable on purpose: on Linux the policy crosses a process boundary
//! into a helper binary as JSON, which is the right shape — the helper is a
//! separate trust domain and should receive data, not share memory.

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};

/// A directory the sandboxed process may write to, minus carve-outs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WritableRoot {
    pub path: Utf8PathBuf,
    /// Subpaths that stay **read-only inside** a writable root.
    ///
    /// This is the single most important detail in the whole crate. Without it,
    /// "write files in the project" transitively grants:
    ///
    /// - `.git/hooks/pre-commit` — arbitrary code on the user's next commit
    /// - `.octane/settings.json` — rewriting the policy that constrains the agent
    ///
    /// Both are privilege escalation reached through a permission the user
    /// reasonably granted. Codex carves out exactly these two.
    pub read_only_subpaths: Vec<Utf8PathBuf>,
}

impl WritableRoot {
    /// A project root with the standard carve-outs applied.
    pub fn project(path: impl Into<Utf8PathBuf>) -> Self {
        let path = path.into();
        Self {
            read_only_subpaths: vec![path.join(".git"), path.join(".octane")],
            path,
        }
    }

    /// A root with no carve-outs, e.g. a temp directory.
    pub fn plain(path: impl Into<Utf8PathBuf>) -> Self {
        Self { path: path.into(), read_only_subpaths: Vec::new() }
    }

    pub fn permits_write(&self, target: &Utf8Path) -> bool {
        let inside_root = target == self.path || target.starts_with(&self.path);
        let carved_out = self
            .read_only_subpaths
            .iter()
            .any(|ro| target == ro.as_path() || target.starts_with(ro));
        inside_root && !carved_out
    }
}

/// Network posture. Off by default: an agent that can reach the network can
/// exfiltrate anything it can read, which turns a read grant into a disclosure.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    #[default]
    Denied,
    /// Full outbound. Honest about what it costs.
    Allowed,
    /// Outbound only to a loopback proxy, which can then log and filter.
    /// The best of the three: the agent gets network, you keep an audit trail.
    ViaProxy { port: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SandboxPolicy {
    /// Read the filesystem, write nothing, no network.
    ReadOnly { network: NetworkPolicy },

    /// Write within specific roots only.
    WorkspaceWrite {
        writable_roots: Vec<WritableRoot>,
        network: NetworkPolicy,
    },

    /// No containment. Must be an explicit user choice, never a fallback.
    DangerFullAccess,

    /// Already inside a container or VM someone else manages. We do not
    /// double-wrap: nesting sandboxes mostly produces confusing failures.
    ExternalSandbox,
}

impl SandboxPolicy {
    /// The default for an interactive session in a project directory.
    pub fn workspace(project_root: impl Into<Utf8PathBuf>, tmpdir: Option<Utf8PathBuf>) -> Self {
        let mut writable_roots = vec![WritableRoot::project(project_root)];
        // Build tooling assumes a writable temp dir; denying it breaks cargo,
        // pytest, and most compilers for no security gain.
        if let Some(tmp) = tmpdir {
            writable_roots.push(WritableRoot::plain(tmp));
        }
        Self::WorkspaceWrite { writable_roots, network: NetworkPolicy::Denied }
    }

    pub fn network(&self) -> NetworkPolicy {
        match self {
            Self::ReadOnly { network } | Self::WorkspaceWrite { network, .. } => network.clone(),
            Self::DangerFullAccess => NetworkPolicy::Allowed,
            Self::ExternalSandbox => NetworkPolicy::Allowed,
        }
    }

    pub fn writable_roots(&self) -> &[WritableRoot] {
        match self {
            Self::WorkspaceWrite { writable_roots, .. } => writable_roots,
            _ => &[],
        }
    }

    pub fn permits_write(&self, target: &Utf8Path) -> bool {
        match self {
            Self::ReadOnly { .. } => false,
            Self::DangerFullAccess | Self::ExternalSandbox => true,
            Self::WorkspaceWrite { writable_roots, .. } => {
                writable_roots.iter().any(|root| root.permits_write(target))
            }
        }
    }

    pub fn is_contained(&self) -> bool {
        matches!(self, Self::ReadOnly { .. } | Self::WorkspaceWrite { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_carves_out_git_and_config() {
        let root = WritableRoot::project("/proj");
        assert!(root.permits_write(Utf8Path::new("/proj/src/main.rs")));
        // The escalation vectors.
        assert!(!root.permits_write(Utf8Path::new("/proj/.git/hooks/pre-commit")));
        assert!(!root.permits_write(Utf8Path::new("/proj/.octane/settings.json")));
    }

    #[test]
    fn writes_outside_roots_are_refused() {
        let policy = SandboxPolicy::workspace("/proj", None);
        assert!(policy.permits_write(Utf8Path::new("/proj/x")));
        assert!(!policy.permits_write(Utf8Path::new("/etc/hosts")));
        assert!(!policy.permits_write(Utf8Path::new("/proj-other/x")));
    }

    #[test]
    fn read_only_permits_nothing() {
        let policy = SandboxPolicy::ReadOnly { network: NetworkPolicy::Denied };
        assert!(!policy.permits_write(Utf8Path::new("/proj/x")));
        assert!(policy.is_contained());
    }

    #[test]
    fn network_defaults_to_denied() {
        assert_eq!(SandboxPolicy::workspace("/proj", None).network(), NetworkPolicy::Denied);
    }

    #[test]
    fn policy_survives_a_json_round_trip() {
        // The Linux helper receives it this way.
        let policy = SandboxPolicy::workspace("/proj", Some("/tmp".into()));
        let json = serde_json::to_string(&policy).unwrap();
        assert_eq!(serde_json::from_str::<SandboxPolicy>(&json).unwrap(), policy);
    }
}
