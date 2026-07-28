//! The startup wordmark.
//!
//! This module once printed to stdout before an inline viewport started, and
//! animated a charge pulse across the letters on the way. Neither survives the
//! move to the alternate screen: stdout written before `EnterAlternateScreen`
//! is hidden a millisecond later, and an animation driven by cursor-up escapes
//! and a sleep cannot run inside `Terminal::draw` — nor should it, with a redraw
//! budget the smoke test measures in bytes.
//!
//! What is left is the art and the rule for picking a size, which the empty
//! state renders like any other widget.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::Widget;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The full Octane mark. 21 columns, 7 rows.
///
/// This is the canonical block form supplied by the project. The open gaps in
/// the first, second, sixth, and seventh rows are part of the mark; keeping the
/// rows literal makes accidental proportion changes obvious in review.
const LOGO: &[&str] = &[
    "██████ █████ ████████",
    "██████ █████ ████████",
    "█████████████████████",
    "█████████████████████",
    "█████████████████████",
    "██████ █████ ████████",
    "██████ █████ ████████",
];

/// Wordmark, block-capital style. 52 columns.
const WORDMARK: &[&str] = &[
    " ██████╗  ██████╗████████╗ █████╗ ███╗   ██╗███████╗",
    "██╔═══██╗██╔════╝╚══██╔══╝██╔══██╗████╗  ██║██╔════╝",
    "██║   ██║██║        ██║   ███████║██╔██╗ ██║█████╗  ",
    "██║   ██║██║        ██║   ██╔══██║██║╚██╗██║██╔══╝  ",
    "╚██████╔╝╚██████╗   ██║   ██║  ██║██║ ╚████║███████╗",
    " ╚═════╝  ╚═════╝   ╚═╝   ╚═╝  ╚═╝╚═╝  ╚═══╝╚══════╝",
];

/// Compact wordmark for narrow terminals.
const WORDMARK_NARROW: &str = "\u{2588}\u{2584}\u{2588} OCTANE";

/// Wordmark for terminals without dependable Unicode.
///
/// The block-art version would render as a field of replacement characters,
/// which looks worse than plain letters and says nothing.
const WORDMARK_ASCII: &[&str] = &[
    "  ___   ____ _____  _    _   _ _____ ",
    " / _ \\ / ___|_   _|/ \\  | \\ | | ____|",
    "| | | | |     | | / _ \\ |  \\| |  _|  ",
    "| |_| | |___  | |/ ___ \\| |\\  | |___ ",
    " \\___/ \\____| |_/_/   \\_\\_| \\_|_____|",
];

/// Draw the banner, animating if the terminal will show it.
///
/// `animate` should be false when output is piped, when `TERM=dumb`, or when the
/// user has asked for quiet — an animation written to a log file is just noise.
/// The wordmark sized for the space available.
///
/// Returned as lines rather than drawn here: under the alternate screen every
/// cell goes through `Terminal::draw`, so a module that writes to stdout — as
/// the original animated banner did, with cursor-up escapes and a sleep — puts
/// its output on the primary screen where it is immediately hidden.
pub fn wordmark(width: u16, height: u16, ascii: bool) -> &'static [&'static str] {
    const NARROW: &[&str] = &[WORDMARK_NARROW];

    // The canonical mark uses full-block cells. A terminal without dependable
    // Unicode gets the letterforms regardless of how much room it has.
    if ascii {
        return if width >= 40 { WORDMARK_ASCII } else { NARROW };
    }

    // The project mark is the primary identity. Seven rows plus the controls
    // below need seventeen rows; on a shorter screen the horizontal wordmark
    // preserves room for the controls.
    if width >= 25 && height >= 17 {
        return LOGO;
    }
    if width >= 56 {
        return WORDMARK;
    }
    NARROW
}

