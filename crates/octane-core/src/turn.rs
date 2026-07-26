//! The turn runner: one user input driven to completion.
//!
//! Holds the sequencing and nothing else. Every decision is delegated:
//!
//! | Question | Answered by |
//! |---|---|
//! | is this allowed? | `octane_permission::Policy` |
//! | what can it reach? | `octane_sandbox` |
//! | is there room? | `octane_context::Budget` |
//! | what does the tool do? | `octane_tools::Tool` |
//! | are we stuck? | [`crate::LoopGuard`] |
//!
//! Because those are all traits or plain data, the loop is testable without a
//! model, a shell, or a network — see the tests at the bottom, which drive a
//! scripted provider.

use std::sync::Arc;

use async_trait::async_trait;
use octane_context::{Budget, Pressure};
use octane_permission::{Decision, PermissionMode, Policy, Resource};
use octane_protocol::{Message, Part, Role, ToolCall, ToolResult};
use octane_tools::{ToolContext, ToolError, ToolRegistry};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::loop_guard::LoopGuard;
use crate::step::{DEFAULT_STEP_LIMIT, StepDecision, StopReason};

/// Successful compactions allowed in one turn.
///
/// The compaction path does not take a step, so the step limit cannot bound
/// it. Without a cap, a large preserved prefix can leave pressure high after a
/// successful compaction and the loop pays for another summarization every
/// pass.
const MAX_COMPACTIONS_PER_TURN: u32 = 2;

/// How the runner asks the user for permission.
///
/// A trait so the loop does not know whether it is talking to a TUI, an RPC
/// client, or a test. The TUI implementation is what renders the diff-review
/// prompt; the loop only needs a yes or no.
#[async_trait]
pub trait Approver: Send + Sync {
    /// Ask about `resource`. `preview` carries a diff for edits.
    ///
    /// Returning `false` ends the turn — see [`StopReason::Denied`].
    async fn request(&self, resource: &Resource, preview: Option<&str>) -> bool;
}

/// One inference step's worth of model output, already normalized.
///
/// The runner consumes this rather than a raw stream so that stream decoding
/// stays in the provider layer and the loop's control flow is testable
/// synchronously.
#[derive(Debug, Clone, Default)]
pub struct StepOutput {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// True when the model asked for tools; the loop continues.
    pub wants_tools: bool,
    /// True when the model was cut off by its own output limit.
    pub truncated: bool,
    pub tokens_used: usize,
}

/// Produces steps. Implemented over a real provider stream in the provider layer;
/// implemented as a script in tests.
#[async_trait]
pub trait StepSource: Send + Sync {
    /// Produce one step, publishing to `sink` as content arrives.
    ///
    /// The sink is what makes streaming possible: a source that only returned
    /// the finished step could not show a token before the step ended.
    async fn next_step(
        &self,
        messages: &[Message],
        sink: &crate::events::EventSink,
    ) -> Result<StepOutput, crate::CoreError>;
}

pub struct TurnRunner {
    pub agent: Agent,
    pub mode: PermissionMode,
    pub policy: Policy,
    pub registry: Arc<ToolRegistry>,
    pub approver: Arc<dyn Approver>,
    pub budget: Budget,
    pub step_limit: u32,
    pub cancel: CancellationToken,
    /// Where the loop publishes as it goes.
    pub events: crate::events::EventSink,
    /// Turns an old span into a paragraph. Without one the loop cannot compact
    /// and fails at the threshold, which is the behaviour before compaction
    /// existed.
    pub summarizer: Option<Arc<dyn octane_context::compact::Summarizer>>,
    /// How many leading messages are the cached prompt prefix, and so must
    /// survive compaction untouched.
    pub preserved_prefix: usize,
    guard: LoopGuard,
}

