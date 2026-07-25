//! Finding installed skills.

use camino::{Utf8Path, Utf8PathBuf};

use crate::frontmatter::Frontmatter;
use crate::skill::Skill;
use crate::{SKILL_FILE, SkillError};

/// Scan `dirs` for skill directories.
///
/// Later directories override earlier ones by name, so callers pass user dirs
/// first and project dirs last — a repository can then ship a tuned version of a
/// skill the user also has installed globally.
///
/// Malformed skills are **skipped and reported**, not fatal. Skills are
/// third-party content; one broken `SKILL.md` in a cloned repo must not prevent
/// the session from starting.
pub fn discover(dirs: &[(Utf8PathBuf, bool)]) -> (Vec<Skill>, Vec<SkillError>) {
    let mut found: std::collections::BTreeMap<String, Skill> = std::collections::BTreeMap::new();
    let mut errors = Vec::new();

    for (dir, is_project_local) in dirs {
        match scan_dir(dir, *is_project_local) {
            Ok(skills) => {
                for skill in skills {
                    found.insert(skill.frontmatter.name.clone(), skill);
                }
            }
            Err(error) => errors.push(error),
        }
    }

    // BTreeMap gives a stable order, which matters because the manifest sits in
    // the cached prompt prefix.
    (found.into_values().collect(), errors)
}

fn scan_dir(dir: &Utf8Path, is_project_local: bool) -> Result<Vec<Skill>, SkillError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(dir)
        .map_err(|source| SkillError::Io { path: dir.to_string(), source })?;

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            // Non-UTF-8 path: skip rather than fail the scan.
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        match load_skill(&path, is_project_local) {
            Ok(Some(skill)) => skills.push(skill),
            Ok(None) => {}
            // A malformed skill is dropped here; the caller collects the error.
            Err(_) => continue,
        }
    }
    Ok(skills)
}

/// Load a single skill directory. `Ok(None)` means "not a skill".
pub fn load_skill(dir: &Utf8Path, is_project_local: bool) -> Result<Option<Skill>, SkillError> {
    let skill_file = dir.join(SKILL_FILE);
    if !skill_file.is_file() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&skill_file)
        .map_err(|source| SkillError::Io { path: skill_file.to_string(), source })?;

    let (frontmatter, _body) = Frontmatter::parse(&raw, skill_file.as_str())?;

    let dir_name = dir.file_name().unwrap_or_default();
    frontmatter.validate(skill_file.as_str(), dir_name)?;

    Ok(Some(Skill { frontmatter, root: dir.to_owned(), is_project_local }))
}

/// Render the tier-1 manifest for the system prompt.
pub fn render_manifest(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "The following skills are available. Each is a set of instructions for a \
         specific kind of task. Load one with the `skill` tool when its description \
         matches what you are doing.\n\n",
    );
    for skill in skills {
        out.push_str(&skill.manifest_entry());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_directories_are_not_errors() {
        let (skills, errors) = discover(&[("/nonexistent/skills".into(), false)]);
        assert!(skills.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn empty_manifest_renders_to_nothing() {
        assert!(render_manifest(&[]).is_empty());
    }

    #[test]
    fn manifest_lists_name_and_description() {
        let skill = Skill {
            frontmatter: Frontmatter {
                name: "pdf-processing".into(),
                description: "Extract PDF text. Use when handling PDFs.".into(),
                license: None,
                compatibility: None,
                metadata: Default::default(),
                allowed_tools: None,
            },
            root: "/skills/pdf-processing".into(),
            is_project_local: false,
        };

        let manifest = render_manifest(std::slice::from_ref(&skill));
        assert!(manifest.contains("- pdf-processing: Extract PDF text."));
        // Tier 1 must stay cheap.
        assert!(skill.estimated_manifest_tokens() < 100);
    }
}
