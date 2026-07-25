//! `settings.toml`.
//!
//! Layered: the user file supplies defaults, the project file overrides. Merging
//! is **per field**, not per file — a project that sets only `model` must not
//! discard the user's permission rules.
//!
//! Every field is optional so a partial file is valid. That is the whole point
//! of layering: a project file should say only what is project-specific.

use serde::{Deserialize, Serialize};

use octane_permission::PermissionMode;
use octane_provider::Thinking;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Settings {
    /// `provider/model`, or a bare name when unambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Model for summarization, titles, and classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faster_model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,

    /// Show reasoning in the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_reasoning: Option<bool>,

    /// Run commands under OS containment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<bool>,

    /// Allow outbound network from sandboxed commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_network: Option<bool>,

    /// Force the ASCII glyph set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ascii: Option<bool>,

    #[serde(default, skip_serializing_if = "Permissions::is_empty")]
    pub permissions: Permissions,
}

/// `action(target)` rules, matching the engine's three lists.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Permissions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

impl Permissions {
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty() && self.ask.is_empty() && self.deny.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },

    #[error("{path}: {source}")]
    Parse { path: String, #[source] source: toml::de::Error },
}

/// Filename at each root.
pub const SETTINGS_FILE: &str = "settings.toml";

impl Settings {
    /// Load and merge every root, least specific first.
    ///
    /// A malformed file is reported and skipped rather than fatal: one bad
    /// project file must not stop a session, and the user needs to know which.
    pub fn load(roots: &[camino::Utf8PathBuf]) -> (Self, Vec<SettingsError>) {
        let mut merged = Self::default();
        let mut errors = Vec::new();

        for root in roots {
            let path = root.join(SETTINGS_FILE);
            if !path.is_file() {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(text) => match toml::from_str::<Self>(&text) {
                    Ok(settings) => merged = merged.merge(settings),
                    Err(source) => {
                        errors.push(SettingsError::Parse { path: path.to_string(), source })
                    }
                },
                Err(source) => errors.push(SettingsError::Io { path: path.to_string(), source }),
            }
        }
        (merged, errors)
    }

    /// Overlay `other` on `self`, field by field.
    ///
    /// Per field rather than per file: a project setting only `model` must not
    /// discard the user's permission rules, which whole-file replacement would.
    pub fn merge(mut self, other: Self) -> Self {
        macro_rules! take {
            ($($field:ident),+ $(,)?) => {$(
                if other.$field.is_some() { self.$field = other.$field; }
            )+};
        }
        take!(
            model,
            faster_model,
            mode,
            thinking,
            show_reasoning,
            sandbox,
            sandbox_network,
            ascii,
        );

        // Rules accumulate rather than replace: a project adding one deny should
        // not silently drop everything the user configured.
        self.permissions.allow.extend(other.permissions.allow);
        self.permissions.ask.extend(other.permissions.ask);
        self.permissions.deny.extend(other.permissions.deny);
        self
    }

    /// Render as TOML, for `/settings` and for writing a starter file.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|error| format!("# unserializable: {error}\n"))
    }
}

/// A commented starter file.
///
/// Every value is the default, commented out — so the file documents what exists
/// without changing anything by existing.
pub const TEMPLATE: &str = r#"# octane settings. Project settings override ~/.octane/settings.toml,
# field by field, so this file need only say what is project-specific.

# model        = "anthropic/sonnet"   # `octane models` lists what is configured
# faster-model = "anthropic/haiku"    # summaries, titles, classification
# mode         = "default"            # default | plan | accept-edits | bypass
# thinking     = "auto"               # auto | off | low | medium | high
# show-reasoning = false
# sandbox      = true                 # OS containment for shell commands
# sandbox-network = false
# ascii        = false                # force the ASCII glyph set

# [permissions]
# allow = ["command(git status)", "command(cargo test)"]
# ask   = ["command(*)"]
# deny  = ["command(sudo)", "write_file(.git/)"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &camino::Utf8Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(SETTINGS_FILE), body).unwrap();
    }

    fn temp() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    #[test]
    fn an_empty_file_is_valid() {
        // Every field optional is what makes layering work.
        assert_eq!(toml::from_str::<Settings>("").unwrap(), Settings::default());
    }

    #[test]
    fn kebab_case_keys_are_what_the_file_uses() {
        let settings: Settings =
            toml::from_str("show-reasoning = true\nfaster-model = \"x/y\"").unwrap();
        assert_eq!(settings.show_reasoning, Some(true));
        assert_eq!(settings.faster_model.as_deref(), Some("x/y"));
    }

    #[test]
    fn a_project_file_overrides_field_by_field() {
        let user: Settings =
            toml::from_str("model = \"user/model\"\nthinking = \"high\"").unwrap();
        let project: Settings = toml::from_str("model = \"project/model\"").unwrap();

        let merged = user.merge(project);
        assert_eq!(merged.model.as_deref(), Some("project/model"));
        // The user's setting survives, which whole-file replacement would lose.
        assert_eq!(merged.thinking, Some(Thinking::High));
    }

    #[test]
    fn permission_rules_accumulate_rather_than_replace() {
        let user: Settings =
            toml::from_str("[permissions]\ndeny = [\"command(sudo)\"]").unwrap();
        let project: Settings =
            toml::from_str("[permissions]\ndeny = [\"command(rm -rf /)\"]").unwrap();

        let merged = user.merge(project);
        // A project adding one deny must not drop the user's.
        assert_eq!(merged.permissions.deny.len(), 2);
    }

    #[test]
    fn a_typo_is_rejected_rather_than_ignored() {
        // A key that silently does nothing is worse than a load error.
        assert!(toml::from_str::<Settings>("modle = \"x\"").is_err());
        assert!(toml::from_str::<Settings>("show_reasoning = true").is_err());
    }

    #[test]
    fn loading_merges_roots_in_order() {
        let (_user_guard, user) = temp();
        let (_project_guard, project) = temp();
        write(&user, "model = \"from-user\"\nthinking = \"low\"");
        write(&project, "model = \"from-project\"");

        let (settings, errors) = Settings::load(&[user, project]);
        assert!(errors.is_empty());
        assert_eq!(settings.model.as_deref(), Some("from-project"));
        assert_eq!(settings.thinking, Some(Thinking::Low));
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let (_guard, dir) = temp();
        let (settings, errors) = Settings::load(&[dir]);
        assert!(errors.is_empty());
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn a_malformed_file_is_reported_and_skipped() {
        let (_bad_guard, bad) = temp();
        let (_good_guard, good) = temp();
        write(&bad, "this is not = = toml");
        write(&good, "model = \"still-loaded\"");

        let (settings, errors) = Settings::load(&[bad, good]);
        // One bad file must not stop a session.
        assert_eq!(settings.model.as_deref(), Some("still-loaded"));
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn the_template_parses_and_changes_nothing() {
        // Everything commented out, so the file documents the options without
        // taking effect by existing.
        let settings: Settings = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn settings_round_trip_through_toml() {
        let original = Settings {
            model: Some("a/b".into()),
            mode: Some(PermissionMode::Plan),
            thinking: Some(Thinking::Medium),
            permissions: Permissions {
                deny: vec!["command(sudo)".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let rendered = original.to_toml();
        assert_eq!(toml::from_str::<Settings>(&rendered).unwrap(), original);
    }
}
