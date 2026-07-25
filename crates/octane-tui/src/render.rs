//! Turning protocol events into scrollback lines.
//!
//! Pure: [`Event`] in, styled [`Line`]s out. Nothing here touches a terminal, so
//! the rendering rules — which are where the bugs live — are testable by calling
//! a function.
//!
//! Two principles taken from the survey (`RESEARCH.md` §I):
//!
//! **Collapsed but available.** Tool calls render as one line by default. The
//! detail exists and can be expanded, but a transcript where every `read` dumps
//! 200 lines is one nobody can follow.
//!
//! **What the user sees, the model sees.** The collapsed line summarizes the same
//! result the model received. It never shows something the model did not get, and
//! never hides an error.

use octane_protocol::{Event, ItemEvent, ItemKind, ItemStatus};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// How much tool detail to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Detail {
    /// One line per tool call.
    #[default]
    Collapsed,
    /// Full tool output.
    Expanded,
}

/// Whether to show model reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reasoning {
    #[default]
    Hidden,
    Shown,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    pub detail: Detail,
    pub reasoning: Reasoning,
    pub theme: Theme,
}

/// Render one event as scrollback lines.
///
/// Returns empty for events with nothing to show — deltas, for instance, which
/// are handled by the live region rather than appended to scrollback. An event
/// producing no lines is normal, not an error.
pub fn render_event(event: &Event, options: &RenderOptions) -> Vec<Line<'static>> {
    match event {
        Event::Item(item_event) => render_item(item_event, options),

        Event::Compaction { before_tokens, after_tokens, strategy } => {
            // Surfaced deliberately. Silent context loss is the most confusing
            // thing that can happen to a user mid-session — the agent appears to
            // forget, with no explanation anywhere.
            vec![notice(
                format!(
                    "context compacted ({strategy}): {} → {} tokens",
                    thousands(*before_tokens),
                    thousands(*after_tokens)
                ),
                options.theme.warning,
            )]
        }

        // Turn lifecycle and usage drive the status line, not the transcript.
        Event::Turn(_) | Event::Usage(_) | Event::Thread { .. } => Vec::new(),
    }
}

fn render_item(event: &ItemEvent, options: &RenderOptions) -> Vec<Line<'static>> {
    // Only completed items reach scrollback. Anything in flight belongs in the
    // live region, where it can still change.
    let ItemEvent::Completed { item, .. } = event else {
        return Vec::new();
    };
    let theme = &options.theme;

    match &item.kind {
        ItemKind::UserMessage { text } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("› ", theme.label(theme.user)),
                Span::styled(first_line(text), Style::default().fg(theme.user)),
            ])];
            // Continuation lines are indented to match the marker width, so a
            // pasted block stays visually attached to its prompt.
            for line in text.lines().skip(1) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line.to_string(), Style::default().fg(theme.user)),
                ]));
            }
            lines.push(Line::default());
            lines
        }

        ItemKind::AgentMessage { text } => {
            let mut lines: Vec<Line<'static>> =
                text.lines().map(|line| Line::raw(line.to_string())).collect();
            lines.push(Line::default());
            lines
        }

        ItemKind::Reasoning { text } => {
            if options.reasoning == Reasoning::Hidden {
                return Vec::new();
            }
            let mut lines: Vec<Line<'static>> = text
                .lines()
                .map(|line| Line::styled(format!("  {line}"), theme.reasoning()))
                .collect();
            lines.push(Line::default());
            lines
        }

        ItemKind::ToolExecution { name, input, .. } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("● ", Style::default().fg(status_color(item.status, theme))),
                Span::styled(name.clone(), theme.label(theme.tool)),
                Span::styled(format!("  {}", summarize_input(name, input)), theme.dim()),
            ])];

            if options.detail == Detail::Expanded {
                for line in input.lines().take(20) {
                    lines.push(Line::styled(format!("    {line}"), theme.dim()));
                }
            }
            lines
        }

        ItemKind::Diff { path, unified } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("✎ ", theme.label(theme.tool)),
                Span::raw(path.clone()),
            ])];
            lines.extend(render_diff(unified, theme));
            lines.push(Line::default());
            lines
        }

        ItemKind::Approval(request) => vec![Line::from(vec![
            Span::styled("? ", theme.label(theme.warning)),
            Span::raw(request.summary.clone()),
        ])],

        ItemKind::Error { message } => {
            let mut lines = vec![Line::from(vec![
                Span::styled("✗ ", theme.label(theme.error)),
                Span::styled(message.clone(), Style::default().fg(theme.error)),
            ])];
            lines.push(Line::default());
            lines
        }
    }
}

