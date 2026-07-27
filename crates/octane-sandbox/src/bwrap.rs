//! Linux containment, via bubblewrap.
//!
//! # Provenance
//!
//! The mount **ordering** below is derived from OpenAI Codex
//! (`codex-rs/linux-sandbox/src/bwrap.rs`), Copyright 2025 OpenAI, licensed
//! under the Apache License 2.0 — see `LICENSE-APACHE` and `NOTICE`. What was
//! taken is the recipe, which is the part that is hard to get right and easy to
//! get subtly wrong; the code is written against octane's own [`SandboxPolicy`]
//! rather than copied, and octane's policy model is the simpler of the two.
//!
//! # Why bubblewrap rather than Landlock directly
//!
//! Landlock is applied by the process to itself, which means a helper binary
//! that restricts itself and then `exec`s the real command. `bwrap` is an
//! ordinary program that takes arguments, which fits the same shape the macOS
//! backend already has: build an argv, hand it back, let the caller spawn it.
//! One model for both platforms is worth more here than the last few percent of
//! confinement fidelity.
//!
//! # The ordering, and why it is not arbitrary
//!
//! 1. A read baseline: `--ro-bind / /` — everything readable, nothing writable.
//! 2. `--dev /dev`, which gives the minimal device nodes (`null`, `zero`,
//!    `random`, `urandom`, `tty`). It must come *after* the root bind, or the
//!    root bind covers it, and *before* the writable roots, so an explicit
//!    writable path under `/dev` still wins.
//! 3. `--bind <root> <root>` per writable root, re-enabling writes.
//! 4. `--ro-bind <sub> <sub>` per read-only subpath, re-applying protection
//!    *inside* a writable root. This must come last: the mount that lands later
//!    is the one that takes effect, and these are the carve-outs that stop
//!    "write files in the project" from reaching `.git/hooks`.
//!
//! Get 3 and 4 the wrong way round and the sandbox reports success while
//! granting exactly what the carve-outs exist to deny.

use camino::Utf8Path;

use crate::policy::{NetworkPolicy, SandboxPolicy};
use crate::{SandboxError, SandboxedCommand};

/// Resolved through `PATH`, unlike macOS's `sandbox-exec`.
///
/// `sandbox-exec` lives at a fixed path on every macOS install, so hardcoding it
/// closes a PATH-impersonation hole for free. `bwrap` has no such path — it is
/// `/usr/bin/bwrap` on Debian, `/usr/bin/bwrap` on Fedora, and something else
/// entirely under Nix — so it must be resolved. A user who can write earlier
/// entries on their own `PATH` can already run code as themselves, so this
/// trades a hole that does not exist here for portability that does.
const BWRAP_PROGRAM: &str = "bwrap";

