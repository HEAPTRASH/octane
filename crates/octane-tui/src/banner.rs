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


/// The full logo. 30 columns, 15 rows.
///
/// Braille, which is one of the ranges declared width-safe: the whole block is
/// East Asian Width Neutral, so every cell is one column in every terminal.
/// It is tall, so [`wordmark`] only returns it when the pane can spare the
/// rows; the block-capital form below is the fallback, not a lesser variant.
const LOGO: &[&str] = &[
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⣀⣀⣀⣀⣀⣀⣀⣀⠀⠀⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⣠⣶⣿⣿⣿⣿⣿⣿⣿⣿⣿⡉⠙⣻⣷⣶⣤⣀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⣼⣿⣿⣿⡿⠋⠀⠀⠀⠀⢹⣿⣿⡟⠉⠉⠉⢻⡿⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠰⣿⣿⣿⣿⠀⠀⠀⠀⠀⠀⠀⣿⣿⣇⠀⠀⠀⠈⠇⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⢿⣿⣿⣿⣷⣄⠀⠀⠀⠀⠀⠉⠛⠿⣷⣤⡤⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠈⠻⣿⣿⣿⣿⣿⣶⣦⣤⣤⣀⣀⣀⡀⠉⠀⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠙⠻⢿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣦⡀⠀⠀⠀⠀",
    "⠀⠀⠀⢀⣀⣤⣄⣀⠀⠀⠀⠀⠀⠀⠀⠉⠉⠙⠛⠿⣿⣿⣿⣿⣿⣿⣦⠀⠀⠀",
    "⠀⠀⣰⣿⣿⣿⣿⣿⣷⣤⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠙⢿⣿⣿⣿⣿⣧⠀⠀",
    "⠀⠀⣿⣿⣿⠁⠀⠈⠙⢿⣿⣦⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢻⣿⣿⣿⣿⠀⠀",
    "⠀⠀⢿⣿⣿⣆⠀⠀⠀⠀⠈⠛⠿⣿⣶⣦⡤⠴⠀⠀⠀⠀⠀⣸⣿⣿⣿⡿⠀⠀",
    "⠀⠀⠈⢿⣿⣿⣷⣄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣰⣿⣿⣿⣿⠃⠀⠀",
    "⠀⠀⠀⠀⠙⢿⣿⣿⣿⣶⣦⣤⣀⣀⡀⠀⠀⠀⣀⣠⣴⣾⣿⣿⣿⡿⠃⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠈⠙⠻⠿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡿⠟⠋⠀⠀⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠉⠙⠛⠛⠛⠛⠛⠛⠉⠁⠀⠀⠀⠀⠀⠀⠀⠀",
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

    // Braille has no ASCII equivalent, so a terminal without dependable
    // Unicode gets the letterforms regardless of how much room it has.
    if ascii {
        return if width >= 40 { WORDMARK_ASCII } else { NARROW };
    }

    // The logo only earns its rows if the hints below it still fit. Its own 15
    // plus the tagline, six hints and their spacing is a little over 26.
    if width >= 34 && height >= 27 {
        return LOGO;
    }
    if width >= 56 {
        return WORDMARK;
    }
    NARROW
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // The full wordmark is 52 columns; wrapping it is worse than not
        // showing it, because a wrapped row reads as corruption.
        assert_eq!(wordmark(40, 20, false), &[WORDMARK_NARROW]);
        assert_eq!(wordmark(30, 20, true), &[WORDMARK_NARROW]);
    }

    #[test]
    fn a_wide_terminal_gets_the_full_wordmark() {
        assert_eq!(wordmark(100, 20, false), WORDMARK);
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
        // It is 15 rows. Taking them from a short pane pushes the key hints
        // below it off screen, which is the one thing the empty state is for.
        assert_eq!(wordmark(100, 40, false), LOGO);
        assert_ne!(wordmark(100, 24, false), LOGO, "a short pane keeps the hints");
        assert_ne!(wordmark(30, 40, false), LOGO, "and a narrow one cannot fit it");
    }

    #[test]
    fn the_logo_is_never_used_as_an_ascii_fallback() {
        // Braille has no ASCII equivalent, so a terminal without dependable
        // Unicode gets letterforms however much room it has.
        assert_ne!(wordmark(100, 60, true), LOGO);
    }

    #[test]
    fn every_logo_cell_is_one_column() {
        // Braille is East Asian Width Neutral across the whole block, which is
        // why it is on the safe list. Asserted rather than assumed, because a
        // double-width cell here would shear the whole picture.
        for row in LOGO {
            for ch in row.chars() {
                assert!(
                    (0x2800..=0x28ff).contains(&(ch as u32)),
                    "{ch:?} (U+{:04X}) is outside the braille block",
                    ch as u32
                );
            }
        }
    }

    #[test]
    fn the_wordmark_fits_a_standard_terminal() {
        assert!(WORDMARK[0].chars().count() <= 80);
    }
}