impl std::fmt::Debug for TurnRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `approver` and `registry` hold trait objects; the rest is what is
        // actually useful when a turn misbehaves.
        f.debug_struct("TurnRunner")
            .field("agent", &self.agent.name)
            .field("mode", &self.mode)
            .field("policy_rules", &self.policy.rule_count())
            .field("tools", &self.registry.len())
            .field("step_limit", &self.step_limit)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// What one turn produced.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub stop_reason: StopReason,
    /// Messages to append to the thread.
    pub messages: Vec<Message>,
    pub steps_taken: u32,
    /// The conversation the caller should keep, when the runner reduced it.
    ///
    /// Without this, compaction is discarded at turn end: the caller holds its
    /// own conversation and re-prefixes it every turn, so the runner's copy,
    /// pruned or compacted, never leaves the loop. The result would be a paid
    /// summarization on every turn that never shrinks anything. `None` means
    /// the caller's copy is still correct.
    pub history_replacement: Option<Vec<Message>>,
}

impl TurnOutcome {
    /// Carry a reduced conversation back to the caller.
    ///
    /// `None` leaves the caller's copy in place, which is correct whenever the
    /// runner did not compact.
    fn with_history(mut self, history: Option<Vec<Message>>) -> Self {
        self.history_replacement = history;
        self
    }
}

impl TurnRunner {
    pub fn new(
        agent: Agent,
        policy: Policy,
        registry: Arc<ToolRegistry>,
        approver: Arc<dyn Approver>,
        budget: Budget,
    ) -> Self {
        let mode = agent.mode;
        Self {
            agent,
            mode,
            policy,
            registry,
            approver,
            budget,
            step_limit: DEFAULT_STEP_LIMIT,
            cancel: CancellationToken::new(),
            events: crate::events::EventSink::discard(),
            summarizer: None,
            preserved_prefix: 0,
            guard: LoopGuard::default(),
        }
    }

