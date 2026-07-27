//! What `/settings` can change, and changing it without destroying the file.
//!
//! Two things live here because they answer one question. The catalogue says
//! which settings have a knowable set of values — those are the ones a picker
//! can offer safely, since every choice it presents is valid by construction.
//! Free-text settings are not editable this way and should not pretend to be.
//!
//! The write is format-preserving. A settings file is hand-edited and mostly
//! comments — [`crate::settings::TEMPLATE`] is *entirely* comments — so
//! serializing the struct back over it would delete the documentation of every
//! setting the user had not set. `toml_edit` rewrites one value and leaves the
//! rest of the bytes alone.

use crate::settings::{SETTINGS_FILE, Settings, TEMPLATE};
use camino::{Utf8Path, Utf8PathBuf};

/// A value a setting can hold. Only the shapes a picker can offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Text(String),
}

impl Value {
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// How the value reads in the UI and in the file.
    pub fn display(&self) -> String {
        match self {
            Self::Bool(value) => value.to_string(),
            Self::Text(value) => value.clone(),
        }
    }

    fn to_item(&self) -> toml_edit::Item {
        match self {
            Self::Bool(value) => toml_edit::value(*value),
            Self::Text(value) => toml_edit::value(value.as_str()),
        }
    }
}

/// One offered value, with the sentence that explains why you would pick it.
#[derive(Debug, Clone)]
pub struct Choice {
    pub value: Value,
    pub label: String,
    pub detail: String,
}

impl Choice {
    pub fn new(value: Value, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { value, label: label.into(), detail: detail.into() }
    }

    fn boolean(value: bool, label: &str, detail: &str) -> Self {
        Self::new(Value::Bool(value), label, detail)
    }
}

/// When a change takes effect.
///
/// Recorded rather than assumed because the honest answer differs per setting,
/// and a change that silently does nothing until restart is the worst outcome
/// an editor can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// The running session picks it up immediately.
    Live,
    /// Read once at startup; the file is correct but this session is not.
    OnRestart,
}

#[derive(Debug, Clone)]
pub struct Editable {
    pub key: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub applies: Applies,
    /// What the value is when no file sets it. Recorded so the UI can show
    /// "false (default)" rather than "unset" — those look the same in a list
    /// and mean different things when you are trying to work out why something
    /// is behaving the way it is. `None` where the default is not a fixed
    /// value: an unset `model` is resolved by role, not by a constant.
    pub default: Option<Value>,
    pub choices: Vec<Choice>,
}

impl Editable {
    /// What the setting is worth right now, and whether anything set it.
    ///
    /// Returned together because the UI needs both: the value to show, and
    /// whether to caveat it as a default nobody chose.
    pub fn effective(&self, settings: &Settings) -> (String, bool) {
        match current(settings, self.key) {
            Some(value) => (value, true),
            None => (
                self.default.as_ref().map(Value::display).unwrap_or_else(|| "unset".into()),
                false,
            ),
        }
    }

    /// The choice matching a stored value, for marking the current one.
    pub fn choice(&self, value: &str) -> Option<&Choice> {
        self.choices.iter().find(|choice| choice.value.display() == value)
    }
}

