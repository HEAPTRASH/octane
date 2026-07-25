//! Finding command files.

use camino::{Utf8Path, Utf8PathBuf};

use crate::command::{Command, CommandFrontmatter};
use crate::CommandError;

/// Scan `dirs` for `.md` command files, recursing into subdirectories.
///
/// A file at `db/migrate.md` becomes `/db:migrate`. Later directories override
/// earlier ones, so callers pass user dirs first and project dirs last.
///
/// A malformed file is skipped and reported. Same reasoning as skills: these are
/// files from a repository, and one bad one must not prevent startup.
pub fn discover(dirs: &[(Utf8PathBuf, bool)]) -> (Vec<Command>, Vec<CommandError>) {
    let mut found: std::collections::BTreeMap<String, Command> = std::collections::BTreeMap::new();
    let mut errors = Vec::new();

    for (dir, is_project_local) in dirs {
        if !dir.is_dir() {
            continue;
        }
        if let Err(error) = scan(dir, dir, *is_project_local, &mut found) {
            errors.push(error);
        }
    }

    (found.into_values().collect(), errors)
}

fn scan(
    dir: &Utf8Path,
    root: &Utf8Path,
    is_project_local: bool,
    out: &mut std::collections::BTreeMap<String, Command>,
) -> Result<(), CommandError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|source| CommandError::Io { path: dir.to_string(), source })?;

    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };

        if path.is_dir() {
            scan(&path, root, is_project_local, out)?;
            continue;
        }
        if path.extension() != Some("md") {
            continue;
        }

        match load(&path, root, is_project_local) {
            Ok(command) => {
                out.insert(command.name.clone(), command);
            }
            Err(_) => continue,
        }
    }
    Ok(())
}

fn load(
    path: &Utf8Path,
    root: &Utf8Path,
    is_project_local: bool,
) -> Result<Command, CommandError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|source| CommandError::Io { path: path.to_string(), source })?;

    let (frontmatter, template) = split_frontmatter(&raw, path)?;

    Ok(Command {
        name: command_name(path, root),
        frontmatter,
        template,
        path: path.to_owned(),
        is_project_local,
    })
}

/// Frontmatter is optional here, unlike skills: a command can be a bare prompt
/// file, and requiring ceremony for the simple case discourages the whole
/// mechanism.
fn split_frontmatter(
    source: &str,
    path: &Utf8Path,
) -> Result<(CommandFrontmatter, String), CommandError> {
    let Some(rest) = source.strip_prefix("---") else {
        return Ok((CommandFrontmatter::default(), source.to_string()));
    };
    let Some((yaml, body)) = rest.split_once("\n---") else {
        return Ok((CommandFrontmatter::default(), source.to_string()));
    };

    let frontmatter = serde_yaml::from_str(yaml).map_err(|source| {
        CommandError::InvalidFrontmatter { path: path.to_string(), source }
    })?;

    Ok((frontmatter, body.trim_start_matches('-').trim_start().to_string()))
}

/// Path relative to the commands root, `/`-separated, becomes `:`-namespaced.
fn command_name(path: &Utf8Path, root: &Utf8Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let without_extension = relative.with_extension("");
    without_extension
        .components()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_file_is_a_bare_name() {
        let name = command_name(Utf8Path::new("/c/review.md"), Utf8Path::new("/c"));
        assert_eq!(name, "review");
    }

    #[test]
    fn subdirectories_namespace_with_colons() {
        let name = command_name(Utf8Path::new("/c/db/migrate.md"), Utf8Path::new("/c"));
        assert_eq!(name, "db:migrate");
        let deep = command_name(Utf8Path::new("/c/a/b/c.md"), Utf8Path::new("/c"));
        assert_eq!(deep, "a:b:c");
    }

    #[test]
    fn frontmatter_is_optional() {
        let (fm, body) = split_frontmatter("Just a prompt.\n", Utf8Path::new("/c/x.md")).unwrap();
        assert_eq!(fm, CommandFrontmatter::default());
        assert_eq!(body, "Just a prompt.\n");
    }

    #[test]
    fn frontmatter_is_parsed_when_present() {
        let source = "---\ndescription: Review a PR\nargument-hint: <pr>\n---\nReview $1\n";
        let (fm, body) = split_frontmatter(source, Utf8Path::new("/c/x.md")).unwrap();
        assert_eq!(fm.description.as_deref(), Some("Review a PR"));
        assert_eq!(fm.argument_hint.as_deref(), Some("<pr>"));
        assert_eq!(body, "Review $1\n");
    }

    #[test]
    fn missing_directories_are_skipped() {
        let (commands, errors) = discover(&[("/nonexistent".into(), false)]);
        assert!(commands.is_empty());
        assert!(errors.is_empty());
    }
}