    /// Drive one turn to completion.
    ///
    /// `history` is the conversation so far; the returned messages are what to
    /// append. Prompt assembly happens in [`crate::PromptAssembler`] — this
    /// function receives already-assembled history so the two concerns stay apart.
    pub async fn run(
        &mut self,
        source: &dyn StepSource,
        mut history: Vec<Message>,
        cwd: camino::Utf8PathBuf,
        workspace: camino::Utf8PathBuf,
    ) -> TurnOutcome {
        let mut appended: Vec<Message> = Vec::new();
        let mut steps = 0u32;
        // Compaction bookkeeping. `compactions` bounds the successful ones:
        // that path does not take a step, so the step limit cannot bound it,
        // and a shrink of one token would otherwise be enough to keep going
        // and pay for another summarization.
        let mut compactions = 0u32;
        let mut compaction_failures = 0u32;
        let mut reduced = false;

        loop {
            if self.cancel.is_cancelled() {
                return self.finish(StopReason::Interrupted, appended, steps);
            }
            if steps >= self.step_limit {
                return self.finish(StopReason::StepLimit { limit: self.step_limit }, appended, steps)
                    .with_history(reduced.then(|| history.clone()));
            }

            // Context is checked *before* the call, not after a failure. Reacting
            // to `prompt_too_long` is a fallback, not a plan.
            let used = octane_context::prune::estimate_tokens(&history);
            let reclaimable =
                octane_context::prune::reclaimable_tokens(&history, octane_context::PRUNE_PROTECT_TOKENS);

            match self.budget.pressure(used, reclaimable) {
                Pressure::ShouldPrune => {
                    let outcome = octane_context::prune(
                        history,
                        octane_context::PRUNE_PROTECT_TOKENS,
                        octane_context::PRUNE_MINIMUM_TOKENS,
                    );

                    // A prune that reclaims nothing must not loop. This arm
                    // does not take a step, so `steps` never advances and the
                    // step limit above cannot end it; with the history
                    // unchanged the next pass computes the same pressure and
                    // arrives back here, forever.
                    //
                    // Unreachable as long as `reclaimable_tokens` counts
                    // exactly what `prune` will clear, which is the property
                    // `the_estimate_never_promises_more_than_the_prune_delivers`
                    // pins down. Kept because that agreement is a convention
                    // between two functions, and the failure it prevents is a
                    // silent hang rather than a wrong answer.
                    if outcome.tokens_reclaimed == 0 {
                        return self.finish(
                            StopReason::Failed {
                                message: "context is full and cannot be pruned further".into(),
                            },
                            appended,
                            steps,
                        );
                    }

                    history = outcome.messages;
                    continue;
                }
                Pressure::ShouldCompact | Pressure::Exceeded => {
                    let Some(summarizer) = self.summarizer.clone() else {
                        return self.finish(
                            StopReason::Failed {
                                message: "context requires compaction, and no summarizer is \
                                          configured"
                                    .into(),
                            },
                            appended,
                            steps,
                        );
                    };

                    if compactions >= MAX_COMPACTIONS_PER_TURN
                        || compaction_failures >= octane_context::COMPACTION_FAILURE_LIMIT
                    {
                        return self.finish_with_history(
                            StopReason::Failed {
                                message: octane_context::ContextError::CircuitOpen {
                                    attempts: compaction_failures.max(compactions),
                                }
                                .to_string(),
                            },
                            appended,
                            steps,
                            history,
                        );
                    }

                    let plan = match octane_context::compact::plan(
                        &history,
                        self.preserved_prefix,
                        self.budget.effective_window(),
                    ) {
                        Ok(plan) => plan,
                        Err(error) => {
                            return self.finish_with_history(
                                StopReason::Failed { message: error.to_string() },
                                appended,
                                steps,
                                history,
                            );
                        }
                    };

                    let summary = match summarizer.summarize(&history[plan.start..plan.end]).await {
                        Ok(summary) => summary,
                        Err(error) => {
                            compaction_failures += 1;
                            if compaction_failures >= octane_context::COMPACTION_FAILURE_LIMIT {
                                return self.finish_with_history(
                                    StopReason::Failed { message: error.to_string() },
                                    appended,
                                    steps,
                                    history,
                                );
                            }
                            continue;
                        }
                    };

                    // Compaction is a paid model call the session would not
                    // otherwise see, and both the status line and /stats
                    // accumulate from reported usage.
                    self.events.usage(summary.usage);

                    // Built and measured before it is installed. Installing
                    // first and checking after leaves a summary larger than
                    // what it replaced sitting in the history, and the retry
                    // then summarizes the summary.
                    let candidate = octane_context::compact::apply(&history, &plan, &summary);
                    let after = octane_context::prune::estimate_tokens(&candidate);
                    if after >= used {
                        compaction_failures += 1;
                        if compaction_failures >= octane_context::COMPACTION_FAILURE_LIMIT {
                            return self.finish_with_history(
                                StopReason::Failed {
                                    message: octane_context::ContextError::CircuitOpen {
                                        attempts: compaction_failures,
                                    }
                                    .to_string(),
                                },
                                appended,
                                steps,
                                history,
                            );
                        }
                        continue;
                    }

                    self.events.send(octane_protocol::Event::Compaction {
                        before_tokens: used as u64,
                        after_tokens: after as u64,
                        strategy: "summarize".into(),
                    });

                    history = candidate;
                    compactions += 1;
                    compaction_failures = 0;
                    reduced = true;
                    continue;
                }
                Pressure::Comfortable | Pressure::Warning => {}
            }

            let step = match source.next_step(&history, &self.events).await {
                Ok(step) => step,
                Err(error) => {
                    return self.finish(
                        StopReason::Failed { message: error.to_string() },
                        appended,
                        steps,
                    );
                }
            };
            steps += 1;

            // Persist what the model said before executing anything, so an
            // interrupt mid-tool leaves a coherent transcript.
            let mut assistant_parts = Vec::new();
            if !step.reasoning.is_empty() {
                assistant_parts.push(Part::Reasoning { text: step.reasoning.clone() });
            }
            if !step.text.is_empty() {
                assistant_parts.push(Part::Text { text: step.text.clone() });
            }
            for call in &step.tool_calls {
                assistant_parts.push(Part::ToolCall(call.clone()));
            }
            if !assistant_parts.is_empty() {
                let message = Message::new(Role::Assistant, assistant_parts);
                history.push(message.clone());
                appended.push(message);
            }

            if step.truncated {
                return self.finish(StopReason::OutputTruncated, appended, steps)
                    .with_history(reduced.then(|| history.clone()));
            }

            // The model produced prose instead of tool calls: the turn is done.
            // This is the only success path, and the model chose it.
            if !step.wants_tools || step.tool_calls.is_empty() {
                return self.finish(StopReason::Completed, appended, steps)
                    .with_history(reduced.then(|| history.clone()));
            }

            let mut result_parts = Vec::new();
            let mut interactions = Vec::new();

            for call in &step.tool_calls {
                if self.cancel.is_cancelled() {
                    return self.finish(StopReason::Interrupted, appended, steps);
                }

                match self.execute_call(call, &cwd, &workspace).await {
                    Ok(outcome) => {
                        // The summary, not the output. Publishing the output
                        // put whole file bodies and build logs in the
                        // transcript under the agent's own voice, including the
                        // `<file path=...>` framing that exists for the model.
                        // The model still gets all of it, below.
                        self.events.item(octane_protocol::ItemKind::ToolResult {
                            call_id: call.id.clone(),
                            name: call.name.clone(),
                            title: outcome.title.clone(),
                            metadata: outcome.metadata.clone(),
                            is_error: false,
                            body: outcome.output.clone(),
                        });
                        interactions.push((call.name.clone(), call.input.clone(), outcome.output.clone()));
                        result_parts.push(Part::ToolResult(ToolResult {
                            call_id: call.id.clone(),
                            output: outcome.output,
                            metadata: outcome.metadata,
                            is_error: false,
                        }));
                    }

                    // A refusal ends the turn. Reporting it to the model invites
                    // it to find another route to the same action, which turns a
                    // clear "no" into a negotiation.
                    Err(ToolError::Denied(resource)) => {
                        return self.finish(StopReason::Denied { resource }, appended, steps);
                    }

                    Err(ToolError::Cancelled) => {
                        return self.finish(StopReason::Interrupted, appended, steps);
                    }

                    // A broken harness is not the model's problem, and telling it
                    // invites a retry of something that cannot succeed. On a
                    // platform with no sandbox backend *every* `bash` call lands
                    // here, so reporting it would burn the whole step budget
                    // rediscovering that the tool is unavailable.
                    Err(error) if !error.is_reportable_to_model() => {
                        return self.finish(
                            StopReason::Failed { message: error.to_string() },
                            appended,
                            steps,
                        );
                    }

                    // Everything else is told to the model as a tool error, which
                    // is usually enough for it to correct itself. A wrong path is a
                    // conversation, not a crash.
                    Err(error) => {
                        let text = error.to_string();
                        self.events.item(octane_protocol::ItemKind::Error { message: text.clone() });
                        interactions.push((call.name.clone(), call.input.clone(), text.clone()));
                        result_parts.push(Part::ToolResult(ToolResult {
                            call_id: call.id.clone(),
                            output: text,
                            metadata: None,
                            is_error: true,
                        }));
                    }
                }
            }

            // Results travel as a user-role message: that is how tool output is
            // transported to the model.
            if !result_parts.is_empty() {
                let message = Message::new(Role::User, result_parts);
                history.push(message.clone());
                appended.push(message);
            }

            self.guard.record(&interactions);
            if let Some((signature, repeats)) = self.guard.detect() {
                return self.finish(StopReason::LoopDetected { signature, repeats }, appended, steps)
                    .with_history(reduced.then(|| history.clone()));
            }
        }
    }

