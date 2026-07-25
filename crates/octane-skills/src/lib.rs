//! Agent Skills — on-demand procedural knowledge.
//!
//! Implements the [Agent Skills spec](https://agentskills.io/specification)
//! rather than a bespoke format, because Claude Code, Antigravity, and opencode
//! all consume it and a skill someone already wrote should just work.
//!
//! ```text
//! skill-name/
//! ├── SKILL.md          required: YAML frontmatter + markdown body
//! ├── scripts/          optional: executables the agent may run
//! ├── references/       optional: docs loaded on demand
//! └── assets/           optional: templates, data
//! ```
//!
//! **Progressive disclosure is the entire point.** Three tiers:
//!
//! | Tier | Loaded | Budget |
//! |---|---|---|
//! | `name` + `description` | at startup, for every installed skill | ~100 tokens |
//! | `SKILL.md` body | when the skill activates | <5k tokens |
//! | `references/`, `scripts/` | only when the body points at them | unbounded |
//!
//! So N installed skills cost ~100N tokens at startup and depth is free until
//! used. That property is what makes it reasonable to have fifty skills
//! installed, and it is lost the moment something eagerly loads bodies.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod discovery;
pub mod frontmatter;
pub mod skill;

pub use discovery::discover;
pub use frontmatter::Frontmatter;
pub use skill::{Skill, SkillBody};

/// The one required file in a skill directory.
pub const SKILL_FILE: &str = "SKILL.md";

/// Spec limits. Enforced rather than trusted, since skills are third-party
/// content and a 10MB `description` would blow the startup budget for every
/// session.
pub mod limits {
    pub const NAME_MAX: usize = 64;
    pub const DESCRIPTION_MAX: usize = 1024;
    pub const COMPATIBILITY_MAX: usize = 500;
    /// Advisory: the spec recommends bodies stay under this.
    pub const BODY_TOKENS_RECOMMENDED: usize = 5_000;
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("reading {path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },

    #[error("{path}: missing YAML frontmatter delimited by ---")]
    MissingFrontmatter { path: String },

    #[error("{path}: invalid frontmatter: {source}")]
    InvalidFrontmatter { path: String, #[source] source: serde_yaml::Error },

    #[error("{path}: {reason}")]
    Invalid { path: String, reason: String },
}
