//! A skill, and the two tiers it is loaded in.

use camino::Utf8PathBuf;

use crate::frontmatter::Frontmatter;

/// Tier 1 — what is known about every installed skill from startup.
///
/// Holds a path, not a body. Keeping the body out of this type is what makes the
/// progressive-disclosure guarantee structural rather than a convention someone
/// can accidentally break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub frontmatter: Frontmatter,
    /// Directory containing `SKILL.md`.
    pub root: Utf8PathBuf,
    /// Project-local skills override user skills of the same name.
    pub is_project_local: bool,
}

impl Skill {
    pub fn name(&self) -> &str {
        &self.frontmatter.name
    }

    pub fn skill_file(&self) -> Utf8PathBuf {
        self.root.join(crate::SKILL_FILE)
    }

    /// Tier-1 line for the system prompt.
    ///
    /// This one line per skill is the whole startup cost, and the only thing the
    /// model uses to decide whether to activate.
    pub fn manifest_entry(&self) -> String {
        format!("- {}: {}", self.frontmatter.name, self.frontmatter.description)
    }

    /// Tier-1 cost estimate.
    pub fn estimated_manifest_tokens(&self) -> usize {
        self.manifest_entry().len().div_ceil(4)
    }

    /// Tier 2 — read the body. Called only on activation.
    pub fn load_body(&self) -> Result<SkillBody, crate::SkillError> {
        let path = self.skill_file();
        let raw = std::fs::read_to_string(&path)
            .map_err(|source| crate::SkillError::Io { path: path.to_string(), source })?;
        let (_, body) = Frontmatter::parse(&raw, path.as_str())?;

        Ok(SkillBody { skill_name: self.frontmatter.name.clone(), root: self.root.clone(), content: body })
    }
}

/// Tier 2 — the activated instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillBody {
    pub skill_name: String,
    pub root: Utf8PathBuf,
    pub content: String,
}

impl SkillBody {
    /// Render for injection into the conversation.
    ///
    /// The absolute root is included because the body refers to its files by
    /// relative path (`scripts/extract.py`), and the model needs to be able to
    /// turn those into something a tool will accept.
    pub fn render(&self) -> String {
        format!(
            "<skill name=\"{}\" root=\"{}\">\n{}\n</skill>",
            self.skill_name,
            self.root,
            self.content.trim_end()
        )
    }

    pub fn estimated_tokens(&self) -> usize {
        self.content.len().div_ceil(4)
    }

    /// Whether this body exceeds the spec's recommended size.
    ///
    /// Advisory only. Worth surfacing as a warning, since an oversized body
    /// defeats the point of tiering — the material belongs in `references/`.
    pub fn exceeds_recommended_size(&self) -> bool {
        self.estimated_tokens() > crate::limits::BODY_TOKENS_RECOMMENDED
    }
}
