//! A discovered command.

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// Optional frontmatter. Everything here is metadata for the picker or a
/// per-command override; the body carries the actual prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandFrontmatter {
    /// Shown in the `/` picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Usage hint, e.g. `<pr-number> [--verbose]`.
    #[serde(default, rename = "argument-hint", skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,

    /// Tool restrictions for this command's turn. Can only narrow the active
    /// policy, never widen it.
    #[serde(default, rename = "allowed-tools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<String>,

    /// Override the model for this command — a cheap model for a mechanical
    /// command, a stronger one for review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Run in a subagent with its own context window instead of inline.
    #[serde(default)]
    pub subagent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Invocation name without the leading slash. Namespaced with `:` when the
    /// file sits in a subdirectory.
    pub name: String,
    pub frontmatter: CommandFrontmatter,
    /// Prompt template, before substitution.
    pub template: String,
    pub path: Utf8PathBuf,
    /// Project commands shadow user commands of the same name.
    pub is_project_local: bool,
}

impl Command {
    /// Highest positional placeholder (`$1`, `$2`, …) the template refers to.
    ///
    /// Used to reject an invocation before running any shell substitution, so a
    /// missing argument is a clean error rather than a command that silently
    /// operates on the empty string.
    pub fn required_positional_count(&self) -> usize {
        let mut highest = 0;
        let bytes = self.template.as_bytes();
        for (index, window) in bytes.windows(2).enumerate() {
            if window[0] != b'$' || !window[1].is_ascii_digit() {
                continue;
            }
            // Guard against `$$1` and against matching the digits of `$12`
            // twice by requiring the `$` to not be escaped.
            if index > 0 && bytes[index - 1] == b'$' {
                continue;
            }
            let digits: String = self.template[index + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse::<usize>() {
                highest = highest.max(n);
            }
        }
        highest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(template: &str) -> Command {
        Command {
            name: "t".into(),
            frontmatter: CommandFrontmatter::default(),
            template: template.into(),
            path: "/x/t.md".into(),
            is_project_local: false,
        }
    }

    #[test]
    fn counts_highest_positional() {
        assert_eq!(command("no placeholders").required_positional_count(), 0);
        assert_eq!(command("review $1").required_positional_count(), 1);
        assert_eq!(command("$2 then $1").required_positional_count(), 2);
        assert_eq!(command("$12").required_positional_count(), 12);
    }

    #[test]
    fn escaped_dollar_is_not_a_placeholder() {
        assert_eq!(command("cost is $$100").required_positional_count(), 0);
    }

}
