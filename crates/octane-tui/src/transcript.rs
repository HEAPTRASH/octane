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

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, Clear, Paragraph, Widget};

use ratatui::text::Line;

/// Lines retained.
///
/// Bounded because a long session otherwise grows without limit, and nobody
/// scrolls back through 200,000 lines. Old content is dropped from the front.
pub const MAX_LINES: usize = 20_000;

/// A run of lines produced by one item, so a click can find its way back.
#[derive(Debug, Clone)]
struct Region {
    id: octane_protocol::ItemId,
    start: usize,
    len: usize,
}

#[derive(Debug, Default)]
pub struct Transcript {
    /// Logical lines with the item that produced them, before wrapping.
    ///
    /// The single source of truth. `lines` and `regions` are both derived, so
    /// a resize re-derives instead of losing content that was clipped away at
    /// the old width.
    logical: Vec<(Option<octane_protocol::ItemId>, Line<'static>)>,
    /// Wrapped to the current width. One entry here is one row on screen,
    /// which is what lets `scroll`, page movement and follow-tail keep
    /// counting rows rather than drifting from them.
    lines: Vec<Line<'static>>,
    /// Width the wrap was computed at, so a resize is detectable.
    width: usize,
    /// Where each expandable item's lines live. Only items that can be
    /// expanded are tracked; everything else is anonymous.
    regions: Vec<Region>,
    /// Lines hidden above the viewport.
    scroll: usize,
    /// Whether new content should pull the view down.
    following: bool,
    /// Height of the last render, for page-sized movement.
    viewport: usize,
    /// Lines still streaming, rendered after the committed ones.
    ///
    /// Held apart rather than appended so a delta can rewrite the in-flight
    /// text without the committed transcript growing a line per token.
    pending: Vec<Line<'static>>,
}

impl Transcript {
    pub fn new() -> Self {
        Self { following: true, ..Default::default() }
    }

    /// Push lines and remember which item produced them.
    pub fn push_owned(&mut self, id: octane_protocol::ItemId, lines: Vec<Line<'static>>) {
        self.append(Some(id), lines);
    }

    /// The expandable item owning an absolute line index, if any.
    pub fn owner_of(&self, line: usize) -> Option<octane_protocol::ItemId> {
        self.regions
            .iter()
            .find(|region| line >= region.start && line < region.start + region.len)
            .map(|region| region.id.clone())
    }

    /// The most recently pushed expandable item.
    pub fn last_owner(&self) -> Option<octane_protocol::ItemId> {
        self.regions.last().map(|region| region.id.clone())
    }

    /// Lines scrolled off the top, so a viewport row maps to an absolute index.
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Swap one item's lines for a new rendering, keeping everything else.
    ///
    /// Used by expand and collapse. Rebuilding the whole transcript instead
    /// would discard the eviction history and the scroll position.
    pub fn replace_region(&mut self, id: &octane_protocol::ItemId, lines: Vec<Line<'static>>) {
        let Some(start) = self.logical.iter().position(|(owner, _)| owner.as_ref() == Some(id))
        else {
            return;
        };
        let end = self.logical[start..]
            .iter()
            .position(|(owner, _)| owner.as_ref() != Some(id))
            .map(|offset| start + offset)
            .unwrap_or(self.logical.len());

        let replacement = lines.into_iter().map(|line| (Some(id.clone()), line));
        self.logical.splice(start..end, replacement);
        self.rewrap();
        if self.following {
            self.scroll = self.lines.len().saturating_sub(self.viewport);
        }
    }

    pub fn push(&mut self, lines: Vec<Line<'static>>) {
        self.append(None, lines);
    }

    fn append(&mut self, owner: Option<octane_protocol::ItemId>, lines: Vec<Line<'static>>) {
        // Only the new lines are wrapped. Re-deriving the whole view on every
        // push is O(n) per push and O(n^2) over a session, on a transcript
        // bounded at twenty thousand lines.
        for line in lines {
            self.wrap_into(owner.as_ref(), &line);
            self.logical.push((owner.clone(), line));
        }

        if self.logical.len() > MAX_LINES {
            let excess = self.logical.len() - MAX_LINES;
            // The scroll offset is measured in *rows*, so what matters is how
            // many rows went away. `rewrap` re-derives from what is left at the
            // same width, which makes the difference exactly that count — no
            // second wrap of the lines being discarded.
            let before = self.lines.len();
            self.logical.drain(..excess);
            self.rewrap();
            // The window has shifted under the offset; move it with the content
            // so the user keeps looking at the same text.
            self.scroll = self.scroll.saturating_sub(before - self.lines.len());
        }

        if self.following {
            self.scroll = self.lines.len().saturating_sub(self.viewport);
        }
    }

