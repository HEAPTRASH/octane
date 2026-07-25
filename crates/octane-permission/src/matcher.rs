//! Pattern matching, one strategy per action kind.
//!
//! Each action matches differently, and getting any of them subtly wrong is a
//! security bug rather than an inconvenience:
//!
//! - **paths** match by subtree, so a grant on a directory covers what's under it
//! - **domains** match host and subdomains, ignoring path and scheme
//! - **commands** match by *per-token anchored regex*, which is the subtle one
//! - **mcp** matches `server/tool` with `server/*` wildcards

use camino::Utf8Path;
use regex::Regex;

use crate::resource::{Action, Resource};

/// A compiled rule pattern, ready to test against concrete resources.
#[derive(Debug, Clone)]
pub enum RuleMatcher {
    /// `*` — every target within this action's namespace.
    Any,
    /// Path subtree match.
    Path(String),
    /// Hostname and its subdomains.
    Domain(String),
    /// One anchored regex per whitespace-separated token.
    CommandTokens(Vec<Regex>),
    /// `server/tool`, where `tool` may be `*`.
    McpTool { server: String, tool: String },
}

impl RuleMatcher {
    /// Compile a rule pattern for a given action.
    ///
    /// A pattern that fails to compile becomes [`RuleMatcher::Path`] over the
    /// literal text rather than an error: a typo'd rule that matches nothing is
    /// a fail-closed outcome, while refusing to load the config would leave the
    /// user with *no* policy at all.
    pub fn compile(action: Action, pattern: &str) -> Self {
        if pattern == "*" {
            return Self::Any;
        }

        match action {
            Action::ReadFile | Action::WriteFile => Self::Path(normalize_path(pattern)),
            Action::ReadUrl | Action::ExecuteUrl => Self::Domain(pattern.trim().to_lowercase()),
            Action::Command | Action::Unsandboxed => Self::CommandTokens(compile_tokens(pattern)),
            Action::Mcp => {
                let (server, tool) = pattern.split_once('/').unwrap_or((pattern, "*"));
                Self::McpTool { server: server.to_string(), tool: tool.to_string() }
            }
        }
    }

    pub fn matches(&self, resource: &Resource) -> bool {
        match self {
            Self::Any => true,
            Self::Path(prefix) => path_contains(prefix, &normalize_path(&resource.target)),
            Self::Domain(domain) => domain_matches(domain, &resource.target),
            Self::CommandTokens(tokens) => command_matches(tokens, &resource.target),
            Self::McpTool { server, tool } => {
                let (res_server, res_tool) =
                    resource.target.split_once('/').unwrap_or((resource.target.as_str(), ""));
                server == res_server && (tool == "*" || tool == res_tool)
            }
        }
    }
}

/// Normalize for cross-platform comparison: backslashes to forward slashes,
/// drive letters stripped, trailing separator removed.
///
/// Windows users should not need different rules than everyone else, and a rule
/// that silently fails to match on one platform is worse than one that errors.
fn normalize_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let without_drive = match unified.as_bytes() {
        [drive, b':', b'/', ..] if drive.is_ascii_alphabetic() => unified[2..].to_string(),
        _ => unified,
    };
    let trimmed = without_drive.trim_end_matches('/');
    if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() }
}

/// Subtree containment, compared component-wise.
///
/// String prefixing would be wrong: `/proj` must not match `/project-secrets`.
fn path_contains(prefix: &str, candidate: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    let prefix = Utf8Path::new(prefix);
    let candidate = Utf8Path::new(candidate);
    candidate == prefix || candidate.starts_with(prefix)
}

/// Host match including subdomains, ignoring scheme and path.
///
/// `google.com` covers `mail.google.com`. The leading-dot check is what stops
/// `google.com` from also covering `notgoogle.com`.
fn domain_matches(domain: &str, target: &str) -> bool {
    let host = target
        .trim()
        .to_lowercase()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string();

    host == domain || host.ends_with(&format!(".{domain}"))
}