    /// Look up, authorize, and run one tool call.
    async fn execute_call(
        &mut self,
        call: &ToolCall,
        cwd: &camino::Utf8Path,
        workspace: &camino::Utf8Path,
    ) -> Result<octane_tools::ToolOutcome, ToolError> {
        if !self.agent.permits_tool(&call.name) {
            // Should be unreachable: a disallowed tool is omitted from the prompt.
            // Reported as recoverable rather than panicking, since a model can
            // hallucinate a tool name.
            return Err(ToolError::Recoverable(format!(
                "tool {:?} is not available to the {} agent",
                call.name, self.agent.name
            )));
        }

        let tool = self
            .registry
            .get(&call.name)
            .ok_or_else(|| ToolError::Recoverable(format!("no tool named {:?}", call.name)))?
            .clone();

        let ctx = ToolContext {
            session_id: octane_protocol::SessionId::new(),
            call_id: call.id.clone(),
            agent: self.agent.name.clone(),
            workspace: workspace.to_owned(),
            cwd: cwd.to_owned(),
            cancel: self.cancel.clone(),
        };

        // Permissions are resolved from the *concrete arguments*, which is why the
        // tool derives them: `bash` needs the command, `write` needs the path.
        for resource in tool.required_permissions(&call.input, &ctx) {
            let Ok(parsed) = resource.parse::<Resource>() else {
                continue;
            };

            match self.policy.evaluate(&parsed, self.mode).decision {
                Decision::Allow => {}
                Decision::Deny => return Err(ToolError::Denied(parsed.to_string())),
                Decision::Ask => {
                    if !self.approver.request(&parsed, None).await {
                        return Err(ToolError::Denied(parsed.to_string()));
                    }
                    // Remember it, so the same question is not asked again.
                    let _ = self.policy.grant_for_session(&parsed);
                }
            }
        }

        tool.execute(&call.input, &ctx).await
    }

