//! The status line.
//!
//! Answers the three questions a user has constantly, none of which should
//! require a command (`RESEARCH.md` §J):
//!
//! - **What happens if I hit enter?** — the mode. `plan` and `bypass` behave
//!   completely differently, and getting this wrong is how people are surprised.
//! - **How much room is left?** — context use, before compaction surprises them.
//! - **What is this costing?** — cumulative spend.
//!
//! Plus the model, since switching mid-session is a feature.

use octane_permission::PermissionMode;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::theme::Theme;

#[derive(Debug, Clone)]
pub struct StatusLine {
    pub mode: PermissionMode,
    pub model: String,
    /// Fraction of the usable context window in use, 0.0-1.0.
    pub context_used: f64,
    pub cost_usd: f64,
    /// Shown while the agent is working.
    pub activity: Option<Activity>,
    /// Marker glyphs, so the ASCII fallback reaches the status line too.
    pub glyphs: crate::glyphs::Glyphs,
    /// The session has a real exchange behind it, so the key hints have done
    /// their teaching and stop being drawn.
    pub conversing: bool,
}

#[derive(Debug, Clone)]
pub struct Activity {
    /// What is happening right now, e.g. "Editing src/main.rs".
    pub label: String,
    pub elapsed_secs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            mode: PermissionMode::default(),
            model: "unset".into(),
            context_used: 0.0,
            cost_usd: 0.0,
            activity: None,
            glyphs: crate::glyphs::UNICODE,
            conversing: false,
        }
    }
}

impl StatusLine {
    /// Render as `(text, is_warning)` segments.
    ///
    /// Returns data rather than styled spans so the layout is testable without
    /// constructing a terminal.
    pub fn segments(&self) -> Vec<Segment> {
        let mut segments = vec![
            Segment::mode(self.mode),
            Segment::plain(self.model.clone()),
            self.context_segment(),
        ];

        // Cost is omitted at zero rather than shown as $0.00: a fresh session
        // should not advertise a number that is not yet meaningful.
        if self.cost_usd > 0.0 {
            segments.push(Segment::plain(format!("${:.2}", self.cost_usd)));
        }
        segments
    }

    fn context_segment(&self) -> Segment {
        // Headroom, not consumption. The row is read to answer "how much room
        // is left before this compacts?", and `ctx 12%` makes the reader do the
        // subtraction every time. Same number, the useful way round.
        let left = 100 - (self.context_used * 100.0).round().min(100.0) as u32;
        let text = format!("{left}% left");

        // Warned at 75% so there is room to finish a thought before compaction,
        // rather than at the threshold where it is already happening.
        if self.context_used >= 0.75 {
            Segment { text, emphasis: Emphasis::Warning }
        } else {
            Segment::plain(text)
        }
    }

    /// The transient activity line shown above the composer while working.
    pub fn activity_line(&self, spinner_frame: usize) -> Option<String> {
        let activity = self.activity.as_ref()?;
        let frames = self.glyphs.spinner;
        let frame = frames[spinner_frame % frames.len()];

        let mut line = format!("{frame} {} · {}S", activity.label, activity.elapsed_secs);
        // Token counters only once there is something to count, so an idle
        // moment does not read as "0 tokens used".
        if activity.input_tokens > 0 || activity.output_tokens > 0 {
            line.push_str(&format!(
                " {} {}{} {}{}",
                self.glyphs.separator,
                self.glyphs.arrow_up,
                compact(activity.input_tokens),
                self.glyphs.arrow_down,
                compact(activity.output_tokens)
            ));
        }
        Some(line)
    }

