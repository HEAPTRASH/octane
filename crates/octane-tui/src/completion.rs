//! Autocomplete for `/` commands and `@` file references.
//!
//! Pure: text plus cursor plus a candidate list in, ranked suggestions out. No
//! filesystem access happens here — the caller supplies the file index, which
//! keeps this testable and keeps the walker out of the UI crate.
//!
//! Matching is subsequence-based rather than prefix-based, so `@octcli` finds
//! `crates/octane-cli/src/main.rs`. Prefix matching would force the user to know
//! where a file lives before they can ask for it, which defeats the point.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// What the caret is currently sitting in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Trigger {
    /// `/` at the very start of the input.
    Command { prefix: String },
    /// `@` starting a word.
    File { prefix: String },
    #[default]
    None,
}

/// A ranked suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text inserted when accepted.
    pub value: String,
    /// Shown beside it in the popup.
    pub detail: String,
    score: i32,
}

impl Candidate {
    pub fn new(value: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { value: value.into(), detail: detail.into(), score: 0 }
    }
}

/// Live completion state.
#[derive(Debug, Default)]
pub struct Completion {
    trigger: Trigger,
    candidates: Vec<Candidate>,
    selected: usize,
    /// Byte range in the input that accepting a candidate replaces.
    replace: Option<(usize, usize)>,
    /// The trigger the user explicitly dismissed.
    ///
    /// Without this, `update` runs on the next keystroke, sees the same `@word`
    /// still under the caret, and reopens the popup — so Esc appears to do
    /// nothing. Cleared as soon as the query changes, because the user typing
    /// more is a request for suggestions again.
    dismissed: Option<Trigger>,
}

/// Suggestions shown at once. Beyond this the list stops being scannable and
/// starts being a directory listing.
pub const MAX_VISIBLE: usize = 8;

impl Completion {
    /// Recompute against the current input.
    ///
    /// `commands` and `files` are the full candidate universes; filtering and
    /// ranking happen here.
    pub fn update(&mut self, text: &str, cursor: usize, commands: &[Candidate], files: &[String]) {
        let previous = self.trigger.clone();
        let (trigger, replace) = detect(text, cursor);

        if self.dismissed.as_ref() == Some(&trigger) {
            self.candidates.clear();
            self.trigger = trigger;
            self.replace = replace;
            return;
        }
        self.dismissed = None;

        self.candidates = match &trigger {
            Trigger::Command { prefix } => rank_commands(commands, prefix),
            Trigger::File { prefix } => rank_files(files, prefix),
            Trigger::None => Vec::new(),
        };

        // Selection resets when the kind of completion changes, but survives
        // typing another character of the same one — otherwise the highlight
        // jumps back to the top on every keystroke and cannot be steered.
        if std::mem::discriminant(&previous) != std::mem::discriminant(&trigger) {
            self.selected = 0;
        }
        self.selected = self.selected.min(self.candidates.len().saturating_sub(1));

        self.trigger = trigger;
        self.replace = replace;
    }

    pub fn is_active(&self) -> bool {
        !self.candidates.is_empty()
    }

