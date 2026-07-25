//! The scrollable message pane.
//!
//! Going full screen means giving up the terminal's own scrollback, search, and
//! selection, so those have to be provided here instead. This module owns the
//! rendered lines and a scroll position; it is pure, so the scrolling rules are
//! testable without a terminal.
//!
//! The behaviour that matters is **follow-tail**: while the view is at the
//! bottom, new content keeps it there. The moment the user scrolls up, it stops
//! following, because content jumping away mid-read is the single most annoying
//! thing a log pane can do. Scrolling back to the bottom resumes following.

use ratatui::text::Line;

/// Lines retained.
///
/// Bounded because a long session otherwise grows without limit, and nobody
/// scrolls back through 200,000 lines. Old content is dropped from the front.
pub const MAX_LINES: usize = 20_000;

#[derive(Debug, Default)]
pub struct Transcript {
    lines: Vec<Line<'static>>,
    /// Lines hidden above the viewport.
    scroll: usize,
    /// Whether new content should pull the view down.
    following: bool,
    /// Height of the last render, for page-sized movement.
    viewport: usize,
}

impl Transcript {
    pub fn new() -> Self {
        Self { following: true, ..Default::default() }
    }

    pub fn push(&mut self, lines: Vec<Line<'static>>) {
        self.lines.extend(lines);

        if self.lines.len() > MAX_LINES {
            let excess = self.lines.len() - MAX_LINES;
            self.lines.drain(..excess);
            // The window has shifted under the scroll position; move it with the
            // content so the user keeps looking at the same text.
            self.scroll = self.scroll.saturating_sub(excess);
        }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.scroll = 0;
        self.following = true;
    }

    pub fn is_following(&self) -> bool {
        self.following
    }

    /// The slice to draw in a viewport of `height` rows.
    ///
    /// Records the height so page movement knows how far a page is.
    pub fn visible(&mut self, height: usize) -> &[Line<'static>] {
        self.viewport = height;

        if self.following {
            self.scroll = self.lines.len().saturating_sub(height);
        } else {
            // A window resize can leave the offset past the end.
            self.scroll = self.scroll.min(self.max_scroll(height));
        }

        let start = self.scroll.min(self.lines.len());
        let end = (start + height).min(self.lines.len());
        &self.lines[start..end]
    }

    fn max_scroll(&self, height: usize) -> usize {
        self.lines.len().saturating_sub(height)
    }