    /// Key hints for the right of the status line.
    ///
    /// Returns an owned string so it can consult `glyphs`. It used to be
    /// `&'static str` holding a hardcoded `\u{2b7e}`, which meant the ASCII
    /// fallback could not reach it, and which was above the U+2900 ceiling the
    /// glyph tests enforce. The modifier is spelled out instead: no symbol for
    /// shift+tab is both widely rendered and unambiguously narrow.
    ///
    /// Worth a permanent row of screen: fewer keys than it looks.
    ///
    /// The full set teaches, then gets out of the way — the same rule the empty
    /// state follows. Held forever they are noise: a user who has sent three
    /// messages knows how to leave, and the row spends its width saying so on
    /// every frame for the rest of the session. `/help` has the whole list.
    ///
    /// Interrupting is the exception, because it is the one key someone needs
    /// at the moment they are least able to go looking for it.
    pub fn hints(&self) -> String {
        if self.activity.is_some() {
            return "ESC INTERRUPT".to_string();
        }
        if self.conversing {
            return String::new();
        }
        format!("SHIFT+TAB MODE {} CTRL+C EXIT", self.glyphs.separator)
    }
}


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub emphasis: Emphasis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Normal,
    Warning,
    /// A mode that changes safety behaviour, and should be visible at a glance.
    Alert,
}

impl Segment {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), emphasis: Emphasis::Normal }
    }

    fn mode(mode: PermissionMode) -> Self {
        Self {
            text: mode.label().to_string(),
            // `bypass` skips every prompt. If the user forgets they are in it, the
            // first they learn is after something has already happened.
            emphasis: match mode {
                PermissionMode::Bypass => Emphasis::Alert,
                PermissionMode::Plan => Emphasis::Warning,
                _ => Emphasis::Normal,
            },
        }
    }

    pub fn color(&self, theme: &Theme) -> ratatui::style::Color {
        match self.emphasis {
            Emphasis::Normal => theme.dim,
            Emphasis::Warning => theme.warning,
            Emphasis::Alert => theme.error,
        }
    }
}

/// Human-scale token counts: `1.2k`, `45.0k`.
fn compact(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    format!("{:.1}k", count as f64 / 1_000.0)
}

/// The status line as a pane: one row, pinned to the bottom.
#[derive(Debug, Clone, Copy)]
pub struct StatusPane<'a> {
    pub status: &'a StatusLine,
    pub options: &'a crate::render::RenderOptions,
}

impl crate::component::Pane for StatusPane<'_> {
    fn constraint(&self, _width: u16) -> Constraint {
        Constraint::Length(1)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let (theme, glyphs) = (&self.options.theme, &self.options.glyphs);

        let segments = self.status.segments();
        let alert = segments
            .iter()
            .any(|segment| segment.emphasis == Emphasis::Alert);
        let signal = if alert {
            theme.error
        } else if segments
            .iter()
            .any(|segment| segment.emphasis == Emphasis::Warning)
        {
            theme.warning
        } else {
            theme.accent
        };
        // In alert the mode's glyph *becomes* the error marker rather than
        // being joined by it: two glyphs in front of one segment reads as a
        // rendering fault, and bypass still owns the loudest glyph on the row
        // as well as the whole-row error background.
        let mode_glyph = if alert { glyphs.error } else { glyphs.radio_on };

        let mode = segments
            .first()
            .map(|segment| segment.text.to_uppercase())
            .unwrap_or_default();
        let model = segments
            .get(1)
            .map(|segment| segment.text.as_str())
            .unwrap_or("unset");
        let context = segments
            .get(2)
            .map(|segment| segment.text.trim_start_matches("ctx ").to_uppercase())
            .unwrap_or_else(|| "0%".into());
        // Each segment's glyph doubles as the separator before it, instead of
        // being added on top of a `·`. A distinct leading glyph divides the row
        // at least as well as an identical dot did, and doing both would cost
        // two more columns per segment — the row already truncates its key
        // hints at 80 with a long model name.
        //
        // The choices are the closest fits the width-safe set actually has;
        // `question`/`?` before a model name would read as "unknown model" and
        // `claw`/`/` disappears into `openrouter/flash`.
        let mut fields = format!(
            " {mode_glyph} MODE/{mode}  {} MODEL/{model}  {} CTX/{context}",
            glyphs.prompt, // a chevron: the model is what answers
            glyphs.bar,    // a gauge mark, for how full the window is
        );
        if let Some(cost) = segments.get(3) {
            // Spend only ever goes up.
            fields.push_str(&format!("  {} COST/{}", glyphs.arrow_up, cost.text));
        }

        let hints = format!("{} ", self.status.hints());
        let width = usize::from(area.width);
        let used = fields.chars().count() + hints.chars().count();
        let text = if used <= width {
            format!("{fields}{}{hints}", " ".repeat(width - used))
        } else {
            fields.chars().take(width).collect()
        };

        let style = theme.signal(signal);
        buf.set_style(area, style);
        Paragraph::new(text).style(style).render(area, buf);
    }
}