/// Colour a unified diff.
pub fn render_diff(unified: &str, theme: &Theme) -> Vec<Line<'static>> {
    unified
        .lines()
        .map(|line| {
            // `+++`/`---` are file headers, not content; colouring them as
            // additions and removals makes every diff look like it rewrote a file.
            let style = if line.starts_with("+++") || line.starts_with("---") {
                theme.dim()
            } else if line.starts_with('+') {
                Style::default().fg(theme.added)
            } else if line.starts_with('-') {
                Style::default().fg(theme.removed)
            } else if line.starts_with("@@") {
                theme.dim()
            } else {
                Style::default()
            };
            Line::styled(format!("  {line}"), style)
        })
        .collect()
}

/// One-line gist of a tool call, for the collapsed view.
///
/// Reaches into the arguments per tool because a generic JSON dump is unreadable
/// at a glance, and the point of the collapsed line is to be readable at a glance.
pub fn summarize_input(tool: &str, input: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return truncate(input, 60);
    };
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).unwrap_or_default();

    let summary = match tool {
        "read" | "write" | "edit" | "list" => field("path").to_string(),
        // The description exists precisely so there is something human to show.
        "bash" => {
            let described = field("description");
            if described.is_empty() { field("command").to_string() } else { described.to_string() }
        }
        "glob" => field("pattern").to_string(),
        "grep" => {
            let pattern = field("pattern");
            match value.get("glob").and_then(|v| v.as_str()) {
                Some(glob) => format!("{pattern}  in {glob}"),
                None => pattern.to_string(),
            }
        }
        _ => input.to_string(),
    };

    truncate(&summary, 60)
}

fn status_color(status: ItemStatus, theme: &Theme) -> ratatui::style::Color {
    match status {
        ItemStatus::Completed => theme.success,
        ItemStatus::Failed => theme.error,
        ItemStatus::Canceled => theme.dim,
        ItemStatus::Started | ItemStatus::Streaming => theme.tool,
    }
}

fn notice(text: String, color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled("· ", Style::default().fg(color)),
        Span::styled(text, Style::default().fg(color)),
    ])
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