    fn finish(&self, reason: StopReason, messages: Vec<Message>, steps: u32) -> TurnOutcome {
        TurnOutcome { stop_reason: reason, messages, steps_taken: steps, history_replacement: None }
    }

    fn finish_with_history(
        &self,
        reason: StopReason,
        messages: Vec<Message>,
        steps: u32,
        history: Vec<Message>,
    ) -> TurnOutcome {
        TurnOutcome {
            stop_reason: reason,
            messages,
            steps_taken: steps,
            history_replacement: Some(history),
        }
    }

    /// Decide what to do after a step. Exposed for testing the rails in isolation.
    pub fn evaluate(&self, steps: u32, wants_tools: bool) -> StepDecision {
        if self.cancel.is_cancelled() {
            return StepDecision::Stop(StopReason::Interrupted);
        }
        if steps >= self.step_limit {
            return StepDecision::Stop(StopReason::StepLimit { limit: self.step_limit });
        }
        if let Some((signature, repeats)) = self.guard.detect() {
            return StepDecision::Stop(StopReason::LoopDetected { signature, repeats });
        }
        if !wants_tools {
            return StepDecision::Stop(StopReason::Completed);
        }
        StepDecision::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_permission::Scope;
    use octane_protocol::ToolCallId;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider replaced by a fixed script of steps.
    struct ScriptedSource {
        steps: Mutex<std::collections::VecDeque<StepOutput>>,
        calls: AtomicUsize,
    }

    impl ScriptedSource {
        fn new(steps: Vec<StepOutput>) -> Self {
            Self { steps: Mutex::new(steps.into()), calls: AtomicUsize::new(0) }
        }

        /// Repeats the same tool call forever — a stuck model.
        fn stuck() -> Self {
            Self::new(Vec::new())
        }
    }

    #[async_trait]
    impl StepSource for ScriptedSource {
        async fn next_step(
            &self,
            _messages: &[Message],
            _sink: &crate::events::EventSink,
        ) -> Result<StepOutput, crate::CoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = self.steps.lock().expect("scripted source lock").pop_front();
            Ok(next.unwrap_or_else(|| StepOutput {
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![ToolCall {
                    id: ToolCallId::new(),
                    name: "echo".into(),
                    input: "{\"text\":\"same\"}".into(),
                }],
                wants_tools: true,
                truncated: false,
                tokens_used: 10,
            }))
        }
    }

    struct AlwaysApprove;
    #[async_trait]
    impl Approver for AlwaysApprove {
        async fn request(&self, _resource: &Resource, _preview: Option<&str>) -> bool {
            true
        }
    }

    struct AlwaysDeny;
    #[async_trait]
    impl Approver for AlwaysDeny {
        async fn request(&self, _resource: &Resource, _preview: Option<&str>) -> bool {
            false
        }
    }

    /// A tool that echoes its input and needs `command(echo)`.
    struct EchoTool;