/// Every setting a picker can offer, in the order they are shown.
///
/// `models` and `themes` are supplied by the caller because the valid values are
/// whatever the provider registry resolved and whatever theme files were
/// discovered — config cannot know either, and a list that went stale here would
/// offer choices that fail on selection.
///
/// Deliberately absent: `permissions`, whose rules are free text and whose
/// broad grants should cost a deliberate file edit rather than one keystroke
/// (see `Policy::evaluate`), and any setting nothing reads yet — offering a
/// switch that does nothing is worse than not offering it.
pub fn catalogue(models: &[String], themes: &[String]) -> Vec<Editable> {
    let mut all = Vec::new();

    if !models.is_empty() {
        all.push(Editable {
            key: "model",
            label: "model",
            detail: "Which model answers",
            applies: Applies::OnRestart,
            default: None,
            choices: models
                .iter()
                .map(|name| Choice::new(Value::text(name), name, "configured provider/model"))
                .collect(),
        });
    }

    all.push(Editable {
        key: "mode",
        label: "mode",
        detail: "How much is asked before it happens",
        applies: Applies::Live,
        default: Some(Value::Text(String::from("default"))),
        choices: vec![
            Choice::new(Value::text("default"), "default", "Ask before commands; edit in the project freely"),
            Choice::new(Value::text("plan"), "plan", "Read-only; propose, change nothing"),
            Choice::new(
                Value::text("accept-edits"),
                "accept-edits",
                "Auto-approve edits anywhere, still ask for commands",
            ),
            Choice::new(
                Value::text("bypass"),
                "bypass",
                "Skip prompts; deny rules and the sandbox still apply",
            ),
        ],
    });

    all.push(Editable {
        key: "thinking",
        label: "thinking",
        detail: "How hard the model reasons",
        applies: Applies::Live,
        default: Some(Value::Text(String::from("auto"))),
        choices: vec![
            Choice::new(Value::text("auto"), "auto", "Let the endpoint decide"),
            Choice::new(Value::text("off"), "off", "Some endpoints refuse this and reason anyway"),
            Choice::new(Value::text("low"), "low", "Least reasoning that still works everywhere"),
            Choice::new(Value::text("medium"), "medium", ""),
            Choice::new(Value::text("high"), "high", "Slowest and most expensive"),
        ],
    });

    all.push(Editable {
        key: "show-reasoning",
        label: "show-reasoning",
        detail: "Whether reasoning appears in the transcript",
        applies: Applies::Live,
        default: Some(Value::Bool(false)),
        choices: vec![
            Choice::boolean(true, "true", "Show it as it streams"),
            Choice::boolean(false, "false", "Hide it; the model still reasons"),
        ],
    });

    all.push(Editable {
        key: "ascii",
        label: "ascii",
        detail: "Force the ASCII glyph set",
        applies: Applies::Live,
        default: Some(Value::Bool(false)),
        choices: vec![
            Choice::boolean(true, "true", "For terminals without dependable Unicode"),
            Choice::boolean(false, "false", "Box drawing and geometric shapes"),
        ],
    });

    if !themes.is_empty() {
        all.push(Editable {
            key: "theme",
            label: "theme",
            detail: "Colour scheme",
            applies: Applies::Live,
            default: Some(Value::Text(String::from("octane"))),
            // No detail per row: a scheme's name is what people recognise it
            // by, and a sentence describing a palette in words is noise beside
            // the thing itself.
            choices: themes
                .iter()
                .map(|name| Choice::new(Value::text(name), name, ""))
                .collect(),
        });
    }

    all.push(Editable {
        key: "sandbox",
        label: "sandbox",
        detail: "OS containment for shell commands",
        applies: Applies::OnRestart,
        default: Some(Value::Bool(true)),
        choices: vec![
            Choice::boolean(true, "true", "Commands run confined to the workspace"),
            Choice::boolean(false, "false", "Unconfined. Policy still asks, nothing enforces"),
        ],
    });

    all.push(Editable {
        key: "sandbox-network",
        label: "sandbox-network",
        detail: "Whether contained commands reach the network",
        applies: Applies::OnRestart,
        default: Some(Value::Bool(false)),
        choices: vec![
            Choice::boolean(false, "false", "Denied, so a build cannot exfiltrate"),
            Choice::boolean(true, "true", "Allowed, for commands that must fetch"),
        ],
    });

    all
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("{path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{path} is not valid TOML, so it cannot be edited safely: {source}")]
    Parse { path: String, source: toml_edit::TomlError },
}

/// Which file a change to `key` should be written to.
///
/// The file that already sets it, if any; otherwise the most specific root,
/// which is the project. Writing blindly to the project would shadow a value
/// the user had set globally — the setting would appear to change, and then
/// diverge from the one file they knew to look in.
pub fn target(roots: &[Utf8PathBuf], key: &str) -> Utf8PathBuf {
    for root in roots {
        let path = root.join(SETTINGS_FILE);
        if !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else { continue };
        if doc.get(key).is_some() {
            return path;
        }
    }

    roots
        .last()
        .map(|root| root.join(SETTINGS_FILE))
        .unwrap_or_else(|| Utf8PathBuf::from(SETTINGS_FILE))
}

/// Set one top-level key, preserving every comment and every other value.
///
/// A file that does not exist is created from [`TEMPLATE`], so a first edit
/// leaves behind the documented starter rather than a one-line file that says
/// nothing about what else could be set.
pub fn set(path: &Utf8Path, key: &str, value: &Value) -> Result<(), EditError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => TEMPLATE.to_string(),
        Err(source) => return Err(EditError::Io { path: path.to_string(), source }),
    };

    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|source| EditError::Parse { path: path.to_string(), source })?;
    doc[key] = value.to_item();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| EditError::Io { path: parent.to_string(), source })?;
    }
    std::fs::write(path, doc.to_string())
        .map_err(|source| EditError::Io { path: path.to_string(), source })
}