/// Build the `bwrap` invocation for `policy`.
pub fn wrap(
    command: &[String],
    policy: &SandboxPolicy,
    cwd: &Utf8Path,
) -> Result<SandboxedCommand, SandboxError> {
    let (writable_roots, network) = match policy {
        SandboxPolicy::ReadOnly { network } => ([].as_slice(), network),
        SandboxPolicy::WorkspaceWrite { writable_roots, network } => {
            (writable_roots.as_slice(), network)
        }
        SandboxPolicy::DangerFullAccess | SandboxPolicy::ExternalSandbox => {
            return Err(SandboxError::Policy(
                "an unconfined policy has no bwrap invocation".into(),
            ));
        }
    };

    let mut args = vec![
        // 1. Read baseline.
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        // 2. Minimal device tree, after the root bind so it is not covered.
        "--dev".to_string(),
        "/dev".to_string(),
        // A private /proc, so the process cannot inspect or signal the rest of
        // the session. Paired with --unshare-pid, which is what makes it honest.
        "--proc".to_string(),
        "/proc".to_string(),
        "--unshare-user".to_string(),
        "--unshare-pid".to_string(),
        // Reaped by bwrap when it exits, so a backgrounded child cannot outlive
        // the sandbox that was containing it.
        "--die-with-parent".to_string(),
    ];

    if matches!(network, NetworkPolicy::Denied) {
        args.push("--unshare-net".to_string());
    }

    // 3. Writable roots. A root that does not exist is skipped rather than
    // fatal: bwrap refuses to bind a missing target, and a config listing a
    // path for another machine must not stop this one from running anything.
    for root in writable_roots {
        if !root.path.as_std_path().exists() {
            continue;
        }
        args.push("--bind".to_string());
        args.push(root.path.to_string());
        args.push(root.path.to_string());
    }

    // 4. Read-only carve-outs, last so they win over the writable root they sit
    // inside. This is the ordering the whole module exists to get right.
    for root in writable_roots {
        for subpath in &root.read_only_subpaths {
            if !subpath.as_std_path().exists() {
                continue;
            }
            args.push("--ro-bind".to_string());
            args.push(subpath.to_string());
            args.push(subpath.to_string());
        }
    }

    args.push("--chdir".to_string());
    args.push(cwd.to_string());

    args.push("--".to_string());
    args.extend(command.iter().cloned());

    Ok(SandboxedCommand {
        program: BWRAP_PROGRAM.to_string(),
        args,
        env: vec![("OCTANE_SANDBOX".to_string(), "bwrap".to_string())],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::WritableRoot;

    fn policy() -> SandboxPolicy {
        SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![WritableRoot {
                path: "/".into(),
                read_only_subpaths: vec!["/etc".into()],
            }],
            network: NetworkPolicy::Denied,
        }
    }

    fn args_of(policy: &SandboxPolicy) -> Vec<String> {
        wrap(&["true".to_string()], policy, "/".into()).unwrap().args
    }

    fn position(args: &[String], flag: &str, target: &str) -> Option<usize> {
        args.windows(3).position(|w| w[0] == flag && w[1] == target)
    }

    /// The whole point of the module. A carve-out that is applied *before* the
    /// writable root it sits inside is silently overwritten by it, and the
    /// sandbox then grants exactly what the carve-out existed to deny — while
    /// reporting success.
    #[test]
    fn a_read_only_subpath_is_bound_after_the_writable_root_that_contains_it() {
        let args = args_of(&policy());
        let write = position(&args, "--bind", "/").expect("writable root is bound");
        let carve = position(&args, "--ro-bind", "/etc").expect("carve-out is bound");
        assert!(carve > write, "the carve-out must land last: {args:?}");
    }

    /// `--dev` after the root bind, or the root bind covers it and the command
    /// has no `/dev/null`.
    #[test]
    fn the_device_tree_is_mounted_after_the_read_baseline() {
        let args = args_of(&policy());
        let root = position(&args, "--ro-bind", "/").expect("read baseline");
        let dev = args.iter().position(|a| a == "--dev").expect("device tree");
        assert!(dev > root, "{args:?}");
    }

    #[test]
    fn denying_the_network_unshares_it_and_allowing_it_does_not() {
        assert!(args_of(&policy()).iter().any(|a| a == "--unshare-net"));

        let allowed = SandboxPolicy::ReadOnly { network: NetworkPolicy::Allowed };
        assert!(!args_of(&allowed).iter().any(|a| a == "--unshare-net"));
    }

    /// Read-only means no `--bind` at all, not a bind of nothing.
    #[test]
    fn a_read_only_policy_makes_no_path_writable() {
        let args = args_of(&SandboxPolicy::ReadOnly { network: NetworkPolicy::Denied });
        assert!(!args.iter().any(|a| a == "--bind"), "{args:?}");
    }

    /// The command must sit after `--`, or bwrap reads it as its own flags.
    #[test]
    fn the_command_is_separated_from_the_sandbox_flags() {
        let wrapped =
            wrap(&["echo".into(), "--unshare-net".into()], &policy(), "/".into()).unwrap();
        let separator = wrapped.args.iter().position(|a| a == "--").expect("separator");
        // The argument that looks like a bwrap flag must be past it, so it is
        // never read as one.
        assert!(wrapped.args[separator + 1..].contains(&"--unshare-net".to_string()));
        assert_eq!(wrapped.args[separator + 1], "echo");
    }

    #[test]
    fn an_unconfined_policy_has_no_invocation_rather_than_an_empty_one() {
        assert!(wrap(&["true".into()], &SandboxPolicy::DangerFullAccess, "/".into()).is_err());
    }
}
