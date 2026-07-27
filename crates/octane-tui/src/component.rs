//! Panes, and the one rule they follow.
//!
//! A pane is this crate's unit of screen: a piece of state that knows how much
//! room it wants and how to draw itself into the room it gets. Each lives in the
//! module that owns its state — the composer's pane is in [`crate::composer`],
//! the status line's in [`crate::status`] — so there is one place to change when
//! a pane changes.
//!
//! [`crate::app`] owns the terminal and little else: it collects a constraint
//! from each pane, splits the screen once, and renders each into its rectangle.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};

/// One horizontal band of the screen.
///
/// Measuring and drawing live on the same type deliberately. Held apart — a
/// `foo_height` function sitting beside a `foo_widget` function — the two drift,
/// and the drift is silent because both halves are individually correct: a pane
/// measured one row short simply loses its last line, which is reliably the line
/// saying what Esc does. Every pane below had that shape before this trait.
pub trait Pane {
    /// How much vertical room this pane wants at `width`.
    ///
    /// A [`Constraint`] rather than a row count, because the transcript wants
    /// whatever is left over while everything else wants exactly what it needs,
    /// and that difference belongs to the pane rather than to the layout code.
    fn constraint(&self, width: u16) -> Constraint;

    /// Draw into `area`.
    ///
    /// `&self`, not `&mut self`: a pane renders from data it was handed and
    /// changes nothing while drawing. Anything that needs `&mut` is computed
    /// before the frame and passed in — see [`crate::transcript::TranscriptView`],
    /// which exists because wrapping the transcript mutates its cache.
    ///
    /// This is also what makes panes testable without a terminal: render into a
    /// [`Buffer`] and assert on cells. `app` is the only module that cannot be
    /// tested that way, and it is now the only one that does not draw.
    fn render(&self, area: Rect, buf: &mut Buffer);
}

/// A box of the given width and height, centred in `area`.
///
/// Clamped, so a small terminal gets a smaller box rather than a box that starts
/// off-screen.
pub fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(20);
    let height = height.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_centred_box_stays_on_screen_when_the_terminal_is_tiny() {
        let screen = Rect { x: 0, y: 0, width: 24, height: 8 };
        let area = centred(screen, 66, 40);
        assert!(area.x + area.width <= screen.width, "{area:?} runs off the right");
        assert!(area.y + area.height <= screen.height, "{area:?} runs off the bottom");
    }
}
