//! The startup logo and quick-start hints.
//!
//! Printed to stdout *before* the inline viewport starts, so the logo lands in
//! scrollback and scrolls away naturally as the session grows. That is also why
//! the animation happens here and nowhere else: once content is written to
//! scrollback it is never redrawn, so anything that moves has to move before it
//! settles.
//!
//! The animation is a highlight sweeping left to right across the wordmark —
//! a charge pulse. Two passes, about 600ms total. Long enough to register, short
//! enough that nobody waits for it, and skipped entirely when output is not a
//! terminal.

use std::io::Write;
use std::time::Duration;

use crate::glyphs::Glyphs;
use crate::theme::{ColorDepth, Theme};

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

/// Minimum width for the full wordmark, with margin.
const WORDMARK_MIN_WIDTH: u16 = 56;

/// Frames in the sweep. Each advances the highlight one column band.
const SWEEP_FRAMES: usize = 18;
const SWEEP_PASSES: usize = 2;
const FRAME_DELAY: Duration = Duration::from_millis(16);

/// Draw the banner, animating if the terminal will show it.
///
/// `animate` should be false when output is piped, when `TERM=dumb`, or when the
/// user has asked for quiet — an animation written to a log file is just noise.
pub fn draw(
    out: &mut impl Write,
    width: u16,
    theme: &Theme,
    glyphs: &Glyphs,
    animate: bool,
) -> std::io::Result<()> {
    let unicode = *glyphs == crate::glyphs::UNICODE;

    if width < WORDMARK_MIN_WIDTH {
        let mark = if unicode { WORDMARK_NARROW } else { "OCTANE" };
        writeln!(out, "{}{mark}{}", fg(theme.accent, theme), reset(theme))?;
        return Ok(());
    }

    if !unicode {
        for line in WORDMARK_ASCII {
            writeln!(out, "{}{line}{}", fg(theme.accent, theme), reset(theme))?;
        }
        return Ok(());
    }

    if animate && theme.is_colored() {
        sweep(out, theme)?;
    }
    for line in WORDMARK {
        writeln!(out, "{}{line}{}", fg(theme.accent, theme), reset(theme))?;
    }
    Ok(())
}

/// Tagline and quick-start hints.
///
/// Deliberately short. A wall of help on every launch is a wall people stop
/// reading by the third session, so this covers only what cannot be discovered
/// by typing `/`.
pub fn tips(width: u16, theme: &Theme, glyphs: &Glyphs, workspace: &str, sandboxed: bool) -> String {
    let mut out = String::new();
    let accent = fg(theme.accent, theme);
    let dim = fg(theme.dim, theme);
    let end = reset(theme);

    out.push_str(&format!(
        "{accent}{}{end} {dim}an agent that codes in your terminal{end}\n\n",
        glyphs.claw_mark()
    ));

    out.push_str(&format!("{dim}  {workspace}{end}\n"));
    out.push_str(&format!(
        "{dim}  sandbox {}{end}\n\n",
        if sandboxed { "on" } else { "OFF — commands run unconfined" }
    ));

    let hints: &[(&str, &str)] = &[
        ("type a message", "ask octane to do something"),
        ("!command", "run a shell command directly"),
        ("@path", "pull a file into the conversation"),
        ("/help", "everything else"),
        ("shift+tab", "cycle permission mode"),
    ];

    for (key, description) in hints {
        // Padded past the longest key, so even that one keeps a gap before its
        // description instead of running into it.
        out.push_str(&format!("{accent}  {key:<16}{end}{dim}{description}{end}\n"));
    }

    if width >= 40 {
        out.push_str(&format!("\n{dim}{}{end}\n", glyphs.rule(width.min(60) as usize)));
    }
    out
}

/// One charge pulse across the wordmark, twice.
///
/// Redraws in place using cursor-up, which is safe here because the wordmark has
/// not been committed to scrollback yet.
fn sweep(out: &mut impl Write, theme: &Theme) -> std::io::Result<()> {
    let height = WORDMARK.len();
    let band = (wordmark_width() / SWEEP_FRAMES).max(1);

    for pass in 0..SWEEP_PASSES {
        for frame in 0..SWEEP_FRAMES {
            if pass > 0 || frame > 0 {
                // Back to the top of the wordmark.
                write!(out, "\x1b[{height}A")?;
            }
            let head = frame * band;

            // Each frame is presented atomically, so the sweep does not tear on
            // a terminal that would otherwise paint it mid-row.
            write!(out, "\x1b[?2026h")?;

            for line in WORDMARK {
                write!(out, "\r\x1b[2K")?;

                // Emit an SGR only when the colour actually changes. Writing one
                // per character is the obvious version and costs ~19 bytes a
                // cell: 227 KB for this animation, which is slow enough to see
                // over SSH and defeats the point of animating at all.
                let mut current: Option<ratatui::style::Color> = None;
                for (index, ch) in line.chars().enumerate() {
                    let distance = index.abs_diff(head);
                    // A short bright core with a falloff behind it, so the pulse
                    // reads as moving rather than as a blinking column.
                    let color = if distance < band {
                        crate::theme::VOLT
                    } else if distance < band * 3 {
                        crate::theme::LIME
                    } else {
                        theme.accent
                    };

                    if current != Some(color) {
                        write!(out, "{}", fg(color, theme))?;
                        current = Some(color);
                    }
                    write!(out, "{ch}")?;
                }
                writeln!(out, "{}", reset(theme))?;
            }

            write!(out, "\x1b[?2026l")?;
            out.flush()?;
            std::thread::sleep(FRAME_DELAY);
        }
    }

    // Leave the cursor back at the top so the caller's static draw overwrites the
    // final animation frame instead of appending a second copy.
    write!(out, "\x1b[{height}A")?;
    Ok(())
}