    /// Whether Enter should submit rather than accept.
    ///
    /// True when what was typed already *is* the only candidate. Accepting then
    /// does nothing visible but swallows the keystroke, so the user presses
    /// Enter, sees no submission, types the next thing, and it lands on the end
    /// of the command they thought they had sent.
    pub fn is_exhausted(&self) -> bool {
        match (&self.trigger, self.candidates.as_slice()) {
            (Trigger::Command { prefix }, [only]) => {
                only.value.trim_start_matches('/') == prefix
            }
            _ => false,
        }
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Candidates to draw, and where the highlight sits within them.
    ///
    /// Scrolls the window rather than the selection, so moving past the bottom
    /// keeps the highlighted row visible instead of running off the popup.
    pub fn visible(&self) -> (&[Candidate], usize) {
        if self.candidates.len() <= MAX_VISIBLE {
            return (&self.candidates, self.selected);
        }
        let start = self.selected.saturating_sub(MAX_VISIBLE - 1).min(self.candidates.len() - MAX_VISIBLE);
        (&self.candidates[start..start + MAX_VISIBLE], self.selected - start)
    }

    /// Move the highlight, wrapping at both ends.
    pub fn select_next(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
    }

    pub fn select_previous(&mut self) {
        if self.candidates.is_empty() {
            return;
        }
        self.selected =
            if self.selected == 0 { self.candidates.len() - 1 } else { self.selected - 1 };
    }

    /// Apply the highlighted candidate to `text`, returning the new text and
    /// caret position.
    ///
    /// A trailing space is appended so the user can keep typing immediately —
    /// after accepting a file reference the next thing is almost always more
    /// prose, not more path.
    pub fn accept(&self, text: &str) -> Option<(String, usize)> {
        let candidate = self.candidates.get(self.selected)?;
        let (start, end) = self.replace?;

        let mut out = String::with_capacity(text.len() + candidate.value.len());
        out.push_str(&text[..start]);
        out.push_str(&candidate.value);
        out.push(' ');

        let cursor = out.len();
        out.push_str(&text[end..]);
        Some((out, cursor))
    }

    /// Close the popup and keep it closed for this exact query.
    pub fn dismiss(&mut self) {
        self.dismissed = Some(self.trigger.clone());
        self.candidates.clear();
        self.selected = 0;
    }
}

/// Work out what the caret is completing, and which bytes accepting replaces.
fn detect(text: &str, cursor: usize) -> (Trigger, Option<(usize, usize)>) {
    let cursor = cursor.min(text.len());
    let before = &text[..cursor];

    // A command only counts at the very start, so `/` inside prose or a path
    // does not open a command popup.
    if let Some(rest) = before.strip_prefix('/') {
        if !rest.contains(char::is_whitespace) {
            return (Trigger::Command { prefix: rest.to_string() }, Some((0, cursor)));
        }
    }

    // `@` must start a word — same rule as the composer's reference extraction,
    // so an email address does not trigger a file popup.
    if let Some(at) = before.rfind('@') {
        let starts_word = at == 0 || before[..at].ends_with(char::is_whitespace);
        let word = &before[at + 1..];
        if starts_word && !word.contains(char::is_whitespace) {
            return (Trigger::File { prefix: word.to_string() }, Some((at, cursor)));
        }
    }

    (Trigger::None, None)
}

fn rank_commands(commands: &[Candidate], prefix: &str) -> Vec<Candidate> {
    let mut matched: Vec<Candidate> = commands
        .iter()
        .filter_map(|candidate| {
            // `/re` should reach `/review`, so match against the name without
            // its leading slash.
            let name = candidate.value.trim_start_matches('/');
            score(name, prefix).map(|score| Candidate { score, ..candidate.clone() })
        })
        .collect();

    matched.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.value.cmp(&b.value)));
    matched
}

fn rank_files(files: &[String], prefix: &str) -> Vec<Candidate> {
    let mut matched: Vec<Candidate> = files
        .iter()
        .filter_map(|path| {
            let score = score(path, prefix)?;
            let detail = path.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap_or_default();
            Some(Candidate { value: format!("@{path}"), detail, score })
        })
        .collect();

    matched.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.value.len().cmp(&b.value.len())));
    matched.truncate(50);
    matched
}

/// Subsequence match with positional bonuses. `None` means no match.
///
/// The bonuses are what make the ranking useful rather than merely correct:
/// without them `@main` surfaces every path containing those letters in order,
/// with the one actually called `main.rs` buried in the middle.
fn score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }

    let hay: Vec<char> = haystack.chars().collect();
    let hay_lower: Vec<char> = haystack.to_lowercase().chars().collect();
    let needle_lower: Vec<char> = needle.to_lowercase().chars().collect();

    let mut total = 0i32;
    let mut from = 0usize;
    let mut previous: Option<usize> = None;

    for want in &needle_lower {
        let found = hay_lower[from..].iter().position(|ch| ch == want)? + from;

        // Consecutive characters are the strongest signal that this is the
        // intended match rather than a coincidence.
        if previous == Some(found.saturating_sub(1)) {
            total += 8;
        }
        // Start of a path segment or a word.
        if found == 0 || matches!(hay.get(found - 1), Some('/' | '_' | '-' | '.' | ' ')) {
            total += 6;
        }

        previous = Some(found);
        from = found + 1;
    }

    // Shorter paths win ties: `src/main.rs` before `vendor/x/src/main.rs`.
    total -= (haystack.len() / 8) as i32;

    // A match inside the final segment beats one in a directory name, because
    // people type the filename they want far more often than its parent.
    if let Some((_, basename)) = haystack.rsplit_once('/') {
        if matches_subsequence(basename, &needle_lower) {
            total += 12;
        }
    }

    Some(total)
}