/// One anchored regex per token in the pattern.
fn compile_tokens(pattern: &str) -> Vec<Regex> {
    pattern
        .split_whitespace()
        .map(|token| {
            // Anchored so a token pattern cannot match a substring of a longer
            // argument: `command(git)` must not authorize `gitleaks`.
            Regex::new(&format!("^(?:{token})$"))
                .unwrap_or_else(|_| Regex::new(&format!("^{}$", regex::escape(token))).expect("escaped literal is a valid regex"))
        })
        .collect()
}

/// Token-prefix match against a command line.
///
/// The pattern matches if every one of its tokens matches the command's tokens
/// at the same position — i.e. the pattern is a *prefix* of the command.
///
/// This is deliberately narrow. `command(npm run (build|test))` authorizes
/// `npm run build`, but the extra tokens in `npm run build --prod` are *not*
/// covered by the pattern, so it does not match and the user is asked. Being
/// stricter than the user expects costs a prompt; being looser costs trust.
///
/// Note what this does **not** do: it has no idea that `npm run build; curl evil`
/// contains two commands. Shell metacharacters must be handled by parsing the
/// command into an AST *before* it reaches the policy engine. See `RESEARCH.md`
/// §2 on Claude Code's bash parser — a matcher alone is not a security boundary.
fn command_matches(tokens: &[Regex], command: &str) -> bool {
    let actual: Vec<&str> = command.split_whitespace().collect();
    if tokens.is_empty() || tokens.len() != actual.len() {
        return false;
    }
    tokens.iter().zip(&actual).all(|(pattern, token)| pattern.is_match(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(action: Action, pattern: &str, target: &str) -> bool {
        RuleMatcher::compile(action, pattern).matches(&Resource::new(action, target))
    }

    #[test]
    fn wildcard_matches_everything() {
        assert!(matches(Action::Command, "*", "anything at all"));
        assert!(matches(Action::ReadFile, "*", "/etc/passwd"));
    }

    #[test]
    fn path_matches_subtree_not_string_prefix() {
        assert!(matches(Action::ReadFile, "/proj", "/proj/src/main.rs"));
        assert!(matches(Action::ReadFile, "/proj", "/proj"));
        // The bug this guards against.
        assert!(!matches(Action::ReadFile, "/proj", "/project-secrets/key"));
        assert!(!matches(Action::ReadFile, "/proj", "/other/file"));
    }

    #[test]
    fn path_normalizes_windows_separators() {
        assert!(matches(Action::WriteFile, r"C:\proj", "/proj/src/lib.rs"));
        assert!(matches(Action::WriteFile, "/proj/", "/proj/src"));
    }

    #[test]
    fn domain_covers_subdomains_only() {
        assert!(matches(Action::ReadUrl, "google.com", "mail.google.com"));
        assert!(matches(Action::ReadUrl, "google.com", "https://google.com/search?q=x"));
        assert!(!matches(Action::ReadUrl, "google.com", "notgoogle.com"));
        assert!(!matches(Action::ReadUrl, "google.com", "google.com.evil.tld"));
    }

    #[test]
    fn command_tokens_are_anchored() {
        assert!(matches(Action::Command, "git", "git"));
        // Anchoring is what stops this.
        assert!(!matches(Action::Command, "git", "gitleaks"));
    }

    #[test]
    fn command_alternation_within_a_token() {
        assert!(matches(Action::Command, "npm run (build|lint|test)", "npm run build"));
        assert!(matches(Action::Command, "npm run (build|lint|test)", "npm run test"));
        assert!(!matches(Action::Command, "npm run (build|lint|test)", "npm run deploy"));
    }

    #[test]
    fn command_arity_must_agree() {
        assert!(!matches(Action::Command, "git", "git push origin main"));
        assert!(!matches(Action::Command, "git push origin main", "git push"));
    }

    #[test]
    fn mcp_server_wildcard() {
        assert!(matches(Action::Mcp, "linter/*", "linter/check"));
        assert!(matches(Action::Mcp, "sql/execute", "sql/execute"));
        assert!(!matches(Action::Mcp, "linter/*", "sql/execute"));
        assert!(!matches(Action::Mcp, "sql/execute", "sql/mutate"));
    }
}