fn truncate(text: &str, limit: usize) -> String {
    let cleaned = text.replace('\n', " ");
    if cleaned.chars().count() <= limit {
        return cleaned;
    }
    let cut: String = cleaned.chars().take(limit.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_protocol::{ApprovalRequest, Item, ItemId, TurnId};

    fn completed(kind: ItemKind) -> Event {
        Event::Item(ItemEvent::Completed {
            turn_id: TurnId::new(),
            item: Item { id: ItemId::new(), kind, status: ItemStatus::Completed },
        })
    }

    fn text_of(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(event: &Event) -> Vec<Line<'static>> {
        render_event(event, &RenderOptions::default())
    }

    #[test]
    fn a_user_message_is_marked_and_indented() {
        let event = completed(ItemKind::UserMessage { text: "line one\nline two".into() });
        let rendered = text_of(&render(&event));
        assert!(rendered.contains("› line one"));
        assert!(rendered.contains("  line two"));
    }

    #[test]
    fn tool_calls_collapse_to_one_line() {
        let event = completed(ItemKind::ToolExecution {
            call_id: octane_protocol::ToolCallId::new(),
            name: "read".into(),
            input: r#"{"path":"src/main.rs"}"#.into(),
        });

        let lines = render(&event);
        assert_eq!(lines.len(), 1, "collapsed means one line");
        assert!(text_of(&lines).contains("read"));
        assert!(text_of(&lines).contains("src/main.rs"));
    }

    #[test]
    fn expanding_shows_the_arguments() {
        let event = completed(ItemKind::ToolExecution {
            call_id: octane_protocol::ToolCallId::new(),
            name: "read".into(),
            input: "{\n  \"path\": \"src/main.rs\"\n}".into(),
        });

        let options = RenderOptions { detail: Detail::Expanded, ..Default::default() };
        assert!(render_event(&event, &options).len() > 1);
    }

    #[test]
    fn bash_is_summarized_by_its_description_not_its_command() {
        // The description field exists so there is something human to show.
        let summary = summarize_input(
            "bash",
            r#"{"command":"cargo test --lib --all-features -- --nocapture","description":"Runs the test suite"}"#,
        );
        assert_eq!(summary, "Runs the test suite");
    }

    #[test]
    fn bash_falls_back_to_the_command_when_undescribed() {
        assert_eq!(summarize_input("bash", r#"{"command":"ls -la"}"#), "ls -la");
    }

    #[test]
    fn grep_summaries_mention_the_file_filter() {
        let summary = summarize_input("grep", r#"{"pattern":"TODO","glob":"**/*.rs"}"#);
        assert!(summary.contains("TODO"));
        assert!(summary.contains("**/*.rs"));
    }

    #[test]
    fn unparseable_input_still_summarizes() {
        assert_eq!(summarize_input("read", "not json at all"), "not json at all");
    }

    #[test]
    fn long_summaries_are_truncated_to_one_line() {
        let summary = summarize_input("read", &format!(r#"{{"path":"{}"}}"#, "x".repeat(200)));
        assert!(summary.chars().count() <= 60);
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn reasoning_is_hidden_by_default_and_showable() {
        let event = completed(ItemKind::Reasoning { text: "let me think".into() });
        assert!(render(&event).is_empty());

        let options = RenderOptions { reasoning: Reasoning::Shown, ..Default::default() };
        assert!(text_of(&render_event(&event, &options)).contains("let me think"));
    }

    #[test]
    fn errors_are_always_shown() {
        let event = completed(ItemKind::Error { message: "permission denied".into() });
        assert!(text_of(&render(&event)).contains("permission denied"));
    }

    #[test]
    fn compaction_is_announced_rather_than_silent() {
        let event = Event::Compaction {
            before_tokens: 167_000,
            after_tokens: 24_000,
            strategy: "summarize".into(),
        };
        let rendered = text_of(&render(&event));
        assert!(rendered.contains("compacted"));
        assert!(rendered.contains("167,000"));
    }

    #[test]
    fn in_flight_items_do_not_reach_scrollback() {
        // Only completed items are appended; anything still changing belongs in
        // the live region.
        let event = Event::Item(ItemEvent::Started {
            turn_id: TurnId::new(),
            item: Item {
                id: ItemId::new(),
                kind: ItemKind::AgentMessage { text: "partial".into() },
                status: ItemStatus::Streaming,
            },
        });
        assert!(render(&event).is_empty());

        let delta = Event::Item(ItemEvent::Delta {
            turn_id: TurnId::new(),
            item_id: ItemId::new(),
            text: "more".into(),
        });
        assert!(render(&delta).is_empty());
    }

    #[test]
    fn diff_headers_are_not_coloured_as_content() {
        let theme = Theme::default();
        let lines = render_diff("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n", &theme);

        // `Line::styled` carries the style on the line, not on its spans.
        let style_of = |index: usize| lines[index].style.fg;
        assert_eq!(style_of(0), Some(theme.dim), "--- is a header, not a removal");
        assert_eq!(style_of(1), Some(theme.dim), "+++ is a header, not an addition");
        assert_eq!(style_of(3), Some(theme.removed));
        assert_eq!(style_of(4), Some(theme.added));
    }

    #[test]
    fn approvals_are_rendered_with_their_summary() {
        let event = completed(ItemKind::Approval(ApprovalRequest {
            resource: "command(rm -rf build)".into(),
            summary: "Removes the build directory".into(),
            diff: None,
        }));
        assert!(text_of(&render(&event)).contains("Removes the build directory"));
    }

    #[test]
    fn thousands_separates_correctly() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(167_000), "167,000");
    }
}