    /// Wrap one logical line onto the end of the view, extending its region.
    ///
    /// The only writer of `lines` and `regions`. Wrapping changes how many rows
    /// a logical line occupies, and regions are measured in rows, so appending
    /// to one without the other puts clicks on the wrong item.
    fn wrap_into(&mut self, owner: Option<&octane_protocol::ItemId>, line: &Line<'static>) {
        let start = self.lines.len();
        if self.width == 0 {
            self.lines.push(line.clone());
        } else {
            self.lines.extend(crate::wrap::wrap_line(line, self.width));
        }
        let len = self.lines.len() - start;
        if let Some(id) = owner {
            match self.regions.last_mut() {
                // Consecutive rows from one item are one region.
                Some(last) if &last.id == id && last.start + last.len == start => last.len += len,
                _ => self.regions.push(Region { id: id.clone(), start, len }),
            }
        }
    }

    /// Rebuild the wrapped view and the regions from the logical lines.
    fn rewrap(&mut self) {
        self.lines.clear();
        self.regions.clear();
        // Moved out and back: iterating `logical` borrows it while the wrap
        // needs `&mut self`. Nothing else can observe the gap.
        let logical = std::mem::take(&mut self.logical);
        for (owner, line) in &logical {
            self.wrap_into(owner.as_ref(), line);
        }
        self.logical = logical;
    }


    /// Replace the streaming region.
    pub fn set_pending(&mut self, lines: Vec<Line<'static>>) {
        self.pending = lines;
    }

    /// Commit the streaming region, or discard it if the caller has pushed the
    /// finished form itself.
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    pub fn len(&self) -> usize {
        self.lines.len() + self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.pending.is_empty()
    }

    pub fn clear(&mut self) {
        self.logical.clear();
        self.regions.clear();
        self.lines.clear();
        self.pending.clear();
        self.scroll = 0;
        self.following = true;
    }

    pub fn is_following(&self) -> bool {
        self.following
    }

    /// The slice to draw in a viewport of `height` rows.
    ///
    /// Records the height so page movement knows how far a page is.
    /// The slice to draw in a viewport of `height` rows.
    ///
    /// Returns owned lines because the streaming region is concatenated on the
    /// way out; borrowing would require the two to be contiguous, which is
    /// exactly what keeping them apart avoids.
    pub fn visible(&mut self, width: usize, height: usize) -> Vec<Line<'static>> {
        // A resize changes how many rows a logical line takes, so the derived
        // view is rebuilt before anything counts rows.
        if width != self.width {
            self.width = width;
            self.rewrap();
        }
        self.viewport = height;
        let total = self.len();

        if self.following {
            self.scroll = total.saturating_sub(height);
        } else {
            // A window resize can leave the offset past the end.
            self.scroll = self.scroll.min(self.max_scroll(height));
        }

        let start = self.scroll.min(total);
        let end = (start + height).min(total);

        (start..end)
            .map(|index| match self.lines.get(index) {
                Some(line) => line.clone(),
                None => self.pending[index - self.lines.len()].clone(),
            })
            .collect()
    }

    fn max_scroll(&self, height: usize) -> usize {
        self.len().saturating_sub(height)
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
}

/// Rows the transcript keeps for itself no matter how large the draft grows.
///
/// Without a floor, a long draft squeezes the transcript to nothing and the user
/// loses sight of what they are replying to.
pub const MIN_ROWS: u16 = 3;

/// The transcript as a pane: already-wrapped lines, ready to draw.
///
/// It holds lines rather than a [`Transcript`] because wrapping mutates the
/// cache — `visible` needs `&mut self` — and a pane renders from `&self`. So the
/// lines are computed for the frame and handed in, which is also what lets the
/// empty-state hints be appended to startup notices rather than replacing them.
#[derive(Debug, Clone)]
pub struct TranscriptView<'a> {
    pub lines: Vec<Line<'a>>,
    pub style: Style,
}

