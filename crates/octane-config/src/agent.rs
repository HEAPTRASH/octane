//! Agent definitions.
//!
//! Markdown with YAML frontmatter in `.octane/agents/*.md`, which is the format
//! Claude Code, opencode, and Antigravity all landed on independently. Filename
//! is the agent name, so there is nothing to keep in sync.
//!
//! ```markdown
//! ---
//! description: Reviews a change for defects it would be embarrassing to ship
//! mode: subagent
//! tools: [read, grep, glob, list]
//! model: anthropic/haiku
//! ---
//! You are reviewing a diff. Report only defects you can point at...
//! ```
//!
//! The `description` is load-bearing: it is what the delegating agent sees in
//! the `task` tool and the only basis on which it picks. "Reviews code" makes an
//! agent dead weight; saying *when* to reach for it is the difference.

use camino::{Utf8Path, Utf8PathBuf};
use octane_permission::PermissionMode;
use serde::{Deserialize, Serialize};

/// Where a definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentScope {
    /// Compiled in. Overridable by a file of the same name.
    Builtin,
    /// `~/.octane/agents/`.
    User,
    /// `<repo>/.octane/agents/`.
    Project,
}

impl AgentScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "built-in",
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// How an agent may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Selectable by the user, and may delegate.
    Primary,
    /// Invoked through the `task` tool. May not delegate further.
    #[default]
    Subagent,
    /// Internal: compaction, titles. Never offered.
    Hidden,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Frontmatter {
    /// What this agent is *for*. Shown to the delegating model.
    pub description: String,

    #[serde(default)]
    pub mode: AgentMode,

    /// Tool names this agent may use. Empty means all.
    ///
    /// Enforced by omitting the schema from the prompt rather than refusing at
    /// call time: a tool the model can see is one it will try, and the refusal
    /// it then reasons around is context spent on nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,

    /// `provider/model`, overriding the session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_override: Option<PermissionMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<octane_provider::Thinking>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefinition {
    pub name: String,
    pub scope: AgentScope,
    pub frontmatter: Frontmatter,
    /// The system prompt.
    pub prompt: String,
    /// `None` for built-ins.
    pub path: Option<Utf8PathBuf>,
}

impl AgentDefinition {
    /// The line the delegating model sees in the `task` tool description.
    pub fn manifest_entry(&self) -> String {
        format!("- {}: {}", self.name, self.frontmatter.description)
    }

    pub fn permits_tool(&self, tool: &str) -> bool {
        self.frontmatter.tools.is_empty()
            || self.frontmatter.tools.iter().any(|name| name == tool)
    }

    /// Whether this agent may itself delegate.
    ///
    /// Subagents may not: unbounded recursion is spend with no upside, and every
    /// harness surveyed draws the line here (`RESEARCH.md` §6).
    pub fn can_delegate(&self) -> bool {
        self.frontmatter.mode == AgentMode::Primary
    }

    pub fn is_offered(&self) -> bool {
        self.frontmatter.mode != AgentMode::Hidden
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("{path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },

    #[error("{path}: missing YAML frontmatter delimited by ---")]
    MissingFrontmatter { path: String },

    #[error("{path}: {source}")]
    InvalidFrontmatter { path: String, #[source] source: serde_yaml::Error },

    #[error("{path}: description must not be empty — it is how the model picks")]
    NoDescription { path: String },
}

pub const AGENTS_DIR: &str = "agents";

/// Built-ins, then user files, then project files.
///
/// Later scopes replace earlier ones by name, so a project can retune an agent
/// without copying the rest.
pub fn discover_agents(roots: &[Utf8PathBuf]) -> (Vec<AgentDefinition>, Vec<AgentError>) {
    let mut found: std::collections::BTreeMap<String, AgentDefinition> = builtin_agents()
        .into_iter()
        .map(|agent| (agent.name.clone(), agent))
        .collect();
    let mut errors = Vec::new();

    for (index, root) in roots.iter().enumerate() {
        // Roots arrive least-specific first, so the last is the project.
        let scope =
            if index + 1 == roots.len() { AgentScope::Project } else { AgentScope::User };

        let dir = root.join(AGENTS_DIR);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
                continue;
            };
            if path.extension() != Some("md") {
                continue;
            }
            match load(&path, scope) {
                Ok(agent) => {
                    found.insert(agent.name.clone(), agent);
                }
                // Reported, not fatal: one malformed file must not stop a
                // session, and the user needs to know which one.
                Err(error) => errors.push(error),
            }
        }
    }

    (found.into_values().collect(), errors)
}

/// Load one definition.
pub fn load(path: &Utf8Path, scope: AgentScope) -> Result<AgentDefinition, AgentError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|source| AgentError::Io { path: path.to_string(), source })?;

    let rest = raw
        .strip_prefix("---")
        .ok_or_else(|| AgentError::MissingFrontmatter { path: path.to_string() })?;
    let (yaml, body) = rest
        .split_once("\n---")
        .ok_or_else(|| AgentError::MissingFrontmatter { path: path.to_string() })?;

    let frontmatter: Frontmatter = serde_yaml::from_str(yaml)
        .map_err(|source| AgentError::InvalidFrontmatter { path: path.to_string(), source })?;

    if frontmatter.description.trim().is_empty() {
        return Err(AgentError::NoDescription { path: path.to_string() });
    }

    Ok(AgentDefinition {
        name: path.file_stem().unwrap_or_default().to_string(),
        scope,
        frontmatter,
        // Trimmed at both ends: leading and trailing whitespace in a system
        // prompt is noise, and a trailing newline is what every editor adds.
        prompt: body.trim_start_matches('-').trim().to_string(),
        path: Some(path.to_owned()),
    })
}

