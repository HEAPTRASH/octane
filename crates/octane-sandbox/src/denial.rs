//! Recognizing sandbox denials.
//!
//! A kernel denial surfaces as an ordinary permission error, so the model reads
//! `EPERM` as "the command is wrong" and burns turns trying to fix code that is
//! fine. Detecting the pattern lets the loop say the true thing instead — *the
//! sandbox blocked this; widen the policy or approve an unsandboxed run* — which
//! is a question only the user can answer.
//!
//! This is a heuristic over error text, so it is deliberately conservative: a
//! false positive tells the user their policy is at fault when it isn't, which
//! erodes trust in every subsequent prompt.

/// Whether a failure was plausibly caused by containment rather than by the
/// command itself.
pub fn is_likely_sandbox_denial(exit_code: Option<i32>, stderr: &str) -> bool {
    // A clean exit is not a denial regardless of what was logged.
    if exit_code == Some(0) {
        return false;
    }

    let haystack = stderr.to_lowercase();

    const SIGNATURES: &[&str] = &[
        // Seatbelt
        "operation not permitted",
        "sandbox-exec",
        "deny file-write",
        "deny network-outbound",
        // Landlock / seccomp
        "permission denied (os error 13)",
        "landlock",
        "seccomp",
        "bad system call",
        // Network specifically, which is the most common denial in practice.
        "could not resolve host",
        "network is unreachable",
        "temporary failure in name resolution",
    ];

    SIGNATURES.iter().any(|signature| haystack.contains(signature))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_seatbelt_write_denial() {
        assert!(is_likely_sandbox_denial(
            Some(1),
            "error: failed to create /etc/foo: Operation not permitted"
        ));
    }

    #[test]
    fn detects_denied_network() {
        assert!(is_likely_sandbox_denial(Some(6), "curl: (6) Could not resolve host: example.com"));
    }

    #[test]
    fn a_successful_command_is_never_a_denial() {
        assert!(!is_likely_sandbox_denial(Some(0), "Operation not permitted"));
    }

    #[test]
    fn ordinary_failures_are_not_denials() {
        assert!(!is_likely_sandbox_denial(Some(1), "error[E0308]: mismatched types"));
        assert!(!is_likely_sandbox_denial(Some(101), "test failed: assertion left == right"));
    }
}
