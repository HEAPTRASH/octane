//! Windows containment, and the honest state of it.
//!
//! # Why there is no wrapper here
//!
//! macOS and Linux both hand back an argv: `sandbox-exec -p … -- cmd` and
//! `bwrap … -- cmd`. Windows has no equivalent program. The mechanisms it does
//! offer — a restricted token, an AppContainer, a job object — are applied by
//! the *creating* process through Win32 calls at `CreateProcess` time, so they
//! cannot be expressed as a command to run.
//!
//! That alone would just be a porting cost. The blocking problem is narrower
//! and worse:
//!
//! **A restricted token cannot express [`WritableRoot::read_only_subpaths`].**
//! It lowers what the process is, not what a particular path permits. So a
//! restricted-token backend would honour "write only inside the workspace" and
//! silently ignore "except `.git/` and `.octane/`" — the carve-outs that stop a
//! granted write from reaching `.git/hooks/pre-commit` and becoming arbitrary
//! code on the next commit. `octane-tools`'s
//! `git_hooks_cannot_be_written_even_though_they_are_inside_the_workspace`
//! exists for exactly that, and a backend that fails it while reporting success
//! is worse than no backend: the user is told they are contained.
//!
//! Doing it properly means per-path deny ACEs on the workspace's protected
//! subpaths, which is what Codex does (`codex-rs/windows-sandbox-rs`, ~11.9k
//! lines built on raw `ACL`/token APIs and pervasively `unsafe`). octane forbids
//! `unsafe` workspace-wide, so that work belongs in a crate that deliberately
//! opts out, and it cannot be trusted until it runs on Windows CI.
//!
//! # What this module does instead
//!
//! Two things, both of which are real:
//!
//! 1. Recognises when the process is *already* inside containment someone else
//!    manages — a container, or WSL — where the right answer is not to
//!    double-wrap. That case is genuinely handled, not refused.
//! 2. Refuses everything else with an error naming the actual options, rather
//!    than the generic "unsupported platform" a user cannot act on.
//!
//! Failing closed is the invariant (`ARCHITECTURE.md`): an unsupported platform
//! is an error, never a silent pass.

use crate::SandboxError;

/// Whether this process is already inside containment someone else manages.
///
/// Checked so a user running octane in a container on Windows gets a working
/// session instead of a refusal: the container *is* the sandbox, and wrapping
/// it again would be both impossible and pointless.
///
/// `lookup` is injected because `unsafe_code` is forbidden workspace-wide,
/// which rules out `std::env::set_var` and so rules out a test that sets one.
pub fn external_containment(lookup: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    // Set by the Windows container runtimes and by CI images that run inside
    // one. Any of them means an outer boundary already exists.
    for name in ["OCTANE_EXTERNAL_SANDBOX", "CONTAINER", "WSL_DISTRO_NAME"] {
        if lookup(name).is_some_and(|value| !value.trim().is_empty()) {
            return Some(match name {
                "WSL_DISTRO_NAME" => "WSL",
                _ => "a container",
            });
        }
    }
    None
}

/// The error a Windows user gets, and what they can do about it.
pub fn unavailable() -> SandboxError {
    SandboxError::MissingHelper(
        "no sandbox is available on Windows yet, and octane will not run commands \
         unconfined. Run inside WSL2 or a container — octane detects both and \
         defers to them — or set `sandbox = false` in settings to accept the risk \
         deliberately."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(
        pairs: &'a [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn wsl_and_containers_are_recognised_as_someone_elses_sandbox() {
        assert_eq!(external_containment(env(&[("WSL_DISTRO_NAME", "Ubuntu")])), Some("WSL"));
        assert_eq!(external_containment(env(&[("CONTAINER", "1")])), Some("a container"));
    }

    /// An empty variable is not containment. Shells export empty strings freely,
    /// and reading one as "already sandboxed" would turn a stray `CONTAINER=`
    /// into running unconfined — the exact failure this crate exists to prevent.
    #[test]
    fn an_empty_variable_is_not_treated_as_containment() {
        assert_eq!(external_containment(env(&[("CONTAINER", "")])), None);
        assert_eq!(external_containment(env(&[("CONTAINER", "   ")])), None);
        assert_eq!(external_containment(env(&[])), None);
    }

    /// The message has to name a way forward. "Unsupported platform" leaves a
    /// user with a tool that refuses to work and no idea why.
    #[test]
    fn the_refusal_names_what_the_user_can_actually_do() {
        let message = unavailable().to_string();
        assert!(message.contains("WSL2"), "{message}");
        assert!(message.contains("sandbox = false"), "{message}");
    }
}
