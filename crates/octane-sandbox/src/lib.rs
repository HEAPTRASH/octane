//! OS-level containment.
//!
//! This crate answers a different question from `octane-permission`. Policy asks
//! *should this be allowed?* and is enforced before a command runs. The sandbox
//! asks *what can this process reach if it does something other than what it
//! said it would?* and is enforced by the kernel while it runs.
//!
//! Both are necessary. Policy without containment trusts that a command does only
//! what its text suggests — `make test` can do anything. Containment without
//! policy silently blocks things and produces failures the model cannot diagnose.
//!
//! Every harness surveyed reached for native kernel facilities over containers,
//! because the overhead is near zero and there is no daemon to install:
//!
//! | Platform | Mechanism |
//! |---|---|
//! | macOS | Seatbelt via `/usr/bin/sandbox-exec` |
//! | Linux | bubblewrap + Landlock (≥5.13) + seccomp-bpf |
//! | Windows | restricted process token / AppContainer |

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod denial;
pub mod policy;
#[cfg(target_os = "macos")]
pub mod seatbelt;

pub use denial::is_likely_sandbox_denial;
pub use policy::{NetworkPolicy, SandboxPolicy, WritableRoot};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandboxing is not implemented for this platform; refusing to run unconfined")]
    UnsupportedPlatform,

    #[error("sandbox helper is missing: {0}")]
    MissingHelper(String),

    #[error("could not build sandbox policy: {0}")]
    Policy(String),
}

/// A command rewritten to run under containment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxedCommand {
    /// Program to spawn — the sandbox wrapper, not the user's command.
    pub program: String,
    pub args: Vec<String>,
    /// Environment additions, so a child can tell it is sandboxed and adjust
    /// (and so failures are attributable in logs).
    pub env: Vec<(String, String)>,
}

/// Wrap a command according to `policy`.
///
/// Returns `Ok(None)` only for [`SandboxPolicy::DangerFullAccess`] and
/// [`SandboxPolicy::ExternalSandbox`] — the latter because we are already inside
/// someone else's container and double-wrapping breaks more than it protects.
///
/// An unsupported platform is an **error, not a pass**. Silently running
/// unconfined because the OS is unfamiliar is precisely the failure mode a
/// sandbox exists to prevent.
pub fn wrap(
    command: &[String],
    policy: &SandboxPolicy,
    cwd: &camino::Utf8Path,
) -> Result<Option<SandboxedCommand>, SandboxError> {
    match policy {
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox => Ok(None),
        SandboxPolicy::ReadOnly { .. } | SandboxPolicy::WorkspaceWrite { .. } => {
            #[cfg(target_os = "macos")]
            {
                seatbelt::wrap(command, policy, cwd).map(Some)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (command, cwd);
                Err(SandboxError::UnsupportedPlatform)
            }
        }
    }
}
