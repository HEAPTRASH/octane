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
//! | `U+2580`–`U+259F` | Block Elements | the logo, shading |
//! | `U+25A0`–`U+25FF` | Geometric Shapes | bullets, state markers |
//! | `U+2800`–`U+28FF` | Braille Patterns | spinners |
//! | `U+2190`–`U+21FF` | Arrows | token counters, hints |
//!
//! Deliberately excluded:
//!
//! - **Emoji** (`U+1F300`+) — double-width, and rendering varies wildly.
//! - **Nerd Font glyphs** (`U+E000`–`U+F8FF`, private use) — as a *default*.
//!   They require a patched font, and a UI that renders as tofu boxes for
//!   everyone without one is a bad thing to ship by default. They are available
//!   as an opt-in third set, [`NERD`]; see its own note on what that costs.
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
    /// Filled radio, for the value currently in effect.
    pub radio_on: &'static str,
    /// Empty radio, for an alternative.
    pub radio_off: &'static str,
    /// Marks a call that succeeded.
    ///
    /// Exists so status is not carried by colour alone. U+2713 is text
    /// presentation and stays one cell; its neighbour U+2714 does not, which
    /// is why the obvious choice is the wrong one.
    pub ok: &'static str,

    /// Horizontal rule.
    pub rule: &'static str,
    /// Separator between status segments.
    pub separator: &'static str,

    /// Input tokens.
    pub arrow_up: &'static str,
    /// Output tokens.
    pub arrow_down: &'static str,

    /// Elision marker for a line that did not fit.
    ///
    /// The one member of this struct that is not a single cell: the ASCII form
    /// is three columns where the Unicode one is one. Anything drawing it must
    /// subtract `ellipsis.chars().count()` from its budget rather than assume a
    /// column, which is what [`crate::render::truncate`] does.
    pub ellipsis: &'static str,

    /// List bullet.
    pub bullet: &'static str,
    /// Elbow that hangs a tool's output under the call that produced it.
    ///
    /// One cell in both sets, so the gutter's left edge is a straight column
    /// whichever set is active — the whole point of hanging output under a
    /// header rather than beside it.
    pub elbow: &'static str,
    /// Left margin for code blocks and quotes.
    pub bar: &'static str,

    /// Decorative slashes. Three of these is the Monster claw motif, and it is
    /// the one piece of branding that costs nothing to render.
    pub claw: &'static str,

    /// Spinner frames.
    pub spinner: &'static [&'static str],
}