/// The agents octane ships with.
///
/// Four specialists plus the two primaries. The split is the one the survey
/// converged on (`RESEARCH.md` §6): delegate anything whose *process* you do not
/// want in the main context, and keep the result.
pub fn builtin_agents() -> Vec<AgentDefinition> {
    let agent = |name: &str, description: &str, mode, tools: &[&str], prompt: &str| {
        AgentDefinition {
            name: name.to_string(),
            scope: AgentScope::Builtin,
            frontmatter: Frontmatter {
                description: description.to_string(),
                mode,
                tools: tools.iter().map(|t| (*t).to_string()).collect(),
                ..Default::default()
            },
            prompt: prompt.to_string(),
            path: None,
        }
    };

    const READ_ONLY: &[&str] = &["read", "grep", "glob", "list"];

    vec![
        agent(
            "build",
            "Implements changes. Full tool access.",
            AgentMode::Primary,
            &[],
            "",
        ),
        agent(
            "plan",
            "Investigates and plans without changing anything.",
            AgentMode::Primary,
            READ_ONLY,
            "",
        ),
        agent(
            "research",
            "Explores unfamiliar code or a broad question and reports findings. \
             Read-only. Use when locating or understanding something would fill \
             the main context with search output.",
            AgentMode::Subagent,
            READ_ONLY,
            "You are researching, not changing anything.\n\n\
             Find what was asked for and report it. Cite file paths and line \
             numbers so the caller can go straight there. Say plainly when \
             something is not present rather than reporting the nearest match as \
             if it were the answer — a confident wrong location costs more than \
             an honest miss.\n\n\
             Return findings, not a narration of your search.",
        ),
        agent(
            "test",
            "Writes and runs tests for a change, and reports what actually failed. \
             Use after implementing something, or to reproduce a reported bug.",
            AgentMode::Subagent,
            &["read", "write", "edit", "grep", "glob", "list", "bash"],
            "You are testing.\n\n\
             Run the tests and report the real output, including failures. Never \
             describe a test as passing that you did not see pass. If a test fails \
             for an environmental reason rather than a defect, say which.\n\n\
             When writing tests, name them for the property they protect rather \
             than the function they call, and prefer one that would have caught \
             the bug over several that restate the implementation.",
        ),
        agent(
            "code",
            "Implements a well-specified change end to end. Use when the approach \
             is already decided and the work is contained.",
            AgentMode::Subagent,
            &["read", "write", "edit", "grep", "glob", "list", "bash"],
            "You are implementing a change that has already been specified.\n\n\
             Match the surrounding code's conventions rather than importing your \
             own. Make the change asked for and stop — unrequested improvements \
             are a cost to review, not a gift.\n\n\
             Report what you changed and anything you could not do, rather than \
             claiming completion you did not verify.",
        ),
        agent(
            "critic",
            "Argues against a change or a claim, looking for what breaks. Use \
             before committing to something expensive or hard to reverse.",
            AgentMode::Subagent,
            READ_ONLY,
            "You are looking for what is wrong.\n\n\
             Your job is to find the defect, the missed case, or the assumption \
             that does not hold — not to be agreeable. For each problem, give the \
             concrete input or situation that triggers it. A criticism you cannot \
             ground in a specific failure is not worth reporting.\n\n\
             If the thing is sound, say so in one line rather than manufacturing \
             objections. False positives cost more than silence, because they \
             teach the caller to stop reading.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    fn write_agent(root: &Utf8Path, name: &str, body: &str) {
        let dir = root.join(AGENTS_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.md")), body).unwrap();
    }

    #[test]
    fn the_builtins_cover_the_four_specialisms() {
        let agents = builtin_agents();
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        for expected in ["research", "test", "code", "critic"] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn every_builtin_says_when_to_use_it_not_just_what_it_is() {
        // The description is the only basis the model picks on.
        for agent in builtin_agents() {
            if agent.frontmatter.mode != AgentMode::Subagent {
                continue;
            }
            assert!(
                agent.frontmatter.description.to_lowercase().contains("use "),
                "{} does not say when to reach for it",
                agent.name
            );
        }
    }

    #[test]
    fn read_only_agents_cannot_reach_mutating_tools() {
        let agents = builtin_agents();
        let research = agents.iter().find(|a| a.name == "research").expect("research agent");
        assert!(research.permits_tool("grep"));
        assert!(!research.permits_tool("write"));
        assert!(!research.permits_tool("bash"));

        let critic = agents.iter().find(|a| a.name == "critic").expect("critic agent");
        assert!(!critic.permits_tool("edit"));
    }

    #[test]
    fn subagents_cannot_delegate_further() {
        // Unbounded recursion is spend with no upside.
        for agent in builtin_agents() {
            if agent.frontmatter.mode == AgentMode::Subagent {
                assert!(!agent.can_delegate(), "{} can delegate", agent.name);
            }
        }
    }

    #[test]
    fn a_definition_loads_from_markdown() {
        let (_guard, root) = temp();
        write_agent(
            &root,
            "reviewer",
            "---\ndescription: Reviews a diff. Use before committing.\nmode: subagent\ntools: [read, grep]\n---\nYou are reviewing.\n",
        );

        let (agents, errors) = discover_agents(&[root]);
        assert!(errors.is_empty());

        let reviewer = agents.iter().find(|a| a.name == "reviewer").unwrap();
        assert_eq!(reviewer.scope, AgentScope::Project);
        assert_eq!(reviewer.frontmatter.tools, vec!["read", "grep"]);
        assert!(reviewer.prompt.starts_with("You are reviewing."));
    }

    #[test]
    fn a_file_overrides_a_builtin_of_the_same_name() {
        let (_guard, root) = temp();
        write_agent(&root, "critic", "---\ndescription: My own critic. Use always.\n---\nMine.\n");

        let (agents, _) = discover_agents(&[root]);
        let critic = agents.iter().find(|a| a.name == "critic").unwrap();
        assert_eq!(critic.scope, AgentScope::Project);
        assert_eq!(critic.prompt, "Mine.");
    }

    #[test]
    fn a_project_file_beats_a_user_file() {
        let (_user_guard, user) = temp();
        let (_project_guard, project) = temp();
        write_agent(&user, "shared", "---\ndescription: from user. Use it.\n---\nUser.\n");
        write_agent(&project, "shared", "---\ndescription: from project. Use it.\n---\nProject.\n");

        let (agents, _) = discover_agents(&[user, project]);
        let shared = agents.iter().find(|a| a.name == "shared").unwrap();
        assert_eq!(shared.prompt, "Project.");
        assert_eq!(shared.scope, AgentScope::Project);
    }

    #[test]
    fn an_empty_description_is_rejected() {
        // An agent the model cannot choose is worse than no agent.
        let (_guard, root) = temp();
        write_agent(&root, "nameless", "---\ndescription: \"\"\n---\nBody.\n");

        let (_agents, errors) = discover_agents(&[root]);
        assert!(matches!(errors.first(), Some(AgentError::NoDescription { .. })));
    }

    #[test]
    fn a_malformed_file_is_reported_but_the_rest_still_load() {
        let (_guard, root) = temp();
        write_agent(&root, "broken", "no frontmatter here\n");
        write_agent(&root, "fine", "---\ndescription: Works. Use it.\n---\nBody.\n");

        let (agents, errors) = discover_agents(&[root]);
        assert!(agents.iter().any(|a| a.name == "fine"));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().contains("broken"));
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let (_guard, root) = temp();
        let (agents, errors) = discover_agents(&[root]);
        assert!(errors.is_empty());
        // ...and the builtins survive.
        assert!(agents.iter().any(|a| a.name == "research"));
    }

    #[test]
    fn hidden_agents_are_not_offered() {
        let (_guard, root) = temp();
        write_agent(&root, "titler", "---\ndescription: Names sessions.\nmode: hidden\n---\nB.\n");

        let (agents, _) = discover_agents(&[root]);
        let titler = agents.iter().find(|a| a.name == "titler").unwrap();
        assert!(!titler.is_offered());
    }
}
