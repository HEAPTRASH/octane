//! Slash commands — canned prompts, discovered from the filesystem.
//!
//! ```text
//! .octane/commands/review.md       →  /review        (project)
//! ~/.octane/commands/review.md     →  /review        (user)
//! .octane/commands/db/migrate.md   →  /db:migrate    (namespaced by directory)
//! ```
//!
//! A command expands to a **prompt**, never to a code path. That single
//! constraint is what keeps the extension model declarative: adding a workflow
//! requires no plugin API, no compilation, and no knowledge of Rust.
//!
//! Three substitutions happen at expansion time:
//!
//! - `$ARGUMENTS` — everything the user typed after the command name
//! - `$1`, `$2`, … — positional arguments
//! - ``!`command` `` — **shell substitution**, so a command can inject live state
//!   (``!`git diff --staged` ``) before the model sees anything
//!
//! The shell substitution is the genuinely useful part and also the dangerous
//! part: it runs before the model is involved, so it is never subject to the
//! model's judgment. It goes through the same permission engine as any other
//! command, and never interpolates user arguments — see [`expand`].

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod command;
pub mod discovery;
pub mod expand;
pub mod registry;

pub use command::{Command, CommandFrontmatter};
pub use discovery::discover;
pub use expand::{Expansion, ShellRequest, expand};
pub use registry::{Builtin, Entry, Kind, Registry};

/// Directory name searched under both the project and user config roots.
pub const COMMANDS_DIR: &str = "commands";

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("reading {path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },

    #[error("{path}: invalid frontmatter: {source}")]
    InvalidFrontmatter { path: String, #[source] source: serde_yaml::Error },

    #[error("unknown command /{0}")]
    Unknown(String),

    #[error("{path}: /{name} is a built-in command and cannot be redefined")]
    ShadowsBuiltin { name: String, path: String },

    #[error("/{name} requires at least {expected} argument(s), got {actual}")]
    MissingArguments { name: String, expected: usize, actual: usize },
}
