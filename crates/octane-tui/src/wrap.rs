//! Wrapping a styled line to a width, in cells.
//!
//! Done here rather than by turning on ratatui's `Wrap`, which is not a style
//! preference but a correctness requirement. `Transcript` counts *logical*
//! lines for its scroll offset, page size and follow-tail check. `Wrap` makes
//! one logical line occupy several rows without telling anyone, so the offset
//! drifts from what is on screen, a page stops being a page, and following the
//! tail stops reaching it. That class of bug renders correctly and is invisible
//! to a unit test, which is exactly the failure mode this codebase has been
//! bitten by twice.
//!
//! Wrapping into real `Line`s keeps one row equal to one line everywhere.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// Widest line rendered, regardless of terminal width.
///
/// Long measures are hard to read, and a transcript is prose as much as it is
/// output. Both opencode and Crush independently settled near this.
pub const MAX_MEASURE: usize = 120;

/// Split `line` so no row exceeds `width` cells.
///
/// Continuation rows are indented to the original line's leading whitespace, so
/// a wrapped tool result or diff line stays visually attached to its own block
/// instead of reading as a new entry at the left margin.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.min(MAX_MEASURE);
    if width == 0 {
        return vec![line.clone()];
    }

    if measure(line) <= width {
        return vec![line.clone()];
    }

    let indent: String = leading_spaces(line);
    // A hanging indent that leaves no room for content would loop forever.
    let indent = if indent.len() + 8 > width { String::new() } else { indent };

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut first = true;

    for span in &line.spans {
        let style = span.style;
        let mut pending = String::new();

        for ch in span.content.chars() {
            let cw = ch.width().unwrap_or(0);
            let budget = if first { width } else { width.saturating_sub(indent.len()) };

            if used + cw > budget && used > 0 {
                if !pending.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut pending), style));
                }
                rows.push(Line::from(std::mem::take(&mut current)));
                first = false;
                used = 0;
                if !indent.is_empty() {
                    current.push(Span::raw(indent.clone()));
                }
            }

            pending.push(ch);
            used += cw;
        }

        if !pending.is_empty() {
            current.push(Span::styled(pending, style));
        }
    }

    if !current.is_empty() {
        rows.push(Line::from(current));
    }
    if rows.is_empty() {
        rows.push(line.clone());
    }
    rows
}

/// Display width of a line, in cells.
fn measure(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .flat_map(|span| span.content.chars())
        .map(|ch| ch.width().unwrap_or(0))
        .sum()
}

/// The line's leading spaces, for the hanging indent.
fn leading_spaces(line: &Line<'_>) -> String {
    let mut out = String::new();
    for span in &line.spans {
        for ch in span.content.chars() {
            if ch == ' ' {
                out.push(' ');
            } else {
                return out;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_short_line_is_left_alone() {
        // Cloning every line through a wrapper that does nothing would be the
        // common case, so it has to be the cheap one.
        let line = Line::raw("short");
        let rows = wrap_line(&line, 40);
        assert_eq!(rows.len(), 1);
        assert_eq!(text(&rows[0]), "short");
    }

    #[test]
    fn a_long_line_becomes_several_and_loses_nothing() {
        // The whole point: clipped text was unreachable, with no wrap and no
        // horizontal scroll to get at it.
        let body = "x".repeat(250);
        let rows = wrap_line(&Line::raw(body.clone()), 80);

        assert!(rows.len() > 1);
        let rejoined: String = rows.iter().map(text).collect();
        assert_eq!(rejoined, body, "wrapping must not drop or duplicate a character");
    }

    #[test]
    fn no_row_exceeds_the_width() {
        let rows = wrap_line(&Line::raw("y".repeat(500)), 40);
        for row in &rows {
            assert!(measure(row) <= 40, "row {:?} is too wide", text(row));
        }
    }

    #[test]
    fn continuation_rows_keep_the_indent() {
        // A wrapped diff or tool-result line that returned to the left margin
        // would read as a new entry rather than the same one continuing.
        let rows = wrap_line(&Line::raw(format!("      {}", "z".repeat(200))), 60);
        assert!(rows.len() > 1);
        assert!(rows[1..].iter().all(|row| text(row).starts_with("      ")));
    }

    #[test]
    fn styles_survive_the_split() {
        // Spans carry the colour. Rebuilding rows from raw text would render a
        // wrapped diff without its add and remove marking.
        let line = Line::from(vec![
            Span::styled("a".repeat(100), ratatui::style::Style::default().fg(ratatui::style::Color::Red)),
            Span::styled("b".repeat(100), ratatui::style::Style::default().fg(ratatui::style::Color::Blue)),
        ]);
        let rows = wrap_line(&line, 60);

        let reds: usize = rows
            .iter()
            .flat_map(|r| &r.spans)
            .filter(|s| s.style.fg == Some(ratatui::style::Color::Red))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(reds, 100, "the styled run must survive intact");
    }

    #[test]
    fn a_double_width_character_is_measured_as_two_cells() {
        // Counting characters instead of cells overflows the row by one cell
        // per CJK character, which shears every box drawn beside it.
        let rows = wrap_line(&Line::raw("\u{6f22}".repeat(60)), 40);
        for row in &rows {
            assert!(measure(row) <= 40);
        }
    }

    #[test]
    fn an_indent_wider_than_the_measure_does_not_hang() {
        // The hanging indent is dropped rather than leaving zero room for
        // content, which would make no progress and loop forever.
        let rows = wrap_line(&Line::raw(format!("{}{}", " ".repeat(30), "q".repeat(100))), 32);
        assert!(rows.len() > 1);
    }
}
