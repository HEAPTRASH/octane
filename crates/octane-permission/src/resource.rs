//! `action(target)` — the vocabulary the engine reasons about.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::PermissionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    ReadFile,
    WriteFile,
    ReadUrl,
    /// Actuating on a page — clicking, typing, submitting — as opposed to
    /// reading it. Separate because the blast radius is completely different.
    ExecuteUrl,
    /// Run a shell command.
    Command,
    /// Run a command *outside* containment. Needed for the handful of things a
    /// sandbox legitimately breaks, e.g. `git push`.
    Unsandboxed,
    /// Call an MCP tool, targeted as `server/tool`.
    Mcp,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::ReadUrl => "read_url",
            Self::ExecuteUrl => "execute_url",
            Self::Command => "command",
            Self::Unsandboxed => "unsandboxed",
            Self::Mcp => "mcp",
        }
    }

    /// How an unmatched resource is treated.
    ///
    /// Everything defaults to `Ask`. Fail-closed is the only defensible default
    /// for a program that runs shell commands on someone else's machine — the
    /// cost of an unnecessary prompt is a keystroke; the cost of an unnecessary
    /// `rm -rf` is a weekend.
    pub fn default_decision(self) -> crate::policy::Decision {
        crate::policy::Decision::Ask
    }
}

impl FromStr for Action {
    type Err = PermissionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "read_file" => Self::ReadFile,
            "write_file" => Self::WriteFile,
            "read_url" => Self::ReadUrl,
            "execute_url" => Self::ExecuteUrl,
            "command" => Self::Command,
            "unsandboxed" => Self::Unsandboxed,
            "mcp" => Self::Mcp,
            other => return Err(PermissionError::UnknownAction(other.to_string())),
        })
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A concrete operation awaiting a decision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Resource {
    pub action: Action,
    /// The concrete target: a resolved absolute path, a hostname, the full
    /// command line, or `server/tool`.
    pub target: String,
}

impl Resource {
    pub fn new(action: Action, target: impl Into<String>) -> Self {
        Self { action, target: target.into() }
    }

    pub fn read_file(path: impl Into<String>) -> Self {
        Self::new(Action::ReadFile, path)
    }

    pub fn write_file(path: impl Into<String>) -> Self {
        Self::new(Action::WriteFile, path)
    }

    pub fn command(cmd: impl Into<String>) -> Self {
        Self::new(Action::Command, cmd)
    }

    pub fn mcp(server: &str, tool: &str) -> Self {
        Self::new(Action::Mcp, format!("{server}/{tool}"))
    }

    /// Resources implied by granting this one.
    ///
    /// Write implies read: a caller that can replace a file's contents gains
    /// nothing from being denied the ability to look at them first, and forcing
    /// two prompts for one edit is how prompt fatigue starts.
    pub fn implied(&self) -> Vec<Resource> {
        match self.action {
            Action::WriteFile => vec![Resource::read_file(self.target.clone())],
            _ => Vec::new(),
        }
    }
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.action, self.target)
    }
}

impl FromStr for Resource {
    type Err = PermissionError;

    /// Parses `action(target)`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let malformed = |reason: &str| PermissionError::MalformedRule {
            rule: s.to_string(),
            reason: reason.to_string(),
        };

        let open = s.find('(').ok_or_else(|| malformed("expected action(target)"))?;
        if !s.ends_with(')') {
            return Err(malformed("missing closing parenthesis"));
        }

        let action: Action = s[..open].trim().parse()?;
        // Slice rather than trim_end_matches: a target may legitimately end in
        // ')', e.g. command(npm run (build|test)).
        let target = &s[open + 1..s.len() - 1];
        if target.is_empty() {
            return Err(malformed("empty target"));
        }

        Ok(Self::new(action, target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_resource() {
        let r: Resource = "command(git)".parse().unwrap();
        assert_eq!(r.action, Action::Command);
        assert_eq!(r.target, "git");
    }

    #[test]
    fn parses_target_containing_parens() {
        let r: Resource = "command(npm run (build|lint))".parse().unwrap();
        assert_eq!(r.target, "npm run (build|lint)");
    }

    #[test]
    fn round_trips_through_display() {
        for text in ["read_file(/etc/hosts)", "mcp(linter/*)", "write_file(src/)"] {
            let parsed: Resource = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
        }
    }

    #[test]
    fn rejects_malformed() {
        assert!("command".parse::<Resource>().is_err());
        assert!("command(".parse::<Resource>().is_err());
        assert!("command()".parse::<Resource>().is_err());
        assert!("nonsense(x)".parse::<Resource>().is_err());
    }

    #[test]
    fn write_implies_read() {
        let implied = Resource::write_file("/p/f.txt").implied();
        assert_eq!(implied, vec![Resource::read_file("/p/f.txt")]);
    }
}
