//! The `task` tool: delegating to a subagent.
//!
//! The highest-leverage tool in the set, and the answer to context collapse
//! (`RESEARCH.md` §6). A search sweep that would burn 40k tokens of the main
//! window comes back as a few hundred, because the *process* stays in the
//! subagent's context and only the result crosses back.
//!
//! # Why the work happens behind a trait
//!
//! Running a subagent means running a whole turn: a model, a tool registry
//! filtered to what that agent may use, a policy, an approver, a budget. Owning
//! all of that here would make this tool untestable and would drag half the
//! application into a leaf module. [`Delegate`] is the seam — the caller supplies
//! the machinery, and this tool owns only the decision of which agent runs and
//! what it is told.

use std::sync::Arc;

use async_trait::async_trait;
use octane_config::AgentDefinition;
use octane_tools::{Tool, ToolContext, ToolError, ToolOutcome};
use tokio_util::sync::CancellationToken;

/// Runs a subagent to completion.
#[async_trait]
pub trait Delegate: Send + Sync {
    /// Run `agent` against `prompt`, returning its final message.
    async fn run(
        &self,
        agent: &AgentDefinition,
        prompt: &str,
        cancel: CancellationToken,
    ) -> Result<String, String>;
}

pub struct TaskTool {
    agents: Vec<AgentDefinition>,
    delegate: Arc<dyn Delegate>,
    description: String,
}

