//! macOS containment via Seatbelt.
//!
//! Apple marks `sandbox-exec` deprecated, and there is no supported replacement
//! for command-line process sandboxing — App Sandbox targets bundled apps, not
//! arbitrary child processes. Every agent harness on macOS uses this anyway, and
//! so do we, with the deprecation noted rather than pretended away.
//!
//! Two structural decisions, both copied from Codex because both are right:
//!
//! 1. The `sandbox-exec` path is **hardcoded**. Resolving it through `PATH` would
//!    let anything earlier on `PATH` impersonate the sandbox and silently disable
//!    containment — the failure would be invisible.
//! 2. Writable roots are passed as **`-D` parameters**, never interpolated into
//!    the policy text. A path containing a quote or paren would otherwise let an
//!    attacker-influenced filename rewrite the policy.

use camino::Utf8Path;

use crate::policy::{NetworkPolicy, SandboxPolicy};
use crate::{SandboxError, SandboxedCommand};

/// Hardcoded: see the module note.
const SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

/// Deny-by-default base profile.
///
/// Each `allow` below is present because something legitimate breaks without it,
/// noted inline. Additions should be justified the same way.
const BASE_PROFILE: &str = r#"(version 1)
(deny default)

; Children inherit this policy, so a shell may still spawn compilers and tests.
(allow process-exec)
(allow process-fork)

; Signals and process info within the sandbox only — enough for job control,
; not enough to inspect or kill anything outside.
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))

; CoreFoundation preference reads. Many tools fail to start without this.
(allow user-preference-read)

; Reads are unrestricted; confidentiality is enforced by policy, not by the
; sandbox. Containment here is about limiting *writes* and *network*.
(allow file-read*)

; /dev/null only, so redirection works without granting device writes.
(allow file-write-data
  (require-all
    (path "/dev/null")
    (vnode-type CHARACTER-DEVICE)))

; Build tools read machine facts to size their thread pools.
(allow sysctl-read
  (sysctl-name "hw.activecpu")
  (sysctl-name "hw.busfrequency_compat")
  (sysctl-name "hw.byteorder")
  (sysctl-name "hw.cachelinesize_compat")
  (sysctl-name "hw.logicalcpu_max")
  (sysctl-name "hw.memsize")
  (sysctl-name "hw.ncpu")
  (sysctl-name "hw.pagesize_compat")
  (sysctl-name "hw.physicalcpu_max")
  (sysctl-name "kern.hostname")
  (sysctl-name "kern.maxfilesperproc")
  (sysctl-name "kern.osrelease")
  (sysctl-name "kern.ostype")
  (sysctl-name "kern.osversion")
  (sysctl-name "kern.usrstack64")
  (sysctl-name "kern.version"))

; Python multiprocessing.
(allow ipc-posix-sem)

; openpty(), needed to run commands under a pty so programs behave as they do
; interactively (progress bars, colour, line buffering).
(allow pseudo-tty)
"#;

/// Extra grants required for TLS and DNS to work at all.
const NETWORK_PROFILE: &str = r#"
(allow network-outbound)
(allow network-inbound (local ip "localhost:*"))
(allow mach-lookup
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.SecurityServer")
  (global-name "com.apple.networkd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.dnssd.service"))
"#;

/// Build the `sandbox-exec` invocation for `command`.
pub fn wrap(
    command: &[String],
    policy: &SandboxPolicy,
    cwd: &Utf8Path,
) -> Result<SandboxedCommand, SandboxError> {
    if command.is_empty() {
        return Err(SandboxError::Policy("empty command".into()));
    }
    if !std::path::Path::new(SEATBELT_EXECUTABLE).exists() {
        return Err(SandboxError::MissingHelper(SEATBELT_EXECUTABLE.into()));
    }

    let (profile, params) = build_profile(policy);

    let mut args = vec!["-p".to_string(), profile];
    for (name, value) in params {
        // `-D NAME=VALUE` as separate argv entries: the value never passes
        // through a shell, so no quoting is needed and none can be escaped.
        args.push(format!("-D{name}={value}"));
    }
    args.push("--".to_string());
    args.extend_from_slice(command);

    Ok(SandboxedCommand {
        program: SEATBELT_EXECUTABLE.to_string(),
        args,
        env: vec![
            // Lets a child process — and a confused model reading `env` — see
            // that it is contained, and makes failures attributable in logs.
            ("OCTANE_SANDBOX".to_string(), "seatbelt".to_string()),
            ("OCTANE_SANDBOX_CWD".to_string(), cwd.to_string()),
        ],
    })
}