/// Whether `needle` (already lowercased) is a subsequence of `haystack`.
///
/// Separate from [`score`] so the basename bonus does not recurse into the
/// scoring rules and double-count them.
fn matches_subsequence(haystack: &str, needle: &[char]) -> bool {
    let mut chars = haystack.to_lowercase().chars().collect::<Vec<_>>().into_iter();
    needle.iter().all(|want| chars.any(|ch| ch == *want))
}

/// The popup, laid out for `width` columns.
///
/// Two columns, both sized here rather than by the caller. Names are padded past
/// the longest one so the details line up — ragged, they read as a list of
/// unrelated strings rather than a table — and each detail is elided to whatever
/// is left. Eliding here rather than at the source is what makes it correct:
/// only this function knows how wide the popup is, and a description cut to a
/// fixed guess is either still too long or needlessly short.
pub fn widget<'a>(
    completion: &Completion,
    width: u16,
    theme: &crate::theme::Theme,
    glyphs: &crate::glyphs::Glyphs,
) -> Paragraph<'a> {
    const MARKER: usize = 2;
    const GAP: usize = 2;

    let (visible, highlight) = completion.visible();

    // Padded past the longest name, exactly as the picker does. Capped, so one
    // pathological entry cannot push every description off the right edge.
    let inside = usize::from(width).saturating_sub(2);
    let name_column = visible
        .iter()
        .map(|candidate| candidate.value.chars().count())
        .max()
        .unwrap_or(0)
        .min(inside.saturating_sub(MARKER + GAP) / 2);
    let detail_room = inside.saturating_sub(MARKER + name_column + GAP);

    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = index == highlight;
            // From the set, or the ASCII fallback stops at the transcript.
            let marker = if selected { format!("{} ", glyphs.edit) } else { "  ".to_string() };
            let style = if selected {
                theme.signal(theme.accent)
            } else {
                Style::default().fg(theme.assistant)
            };
            let line = Line::from(vec![
                Span::styled(
                    marker,
                    if selected {
                        theme.signal(theme.accent)
                    } else {
                        theme.accent()
                    },
                ),
                Span::styled(format!("{:<name_column$}", candidate.value), style),
                Span::styled(
                    format!(
                        "{:GAP$}{}",
                        "",
                        crate::render::truncate(&candidate.detail, detail_room, glyphs.ellipsis)
                    ),
                    if selected {
                        theme.signal(theme.accent)
                    } else {
                        theme.dim()
                    },
                ),
            ]);
            if selected {
                line.style(theme.signal(theme.accent))
            } else {
                line
            }
        })
        .collect();

    Paragraph::new(lines)
        .style(theme.panel())
        .block(
            Block::default()
                .style(theme.panel())
                .borders(Borders::ALL)
                .border_style(theme.dim())
                .title(Span::styled(" INDEX / MATCHES ", theme.accent())),
        )
}