/// The current value of `key` in the merged settings, as the file would spell it.
///
/// Goes through the struct rather than reading the file so what the picker marks
/// as current is what the session actually resolved, layering included.
pub fn current(settings: &Settings, key: &str) -> Option<String> {
    match key {
        "model" => settings.model.clone(),
        "mode" => settings.mode.map(|mode| mode.label().to_string()),
        "thinking" => settings.thinking.map(|thinking| thinking.label()),
        "show-reasoning" => settings.show_reasoning.map(|value| value.to_string()),
        "ascii" => settings.ascii.map(|value| value.to_string()),
        "theme" => settings.theme.clone(),
        "sandbox" => settings.sandbox.map(|value| value.to_string()),
        "sandbox-network" => settings.sandbox_network.map(|value| value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    #[test]
    fn an_edit_keeps_every_comment_in_the_file() {
        // The whole reason for toml_edit. The template is entirely comments, so
        // a struct round-trip would leave the user with a one-line file and no
        // record of what else exists.
        let (_dir, root) = temp();
        let path = root.join(SETTINGS_FILE);
        std::fs::write(&path, TEMPLATE).unwrap();

        set(&path, "thinking", &Value::text("high")).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# octane settings."), "the header comment survived");
        assert!(after.contains("# model        = "), "commented-out settings survived");
        assert!(after.contains("thinking = \"high\""), "the edit landed");
    }

    #[test]
    fn an_edit_replaces_rather_than_appends() {
        let (_dir, root) = temp();
        let path = root.join(SETTINGS_FILE);
        std::fs::write(&path, "thinking = \"low\"\nascii = true\n").unwrap();

        set(&path, "thinking", &Value::text("high")).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.matches("thinking").count(), 1, "one key, not two");
        assert!(after.contains("ascii = true"), "the untouched key survived");
    }

    #[test]
    fn a_new_key_lands_outside_an_existing_table() {
        // The failure this guards: appending text to a file ending in
        // `[permissions]` puts the key inside that table, where it means
        // something else entirely and silently stops applying.
        let (_dir, root) = temp();
        let path = root.join(SETTINGS_FILE);
        std::fs::write(&path, "[permissions]\ndeny = [\"command(sudo)\"]\n").unwrap();

        set(&path, "ascii", &Value::Bool(true)).unwrap();

        let reloaded: Settings =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(reloaded.ascii, Some(true));
        assert_eq!(reloaded.permissions.deny, vec!["command(sudo)".to_string()]);
    }

    #[test]
    fn a_missing_file_is_created_from_the_template() {
        let (_dir, root) = temp();
        let path = root.join(SETTINGS_FILE);

        set(&path, "ascii", &Value::Bool(true)).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# octane settings."), "the starter documentation is there");
        assert!(after.contains("ascii = true"));
    }

    #[test]
    fn an_edit_goes_to_the_file_that_already_sets_it() {
        // Otherwise a globally-set value gets shadowed by a project override,
        // and the file the user knows about stops matching what they see.
        let (_dir, root) = temp();
        let (user, project) = (root.join("user"), root.join("project"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(user.join(SETTINGS_FILE), "ascii = true\n").unwrap();
        std::fs::write(project.join(SETTINGS_FILE), "thinking = \"low\"\n").unwrap();

        let roots = vec![user.clone(), project.clone()];
        assert_eq!(target(&roots, "ascii"), user.join(SETTINGS_FILE));
        assert_eq!(target(&roots, "thinking"), project.join(SETTINGS_FILE));
    }

    #[test]
    fn an_unset_setting_is_written_to_the_project() {
        // The narrower blast radius is the right default for a value with no
        // existing home.
        let (_dir, root) = temp();
        let (user, project) = (root.join("user"), root.join("project"));
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join(SETTINGS_FILE), "ascii = true\n").unwrap();

        let roots = vec![user, project.clone()];
        assert_eq!(target(&roots, "mode"), project.join(SETTINGS_FILE));
    }

    #[test]
    fn a_malformed_file_is_refused_rather_than_overwritten() {
        // Rewriting it would be the one action that loses the user's work.
        let (_dir, root) = temp();
        let path = root.join(SETTINGS_FILE);
        std::fs::write(&path, "thinking = = broken\n").unwrap();

        assert!(matches!(
            set(&path, "ascii", &Value::Bool(true)),
            Err(EditError::Parse { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "thinking = = broken\n");
    }

    #[test]
    fn every_offered_choice_round_trips_through_the_parser() {
        // The picker's promise is that it cannot offer an invalid value. That
        // only holds if every catalogue entry parses back into the struct.
        let (_dir, root) = temp();
        let path = root.join(SETTINGS_FILE);

        let themes = ["octane".to_string(), "nord".to_string(), "mine".to_string()];
        for editable in catalogue(&["openrouter/flash".to_string()], &themes) {
            for choice in &editable.choices {
                std::fs::write(&path, "").unwrap();
                set(&path, editable.key, &choice.value).unwrap();
                let text = std::fs::read_to_string(&path).unwrap();
                let parsed = toml::from_str::<Settings>(&text).unwrap_or_else(|error| {
                    panic!("{} = {} is not loadable: {error}", editable.key, choice.value.display())
                });
                assert_eq!(
                    current(&parsed, editable.key).as_deref(),
                    Some(choice.value.display().as_str()),
                    "{} did not round-trip",
                    editable.key
                );
            }
        }
    }
}