    #[async_trait]
    impl octane_tools::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes text."
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        fn required_permissions(&self, _input: &str, _ctx: &ToolContext) -> Vec<String> {
            vec!["command(echo)".into()]
        }
        async fn execute(
            &self,
            input: &str,
            _ctx: &ToolContext,
        ) -> Result<octane_tools::ToolOutcome, ToolError> {
            Ok(octane_tools::ToolOutcome::new("echo", format!("echoed: {input}")))
        }
    }

    fn registry() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        Arc::new(registry)
    }

    fn budget() -> Budget {
        Budget::new(200_000, 180_000, 170_000)
    }

    fn runner(approver: Arc<dyn Approver>, policy: Policy) -> TurnRunner {
        TurnRunner::new(Agent::build(), policy, registry(), approver, budget())
    }

    fn permissive_policy() -> Policy {
        Policy::builder().allow("command(*)", Scope::User).build().0
    }

    fn final_answer(text: &str) -> StepOutput {
        StepOutput {
            text: text.into(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            wants_tools: false,
            truncated: false,
            tokens_used: 5,
        }
    }

    fn tool_step(name: &str, input: &str) -> StepOutput {
        StepOutput {
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: ToolCallId::new(),
                name: name.into(),
                input: input.into(),
            }],
            wants_tools: true,
            truncated: false,
            tokens_used: 10,
        }
    }

    #[tokio::test]
    async fn a_plain_answer_completes_the_turn() {
        let mut runner = runner(Arc::new(AlwaysApprove), permissive_policy());
        let source = ScriptedSource::new(vec![final_answer("all done")]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;

        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.steps_taken, 1);
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tool_results_feed_the_next_step() {
        let mut runner = runner(Arc::new(AlwaysApprove), permissive_policy());
        let source = ScriptedSource::new(vec![
            tool_step("echo", "{\"text\":\"hi\"}"),
            final_answer("done"),
        ]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;

        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(outcome.steps_taken, 2);
        // The transcript must contain the call and its result, paired.
        let calls = outcome
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, Part::ToolCall(_)))
            .count();
        let results = outcome
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .filter(|p| matches!(p, Part::ToolResult(_)))
            .count();
        assert_eq!(calls, 1);
        assert_eq!(results, 1);
    }

    #[tokio::test]
    async fn a_denial_ends_the_turn_without_retrying() {
        let policy = Policy::builder().ask("command(*)", Scope::User).build().0;
        let mut runner = runner(Arc::new(AlwaysDeny), policy);
        let source = ScriptedSource::new(vec![
            tool_step("echo", "{\"text\":\"hi\"}"),
            final_answer("should never be reached"),
        ]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;

        assert!(matches!(outcome.stop_reason, StopReason::Denied { .. }));
        // Critically: the model was not given another step to route around it.
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_deny_rule_never_reaches_the_user() {
        let policy = Policy::builder().deny("command(echo)", Scope::Managed).build().0;
        // AlwaysApprove would say yes — but a deny rule must not be presented.
        let mut runner = runner(Arc::new(AlwaysApprove), policy);
        let source = ScriptedSource::new(vec![tool_step("echo", "{}")]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;
        assert!(matches!(outcome.stop_reason, StopReason::Denied { .. }));
    }

    #[tokio::test]
    async fn the_step_cap_bounds_a_runaway() {
        let mut runner = runner(Arc::new(AlwaysApprove), permissive_policy());
        runner.step_limit = 3;
        // Also disable loop detection so the cap is what stops it.
        runner.guard = LoopGuard::new(1_000, 1_000);

        let outcome = runner.run(&ScriptedSource::stuck(), Vec::new(), "/p".into(), "/p".into()).await;

        assert_eq!(outcome.stop_reason, StopReason::StepLimit { limit: 3 });
        assert_eq!(outcome.steps_taken, 3);
    }

    #[tokio::test]
    async fn loop_detection_stops_a_stuck_model_before_the_cap() {
        let mut runner = runner(Arc::new(AlwaysApprove), permissive_policy());
        runner.step_limit = 500;

        let outcome = runner.run(&ScriptedSource::stuck(), Vec::new(), "/p".into(), "/p".into()).await;

        assert!(matches!(outcome.stop_reason, StopReason::LoopDetected { .. }));
        // Far fewer than the cap: the point is to stop paying early.
        assert!(outcome.steps_taken < 20, "took {} steps", outcome.steps_taken);
    }

    #[tokio::test]
    async fn cancellation_is_honoured() {
        let mut runner = runner(Arc::new(AlwaysApprove), permissive_policy());
        runner.cancel.cancel();

        let source = ScriptedSource::new(vec![final_answer("never")]);
        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;

        assert_eq!(outcome.stop_reason, StopReason::Interrupted);
        assert_eq!(source.calls.load(Ordering::SeqCst), 0, "must not call the model at all");
    }

    #[tokio::test]
    async fn truncated_output_is_not_reported_as_success() {
        let mut runner = runner(Arc::new(AlwaysApprove), permissive_policy());
        let source = ScriptedSource::new(vec![StepOutput {
            text: "half an ans".into(),
            truncated: true,
            ..Default::default()
        }]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;
        assert_eq!(outcome.stop_reason, StopReason::OutputTruncated);
        assert!(!outcome.stop_reason.is_success());
    }

    #[tokio::test]
    async fn an_unknown_tool_is_reported_to_the_model_not_fatal() {
        let mut runner = runner(Arc::new(AlwaysApprove), permissive_policy());
        let source = ScriptedSource::new(vec![
            tool_step("no_such_tool", "{}"),
            final_answer("recovered"),
        ]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;

        // The model got a chance to correct itself.
        assert_eq!(outcome.stop_reason, StopReason::Completed);
        let has_error_result = outcome
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .any(|p| matches!(p, Part::ToolResult(r) if r.is_error));
        assert!(has_error_result);
    }

    #[tokio::test]
    async fn a_plan_agent_cannot_run_commands() {
        let mut runner = TurnRunner::new(
            Agent::plan(),
            permissive_policy(),
            registry(),
            Arc::new(AlwaysApprove),
            budget(),
        );
        let source = ScriptedSource::new(vec![tool_step("echo", "{}")]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;

        // Even with allow(command(*)) and an approver that says yes, plan mode
        // forces a deny.
        assert!(matches!(outcome.stop_reason, StopReason::Denied { .. }));
    }

    #[tokio::test]
    async fn approving_once_does_not_ask_again() {
        struct CountingApprover(AtomicUsize);
        #[async_trait]
        impl Approver for CountingApprover {
            async fn request(&self, _resource: &Resource, _preview: Option<&str>) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
        }

        let approver = Arc::new(CountingApprover(AtomicUsize::new(0)));
        let policy = Policy::builder().ask("command(*)", Scope::User).build().0;
        let mut runner = runner(approver.clone(), policy);

        let source = ScriptedSource::new(vec![
            tool_step("echo", "{\"text\":\"1\"}"),
            tool_step("echo", "{\"text\":\"2\"}"),
            final_answer("done"),
        ]);

        let outcome = runner.run(&source, Vec::new(), "/p".into(), "/p".into()).await;

        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(approver.0.load(Ordering::SeqCst), 1, "the grant should have been remembered");
    }


    /// A summarizer that returns a fixed paragraph and counts its calls.
    struct CannedSummarizer {
        text: String,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl octane_context::compact::Summarizer for CannedSummarizer {
        async fn summarize(
            &self,
            _span: &[Message],
        ) -> Result<octane_context::compact::Summary, octane_context::ContextError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(octane_context::compact::Summary {
                text: self.text.clone(),
                usage: octane_protocol::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    ..Default::default()
                },
            })
        }
    }

    fn crowded_history(messages: usize, filler: usize) -> Vec<Message> {
        (0..messages)
            .map(|i| Message::text(Role::User, format!("{i} {}", "x".repeat(filler))))
            .collect()
    }

    #[tokio::test]
    async fn a_compacted_history_reaches_the_caller() {
        // The runner holds its own copy while the caller keeps the
        // conversation and re-prefixes it every turn. Without
        // `history_replacement` the compaction is discarded at turn end, and
        // the session pays for a summarization every turn that never shrinks
        // anything.
        let summarizer = Arc::new(CannedSummarizer {
            text: "earlier: they discussed the parser".into(),
            calls: AtomicUsize::new(0),
        });

        let history = crowded_history(200, 400);
        let used = octane_context::prune::estimate_tokens(&history);

        let mut runner = TurnRunner::new(
            Agent::build(),
            permissive_policy(),
            registry(),
            Arc::new(AlwaysApprove),
            Budget::new(used + 1_000, used.saturating_sub(1_000), 1),
        );
        runner.summarizer = Some(summarizer.clone());

        let source = ScriptedSource::new(vec![final_answer("done")]);
        let outcome = runner.run(&source, history, "/p".into(), "/p".into()).await;

        assert_eq!(outcome.stop_reason, StopReason::Completed);
        assert_eq!(summarizer.calls.load(Ordering::SeqCst), 1, "it summarized once");

        let replacement = outcome
            .history_replacement
            .expect("a compacted turn must hand its history back");
        assert!(
            octane_context::prune::estimate_tokens(&replacement) < used,
            "the returned history must be smaller than what went in"
        );
        // Synthetic, not Text: `text_content` deliberately collects only the
        // model's own prose, and the summary is a developer message.
        assert!(
            replacement.iter().flat_map(|m| &m.parts).any(|part| matches!(
                part,
                Part::Synthetic { text } if text.contains("they discussed the parser")
            )),
            "the summary must be in it"
        );
    }

    #[tokio::test]
    async fn without_a_summarizer_the_turn_fails_rather_than_pretending() {
        let history = crowded_history(200, 400);
        let used = octane_context::prune::estimate_tokens(&history);

        let mut runner = TurnRunner::new(
            Agent::build(),
            permissive_policy(),
            registry(),
            Arc::new(AlwaysApprove),
            Budget::new(used + 1_000, used.saturating_sub(1_000), 1),
        );
        runner.summarizer = None;

        let source = ScriptedSource::new(vec![final_answer("unreachable")]);
        let outcome = runner.run(&source, history, "/p".into(), "/p".into()).await;

        assert!(matches!(outcome.stop_reason, StopReason::Failed { .. }));
        assert_eq!(source.calls.load(Ordering::SeqCst), 0, "the model was never called");
    }

    #[tokio::test]
    async fn compaction_is_bounded_within_one_turn() {
        // The compaction path does not take a step, so the step limit cannot
        // bound it. A summarizer that barely shrinks anything would otherwise
        // be paid for on every pass.
        struct BarelyShrinks(AtomicUsize);

        #[async_trait]
        impl octane_context::compact::Summarizer for BarelyShrinks {
            async fn summarize(
                &self,
                span: &[Message],
            ) -> Result<octane_context::compact::Summary, octane_context::ContextError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                // One byte under what it replaced, so it always "succeeds".
                let size = octane_context::prune::estimate_tokens(span) * 4;
                Ok(octane_context::compact::Summary {
                    text: "y".repeat(size.saturating_sub(200)),
                    usage: octane_protocol::Usage::default(),
                })
            }
        }

        let summarizer = Arc::new(BarelyShrinks(AtomicUsize::new(0)));
        let history = crowded_history(400, 400);
        let used = octane_context::prune::estimate_tokens(&history);

        let mut runner = TurnRunner::new(
            Agent::build(),
            permissive_policy(),
            registry(),
            Arc::new(AlwaysApprove),
            Budget::new(used + 10, used.saturating_sub(10), 1),
        );
        runner.summarizer = Some(summarizer.clone());

        let source = ScriptedSource::new(vec![final_answer("done")]);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            runner.run(&source, history, "/p".into(), "/p".into()),
        )
        .await
        .expect("the turn must terminate");

        assert!(
            summarizer.0.load(Ordering::SeqCst) <= 3,
            "at most a small, fixed number of paid summarizations, got {}",
            summarizer.0.load(Ordering::SeqCst)
        );
        let _ = outcome;
    }
}
