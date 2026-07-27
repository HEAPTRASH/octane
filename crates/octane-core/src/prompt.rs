//! Prompt assembly, ordered for cache stability.
//!
//! The ordering here is the whole point of the module. Content is layered
//! **most-static first**, so that every turn shares the longest possible prefix
//! with the previous one and the provider's prefix cache covers it:
//!
//! ```text
//! [ system instructions ]  never changes          ┐
//! [ tool schemas        ]  stable, sorted         │ cached prefix
//! [ sandbox / env       ]  rarely changes         │
//! [ skill manifest      ]  per session            │
//! [ project memory      ]  per session            ┘
//! [ conversation        ]  append-only
//! [ new user input      ]  the only new bytes
//! ```
//!
//! Two rules follow, and violating either silently multiplies cost:
//!
//! 1. **Never edit or reorder what was already sent.** Configuration changed
//!    mid-session? Append a new developer message. Codex appends rather than
//!    rewrites for exactly this reason.
//! 2. **Tool schemas must be order-stable.** They sit in the prefix, so a registry
//!    yielding them in hash order invalidates the cache every turn. `ToolRegistry`
//!    uses a `BTreeMap` to make that impossible.

use octane_protocol::{Message, Part, Role};
use octane_provider::model::ToolSchema;

/// The primary agent's identity and operating rules.
///
/// Kept short on purpose. Every token here sits in the cached prefix of every
/// turn for the whole session, and a long prompt is not a more obedient one —
/// the instructions that survive contact are the ones a model can hold while
/// also holding the task. Anything situational (what the sandbox permits, what
/// is in the project's memory files) is a separate layer below, so it can
/// change without invalidating this one.
pub const BASE_INSTRUCTIONS: &str = "\
You are octane, an AI coding agent working in a terminal.

Work directly on the user's codebase using the tools you are given. Read before \
you write: inspect the file you are about to change, and prefer a narrow edit \
over a rewrite. Match the conventions of the surrounding code rather than \
importing your own.

Tool calls are how you act; prose is how you report. Do not narrate what you \
are about to do before doing it, and do not describe a change you have not \
made. When you have finished, say what you did in a sentence or two — the user \
is reading a terminal, not a report.

Work the task to completion in one turn. Reading a file is not finishing; \
neither is describing a plan. Keep calling tools until the change is actually \
made, then report. Do not stop to ask whether to continue, and do not end a \
turn expecting the user to tell you to carry on: they asked once. If part \
of the task is genuinely blocked, do the rest and say which part is \
blocked and why.

You may be refused. A denial is the user's decision, not an obstacle to route \
around: do not retry the same action by another means. If a command is blocked \
or a path is unwritable, say so plainly and stop.

Say what is true about what you did. If a test failed, quote it. If you skipped \
part of the task, name the part. Never claim a verification you did not run.";

/// Assembles the prompt for one inference step.
#[derive(Debug, Default)]
pub struct PromptAssembler {
    base_instructions: String,
    environment: Option<String>,
    sandbox_description: Option<String>,
    skill_manifest: Option<String>,
    memory: Option<String>,
    /// Fenced, attributed guidance from connected MCP servers.
    mcp_instructions: Option<String>,
}

impl PromptAssembler {
    pub fn new(base_instructions: impl Into<String>) -> Self {
        Self { base_instructions: base_instructions.into(), ..Default::default() }
    }

    /// cwd, platform, date. A snapshot — stated as such in the rendered text,
    /// because a model told the date is live will reason about staleness wrongly.
    pub fn environment(mut self, description: impl Into<String>) -> Self {
        self.environment = Some(description.into());
        self
    }

    /// What the sandbox permits.
    ///
    /// Worth including: a model that does not know it is confined interprets a
    /// denial as a broken command and tries to "fix" working code.
    pub fn sandbox(mut self, description: impl Into<String>) -> Self {
        self.sandbox_description = Some(description.into());
        self
    }

    /// Tier-1 skill list — names and descriptions only.
    pub fn skills(mut self, manifest: impl Into<String>) -> Self {
        self.skill_manifest = Some(manifest.into());
        self
    }