/// Shown when there are no messages yet.
///
/// The negative space is deliberate: an empty session should look calm and
/// finished, not like a screen waiting for content that failed to load.
/// The wordmark and key hints, as lines.
///
/// Separate from the widget so they can be appended below startup notices
/// rather than replaced by them.
pub fn empty_state_lines<'a>(
    theme: &crate::theme::Theme,
    glyphs: &crate::glyphs::Glyphs,
    width: u16,
    height: u16,
) -> Vec<Line<'a>> {
    let hints: &[(&str, &str)] = &[
        ("type a message", "ask octane to do something"),
        ("!command", "run a shell command"),
        ("@path", "attach a file"),
        ("/", "commands"),
        ("shift/alt+enter", "newline"),
        ("shift+tab", "cycle mode"),
    ];

    let mut lines = vec![Line::default()];

    // The wordmark lives here rather than in a pre-session print: under the
    // alternate screen the empty transcript is the only place with room for it,
    // and it disappears on its own once the session has content.
    // Centred, because the logo is a picture rather than a line of text and a
    // left-flush picture reads as misaligned.
    let art = wordmark(width, height, glyphs.rule == crate::glyphs::ASCII.rule);
    let art_width = art.iter().map(|row| row.chars().count()).max().unwrap_or(0);
    let indent = " ".repeat(usize::from(width).saturating_sub(art_width) / 2);
    for row in art {
        lines.push(Line::styled(
            format!("{indent}{row}"),
            Style::default().fg(theme.accent),
        ));
    }

    lines.push(Line::default());

    for (key, description) in hints {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {key:<18}"),
                Style::default()
                    .fg(theme.assistant)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(description.to_string(), theme.dim()),
        ]));
    }

    if width >= 40 {
        lines.push(Line::default());
        lines.push(Line::styled(
            format!(
                "  {}",
                glyphs.rule((width as usize).min(58).saturating_sub(2))
            ),
            theme.dim(),
        ));
    }

    lines
}

/// Shorten a path to `room` columns, keeping the end.
///
/// `$HOME` becomes `~` first, which is both shorter and how people say it.
/// Beyond that, leading components are dropped rather than trailing ones: the
/// basename names the project and is the one part worth keeping.
fn shorten_path(path: &str, room: usize, ellipsis: &str) -> String {
    let path = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    };
    if path.chars().count() <= room {
        return path;
    }

    let marker = ellipsis.chars().count();
    let mut kept = String::new();
    for component in path.rsplit('/') {
        let candidate = if kept.is_empty() {
            component.to_string()
        } else {
            format!("{component}/{kept}")
        };
        if candidate.chars().count() + marker + 1 > room {
            break;
        }
        kept = candidate;
    }

    if kept.is_empty() {
        // Not even the basename fits; show its tail rather than nothing.
        let basename = path.rsplit('/').next().unwrap_or(&path);
        return basename
            .chars()
            .rev()
            .take(room)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
    }
    format!("{ellipsis}/{kept}")
}

/// The brand header: wordmark, workspace, sandbox state.
#[derive(Debug, Clone, Copy)]
pub struct Header<'a> {
    pub workspace: &'a str,
    pub sandboxed: bool,
    pub options: &'a crate::render::RenderOptions,
}

