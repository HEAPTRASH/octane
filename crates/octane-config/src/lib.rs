//! Configuration: settings and agent definitions.
//!
//! Two scopes, both named `.octane`, with the project winning:
//!
//! ```text
//! ~/.octane/            user, every project
//!   settings.toml
//!   providers/*.json
//!   agents/*.md
//! <repo>/.octane/       project, committed and shared
//!   settings.toml
//!   providers/*.json
//!   agents/*.md
//! ```
//!
//! # Why two formats
//!
//! `settings.toml` is edited by hand, and TOML has comments — which for a
//! settings file is worth more than the consistency of using one format
//! everywhere. Provider files are mostly written by `octane connect`, and JSON
//! keeps them portable with Junie and catwalk, which was a deliberate goal
//! (`RESEARCH.md` §M–O).
//!
//! Agents are markdown with YAML frontmatter, matching the convention Claude
//! Code, opencode, and Antigravity all landed on independently — a format people
//! may already have files for is worth more than a marginally nicer one.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod edit;
pub mod settings;

pub use agent::{AgentDefinition, AgentScope, discover_agents};
pub use edit::{Applies, Editable, Value};
pub use settings::{Settings, SettingsError};

/// Directory name at both scopes.
pub const CONFIG_DIR: &str = ".octane";

/// Config roots, least specific first, so a project file wins.
///
/// The order is the merge order — callers fold later over earlier.
pub fn roots(workspace: &camino::Utf8Path) -> Vec<camino::Utf8PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME").and_then(|home| {
        camino::Utf8PathBuf::from_path_buf(std::path::PathBuf::from(home)).ok()
    }) {
        roots.push(home.join(CONFIG_DIR));
    }
    roots.push(workspace.join(CONFIG_DIR));
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_root_comes_last_so_it_wins() {
        let roots = roots(camino::Utf8Path::new("/repo"));
        assert_eq!(roots.last().unwrap(), "/repo/.octane");
    }
}
