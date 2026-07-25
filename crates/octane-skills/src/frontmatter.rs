//! `SKILL.md` frontmatter parsing and validation.

use serde::{Deserialize, Serialize};

use crate::{SkillError, limits};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frontmatter {
    /// Must equal the parent directory name — see [`Frontmatter::validate`].
    pub name: String,

    /// Must say both *what the skill does* and *when to use it*.
    ///
    /// This field is the entire basis on which the model decides to activate a
    /// skill, and it is the only part loaded at startup. "Helps with PDFs" makes
    /// a skill dead weight; the description is the skill's interface.
    pub description: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,

    /// Environment requirements: required binaries, network access, runtimes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,

    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, String>,

    /// Space-separated pre-approved tools, e.g. `Bash(git:*) Read`.
    ///
    /// Experimental in the spec, and treated as a *request* here: it can only
    /// narrow what the active policy already permits, never widen it. A skill
    /// file in a cloned repository must not be able to grant itself shell access.
    #[serde(default, rename = "allowed-tools", skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<String>,
}

impl Frontmatter {
    /// Split a `SKILL.md` into frontmatter and body.
    pub fn parse(source: &str, path: &str) -> Result<(Self, String), SkillError> {
        let rest = source
            .strip_prefix("---")
            .ok_or_else(|| SkillError::MissingFrontmatter { path: path.to_string() })?;

        let (yaml, body) = rest
            .split_once("\n---")
            .ok_or_else(|| SkillError::MissingFrontmatter { path: path.to_string() })?;

        let frontmatter: Self = serde_yaml::from_str(yaml)
            .map_err(|source| SkillError::InvalidFrontmatter { path: path.to_string(), source })?;

        Ok((frontmatter, body.trim_start_matches('-').trim_start().to_string()))
    }

    /// Enforce the spec's constraints.
    ///
    /// `dir_name` must match `name`: without it, a skill directory could claim
    /// another skill's name and shadow it in the registry.
    pub fn validate(&self, path: &str, dir_name: &str) -> Result<(), SkillError> {
        let invalid = |reason: String| SkillError::Invalid { path: path.to_string(), reason };

        if self.name.is_empty() || self.name.len() > limits::NAME_MAX {
            return Err(invalid(format!("name must be 1-{} characters", limits::NAME_MAX)));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(invalid("name may contain only a-z, 0-9 and hyphens".into()));
        }
        if self.name.starts_with('-') || self.name.ends_with('-') {
            return Err(invalid("name must not start or end with a hyphen".into()));
        }
        if self.name.contains("--") {
            return Err(invalid("name must not contain consecutive hyphens".into()));
        }
        if self.name != dir_name {
            return Err(invalid(format!(
                "name {:?} must match its directory name {dir_name:?}",
                self.name
            )));
        }
        if self.description.trim().is_empty() {
            return Err(invalid("description must not be empty".into()));
        }
        if self.description.len() > limits::DESCRIPTION_MAX {
            return Err(invalid(format!(
                "description must be at most {} characters",
                limits::DESCRIPTION_MAX
            )));
        }
        if let Some(compat) = &self.compatibility {
            if compat.len() > limits::COMPATIBILITY_MAX {
                return Err(invalid(format!(
                    "compatibility must be at most {} characters",
                    limits::COMPATIBILITY_MAX
                )));
            }
        }
        Ok(())
    }

    /// `allowed-tools` split into individual entries.
    pub fn allowed_tool_list(&self) -> Vec<&str> {
        self.allowed_tools
            .as_deref()
            .map(|raw| raw.split_whitespace().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "---\nname: pdf-processing\ndescription: Extract PDF text. Use when handling PDFs.\n---\n\n# Body\n\nSteps here.\n";

    #[test]
    fn parses_minimal_skill() {
        let (fm, body) = Frontmatter::parse(MINIMAL, "SKILL.md").unwrap();
        assert_eq!(fm.name, "pdf-processing");
        assert!(body.starts_with("# Body"));
        fm.validate("SKILL.md", "pdf-processing").unwrap();
    }

    #[test]
    fn requires_frontmatter() {
        assert!(Frontmatter::parse("# Just markdown\n", "SKILL.md").is_err());
        assert!(Frontmatter::parse("---\nname: x\n", "SKILL.md").is_err());
    }

    #[test]
    fn rejects_spec_violating_names() {
        let cases = ["PDF-Processing", "-pdf", "pdf-", "pdf--processing", "pdf_processing"];
        for name in cases {
            let fm = Frontmatter {
                name: name.to_string(),
                description: "d".into(),
                license: None,
                compatibility: None,
                metadata: Default::default(),
                allowed_tools: None,
            };
            assert!(fm.validate("SKILL.md", name).is_err(), "{name} should be rejected");
        }
    }

    #[test]
    fn name_must_match_directory() {
        let (fm, _) = Frontmatter::parse(MINIMAL, "SKILL.md").unwrap();
        // Shadowing another skill's identity.
        assert!(fm.validate("SKILL.md", "something-else").is_err());
    }

    #[test]
    fn rejects_oversized_description() {
        let fm = Frontmatter {
            name: "x".into(),
            description: "d".repeat(limits::DESCRIPTION_MAX + 1),
            license: None,
            compatibility: None,
            metadata: Default::default(),
            allowed_tools: None,
        };
        assert!(fm.validate("SKILL.md", "x").is_err());
    }

    #[test]
    fn splits_allowed_tools() {
        let source = "---\nname: g\ndescription: d\nallowed-tools: Bash(git:*) Bash(jq:*) Read\n---\nbody\n";
        let (fm, _) = Frontmatter::parse(source, "SKILL.md").unwrap();
        assert_eq!(fm.allowed_tool_list(), vec!["Bash(git:*)", "Bash(jq:*)", "Read"]);
    }
}