/// The full set. Every glyph is single-width in every terminal tested.
pub const UNICODE: Glyphs = Glyphs {
    prompt: "\u{203a}",   // › single right angle quote
    tool: "\u{25cf}",     // ● black circle
    edit: "\u{25b8}",     // ▸ small right triangle
    error: "\u{2717}",    // ✗ ballot X — text presentation, unlike ✘
    question: "\u{25c6}", // ◆ black diamond
    notice: "\u{00b7}",   // · middle dot
    radio_on: "\u{25cf}",  // ● black circle, reused from `tool`
    radio_off: "\u{25cb}", // ○ white circle, same block and width
    ok: "\u{2713}",       // ✓ check mark, text presentation

    rule: "\u{2500}",      // ─
    separator: "\u{00b7}", // ·

    arrow_up: "\u{2191}",   // ↑
    arrow_down: "\u{2193}", // ↓

    ellipsis: "\u{2026}", // … horizontal ellipsis, one cell

    bullet: "\u{2022}", // •
    elbow: "\u{2514}",  // └
    bar: "\u{2502}",    // │

    claw: "\u{2571}", // ╱ box drawing diagonal

    // Braille spinner: eight dots rotating. Smoother than the ASCII spinner
    // because each frame differs by one cell.
    spinner: &[
        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
        "\u{2827}", "\u{2807}", "\u{280f}",
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
    radio_on: "*",
    radio_off: "-",
    ok: "v",

    rule: "-",
    separator: "|",

    arrow_up: "^",
    arrow_down: "v",

    ellipsis: "...",

    bullet: "*",
    elbow: "\\",
    bar: "|",

    claw: "/",

    spinner: &["-", "\\", "|", "/"],
};

/// Nerd Font markers, for a terminal running a patched font.
///
/// **Opt-in, never detected.** There is no reliable way to ask a terminal
/// whether its font is patched — the glyphs live in the Private Use Area, so a
/// font without them renders tofu rather than failing in any way a program can
/// see. Guessing wrong produces a screen of empty boxes, so this is only ever
/// chosen deliberately.
///
/// Only the *markers* change. Rules, bars, the elbow and the braille spinner
/// stay as they are: those are ordinary Unicode, every patched font includes
/// them unchanged, and swapping them would put the layout's geometry at the
/// mercy of a font this crate cannot measure.
///
/// Codepoints are from the Nerd Fonts project's own `glyphnames.json`, and all
/// sit in the classic plane — the Material Design additions at `U+F0000`+ need
/// a v3 font, which is a narrower promise than this set wants to make.
///
/// Widths are **not** covered by `every_marker_is_one_cell`. `unicode_width`
/// reports PUA codepoints as neutral, so that test would assert nothing here;
/// the single-cell property is a promise of the font, not of this table.
pub const NERD: Glyphs = Glyphs {
    // oct-chevron_right, matching the `\u{203a}` it replaces.
    prompt: "\u{f460}",
    // oct-terminal: a tool call is a command that ran.
    tool: "\u{f489}",
    // fa-pencil.
    edit: "\u{f040}",
    // oct-x, not fa-times: the octicon set is drawn on the same grid as the
    // check below, so the two markers are the same visual weight.
    error: "\u{f467}",
    // oct-question.
    question: "\u{f420}",
    // oct-info.
    notice: "\u{f449}",
    // oct-check.
    ok: "\u{f42e}",
    radio_on: "\u{25cf}",  // ● black circle, reused from `tool`
    radio_off: "\u{25cb}", // ○ white circle, same block and width
    rule: "\u{2500}",      // ─
    separator: "\u{00b7}", // ·
    arrow_up: "\u{2191}",   // ↑
    arrow_down: "\u{2193}", // ↓
    ellipsis: "\u{2026}", // … horizontal ellipsis, one cell
    bullet: "\u{2022}", // •
    elbow: "\u{2514}",  // └
    bar: "\u{2502}",    // │
    claw: "\u{2571}", // ╱ box drawing diagonal
    spinner: &[
        "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
        "\u{2827}", "\u{2807}", "\u{280f}",
    ],
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

    /// Every marker the Nerd set replaces has a counterpart in the others, and
    /// every structural glyph is shared with `UNICODE` rather than re-chosen.
    ///
    /// Widths are deliberately unasserted: `unicode_width` reports Private Use
    /// codepoints as neutral, so a width test here would pass on a font that
    /// renders them as double-width tofu and prove nothing. The single-cell
    /// property is the patched font's promise, not this table's.
    #[test]
    fn the_nerd_set_changes_markers_and_leaves_the_geometry_alone() {
        for (nerd, unicode) in [
            (NERD.prompt, UNICODE.prompt),
            (NERD.tool, UNICODE.tool),
            (NERD.error, UNICODE.error),
            (NERD.ok, UNICODE.ok),
        ] {
            assert_ne!(nerd, unicode, "a marker the set exists to replace");
            assert!(
                nerd.chars().all(|c| ('\u{e000}'..='\u{f8ff}').contains(&c)),
                "{nerd:?} must sit in the classic private-use plane; the \
                 supplementary additions need a v3 font",
            );
        }

        // Structure is shared, so the layout maths is identical in every set.
        for (nerd, unicode) in [
            (NERD.rule, UNICODE.rule),
            (NERD.bar, UNICODE.bar),
            (NERD.elbow, UNICODE.elbow),
            (NERD.ellipsis, UNICODE.ellipsis),
        ] {
            assert_eq!(nerd, unicode, "structural glyphs must not diverge between sets");
        }
        assert_eq!(NERD.spinner, UNICODE.spinner);
    }

    #[test]
    fn every_marker_is_one_cell() {
        for set in [UNICODE, ASCII] {
            for glyph in [
                set.prompt, set.tool, set.edit, set.error, set.question, set.notice, set.rule,
                set.separator, set.arrow_up, set.arrow_down, set.claw, set.bullet, set.bar,
                set.ok, set.radio_on, set.radio_off,
            ] {
                assert!(single_width(glyph), "{glyph:?} must be one cell");
            }
            for frame in set.spinner {
                assert!(single_width(frame), "spinner frame {frame:?} must be one cell");
            }
        }
    }

    /// The ellipsis is the deliberate exemption from `every_marker_is_one_cell`.
    ///
    /// It is left out of that list rather than forced to one cell, because the
    /// only single-cell ASCII elision marker is a bare `.`, which reads as a
    /// full stop and not as "there is more". The cost is that its width has to
    /// be measured by whoever draws it; `render::truncate` is the one place
    /// that does.
    #[test]
    fn the_ellipsis_is_the_one_marker_wider_than_a_cell() {
        assert_eq!(UNICODE.ellipsis.chars().count(), 1);
        assert_eq!(ASCII.ellipsis, "...");
        assert!(ASCII.ellipsis.is_ascii());
    }

    #[test]
    fn no_glyph_strays_into_emoji_territory() {
        // Anything at or above U+1F300 is double-width, and several symbols in
        // U+2600-U+27BF default to emoji presentation. Staying below U+2900
        // keeps every glyph unambiguously narrow.
        for set in [UNICODE, ASCII] {
            for glyph in [set.prompt, set.tool, set.edit, set.error, set.question, set.claw, set.ok, set.radio_on, set.radio_off] {
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
    fn no_source_literal_strays_above_the_ceiling() {
        // The struct-field check above only sees glyphs that went through the
        // set. A hardcoded literal elsewhere bypasses it entirely, which is how
        // `\u{2b7e}` reached the status line and survived the ASCII fallback:
        // the function returned `&'static str` and so could not consult the
        // set at all.
        //
        // Comment lines are skipped, because the module doc names banned
        // codepoints on purpose in order to explain why they are banned.
        //
        // This scans characters, so it catches a pasted glyph and not a
        // `\u{...}` escape. That is the right trade: pasting is how the
        // violation actually happened, while an escape spells the codepoint out
        // where a reviewer can see it.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();

        for entry in std::fs::read_dir(&root).expect("src is readable") {
            let path = entry.expect("entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("source is utf-8");
            for (number, line) in source.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }
                for ch in line.chars() {
                    if ch as u32 >= 0x2900 {
                        offenders.push(format!(
                            "{}:{}: U+{:04X}",
                            path.file_name().unwrap_or_default().to_string_lossy(),
                            number + 1,
                            ch as u32
                        ));
                    }
                }
            }
        }

        assert!(offenders.is_empty(), "codepoints above U+2900 in source: {offenders:#?}");
    }
}