impl crate::component::Pane for Header<'_> {
    fn constraint(&self, _width: u16) -> Constraint {
        // One row. The workspace used to own a row of its own while the strip
        // beside it ran mostly empty, and a rule under it drew a line between
        // two things a reversed-video strip already separates. Both went to the
        // transcript, which is the only pane whose content is unbounded.
        Constraint::Length(1)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let (theme, glyphs) = (&self.options.theme, &self.options.glyphs);

        // Keep the strip factual: product name, plus a warning only when a
        // safety boundary has actually been disabled.
        let signal_color = if self.sandboxed {
            theme.accent
        } else {
            theme.error
        };
        let signal_area = Rect { height: 1, ..area };
        buf.set_style(signal_area, theme.signal(signal_color));
        let left = " OCTANE";
        let right = if self.sandboxed { "" } else { "SANDBOX OFF " };
        let width = usize::from(area.width);

        // The workspace sits between them, and is shortened from the *front*:
        // the basename is what identifies the project, and cutting the tail —
        // which is what `truncate` does — removes precisely that.
        let room = width.saturating_sub(left.len() + right.len() + 3);
        let workspace = shorten_path(self.workspace, room, glyphs.ellipsis);

        let used = left.len() + workspace.chars().count() + right.len() + 2;
        let gap = width.saturating_sub(used).max(1);
        let signal = if width > used {
            format!("{left}  {workspace}{}{right}", " ".repeat(gap))
        } else {
            // Too narrow for all three: the safety warning outranks the name,
            // because a session running unconfined must say so at any width.
            let fallback = if right.is_empty() {
                left.to_string()
            } else {
                right.to_string()
            };
            fallback.chars().take(width).collect()
        };
        Paragraph::new(signal)
            .style(theme.signal(signal_color))
            .render(signal_area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Pane;

    #[test]
    fn every_wordmark_row_is_the_same_width() {
        // A ragged row shifts the art and, worse, is invisible until someone
        // sees it in a terminal narrower than the one it was written in.
        for art in [WORDMARK, WORDMARK_ASCII, LOGO] {
            let width = art[0].chars().count();
            for row in art {
                assert_eq!(row.chars().count(), width, "{row:?}");
            }
        }
    }

    #[test]
    fn a_narrow_terminal_gets_the_compact_mark() {
        // The full mark is 21 columns; wrapping it is worse than not
        // showing it, because a wrapped row reads as corruption.
        assert_eq!(wordmark(24, 20, false), &[WORDMARK_NARROW]);
        assert_eq!(wordmark(30, 20, true), &[WORDMARK_NARROW]);
    }

    #[test]
    fn a_wide_but_short_terminal_gets_the_horizontal_wordmark() {
        assert_eq!(wordmark(100, 16, false), WORDMARK);
        assert_eq!(wordmark(100, 20, true), WORDMARK_ASCII);
    }

    #[test]
    fn the_ascii_wordmark_survives_a_non_utf8_terminal() {
        // The block-art version would render as a field of replacement
        // characters, which says nothing and looks broken.
        for row in WORDMARK_ASCII {
            assert!(row.is_ascii(), "{row:?}");
        }
    }

    #[test]
    fn the_logo_only_appears_when_its_rows_are_affordable() {
        // It is seven rows. Taking them from a short pane pushes the key hints
        // below it off screen, which is the one thing the empty state is for.
        assert_eq!(wordmark(100, 20, false), LOGO);
        assert_ne!(
            wordmark(100, 16, false),
            LOGO,
            "a short pane keeps the hints"
        );
        assert_ne!(
            wordmark(24, 40, false),
            LOGO,
            "and a narrow one cannot fit it"
        );
    }

    #[test]
    fn the_logo_is_never_used_as_an_ascii_fallback() {
        // Full blocks have no ASCII equivalent, so a terminal without dependable
        // Unicode gets letterforms however much room it has.
        assert_ne!(wordmark(100, 60, true), LOGO);
    }

    #[test]
    fn every_logo_cell_is_one_column() {
        // The mark deliberately contains only full blocks and spaces. A stray
        // glyph can change its proportions or render at a different width.
        for row in LOGO {
            for ch in row.chars() {
                assert!(matches!(ch, '█' | ' '), "unexpected logo cell {ch:?}");
            }
        }
    }

    #[test]
    fn the_wordmark_fits_a_standard_terminal() {
        assert!(WORDMARK[0].chars().count() <= 80);
    }

    #[test]
    fn the_header_signal_spans_the_terminal_without_invented_status_copy() {
        let options = crate::render::RenderOptions {
            theme: crate::theme::Theme::new(crate::theme::ColorDepth::TrueColor),
            ..Default::default()
        };
        let header = Header {
            workspace: "/workspace",
            sandboxed: true,
            options: &options,
        };
        let area = Rect::new(0, 0, 80, 3);
        let mut buf = Buffer::empty(area);
        header.render(area, &mut buf);

        let row: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains("OCTANE"));
        assert!(!row.contains("CONTAINED"));
        assert!(!row.contains("SYS."));
        for x in 0..area.width {
            assert_eq!(buf[(x, 0)].bg, options.theme.accent);
            assert_eq!(buf[(x, 0)].fg, options.theme.on_accent);
        }
    }

    #[test]
    fn an_uncontained_header_switches_the_signal_to_safety_orange() {
        let options = crate::render::RenderOptions {
            theme: crate::theme::Theme::new(crate::theme::ColorDepth::TrueColor),
            ..Default::default()
        };
        let header = Header {
            workspace: "/workspace",
            sandboxed: false,
            options: &options,
        };
        let area = Rect::new(0, 0, 80, 3);
        let mut buf = Buffer::empty(area);
        header.render(area, &mut buf);

        let row: String = (0..area.width).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(row.contains("SANDBOX OFF"));
        assert!((0..area.width).all(|x| buf[(x, 0)].bg == options.theme.error));
    }
}
