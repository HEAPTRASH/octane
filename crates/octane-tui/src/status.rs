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
        let percent = (self.context_used * 100.0).round() as u32;
        let text = format!("ctx {percent}%");

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

        let mut line = format!("{frame} {} · {}s", activity.label, activity.elapsed_secs);
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

    /// Keybinding hints, right-aligned.
    ///
    /// Shows the escape hatch while working and the mode switch while idle —
    /// what the user most plausibly wants next in each state.
    /// Key hints for the right of the status line.
    ///
    /// Returns an owned string so it can consult `glyphs`. It used to be
    /// `&'static str` holding a hardcoded `\u{2b7e}`, which meant the ASCII
    /// fallback could not reach it, and which was above the U+2900 ceiling the
    /// glyph tests enforce. The modifier is spelled out instead: no symbol for
    /// shift+tab is both widely rendered and unambiguously narrow.
    pub fn hints(&self) -> String {
        if self.activity.is_some() {
            "esc interrupt".to_string()
        } else {
            format!("shift+tab mode {} ctrl+c exit", self.glyphs.separator)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> StatusLine {
        StatusLine { model: "sonnet-5".into(), ..Default::default() }
    }

    #[test]
    fn shows_mode_model_and_context() {
        let texts: Vec<String> = status().segments().into_iter().map(|s| s.text).collect();
        assert_eq!(texts, vec!["default", "sonnet-5", "ctx 0%"]);
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
        assert_eq!(tight.context_segment().text, "ctx 80%");
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
        assert!(line.contains("12s"));
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
        assert!(status().hints().contains("mode"));

        let working = StatusLine {
            activity: Some(Activity {
                label: "x".into(),
                elapsed_secs: 0,
                input_tokens: 0,
                output_tokens: 0,
            }),
            ..status()
        };
        assert!(working.hints().contains("interrupt"));
    }

    #[test]
    fn token_counts_are_compacted() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1_240), "1.2k");
        assert_eq!(compact(45_000), "45.0k");
    }
}