/// Place the popup directly above the composer, clamped to the screen.
pub fn popup_area(composer: Rect, completion: &Completion, screen: Rect) -> Rect {
    let rows = (completion.visible().0.len() as u16).min(MAX_VISIBLE as u16) + 2;
    let height = rows.min(screen.height.saturating_sub(composer.height).max(3));
    let width = composer.width;

    Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn popup(width: u16) -> ratatui::buffer::Buffer {
        let mut completion = Completion::default();
        completion.update(
            "/",
            1,
            &[
                Candidate::new("/cs", "search the codebase with parallel research agents"),
                Candidate::new("/rust-review", "Reviews Rust for the defects that actually ship in this codebase. Use after any change."),
            ],
            &[],
        );
        let area = ratatui::layout::Rect::new(0, 0, width, 6);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(
            widget(&completion, width, &crate::theme::Theme::default(), &crate::glyphs::UNICODE),
            area,
            &mut buf,
        );
        buf
    }

    fn row(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// Ragged, the popup reads as a list of unrelated strings rather than a
    /// table. The picker already aligns; this is the same rule.
    #[test]
    fn every_detail_starts_in_the_same_column() {
        let buf = popup(100);
        // Columns, not bytes: the border and the selection marker are three
        // bytes each, so a byte offset says these disagree when they align.
        let column = |y, needle: &str| {
            let text = row(&buf, y);
            text.char_indices().position(|(i, _)| text[i..].starts_with(needle)).expect(needle)
        };
        assert_eq!(
            column(1, "search"),
            column(2, "Reviews"),
            "details must line up:\n{:?}\n{:?}",
            row(&buf, 1),
            row(&buf, 2),
        );
    }

    /// The bug this replaced cut descriptions to a fixed 70 characters at the
    /// source, which ended one entry mid-sentence on the word "Use" while the
    /// popup had room to spare.
    #[test]
    fn a_long_detail_is_elided_at_the_edge_rather_than_at_a_guess() {
        let buf = popup(100);
        let text = row(&buf, 2);
        assert!(text.contains('\u{2026}'), "a clipped detail must say so: {text:?}");
        assert!(text.chars().count() as u16 <= 100, "the line must not exceed the popup");
    }

    /// A terminal narrow enough that the name column alone would consume the row.
    /// The arithmetic is all `saturating_sub`, so the failure mode is a panic on
    /// a bad format width rather than a wrong pixel.
    #[test]
    fn a_narrow_popup_still_renders() {
        for width in 4..=24 {
            let buf = popup(width);
            for y in 0..buf.area.height {
                assert_eq!(row(&buf, y).chars().count() as u16, width);
            }
        }
    }

    #[test]
    fn the_popup_sits_above_the_composer() {
        let screen = Rect::new(0, 0, 80, 30);
        let composer = Rect::new(0, 24, 80, 4);

        let mut completion = Completion::default();
        let files: Vec<String> = (0..20).map(|i| format!("src/f{i}.rs")).collect();
        completion.update("@f", 2, &[], &files);

        let popup = popup_area(composer, &completion, screen);

        assert!(popup.y + popup.height <= composer.y, "popup must not cover the input");
        assert_eq!(popup.x, composer.x);
        assert_eq!(popup.width, composer.width);
    }

    #[test]
    fn the_popup_never_runs_off_the_top() {
        // A short terminal with a tall draft is the case that overflows.
        let screen = Rect::new(0, 0, 80, 10);
        let composer = Rect::new(0, 2, 80, 6);

        let mut completion = Completion::default();
        let files: Vec<String> = (0..50).map(|i| format!("src/f{i}.rs")).collect();
        completion.update("@f", 2, &[], &files);

        let popup = popup_area(composer, &completion, screen);
        assert!(popup.y < screen.height);
        assert!(popup.height > 0);
    }

    fn commands() -> Vec<Candidate> {
        vec![
            Candidate::new("/help", "show help"),
            Candidate::new("/exit", "quit"),
            Candidate::new("/review", "review a PR"),
            Candidate::new("/resume", "resume a session"),
        ]
    }

    fn files() -> Vec<String> {
        vec![
            "src/main.rs".into(),
            "crates/octane-cli/src/main.rs".into(),
            "crates/octane-tui/src/app.rs".into(),
            "README.md".into(),
            "vendor/deep/nested/main.rs".into(),
        ]
    }

    fn completion_for(text: &str) -> Completion {
        let mut completion = Completion::default();
        completion.update(text, text.len(), &commands(), &files());
        completion
    }

    #[test]
    fn a_leading_slash_opens_command_completion() {
        let completion = completion_for("/re");
        assert!(completion.is_active());
        let values: Vec<&str> =
            completion.candidates().iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"/review"));
        assert!(values.contains(&"/resume"));
        assert!(!values.contains(&"/exit"));
    }

    #[test]
    fn a_slash_mid_sentence_is_not_a_command() {
        // Otherwise typing a path in prose pops a command list.
        assert!(!completion_for("look at src/main.rs").is_active());
        assert!(!completion_for("/help and then").is_active());
    }

    #[test]
    fn at_opens_file_completion() {
        let completion = completion_for("@main");
        assert!(completion.is_active());
        assert!(completion.candidates().iter().all(|c| c.value.starts_with('@')));
    }

    #[test]
    fn file_matching_is_fuzzy_not_prefix() {
        // The whole point: finding a file without knowing where it lives.
        let completion = completion_for("@octcli");
        let values: Vec<&str> =
            completion.candidates().iter().map(|c| c.value.as_str()).collect();
        assert!(
            values.contains(&"@crates/octane-cli/src/main.rs"),
            "expected a fuzzy hit, got {values:?}"
        );
    }

    #[test]
    fn the_shortest_and_most_direct_match_ranks_first() {
        let completion = completion_for("@main.rs");
        assert_eq!(
            completion.candidates()[0].value,
            "@src/main.rs",
            "got {:?}",
            completion.candidates().iter().map(|c| &c.value).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_email_address_does_not_open_file_completion() {
        assert!(!completion_for("mail bob@example.com").is_active());
    }

    #[test]
    fn accepting_replaces_the_trigger_and_leaves_a_space() {
        let completion = completion_for("@main.rs");
        let (text, cursor) = completion.accept("@main.rs").unwrap();

        assert_eq!(text, "@src/main.rs ");
        assert_eq!(cursor, text.len(), "caret should sit after the space");
    }

    #[test]
    fn accepting_preserves_surrounding_text() {
        let mut completion = Completion::default();
        let text = "explain @main and stop";
        // Caret just after "@main".
        completion.update(text, 13, &commands(), &files());

        let (result, cursor) = completion.accept(text).unwrap();
        assert!(result.starts_with("explain @src/main.rs "));
        assert!(result.ends_with(" and stop"));
        assert_eq!(&result[cursor..], " and stop");
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut completion = completion_for("@");
        let count = completion.candidates().len();
        assert!(count > 1);

        completion.select_previous();
        assert_eq!(completion.selected(), count - 1, "up from the top wraps to the bottom");

        completion.select_next();
        assert_eq!(completion.selected(), 0);
    }

    #[test]
    fn selection_survives_typing_another_character() {
        let mut completion = Completion::default();
        completion.update("@m", 2, &commands(), &files());
        completion.select_next();
        let chosen = completion.selected();
        assert_eq!(chosen, 1);

        // Refining the same query must not yank the highlight back to the top.
        completion.update("@ma", 3, &commands(), &files());
        assert_eq!(completion.selected(), 1);
    }

    #[test]
    fn switching_completion_kind_resets_the_selection() {
        let mut completion = Completion::default();
        completion.update("@m", 2, &commands(), &files());
        completion.select_next();

        completion.update("/re", 3, &commands(), &files());
        assert_eq!(completion.selected(), 0);
    }

    #[test]
    fn the_visible_window_follows_the_selection() {
        let many: Vec<String> = (0..40).map(|i| format!("src/file{i:02}.rs")).collect();
        let mut completion = Completion::default();
        completion.update("@file", 5, &commands(), &many);

        for _ in 0..12 {
            completion.select_next();
        }
        let (visible, highlight) = completion.visible();

        assert_eq!(visible.len(), MAX_VISIBLE);
        assert!(highlight < MAX_VISIBLE, "the highlight must stay inside the popup");
    }

    #[test]
    fn no_trigger_means_no_popup() {
        assert!(!completion_for("just some prose").is_active());
        assert!(!completion_for("").is_active());
    }

    #[test]
    fn a_fully_typed_command_stops_intercepting_enter() {
        // Otherwise Enter is swallowed, and the next thing typed lands on the
        // end of the command the user thought they had already sent.
        let completion = completion_for("/help");
        assert!(completion.is_active());
        assert!(completion.is_exhausted());
    }

    #[test]
    fn a_partial_command_still_wants_enter() {
        let completion = completion_for("/hel");
        assert!(completion.is_active());
        assert!(!completion.is_exhausted());
    }

    #[test]
    fn an_ambiguous_command_still_wants_enter() {
        // `/re` matches both /review and /resume, so Enter should pick one.
        assert!(!completion_for("/re").is_exhausted());
    }

    #[test]
    fn file_completion_never_counts_as_exhausted() {
        // A path is rarely typed in full, and accepting is the point.
        assert!(!completion_for("@src/main.rs").is_exhausted());
    }

    #[test]
    fn dismissing_clears_everything() {
        let mut completion = completion_for("@main");
        assert!(completion.is_active());
        completion.dismiss();
        assert!(!completion.is_active());
        assert!(completion.accept("@main").is_none());
    }

    #[test]
    fn dismissing_survives_the_next_refresh() {
        // The refresh runs after every keystroke. Without a memory of the
        // dismissal it reopens the popup instantly and Esc appears inert.
        let mut completion = Completion::default();
        completion.update("@main", 5, &commands(), &files());
        completion.dismiss();

        completion.update("@main", 5, &commands(), &files());
        assert!(!completion.is_active(), "Esc must stick");
    }

    #[test]
    fn typing_more_brings_the_popup_back() {
        let mut completion = Completion::default();
        completion.update("@main", 5, &commands(), &files());
        completion.dismiss();

        // Refining the query is a request for suggestions again.
        completion.update("@main.", 6, &commands(), &files());
        assert!(completion.is_active());
    }

    #[test]
    fn a_dismissal_does_not_suppress_a_different_trigger() {
        let mut completion = Completion::default();
        completion.update("@main", 5, &commands(), &files());
        completion.dismiss();

        completion.update("/re", 3, &commands(), &files());
        assert!(completion.is_active(), "a command popup is a different question");
    }
}