fn wordmark_width() -> usize {
    WORDMARK.iter().map(|line| line.chars().count()).max().unwrap_or(52)
}

/// SGR foreground escape, or nothing when colour is off.
fn fg(color: ratatui::style::Color, theme: &Theme) -> String {
    if !theme.is_colored() {
        return String::new();
    }
    match color {
        ratatui::style::Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m"),
        ratatui::style::Color::Indexed(index) => format!("\x1b[38;5;{index}m"),
        _ => String::new(),
    }
}

fn reset(theme: &Theme) -> &'static str {
    if theme.is_colored() { "\x1b[0m" } else { "" }
}

/// Whether an animation is appropriate here.
pub fn should_animate(is_tty: bool, depth: ColorDepth) -> bool {
    if !is_tty || depth == ColorDepth::None {
        return false;
    }
    // An explicit opt-out, for people who launch this a hundred times a day.
    std::env::var_os("OCTANE_NO_ANIMATION").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::UNICODE;

    #[test]
    fn every_wordmark_row_is_the_same_width() {
        // Ragged rows make the logo look broken and the sweep misalign.
        let widths: Vec<usize> = WORDMARK.iter().map(|line| line.chars().count()).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "rows differ in width: {widths:?}"
        );
    }

    #[test]
    fn the_wordmark_fits_a_standard_terminal() {
        assert!(wordmark_width() <= 80, "must fit 80 columns, got {}", wordmark_width());
        assert!(WORDMARK_MIN_WIDTH as usize > wordmark_width());
    }

    #[test]
    fn a_narrow_terminal_gets_the_compact_mark() {
        let theme = Theme::new(ColorDepth::TrueColor);
        let mut out = Vec::new();
        draw(&mut out, 40, &theme, &UNICODE, false).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("OCTANE"));
        assert!(text.lines().count() <= 2, "compact mark should be one line");
    }

    #[test]
    fn a_wide_terminal_gets_the_full_wordmark() {
        let theme = Theme::new(ColorDepth::TrueColor);
        let mut out = Vec::new();
        draw(&mut out, 100, &theme, &UNICODE, false).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), WORDMARK.len());
    }

    #[test]
    fn no_color_emits_no_escape_sequences() {
        let theme = Theme::new(ColorDepth::None);
        let mut out = Vec::new();
        draw(&mut out, 100, &theme, &UNICODE, true).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains('\x1b'), "NO_COLOR output must be plain");
    }

    #[test]
    fn tips_name_the_things_that_are_not_discoverable() {
        let theme = Theme::new(ColorDepth::None);
        let text = tips(80, &theme, &UNICODE, "/repo", true);

        // `/help` can be found by typing `/`; these cannot.
        assert!(text.contains("!command"));
        assert!(text.contains("@path"));
        assert!(text.contains("shift+tab"));
        assert!(text.contains("/repo"));
    }

    #[test]
    fn tips_say_plainly_when_the_sandbox_is_off() {
        let theme = Theme::new(ColorDepth::None);

        let on = tips(80, &theme, &UNICODE, "/repo", true);
        assert!(on.contains("sandbox on"));

        let off = tips(80, &theme, &UNICODE, "/repo", false);
        // Running unconfined is a thing the user should not learn by surprise.
        assert!(off.contains("unconfined"));
    }

    #[test]
    fn tips_stay_short_enough_to_read() {
        let theme = Theme::new(ColorDepth::None);
        let text = tips(80, &theme, &UNICODE, "/repo", true);
        assert!(
            text.lines().count() <= 14,
            "a wall of help stops being read: {} lines",
            text.lines().count()
        );
    }

    #[test]
    fn an_ascii_terminal_gets_letters_not_block_art() {
        let theme = Theme::new(ColorDepth::None);
        let mut out = Vec::new();
        draw(&mut out, 100, &theme, &crate::glyphs::ASCII, false).unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.is_ascii(), "a non-Unicode terminal must get pure ASCII");
        assert!(!text.contains('\u{2588}'));
    }

    #[test]
    fn every_ascii_wordmark_row_is_the_same_width() {
        let widths: Vec<usize> = WORDMARK_ASCII.iter().map(|line| line.chars().count()).collect();
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "{widths:?}");
    }

    #[test]
    fn the_hint_key_column_never_collides_with_its_description() {
        let theme = Theme::new(ColorDepth::None);
        let text = tips(80, &theme, &UNICODE, "/repo", true);

        for line in text.lines().filter(|l| l.trim_start().starts_with(|c: char| c.is_alphanumeric() || c == '!' || c == '@' || c == '/')) {
            if let Some((_, rest)) = line.trim_end().split_once("  ") {
                assert!(
                    !rest.trim_start().is_empty(),
                    "hint line has no gap between key and description: {line:?}"
                );
            }
        }
        // The longest key specifically, which is the one that collided.
        assert!(text.contains("type a message  "), "longest key needs a trailing gap");
    }

    #[test]
    fn animation_is_skipped_where_it_would_be_noise() {
        assert!(!should_animate(false, ColorDepth::TrueColor), "piped output");
        assert!(!should_animate(true, ColorDepth::None), "no colour to animate with");
    }
}
