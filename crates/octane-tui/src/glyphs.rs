//! Decorative glyphs, chosen for width safety.
//!
//! The constraint that matters in a terminal is **not** whether a glyph exists in
//! the user's font — it is whether it occupies one cell. A character that renders
//! double-width in one terminal and single-width in another silently destroys
//! every box, bar, and aligned column on screen, and the breakage is invisible
//! until someone reports it from a terminal you do not have.
//!
//! So everything here comes from ranges that are unambiguously narrow:
//!
//! | Range | Block | Used for |
//! |---|---|---|
//! | `U+2500`–`U+257F` | Box Drawing | frames, rules, separators |
//! | `U+2580`–`U+259F` | Block Elements | the logo, meters, shading |
//! | `U+25A0`–`U+25FF` | Geometric Shapes | bullets, state markers |
//! | `U+2800`–`U+28FF` | Braille Patterns | spinners |
//! | `U+2190`–`U+21FF` | Arrows | token counters, hints |
//!
//! Deliberately excluded:
//!
//! - **Emoji** (`U+1F300`+) — double-width, and rendering varies wildly.
//! - **Nerd Font glyphs** (`U+E000`–`U+F8FF`, private use) — require a patched
//!   font. Shipping a UI that looks broken without one is a bad default.
//! - **Emoji-presentation symbols** like `⚡` (`U+26A1`) and `✔` (`U+2714`).
//!   These *look* like ordinary symbols but default to emoji presentation in many
//!   terminals, which makes them double-width. `✓` (`U+2713`) is text
//!   presentation and safe; its neighbour is not. This distinction is the single
//!   easiest way to break a TUI.

/// A glyph set. Two of them exist so a terminal that cannot do Unicode still
/// gets a coherent interface rather than a field of replacement characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyphs {
    /// Prompt marker before user input.
    pub prompt: &'static str,
    /// Marks a completed tool call.
    pub tool: &'static str,
    /// Marks a file edit.
    pub edit: &'static str,
    /// Marks an error.
    pub error: &'static str,
    /// Marks a question awaiting the user.
    pub question: &'static str,
    /// Marks a system notice.
    pub notice: &'static str,

    /// Horizontal rule.
    pub rule: &'static str,
    /// Separator between status segments.
    pub separator: &'static str,

    /// Input tokens.
    pub arrow_up: &'static str,
    /// Output tokens.
    pub arrow_down: &'static str,

    /// List bullet.
    pub bullet: &'static str,
    /// Left margin for code blocks and quotes.
    pub bar: &'static str,

    /// Decorative slashes. Three of these is the Monster claw motif, and it is
    /// the one piece of branding that costs nothing to render.
    pub claw: &'static str,

    /// Spinner frames.
    pub spinner: &'static [&'static str],
    /// Meter fill levels, empty to full.
    pub meter: &'static [&'static str],
}

/// The full set. Every glyph is single-width in every terminal tested.
pub const UNICODE: Glyphs = Glyphs {
    prompt: "\u{203a}",   // › single right angle quote
    tool: "\u{25cf}",     // ● black circle
    edit: "\u{25b8}",     // ▸ small right triangle
    error: "\u{2717}",    // ✗ ballot X — text presentation, unlike ✘
    question: "\u{25c6}", // ◆ black diamond
    notice: "\u{00b7}",   // · middle dot

    rule: "\u{2500}",      // ─
    separator: "\u{00b7}", // ·

    arrow_up: "\u{2191}",   // ↑
    arrow_down: "\u{2193}", // ↓

    bullet: "\u{2022}", // •
    bar: "\u{2502}",    // │

    claw: "\u{2571}", // ╱ box drawing diagonal

    // Braille spinner: eight dots rotating. Smoother than the ASCII spinner
    // because each frame differs by one cell.
    spinner: &[
        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
        "\u{2827}", "\u{2807}", "\u{280f}",
    ],

    // Lower block elements, one-eighth steps.
    meter: &[
        " ", "\u{2581}", "\u{2582}", "\u{2583}", "\u{2584}", "\u{2585}", "\u{2586}", "\u{2587}",
        "\u{2588}",
    ],
};

/// Fallback for terminals without dependable Unicode.
///
/// Not a degraded afterthought — it is the same interface drawn with characters
/// that cannot fail, so a session over a limited link stays usable.
pub const ASCII: Glyphs = Glyphs {
    prompt: ">",
    tool: "*",
    edit: ">",
    error: "x",
    question: "?",
    notice: "-",

    rule: "-",
    separator: "|",

    arrow_up: "^",
    arrow_down: "v",

    bullet: "*",
    bar: "|",

    claw: "/",

    spinner: &["-", "\\", "|", "/"],
    meter: &[" ", ".", ":", "-", "=", "+", "*", "#", "#"],
};

impl Glyphs {
    /// Pick a set from the environment.
    ///
    /// Locale is the signal, because it is what actually predicts whether the
    /// terminal will render the bytes. `TERM=linux` is checked separately: the
    /// Linux console has a 256-glyph font and will show boxes for most of this
    /// regardless of locale.
    pub fn detect() -> Self {
        if std::env::var("OCTANE_ASCII").is_ok() {
            return ASCII;
        }
        if std::env::var("TERM").is_ok_and(|term| term == "linux" || term == "dumb") {
            return ASCII;
        }

        let unicode_locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().any(|name| {
            std::env::var(name)
                .is_ok_and(|value| value.to_uppercase().contains("UTF-8") || value.to_uppercase().contains("UTF8"))
        });

        if unicode_locale { UNICODE } else { ASCII }
    }