    /// Scroll up, which stops following.
    pub fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.following = false;
    }

    /// Scroll down. Reaching the bottom resumes following, so a user who scrolls
    /// back down does not have to do anything else to get live output again.
    pub fn scroll_down(&mut self, amount: usize) {
        let limit = self.max_scroll(self.viewport);
        self.scroll = (self.scroll + amount).min(limit);
        if self.scroll >= limit {
            self.following = true;
        }
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.viewport.saturating_sub(1).max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.viewport.saturating_sub(1).max(1));
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
        self.following = false;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.following = true;
        self.scroll = self.max_scroll(self.viewport);
    }

    /// Scroll position as a 0.0-1.0 fraction, for a scrollbar.
    pub fn progress(&self) -> f64 {
        let limit = self.max_scroll(self.viewport);
        if limit == 0 {
            return 1.0;
        }
        self.scroll as f64 / limit as f64
    }

    /// Whether content extends past the viewport in either direction.
    pub fn overflows(&self) -> bool {
        self.lines.len() > self.viewport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(range: std::ops::Range<usize>) -> Vec<Line<'static>> {
        range.map(|i| Line::raw(format!("line {i}"))).collect()
    }

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn an_empty_transcript_shows_nothing() {
        let mut transcript = Transcript::new();
        assert!(transcript.visible(10).is_empty());
        assert!(transcript.is_empty());
    }

    #[test]
    fn short_content_is_shown_whole() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..3));
        assert_eq!(text_of(transcript.visible(10)).len(), 3);
    }

    #[test]
    fn the_view_follows_the_tail_by_default() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));

        let visible = text_of(transcript.visible(5));
        assert_eq!(visible, vec!["line 95", "line 96", "line 97", "line 98", "line 99"]);
    }

    #[test]
    fn new_content_keeps_the_view_at_the_bottom() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(5);

        transcript.push(lines(100..105));
        let visible = text_of(transcript.visible(5));
        assert_eq!(visible.last().unwrap(), "line 104");
    }

    #[test]
    fn scrolling_up_stops_the_view_running_away() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(5);

        transcript.scroll_up(10);
        assert!(!transcript.is_following());
        let before = text_of(transcript.visible(5));

        // The single most annoying thing a log pane can do is move while being
        // read, so new content must not shift the view.
        transcript.push(lines(100..120));
        assert_eq!(text_of(transcript.visible(5)), before);
    }

    #[test]
    fn scrolling_back_to_the_bottom_resumes_following() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(5);

        transcript.scroll_up(10);
        assert!(!transcript.is_following());

        transcript.scroll_down(100);
        assert!(transcript.is_following(), "reaching the bottom should resume live output");

        transcript.push(lines(100..102));
        assert_eq!(text_of(transcript.visible(5)).last().unwrap(), "line 101");
    }

    #[test]
    fn scrolling_cannot_run_past_either_end() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..20));
        transcript.visible(5);

        transcript.scroll_up(1_000);
        assert_eq!(text_of(transcript.visible(5))[0], "line 0");

        transcript.scroll_down(1_000);
        assert_eq!(text_of(transcript.visible(5)).last().unwrap(), "line 19");
    }

    #[test]
    fn paging_moves_by_a_screen_less_one_line_of_overlap() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(10);

        transcript.page_up();
        let visible = text_of(transcript.visible(10));
        // 100 lines, viewport 10, bottom offset 90; a page up is 9 lines.
        assert_eq!(visible[0], "line 81");
    }

    #[test]
    fn top_and_bottom_jump_all_the_way() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(5);

        transcript.scroll_to_top();
        assert_eq!(text_of(transcript.visible(5))[0], "line 0");
        assert!(!transcript.is_following());

        transcript.scroll_to_bottom();
        assert!(transcript.is_following());
        assert_eq!(text_of(transcript.visible(5)).last().unwrap(), "line 99");
    }

    #[test]
    fn a_shrinking_window_does_not_strand_the_view_past_the_end() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..50));
        transcript.visible(40);
        transcript.scroll_up(5);

        // The user shrinks the terminal; the offset was valid, now it is not.
        let visible = text_of(transcript.visible(45));
        assert!(!visible.is_empty(), "a resize must not blank the pane");
    }

    #[test]
    fn old_lines_are_dropped_without_moving_what_is_on_screen() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..MAX_LINES));
        transcript.visible(10);
        transcript.scroll_up(100);

        let before = text_of(transcript.visible(10));
        transcript.push(lines(0..500));

        assert!(transcript.len() <= MAX_LINES);
        // The scroll offset must move with the dropped content, or the pane
        // silently jumps while the user is reading it.
        assert_eq!(text_of(transcript.visible(10)), before);
    }

    #[test]
    fn overflow_is_reported_so_a_scrollbar_can_be_hidden() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..3));
        transcript.visible(10);
        assert!(!transcript.overflows());

        transcript.push(lines(3..50));
        transcript.visible(10);
        assert!(transcript.overflows());
    }

    #[test]
    fn progress_spans_top_to_bottom() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(10);

        transcript.scroll_to_top();
        assert_eq!(transcript.progress(), 0.0);

        transcript.scroll_to_bottom();
        assert_eq!(transcript.progress(), 1.0);
    }

    #[test]
    fn clearing_resets_to_a_following_empty_pane() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.scroll_up(50);

        transcript.clear();
        assert!(transcript.is_empty());
        assert!(transcript.is_following());
    }
}