impl std::fmt::Debug for TaskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskTool")
            .field("agents", &self.agents.iter().map(|a| &a.name).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl TaskTool {
    pub fn new(agents: Vec<AgentDefinition>, delegate: Arc<dyn Delegate>) -> Self {
        // Only agents that can actually be delegated to, and only ones the model
        // is allowed to choose. Offering a primary agent here would let the model
        // spawn something that can delegate again.
        let agents: Vec<AgentDefinition> = agents
            .into_iter()
            .filter(|agent| {
                agent.is_offered() && !agent.can_delegate()
            })
            .collect();

        let description = Self::describe(&agents);
        Self { agents, delegate, description }
    }

    /// Build the tool description from the available agents.
    ///
    /// The agent list *is* the description — that is the trick opencode and
    /// Claude Code both use. A static description cannot tell the model which
    /// specialist exists, so the roster has to be generated.
    fn describe(agents: &[AgentDefinition]) -> String {
        let mut out = String::from(
            "Delegates a task to a specialised subagent running in its own context.\n\n\
             Use this when the *process* of a task would fill your context with output \
             you do not need to keep — a broad search, a test run, a review. What comes \
             back is the subagent's report, not its transcript.\n\n\
             Available agents:\n",
        );
        for agent in agents {
            out.push_str(&agent.manifest_entry());
            out.push('\n');
        }
        out.push_str(
            "\nUsage:\n\
             - Launch several in one message when the work is independent; they run \
               concurrently.\n\
             - Each invocation is stateless and one-shot. The subagent cannot ask you \
               anything, so `prompt` must be self-contained and must say exactly what to \
               report back.\n\
             - Say whether you want investigation or changes — the subagent does not know \
               what the user asked for.\n\
             - The result is not shown to the user. Summarise anything they should see.\n\
             - Do not delegate work you can do in one or two tool calls; the round trip \
               costs more than it saves.\n",
        );
        out
    }

    pub fn agents(&self) -> &[AgentDefinition] {
        &self.agents
    }
}

#[derive(Debug, serde::Deserialize)]
struct Input {
    /// Which agent to run.
    agent: String,
    /// What it should do. Must be self-contained.
    prompt: String,
    /// A few words for the UI, so a running task is identifiable.
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        let names: Vec<&str> = self.agents.iter().map(|agent| agent.name.as_str()).collect();
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    // Enumerated rather than free text: a hallucinated agent name
                    // is a wasted round trip, and the schema can prevent it.
                    "enum": names,
                    "description": "Which subagent to run."
                },
                "prompt": {
                    "type": "string",
                    "description": "Self-contained instructions, including what to report back."
                },
                "description": {
                    "type": "string",
                    "description": "3-5 words naming the task, shown to the user."
                }
            },
            "required": ["agent", "prompt"],
            "additionalProperties": false
        })
    }

    fn required_permissions(&self, _input: &str, _ctx: &ToolContext) -> Vec<String> {
        // Delegation itself grants nothing. The subagent's own tool calls are
        // authorized individually, against the same policy, as they happen —
        // so a `plan` subagent cannot become a way to reach `write`.
        Vec::new()
    }

    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<ToolOutcome, ToolError> {
        let parsed: Input = serde_json::from_str(input)
            .map_err(|error| ToolError::Recoverable(format!("invalid arguments: {error}")))?;

        let agent = self
            .agents
            .iter()
            .find(|agent| agent.name == parsed.agent)
            .ok_or_else(|| {
                ToolError::Recoverable(format!(
                    "no agent named {:?}. Available: {}",
                    parsed.agent,
                    self.agents
                        .iter()
                        .map(|agent| agent.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        if parsed.prompt.trim().is_empty() {
            return Err(ToolError::Recoverable(
                "prompt must not be empty — the subagent cannot ask what you meant".into(),
            ));
        }

        let title = parsed
            .description
            .clone()
            .unwrap_or_else(|| format!("{} subagent", agent.name));

        match self.delegate.run(agent, &parsed.prompt, ctx.cancel.clone()).await {
            Ok(report) if report.trim().is_empty() => {
                // A silent subagent is a failure the caller must hear about, not
                // an empty string it will try to interpret.
                Err(ToolError::Recoverable(format!(
                    "the {} subagent returned nothing",
                    agent.name
                )))
            }
            Ok(report) => Ok(ToolOutcome::new(title, report).with_metadata(serde_json::json!({
                "agent": agent.name,
                "scope": agent.scope.label(),
            }))),
            Err(error) => Err(ToolError::Recoverable(format!(
                "the {} subagent failed: {error}",
                agent.name
            ))),
        }
    }

    fn is_mutating(&self) -> bool {
        // Depends entirely on which agent is chosen, so the conservative answer
        // is the only safe one: a `plan` agent must not gain write access by
        // delegating to `code`.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_config::agent::AgentMode;
    use octane_protocol::{SessionId, ToolCallId};

    struct Echo;

    #[async_trait]
    impl Delegate for Echo {
        async fn run(
            &self,
            agent: &AgentDefinition,
            prompt: &str,
            _cancel: CancellationToken,
        ) -> Result<String, String> {
            Ok(format!("{} handled: {prompt}", agent.name))
        }
    }

    struct Failing;

    #[async_trait]
    impl Delegate for Failing {
        async fn run(
            &self,
            _agent: &AgentDefinition,
            _prompt: &str,
            _cancel: CancellationToken,
        ) -> Result<String, String> {
            Err("model unavailable".into())
        }
    }

    struct Silent;

    #[async_trait]
    impl Delegate for Silent {
        async fn run(
            &self,
            _agent: &AgentDefinition,
            _prompt: &str,
            _cancel: CancellationToken,
        ) -> Result<String, String> {
            Ok("   ".into())
        }
    }

    fn tool(delegate: Arc<dyn Delegate>) -> TaskTool {
        TaskTool::new(octane_config::agent::builtin_agents(), delegate)
    }

    fn context() -> ToolContext {
        ToolContext {
            session_id: SessionId::new(),
            call_id: ToolCallId::new(),
            agent: "build".into(),
            workspace: "/p".into(),
            cwd: "/p".into(),
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn only_subagents_are_offered() {
        // A primary agent could delegate again; offering one here would allow
        // unbounded recursion through the tool.
        let tool = tool(Arc::new(Echo));
        let names: Vec<&str> = tool.agents().iter().map(|a| a.name.as_str()).collect();

        assert!(names.contains(&"research"));
        assert!(!names.contains(&"build"), "primary agents must not be delegable");
        assert!(!names.contains(&"plan"));
    }

    #[test]
    fn the_description_is_the_roster() {
        // A static description cannot say which specialists exist.
        let tool = tool(Arc::new(Echo));
        let description = tool.description();

        assert!(description.contains("research:"));
        assert!(description.contains("critic:"));
        // ...and the guidance that makes delegation work rather than thrash.
        assert!(description.contains("self-contained"));
        assert!(description.contains("concurrently"));
    }

    #[test]
    fn the_schema_enumerates_agents_rather_than_taking_free_text() {
        // A hallucinated name is a wasted round trip the schema can prevent.
        let tool = tool(Arc::new(Echo));
        let schema = tool.input_schema();
        let names = schema["properties"]["agent"]["enum"].as_array().unwrap();

        assert!(names.iter().any(|name| name == "research"));
        assert!(!names.iter().any(|name| name == "build"));
    }

    #[test]
    fn delegation_itself_requires_no_permission() {
        // The subagent's own calls are authorized individually as they happen,
        // so a plan agent cannot reach `write` by delegating.
        let tool = tool(Arc::new(Echo));
        assert!(tool.required_permissions(r#"{"agent":"code","prompt":"x"}"#, &context()).is_empty());
    }

    #[tokio::test]
    async fn a_task_runs_and_returns_the_report() {
        let tool = tool(Arc::new(Echo));
        let outcome = tool
            .execute(
                r#"{"agent":"research","prompt":"find the loop","description":"locate loop"}"#,
                &context(),
            )
            .await
            .unwrap();

        assert_eq!(outcome.output, "research handled: find the loop");
        assert_eq!(outcome.title, "locate loop");
        assert_eq!(outcome.metadata.unwrap()["agent"], "research");
    }

    #[tokio::test]
    async fn an_unknown_agent_lists_the_real_ones() {
        let tool = tool(Arc::new(Echo));
        let error = tool
            .execute(r#"{"agent":"nonexistent","prompt":"x"}"#, &context())
            .await
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("nonexistent"));
        assert!(message.contains("research"), "should list what does exist");
    }

    #[tokio::test]
    async fn an_empty_prompt_is_refused() {
        // The subagent is one-shot and cannot ask what was meant.
        let tool = tool(Arc::new(Echo));
        let error = tool
            .execute(r#"{"agent":"research","prompt":"   "}"#, &context())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot ask"));
    }

    #[tokio::test]
    async fn a_silent_subagent_is_an_error_not_an_empty_result() {
        let tool = tool(Arc::new(Silent));
        let error = tool
            .execute(r#"{"agent":"research","prompt":"x"}"#, &context())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("returned nothing"));
    }

    #[tokio::test]
    async fn a_failing_subagent_is_reported_to_the_model_not_fatal() {
        let tool = tool(Arc::new(Failing));
        let error = tool
            .execute(r#"{"agent":"research","prompt":"x"}"#, &context())
            .await
            .unwrap_err();

        assert!(error.is_reportable_to_model());
        assert!(error.to_string().contains("model unavailable"));
    }

    #[tokio::test]
    async fn a_task_without_a_description_still_has_a_title() {
        let tool = tool(Arc::new(Echo));
        let outcome = tool
            .execute(r#"{"agent":"critic","prompt":"review this"}"#, &context())
            .await
            .unwrap();
        assert!(outcome.title.contains("critic"));
    }

    #[test]
    fn hidden_agents_are_never_offered() {
        let mut agents = octane_config::agent::builtin_agents();
        agents.push(AgentDefinition {
            name: "titler".into(),
            scope: octane_config::AgentScope::Builtin,
            frontmatter: octane_config::agent::Frontmatter {
                description: "Names sessions.".into(),
                mode: AgentMode::Hidden,
                ..Default::default()
            },
            prompt: String::new(),
            path: None,
        });

        let tool = TaskTool::new(agents, Arc::new(Echo));
        assert!(!tool.agents().iter().any(|agent| agent.name == "titler"));
    }
}