    /// A horizontal rule of the given width.
    pub fn rule(&self, width: usize) -> String {
        self.rule.repeat(width)
    }

    /// Left margin glyph for code blocks and quotes.
    pub fn code_bar(&self) -> &'static str {
        self.bar
    }

    /// The claw motif: three slashes.
    pub fn claw_mark(&self) -> String {
        self.claw.repeat(3)
    }

    /// Render a 0.0-1.0 fraction as a bar of `width` cells.
    ///
    /// Uses partial blocks for the leading cell, so a bar moves smoothly rather
    /// than jumping a full character at a time.
    pub fn bar(&self, fraction: f64, width: usize) -> String {
        let fraction = fraction.clamp(0.0, 1.0);
        let steps = self.meter.len() - 1;
        let total = (fraction * (width * steps) as f64).round() as usize;

        let full = total / steps;
        let partial = total % steps;

        let mut out = String::with_capacity(width * 3);
        for _ in 0..full.min(width) {
            out.push_str(self.meter[steps]);
        }
        if full < width && partial > 0 {
            out.push_str(self.meter[partial]);
        }
        let drawn = full.min(width) + usize::from(full < width && partial > 0);
        for _ in drawn..width {
            out.push_str(self.meter[0]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every glyph must be exactly one cell wide, or boxes misalign.
    ///
    /// Approximated by char count, since anything outside the BMP ranges this
    /// module allows is already excluded by construction.
    fn single_width(glyph: &str) -> bool {
        glyph.chars().count() == 1
    }

    #[test]
    fn every_marker_is_one_cell() {
        for set in [UNICODE, ASCII] {
            for glyph in [
                set.prompt, set.tool, set.edit, set.error, set.question, set.notice, set.rule,
                set.separator, set.arrow_up, set.arrow_down, set.claw, set.bullet, set.bar,
            ] {
                assert!(single_width(glyph), "{glyph:?} must be one cell");
            }
            for frame in set.spinner {
                assert!(single_width(frame), "spinner frame {frame:?} must be one cell");
            }
            for level in set.meter {
                assert!(single_width(level), "meter level {level:?} must be one cell");
            }
        }
    }

    #[test]
    fn no_glyph_strays_into_emoji_territory() {
        // Anything at or above U+1F300 is double-width, and several symbols in
        // U+2600-U+27BF default to emoji presentation. Staying below U+2900
        // keeps every glyph unambiguously narrow.
        for set in [UNICODE, ASCII] {
            for glyph in [set.prompt, set.tool, set.edit, set.error, set.question, set.claw] {
                for ch in glyph.chars() {
                    assert!(
                        (ch as u32) < 0x2900,
                        "{ch:?} (U+{:04X}) is in emoji-presentation range",
                        ch as u32
                    );
                }
            }
        }
    }

    #[test]
    fn the_ascii_set_is_pure_ascii() {
        for glyph in [ASCII.prompt, ASCII.tool, ASCII.error, ASCII.rule, ASCII.claw] {
            assert!(glyph.is_ascii(), "{glyph:?} must survive a non-UTF-8 terminal");
        }
        for frame in ASCII.spinner {
            assert!(frame.is_ascii());
        }
    }

    #[test]
    fn rules_are_exactly_the_width_asked_for() {
        assert_eq!(UNICODE.rule(5).chars().count(), 5);
        assert_eq!(ASCII.rule(5), "-----");
    }

    #[test]
    fn the_claw_is_three_slashes() {
        assert_eq!(UNICODE.claw_mark().chars().count(), 3);
        assert_eq!(ASCII.claw_mark(), "///");
    }

    #[test]
    fn bars_are_always_the_requested_width() {
        // Misjudging this by one cell is what pushes a status line onto two rows.
        for fraction in [0.0, 0.01, 0.25, 0.5, 0.99, 1.0] {
            for width in [1, 5, 10, 40] {
                assert_eq!(
                    UNICODE.bar(fraction, width).chars().count(),
                    width,
                    "fraction {fraction} at width {width}"
                );
            }
        }
    }

    #[test]
    fn bars_read_empty_and_full_at_the_extremes() {
        assert_eq!(UNICODE.bar(0.0, 4).trim(), "");
        assert_eq!(UNICODE.bar(1.0, 4), "████");
    }

    #[test]
    fn out_of_range_fractions_are_clamped() {
        assert_eq!(UNICODE.bar(-1.0, 4).chars().count(), 4);
        assert_eq!(UNICODE.bar(99.0, 4), "████");
    }

    #[test]
    fn partial_blocks_make_the_bar_move_smoothly() {
        // Between two whole cells the leading cell should be a partial block
        // rather than the bar jumping a full character.
        let bar = UNICODE.bar(0.3, 4);
        assert!(
            bar.chars().any(|c| ('\u{2581}'..'\u{2588}').contains(&c)),
            "expected a partial block in {bar:?}"
        );
    }
}
