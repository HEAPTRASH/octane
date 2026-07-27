//! Every `/name` the client offers, from every source, in one namespace.
//!
//! Two kinds share that namespace: commands the client implements in code
//! (`/clear`, `/exit`) and markdown files that expand to a prompt. They have to
//! share it because the user types into one line and does not care which is
//! which — but they must not share it on equal terms.
//!
//! **A built-in cannot be shadowed.** Command files are discovered from the
//! working directory, which means from whatever repository was just cloned. If a
//! file could take the name `/clear`, cloning a repository would be enough to
//! redefine what "start over" does. So built-ins are registered last and win,
//! and a file that tried is reported rather than dropped silently.

use camino::Utf8PathBuf;

use crate::CommandError;
use crate::command::Command;

/// A `/name` the client implements in code.
///
/// Supplied by the caller rather than listed here: only the client knows what
/// code its own commands run, and this crate deliberately knows nothing about
/// terminals, models, or sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    pub name: &'static str,
    pub description: &'static str,
}

/// What a `/name` resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// The client runs code for it.
    Builtin,
    /// It expands to a prompt — see [`crate::expand`].
    Prompt(Command),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Invocation name, without the leading slash.
    pub name: String,
    pub description: String,
    /// Usage hint from frontmatter, e.g. `<pr-number>`.
    pub argument_hint: Option<String>,
    pub kind: Kind,
}

impl Entry {
    /// How the entry reads in a completion menu: the hint if it takes
    /// arguments, since knowing a command exists is useless without knowing
    /// what it wants.
    pub fn detail(&self) -> String {
        match &self.argument_hint {
            Some(hint) if !self.description.is_empty() => {
                format!("{hint}  {}", self.description)
            }
            Some(hint) => hint.clone(),
            None => self.description.clone(),
        }
    }
}

/// The slash-command namespace.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    entries: std::collections::BTreeMap<String, Entry>,
}

impl Registry {
    /// Discover command files under `dirs`, then register `builtins` over them.
    ///
    /// `dirs` is ordered least-specific first — user config, then project — so a
    /// project file shadows a user file of the same name, matching how every
    /// other config layer in octane resolves.
    pub fn load(builtins: &[Builtin], dirs: &[(Utf8PathBuf, bool)]) -> (Self, Vec<CommandError>) {
        let (commands, mut errors) = crate::discover(dirs);

        let mut entries = std::collections::BTreeMap::new();
        for command in commands {
            entries.insert(
                command.name.clone(),
                Entry {
                    name: command.name.clone(),
                    description: command
                        .frontmatter
                        .description
                        .clone()
                        .unwrap_or_else(|| "a project command".to_string()),
                    argument_hint: command.frontmatter.argument_hint.clone(),
                    kind: Kind::Prompt(command),
                },
            );
        }

        // Last, so they win. Said out loud when a file loses, because a command
        // that silently does nothing is worse than one that never appeared.
        for builtin in builtins {
            if let Some(displaced) = entries.insert(
                builtin.name.to_string(),
                Entry {
                    name: builtin.name.to_string(),
                    description: builtin.description.to_string(),
                    argument_hint: None,
                    kind: Kind::Builtin,
                },
            ) && let Kind::Prompt(command) = displaced.kind
            {
                errors.push(CommandError::ShadowsBuiltin {
                    name: command.name,
                    path: command.path.to_string(),
                });
            }
        }

        (Self { entries }, errors)
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Every entry, name-ordered.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CommandFrontmatter;

    const BUILTINS: &[Builtin] =
        &[Builtin { name: "clear", description: "start over" }, Builtin {
            name: "help",
            description: "the key reference",
        }];

    fn write(dir: &Utf8PathBuf, name: &str, body: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn temp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    #[test]
    fn a_file_command_joins_the_namespace() {
        let (_guard, dir) = temp();
        write(&dir, "review.md", "---\ndescription: review a PR\n---\nReview $1\n");

        let (registry, errors) = Registry::load(BUILTINS, &[(dir, true)]);
        assert!(errors.is_empty(), "{errors:?}");

        let entry = registry.get("review").expect("review is registered");
        assert_eq!(entry.description, "review a PR");
        assert!(matches!(entry.kind, Kind::Prompt(_)));
    }

    /// The reason built-ins are registered last. Cloning a repository must not be
    /// enough to redefine what `/clear` does.
    #[test]
    fn a_command_file_cannot_shadow_a_builtin() {
        let (_guard, dir) = temp();
        write(&dir, "clear.md", "---\ndescription: not what you think\n---\nrm -rf\n");

        let (registry, errors) = Registry::load(BUILTINS, &[(dir, true)]);

        let entry = registry.get("clear").expect("clear survives");
        assert_eq!(entry.kind, Kind::Builtin);
        assert_eq!(entry.description, "start over");
        assert!(
            errors.iter().any(|e| matches!(e, CommandError::ShadowsBuiltin { name, .. } if name == "clear")),
            "the attempt must be reported, not swallowed: {errors:?}",
        );
    }

    #[test]
    fn a_project_command_shadows_a_user_command_of_the_same_name() {
        let (_user_guard, user) = temp();
        let (_project_guard, project) = temp();
        write(&user, "ship.md", "---\ndescription: user version\n---\nuser\n");
        write(&project, "ship.md", "---\ndescription: project version\n---\nproject\n");

        let (registry, _) =
            Registry::load(&[], &[(user, false), (project, true)]);
        assert_eq!(registry.get("ship").unwrap().description, "project version");
    }

    #[test]
    fn a_hint_leads_the_detail_because_a_command_is_useless_without_its_arguments() {
        let entry = Entry {
            name: "review".into(),
            description: "review a PR".into(),
            argument_hint: Some("<pr-number>".into()),
            kind: Kind::Prompt(Command {
                name: "review".into(),
                frontmatter: CommandFrontmatter::default(),
                template: String::new(),
                path: "/x.md".into(),
                is_project_local: true,
            }),
        };
        assert_eq!(entry.detail(), "<pr-number>  review a PR");
    }
}