/// Render the profile text and the `-D` parameter list.
///
/// Split out from [`wrap`] so it is testable without `/usr/bin/sandbox-exec`
/// present, which also means it is testable on Linux CI.
pub fn build_profile(policy: &SandboxPolicy) -> (String, Vec<(String, String)>) {
    let mut profile = String::from(BASE_PROFILE);
    let mut params: Vec<(String, String)> = Vec::new();

    let roots = policy.writable_roots();
    if !roots.is_empty() {
        profile.push_str("\n; Writable roots, passed as parameters.\n(allow file-write*\n");

        for (root_index, root) in roots.iter().enumerate() {
            let root_param = format!("WRITABLE_ROOT_{root_index}");
            params.push((root_param.clone(), root.path.to_string()));

            if root.read_only_subpaths.is_empty() {
                profile.push_str(&format!("  (subpath (param \"{root_param}\"))\n"));
                continue;
            }

            // require-all: inside the root AND outside every carve-out.
            profile.push_str("  (require-all\n");
            profile.push_str(&format!("    (subpath (param \"{root_param}\"))\n"));
            for (ro_index, ro_path) in root.read_only_subpaths.iter().enumerate() {
                let ro_param = format!("{root_param}_RO_{ro_index}");
                params.push((ro_param.clone(), ro_path.to_string()));
                profile.push_str(&format!(
                    "    (require-not (subpath (param \"{ro_param}\")))\n"
                ));
            }
            profile.push_str("  )\n");
        }
        profile.push_str(")\n");
    }

    match policy.network() {
        NetworkPolicy::Denied => {}
        NetworkPolicy::Allowed => profile.push_str(NETWORK_PROFILE),
        NetworkPolicy::ViaProxy { port } => {
            // Loopback proxy only. Everything else stays denied, so the proxy is
            // a genuine chokepoint rather than a suggestion.
            profile.push_str(&format!(
                "\n(allow network-outbound (remote ip \"localhost:{port}\"))\n"
            ));
        }
    }

    (profile, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::WritableRoot;

    #[test]
    fn base_profile_is_deny_by_default() {
        let (profile, _) = build_profile(&SandboxPolicy::ReadOnly { network: NetworkPolicy::Denied });
        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(!profile.contains("file-write*"));
    }

    #[test]
    fn writable_roots_become_parameters_never_literals() {
        let policy = SandboxPolicy::workspace("/proj", Some("/tmp".into()));
        let (profile, params) = build_profile(&policy);

        // The path must not appear in the policy text itself.
        assert!(!profile.contains("/proj"));
        assert!(profile.contains("(param \"WRITABLE_ROOT_0\")"));
        assert_eq!(params[0], ("WRITABLE_ROOT_0".to_string(), "/proj".to_string()));
    }

    #[test]
    fn git_and_config_are_carved_out_as_require_not() {
        let (profile, params) = build_profile(&SandboxPolicy::workspace("/proj", None));

        assert!(profile.contains("require-not"));
        let carved: Vec<&String> = params.iter().map(|(_, value)| value).collect();
        assert!(carved.contains(&&"/proj/.git".to_string()));
        assert!(carved.contains(&&"/proj/.octane".to_string()));
    }

    #[test]
    fn a_path_with_shell_metacharacters_cannot_reach_the_profile() {
        // A directory literally named `") (allow default) ("` would rewrite the
        // policy if paths were interpolated. Parameters make it inert.
        let evil = r#"/proj/") (allow default) (""#;
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![WritableRoot::plain(evil)],
            network: NetworkPolicy::Denied,
        };
        let (profile, params) = build_profile(&policy);

        assert!(!profile.contains("allow default"));
        assert_eq!(params[0].1, evil);
    }

    #[test]
    fn proxy_mode_allows_only_the_loopback_port() {
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network: NetworkPolicy::ViaProxy { port: 43128 },
        };
        let (profile, _) = build_profile(&policy);
        assert!(profile.contains(r#"(allow network-outbound (remote ip "localhost:43128"))"#));
        // Not the blanket grant.
        assert!(!profile.contains("(allow network-outbound)\n"));
    }
}