    pub fn memory(mut self, rendered: impl Into<String>) -> Self {
        self.memory = Some(rendered.into());
        self
    }

    /// Guidance supplied by connected MCP servers.
    ///
    /// Third-party text, already fenced and attributed by
    /// `McpClient::rendered_instructions`. It rides as a developer message like
    /// the rest of the static context, *after* project memory: a server
    /// describes how to use its own tools and must not read as though it
    /// outranked the project's own instructions.
    pub fn mcp_instructions(mut self, rendered: impl Into<String>) -> Self {
        let rendered = rendered.into();
        if !rendered.trim().is_empty() {
            self.mcp_instructions = Some(rendered);
        }
        self
    }

    /// How many messages `assemble` puts in front of the history.
    ///
    /// The runner needs this to know which leading messages are the cached
    /// prefix and must survive compaction. Derived from the same options
    /// `assemble` reads, so the two cannot disagree.
    pub fn preamble_len(&self) -> usize {
        1 + [
            self.sandbox_description.is_some(),
            self.skill_manifest.is_some(),
            self.memory.is_some(),
            self.mcp_instructions.is_some(),
            self.environment.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
    }

    /// Build the leading messages, in cache order.
    ///
    /// Returns messages only; tool schemas are passed separately in the request
    /// but must be sorted — see [`sorted_tools`].
    pub fn assemble(&self, history: &[Message], new_input: Option<Message>) -> Vec<Message> {
        let mut messages = Vec::with_capacity(history.len() + 4);

        // 1. System instructions. First so nothing later can outrank them, and so
        //    the cache prefix starts at a fixed point.
        messages.push(Message::text(Role::System, &self.base_instructions));

        // 2-4. Developer context, most-static first.
        if let Some(sandbox) = &self.sandbox_description {
            messages.push(Message::text(Role::Developer, sandbox));
        }
        if let Some(skills) = &self.skill_manifest {
            messages.push(Message::text(Role::Developer, skills));
        }
        if let Some(memory) = &self.memory {
            messages.push(Message::text(Role::Developer, memory));
        }
        if let Some(instructions) = &self.mcp_instructions {
            messages.push(Message::text(Role::Developer, instructions));
        }
        if let Some(environment) = &self.environment {
            messages.push(Message::text(Role::Developer, environment));
        }

        // 5. History, verbatim and in order.
        messages.extend_from_slice(history);

        // 6. The new input.
        if let Some(input) = new_input {
            messages.push(input);
        }

        messages
    }

    /// Append a mid-session change without disturbing the prefix.
    ///
    /// The alternative — rewriting the earlier developer message — is what
    /// silently costs a full cache miss on every subsequent turn.
    pub fn append_change_notice(history: &mut Vec<Message>, notice: impl Into<String>) {
        history.push(Message::new(
            Role::Developer,
            vec![Part::Synthetic { text: notice.into() }],
        ));
    }
}

/// Sort tool schemas by name.
///
/// Belongs here rather than in the provider layer so that the invariant is
/// visible at the point where the prompt is built, and applied in exactly one
/// place, because being wrong about this is expensive and silent.
pub fn sorted_tools(mut tools: Vec<ToolSchema>) -> Vec<ToolSchema> {
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

/// A mode switch, injected so the model knows its constraints changed.
///
/// Without it, a model that has been in `plan` mode keeps behaving read-only after
/// the user switches to `build`, because nothing in its context says otherwise.
/// opencode injects an equivalent synthetic reminder.
pub fn mode_switch_notice(from: &str, to: &str) -> String {
    format!(
        "<system-reminder>\n\
         Your operational mode changed from {from} to {to}.\n\
         The tools and permissions available to you have changed accordingly.\n\
         </system-reminder>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Server guidance must land in the cached prefix and behind project memory.
    ///
    /// In front of it, a third-party server would read as outranking the
    /// project's own instructions; outside the preamble, it would sit in the
    /// history where compaction can drop it.
    #[test]
    fn mcp_instructions_ride_in_the_preamble_after_memory() {
        let assembler = PromptAssembler::new("base")
            .memory("project rules")
            .mcp_instructions("<mcp-server-instructions server=\"x\" trust=\"third-party\">hi</mcp-server-instructions>");

        let messages = assembler.assemble(&[], None);
        assert_eq!(messages.len(), assembler.preamble_len(), "preamble_len must match");

        let memory =
            messages.iter().position(|m| m.text_content().contains("project rules")).unwrap();
        let mcp = messages
            .iter()
            .position(|m| m.text_content().contains("mcp-server-instructions"))
            .unwrap();
        assert!(mcp > memory, "server guidance must not precede project memory");
        assert_eq!(messages[mcp].role, Role::Developer, "it is context, never a system rule");
    }

    /// No servers, no message — and critically, no change to `preamble_len`, or
    /// every existing session's cached prefix would shift by one.
    #[test]
    fn no_servers_adds_nothing_to_the_prefix() {
        let bare = PromptAssembler::new("base");
        let empty = PromptAssembler::new("base").mcp_instructions("   ");
        assert_eq!(bare.preamble_len(), empty.preamble_len());
        assert_eq!(bare.assemble(&[], None).len(), empty.assemble(&[], None).len());
    }

    fn assembler() -> PromptAssembler {
        PromptAssembler::new("You are octane.")
            .sandbox("Writes are limited to the workspace.")
            .skills("- pdf: handle PDFs")
            .memory("<instructions>Use tabs.</instructions>")
            .environment("cwd: /proj")
    }

    #[test]
    fn system_instructions_come_first() {
        let messages = assembler().assemble(&[], None);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].text_content(), "You are octane.");
    }

    #[test]
    fn static_layers_precede_dynamic_ones() {
        let messages = assembler().assemble(&[], None);
        let text: Vec<String> = messages.iter().map(Message::text_content).collect();

        let sandbox = text.iter().position(|t| t.contains("Writes are limited")).unwrap();
        let environment = text.iter().position(|t| t.contains("cwd:")).unwrap();
        // Environment changes more often than the sandbox description, so it must
        // sit later in the prefix.
        assert!(sandbox < environment);
    }

    #[test]
    fn prefix_is_identical_across_turns() {
        let assembler = assembler();

        let turn_one = assembler.assemble(&[], Some(Message::text(Role::User, "first")));

        // `history` is conversation-only. `assemble` output is *not* history: it
        // includes the preamble, and feeding it back would duplicate it.
        let conversation = vec![
            Message::text(Role::User, "first"),
            Message::text(Role::Assistant, "reply"),
        ];
        let turn_two =
            assembler.assemble(&conversation, Some(Message::text(Role::User, "second")));

        // Everything from turn one must reappear byte-identically at the front of
        // turn two, or the prefix cache misses and cost goes quadratic.
        assert_eq!(&turn_two[..turn_one.len()], &turn_one[..]);
    }

    #[test]
    fn history_order_is_preserved() {
        let history = vec![
            Message::text(Role::User, "a"),
            Message::text(Role::Assistant, "b"),
            Message::text(Role::User, "c"),
        ];
        let messages = assembler().assemble(&history, None);
        let tail = &messages[messages.len() - 3..];
        assert_eq!(tail, &history[..]);
    }

    #[test]
    fn change_notices_append_rather_than_edit() {
        let mut history = vec![Message::text(Role::User, "original")];
        let before = history[0].clone();

        PromptAssembler::append_change_notice(&mut history, "cwd changed");

        // The existing message is untouched; the notice is at the end.
        assert_eq!(history[0], before);
        assert_eq!(history.len(), 2);
        assert!(matches!(history[1].parts[0], Part::Synthetic { .. }));
    }

    #[test]
    fn tools_sort_by_name() {
        let schema = |name: &str| ToolSchema {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
        };
        let sorted = sorted_tools(vec![schema("write"), schema("bash"), schema("read")]);
        let names: Vec<&str> = sorted.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["bash", "read", "write"]);
    }

    #[test]
    fn mode_switch_notice_names_both_modes() {
        let notice = mode_switch_notice("plan", "build");
        assert!(notice.contains("plan"));
        assert!(notice.contains("build"));
        assert!(notice.contains("system-reminder"));
    }
}