impl crate::component::Pane for TranscriptView<'_> {
    fn constraint(&self, _width: u16) -> Constraint {
        // The one pane that takes what is left rather than what it needs.
        Constraint::Min(MIN_ROWS)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        // Clear first: `Paragraph` writes only the cells it covers, so a line
        // shorter than the one it replaces would leave the old tail behind.
        // Then restore the canvas, because `Clear` intentionally resets styles.
        Clear.render(area, buf);
        Block::default().style(self.style).render(area, buf);
        // ponytail: clones the frame's visible lines because `Paragraph` wants
        // them owned and `render` only has `&self`. Bounded by terminal height
        // and only on a dirty frame; if it ever shows up in a profile, ratatui's
        // `WidgetRef` lets the paragraph be built once and drawn by reference.
        Paragraph::new(self.lines.clone())
            .style(self.style)
            .render(area, buf);
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
        assert!(transcript.visible(200, 10).is_empty());
        assert!(transcript.is_empty());
    }

    #[test]
    fn short_content_is_shown_whole() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..3));
        assert_eq!(text_of(&transcript.visible(200, 10)).len(), 3);
    }

    #[test]
    fn the_view_follows_the_tail_by_default() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));

        let visible = text_of(&transcript.visible(200, 5));
        assert_eq!(visible, vec!["line 95", "line 96", "line 97", "line 98", "line 99"]);
    }

    #[test]
    fn new_content_keeps_the_view_at_the_bottom() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(200, 5);

        transcript.push(lines(100..105));
        let visible = text_of(&transcript.visible(200, 5));
        assert_eq!(visible.last().unwrap(), "line 104");
    }

    #[test]
    fn scrolling_up_stops_the_view_running_away() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(200, 5);

        transcript.scroll_up(10);
        assert!(!transcript.is_following());
        let before = text_of(&transcript.visible(200, 5));

        // The single most annoying thing a log pane can do is move while being
        // read, so new content must not shift the view.
        transcript.push(lines(100..120));
        assert_eq!(text_of(&transcript.visible(200, 5)), before);
    }

    #[test]
    fn scrolling_back_to_the_bottom_resumes_following() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(200, 5);

        transcript.scroll_up(10);
        assert!(!transcript.is_following());

        transcript.scroll_down(100);
        assert!(transcript.is_following(), "reaching the bottom should resume live output");

        transcript.push(lines(100..102));
        assert_eq!(text_of(&transcript.visible(200, 5)).last().unwrap(), "line 101");
    }

    #[test]
    fn scrolling_cannot_run_past_either_end() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..20));
        transcript.visible(200, 5);

        transcript.scroll_up(1_000);
        assert_eq!(text_of(&transcript.visible(200, 5))[0], "line 0");

        transcript.scroll_down(1_000);
        assert_eq!(text_of(&transcript.visible(200, 5)).last().unwrap(), "line 19");
    }

    #[test]
    fn paging_moves_by_a_screen_less_one_line_of_overlap() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(200, 10);

        transcript.page_up();
        let visible = text_of(&transcript.visible(200, 10));
        // 100 lines, viewport 10, bottom offset 90; a page up is 9 lines.
        assert_eq!(visible[0], "line 81");
    }

    #[test]
    fn top_and_bottom_jump_all_the_way() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(200, 5);

        transcript.scroll_to_top();
        assert_eq!(text_of(&transcript.visible(200, 5))[0], "line 0");
        assert!(!transcript.is_following());

        transcript.scroll_to_bottom();
        assert!(transcript.is_following());
        assert_eq!(text_of(&transcript.visible(200, 5)).last().unwrap(), "line 99");
    }

    #[test]
    fn a_shrinking_window_does_not_strand_the_view_past_the_end() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..50));
        transcript.visible(200, 40);
        transcript.scroll_up(5);

        // The user shrinks the terminal; the offset was valid, now it is not.
        let visible = text_of(&transcript.visible(200, 45));
        assert!(!visible.is_empty(), "a resize must not blank the pane");
    }

    #[test]
    fn old_lines_are_dropped_without_moving_what_is_on_screen() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..MAX_LINES));
        transcript.visible(200, 10);
        transcript.scroll_up(100);

        let before = text_of(&transcript.visible(200, 10));
        transcript.push(lines(0..500));

        assert!(transcript.len() <= MAX_LINES);
        // The scroll offset must move with the dropped content, or the pane
        // silently jumps while the user is reading it.
        assert_eq!(text_of(&transcript.visible(200, 10)), before);
    }

    #[test]
    fn streaming_lines_render_after_the_committed_ones() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..3));
        transcript.set_pending(vec![Line::raw("streaming…")]);

        let visible = text_of(&transcript.visible(200, 10));
        assert_eq!(visible.last().unwrap(), "streaming…");
        assert_eq!(visible.len(), 4);
    }

    #[test]
    fn a_delta_rewrites_the_streaming_region_rather_than_appending() {
        // Appending would grow the transcript by a line per token.
        let mut transcript = Transcript::new();
        transcript.set_pending(vec![Line::raw("hel")]);
        transcript.set_pending(vec![Line::raw("hello")]);

        assert_eq!(text_of(&transcript.visible(200, 10)), vec!["hello"]);
        assert_eq!(transcript.len(), 1);
    }

    #[test]
    fn the_view_follows_streaming_content_too() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(200, 5);

        transcript.set_pending(vec![Line::raw("newest")]);
        assert_eq!(text_of(&transcript.visible(200, 5)).last().unwrap(), "newest");
    }

    #[test]
    fn scrolling_up_holds_still_while_content_streams_in() {
        let mut transcript = Transcript::new();
        transcript.push(lines(0..100));
        transcript.visible(200, 5);
        transcript.scroll_up(20);

        let before = text_of(&transcript.visible(200, 5));
        transcript.set_pending(vec![Line::raw("streaming")]);
        assert_eq!(text_of(&transcript.visible(200, 5)), before);
    }

    #[test]
    fn committing_replaces_the_streaming_region() {
        let mut transcript = Transcript::new();
        transcript.set_pending(vec![Line::raw("partial")]);
        assert_eq!(text_of(&transcript.visible(200, 10)), vec!["partial"]);

        transcript.clear_pending();
        transcript.push(vec![Line::raw("final")]);

        // The streaming region is replaced, not appended to — otherwise every
        // committed message would appear twice.
        assert_eq!(text_of(&transcript.visible(200, 10)), vec!["final"]);
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

    #[test]
    fn replacing_a_region_moves_the_ones_after_it() {
        // Expanding grows a block in the middle of the transcript. Every later
        // region indexes into the same vector, so their starts move with it or
        // a click lands on the wrong item.
        let mut transcript = Transcript::new();
        let first = octane_protocol::ItemId::new();
        let second = octane_protocol::ItemId::new();

        transcript.push_owned(first.clone(), vec![Line::raw("a")]);
        transcript.push_owned(second.clone(), vec![Line::raw("b")]);
        assert_eq!(transcript.owner_of(0), Some(first.clone()));
        assert_eq!(transcript.owner_of(1), Some(second.clone()));

        // The first grows by two lines.
        transcript.replace_region(
            &first,
            vec![Line::raw("a"), Line::raw("a2"), Line::raw("a3")],
        );

        assert_eq!(transcript.owner_of(0), Some(first.clone()));
        assert_eq!(transcript.owner_of(2), Some(first));
        assert_eq!(transcript.owner_of(3), Some(second), "the later region moved");
    }

    #[test]
    fn an_evicted_region_cannot_be_clicked() {
        // Regions index into `lines`, and the front is dropped past MAX_LINES.
        // A stale region would map a click onto whatever text took its place.
        let mut transcript = Transcript::new();
        let doomed = octane_protocol::ItemId::new();
        transcript.push_owned(doomed.clone(), vec![Line::raw("old")]);
        transcript.push(vec![Line::raw("filler"); MAX_LINES]);

        assert!(
            transcript.owner_of(0) != Some(doomed),
            "an evicted region must not still claim a line"
        );
    }
}