/// The spinner row, between the transcript and the composer.
///
/// Collapses to nothing when no work is in flight, rather than holding an empty
/// row: a permanently reserved line reads as a rendering fault.
#[derive(Debug, Clone, Copy)]
pub struct ActivityPane<'a> {
    pub status: &'a StatusLine,
    pub spinner_frame: usize,
    pub options: &'a crate::render::RenderOptions,
}

impl crate::component::Pane for ActivityPane<'_> {
    fn constraint(&self, _width: u16) -> Constraint {
        Constraint::Length(u16::from(self.status.activity.is_some()))
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some(line) = self.status.activity_line(self.spinner_frame) {
            let theme = &self.options.theme;
            buf.set_style(area, theme.panel());
            Paragraph::new(Line::from(vec![
                Span::styled(" ACTIVITY / ", theme.accent()),
                Span::styled(line, theme.dim()),
            ]))
            .style(theme.panel())
            .render(area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Pane;

    fn status() -> StatusLine {
        StatusLine { model: "sonnet-5".into(), ..Default::default() }
    }

    #[test]
    fn shows_mode_model_and_context() {
        let texts: Vec<String> = status().segments().into_iter().map(|s| s.text).collect();
        assert_eq!(texts, vec!["default", "sonnet-5", "100% left"]);
    }

    #[test]
    fn cost_appears_only_once_there_is_one() {
        assert!(!status().segments().iter().any(|s| s.text.starts_with('$')));

        let spending = StatusLine { cost_usd: 0.0412, ..status() };
        assert!(spending.segments().iter().any(|s| s.text == "$0.04"));
    }

    #[test]
    fn context_pressure_is_warned_before_it_bites() {
        let comfortable = StatusLine { context_used: 0.5, ..status() };
        assert_eq!(comfortable.context_segment().emphasis, Emphasis::Normal);

        let tight = StatusLine { context_used: 0.8, ..status() };
        assert_eq!(tight.context_segment().emphasis, Emphasis::Warning);
        assert_eq!(tight.context_segment().text, "20% left");
    }

    #[test]
    fn bypass_mode_is_visually_alarming() {
        let bypass = StatusLine { mode: PermissionMode::Bypass, ..status() };
        assert_eq!(bypass.segments()[0].emphasis, Emphasis::Alert);

        let plan = StatusLine { mode: PermissionMode::Plan, ..status() };
        assert_eq!(plan.segments()[0].emphasis, Emphasis::Warning);
    }

    #[test]
    fn there_is_no_activity_line_when_idle() {
        assert!(status().activity_line(0).is_none());
    }

    #[test]
    fn the_activity_line_reports_what_and_how_long() {
        let working = StatusLine {
            activity: Some(Activity {
                label: "Editing src/main.rs".into(),
                elapsed_secs: 12,
                input_tokens: 1_240,
                output_tokens: 340,
            }),
            ..status()
        };

        let line = working.activity_line(0).unwrap();
        assert!(line.contains("Editing src/main.rs"));
        assert!(line.contains("12S"));
        assert!(line.contains("↑1.2k"));
        assert!(line.contains("↓340"));
    }

    #[test]
    fn token_counters_are_hidden_until_there_are_tokens() {
        let starting = StatusLine {
            activity: Some(Activity {
                label: "Thinking".into(),
                elapsed_secs: 1,
                input_tokens: 0,
                output_tokens: 0,
            }),
            ..status()
        };
        let line = starting.activity_line(0).unwrap();
        assert!(!line.contains('↑'), "0 tokens should not be advertised: {line}");
    }

    #[test]
    fn the_spinner_cycles_and_wraps() {
        let working = StatusLine {
            activity: Some(Activity {
                label: "x".into(),
                elapsed_secs: 0,
                input_tokens: 0,
                output_tokens: 0,
            }),
            ..status()
        };
        let first = working.activity_line(0).unwrap();
        let second = working.activity_line(1).unwrap();
        assert_ne!(first, second);
        // Must not panic past the end of the frame list.
        assert_eq!(working.activity_line(working.glyphs.spinner.len()).unwrap(), first);
    }

    #[test]
    fn hints_offer_the_escape_hatch_while_working() {
        assert!(status().hints().contains("MODE"));

        let working = StatusLine {
            activity: Some(Activity {
                label: "x".into(),
                elapsed_secs: 0,
                input_tokens: 0,
                output_tokens: 0,
            }),
            ..status()
        };
        assert!(working.hints().contains("INTERRUPT"));
    }

    #[test]
    fn token_counts_are_compacted() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_240), "1.2k");
        assert_eq!(compact(45_000), "45.0k");
    }

    #[test]
    fn the_ready_status_is_a_full_width_acid_signal() {
        let status = status();
        let options = crate::render::RenderOptions {
            theme: crate::theme::Theme::new(crate::theme::ColorDepth::TrueColor),
            ..Default::default()
        };
        let pane = StatusPane {
            status: &status,
            options: &options,
        };
        let area = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf);

        let row: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains("MODE/DEFAULT"));
        assert!(row.contains("MODEL/sonnet-5"));
        assert!(row.contains("CTX/100% LEFT"));
        assert!((0..area.width).all(|x| buf[(x, 0)].bg == options.theme.accent));
    }

    /// Render the status row as a string, at a chosen width and glyph set.
    fn row(status: &StatusLine, width: u16, glyphs: crate::glyphs::Glyphs) -> String {
        let options = crate::render::RenderOptions {
            theme: crate::theme::Theme::new(crate::theme::ColorDepth::TrueColor),
            glyphs,
            ..Default::default()
        };
        let pane = StatusPane { status, options: &options };
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf);
        (0..width).map(|x| buf[(x, 0)].symbol()).collect()
    }

    #[test]
    fn every_segment_is_findable_by_its_own_glyph() {
        let spending = StatusLine { cost_usd: 0.04, ..status() };
        for glyphs in [crate::glyphs::UNICODE, crate::glyphs::ASCII] {
            let row = row(&spending, 100, glyphs);
            for (glyph, field) in [
                (glyphs.radio_on, "MODE/"),
                (glyphs.prompt, "MODEL/"),
                (glyphs.bar, "CTX/"),
                (glyphs.arrow_up, "COST/"),
            ] {
                assert!(
                    row.contains(&format!("{glyph} {field}")),
                    "{field} should be marked with {glyph:?}: {row}"
                );
            }
        }
    }

    /// The glyphs are decoration; they must not push the row into a second one
    /// or cost the fields their room at the widths people actually use.
    #[test]
    fn the_row_stays_one_row_at_eighty_and_at_forty() {
        for width in [40u16, 80] {
            for glyphs in [crate::glyphs::UNICODE, crate::glyphs::ASCII] {
                let row = row(&status(), width, glyphs);
                assert_eq!(row.chars().count(), usize::from(width));
                // Truncation eats from the right, so the mode — the segment
                // that answers "what happens if I hit enter" — survives even
                // the narrow terminal.
                assert!(row.contains("MODE/DEFAULT"), "{width}: {row}");
            }
        }
    }

    #[test]
    fn bypass_replaces_the_mode_glyph_rather_than_stacking_on_it() {
        let bypass = StatusLine { mode: PermissionMode::Bypass, ..status() };
        let glyphs = crate::glyphs::UNICODE;
        let row = row(&bypass, 100, glyphs);

        assert!(row.contains(&format!("{} MODE/BYPASS", glyphs.error)), "{row}");
        assert!(!row.contains(glyphs.radio_on), "the calm marker has no place here: {row}");
    }

    #[test]
    fn bypass_turns_the_status_signal_safety_orange() {
        let status = StatusLine {
            mode: PermissionMode::Bypass,
            ..status()
        };
        let options = crate::render::RenderOptions {
            theme: crate::theme::Theme::new(crate::theme::ColorDepth::TrueColor),
            ..Default::default()
        };
        let pane = StatusPane {
            status: &status,
            options: &options,
        };
        let area = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(area);
        pane.render(area, &mut buf);

        assert!((0..area.width).all(|x| buf[(x, 0)].bg == options.theme.error));
    }
}
