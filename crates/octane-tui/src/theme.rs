//! The Octane palette.
//!
//! The website's industrial system, translated cell-for-cell: an ink-black
//! canvas, warm paper text, an acid-green signal colour, and safety orange.
//! Structure comes from hard contrast and type hierarchy rather than shadows or
//! decorative colour.
//!
//! # Degradation
//!
//! Three tiers, detected rather than assumed, because a palette that only works
//! on one terminal is a palette that looks broken everywhere else:
//!
//! | Tier | When | What |
//! |---|---|---|
//! | [`ColorDepth::TrueColor`] | `COLORTERM=truecolor` or `24bit` | exact brand hexes |
//! | [`ColorDepth::Ansi256`] | a colour-capable `TERM` | nearest xterm-256 indices |
//! | [`ColorDepth::None`] | `NO_COLOR`, `TERM=dumb`, or not a tty | bold and dim only |
//!
//! `NO_COLOR` is honoured because people set it for a reason, and an agent that
//! ignores it is one they will stop trusting about other things too.
//!
//! # Other schemes
//!
//! [`Theme::named`] resolves the built-in schemes in [`crate::themes`], and
//! [`Palette`] is the same thing as data: an all-optional description a user can
//! write in a config file, applied over the Octane palette by
//! [`Theme::from_palette`]. Naming three colours overrides three colours.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// Canvas black.
pub const INK: Color = Color::Rgb(0x10, 0x11, 0x0F);
/// Raised panels and overlays.
pub const SURFACE: Color = Color::Rgb(0x19, 0x1A, 0x17);
/// Signature acid green. The brand colour, and Octane's primary signal.
pub const ACID: Color = Color::Rgb(0xB8, 0xF5, 0x00);
/// Hotter lime, for things that should pull the eye.
pub const LIME: Color = Color::Rgb(0x6A, 0xCD, 0x0C);
/// Acid yellow, for warnings.
pub const VOLT: Color = Color::Rgb(0xFA, 0xEF, 0x55);
/// Yellow-green, between acid and volt.
pub const CITRON: Color = Color::Rgb(0xC2, 0xD7, 0x2D);
/// Deep green, for receded structure.
pub const MOSS: Color = Color::Rgb(0x30, 0xAA, 0x49);
/// Safety orange for failures and uncontained execution.
pub const FLARE: Color = Color::Rgb(0xFF, 0x5C, 0x35);
/// Body text: the website's warm paper.
pub const BONE: Color = Color::Rgb(0xE9, 0xE6, 0xDC);
/// Secondary text.
pub const ASH: Color = Color::Rgb(0xC4, 0xC1, 0xB8);
/// Furthest-back text that is still legible.
pub const SHADOW: Color = Color::Rgb(0x72, 0x71, 0x6B);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Ansi256,
    None,
}

impl ColorDepth {
    pub fn detect() -> Self {
        // NO_COLOR is a request, not a hint. Any value counts, per the spec.
        if std::env::var_os("NO_COLOR").is_some() {
            return Self::None;
        }
        match std::env::var("TERM").as_deref() {
            Ok("dumb") | Ok("") => return Self::None,
            Err(_) => return Self::None,
            _ => {}
        }
        if std::env::var("COLORTERM")
            .is_ok_and(|value| value.contains("truecolor") || value.contains("24bit"))
        {
            return Self::TrueColor;
        }
        Self::Ansi256
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub depth: ColorDepth,

    /// Raised panels and overlays.
    pub surface: Color,
    /// Text drawn on a bright signal strip.
    pub on_accent: Color,
    /// User input.
    pub user: Color,
    /// Agent prose.
    pub assistant: Color,
    /// Model reasoning.
    pub reasoning: Color,
    /// Tool activity.
    pub tool: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    /// Secondary text: hints, counts, timings.
    pub dim: Color,
    /// Brand green, for the logo and accents.
    pub accent: Color,
    /// Code, inline and fenced.
    pub code: Color,
    /// Structure that groups content without being content: the gutter under a
    /// tool call, the composer's rail.
    ///
    /// Deliberately a green rather than a grey, and darker than `dim`. A frame
    /// that competes with the text inside it is worse than no frame, but a grey
    /// one would read as disabled text rather than as structure.
    pub rail: Color,

    pub added: Color,
    pub removed: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(ColorDepth::detect())
    }
}

impl Theme {
    pub fn new(depth: ColorDepth) -> Self {
        match depth {
            ColorDepth::TrueColor => Self {
                depth,
                surface: SURFACE,
                on_accent: INK,
                user: ACID,
                assistant: BONE,
                reasoning: SHADOW,
                tool: MOSS,
                success: LIME,
                error: FLARE,
                warning: VOLT,
                dim: ASH,
                accent: ACID,
                code: CITRON,
                rail: MOSS,
                added: LIME,
                removed: FLARE,
            },

            // Nearest xterm-256 indices to the brand colours. Picked by eye
            // against the hexes above rather than computed, because the nearest
            // index by distance is often not the nearest by appearance.
            ColorDepth::Ansi256 => Self {
                depth,
                surface: Color::Indexed(234),
                on_accent: Color::Indexed(233),
                user: Color::Indexed(154),      // acid green
                assistant: Color::Indexed(254), // warm near-white
                reasoning: Color::Indexed(240),
                tool: Color::Indexed(71), // muted green
                success: Color::Indexed(118),
                error: Color::Indexed(202), // orange-red
                warning: Color::Indexed(227),
                dim: Color::Indexed(244),
                accent: Color::Indexed(154),
                code: Color::Indexed(186),
                rail: Color::Indexed(65), // muted green, darker than dim
                added: Color::Indexed(118),
                removed: Color::Indexed(202),
            },

            // No colour at all. Everything resets, and the interface leans on
            // bold, dim, and the glyphs to carry structure.
            ColorDepth::None => Self {
                depth,
                surface: Color::Reset,
                on_accent: Color::Reset,
                user: Color::Reset,
                assistant: Color::Reset,
                reasoning: Color::Reset,
                tool: Color::Reset,
                success: Color::Reset,
                error: Color::Reset,
                warning: Color::Reset,
                dim: Color::Reset,
                accent: Color::Reset,
                code: Color::Reset,
                // Reset, like everything else. This was MOSS, which emitted a
                // 24-bit escape under NO_COLOR — the one thing this tier
                // promises not to do. The rail is structure, and the glyphs
                // still draw it.
                rail: Color::Reset,
                added: Color::Reset,
                removed: Color::Reset,
            },
        }
    }

    /// A built-in scheme by name, case-insensitively.
    ///
    /// `-`, `_` and ` ` are interchangeable in the name, because a settings file
    /// written by hand will contain all three and none of them is wrong.
    /// Returns `None` for an unknown name rather than silently falling back to
    /// the default: a typo that quietly does nothing is a support ticket.
    pub fn named(name: &str, depth: ColorDepth) -> Option<Self> {
        if same_name("octane", name) {
            return Some(Self::new(depth));
        }
        crate::themes::PALETTES
            .iter()
            .find(|(key, _)| same_name(key, name))
            .map(|(_, palette)| Self::from_palette(palette, depth))
    }

    /// Every built-in theme name, for a picker to offer. `octane` is first.
    pub fn built_in_names() -> &'static [&'static str] {
        crate::themes::BUILT_IN
    }

    /// A theme from a palette description, with the Octane palette underneath
    /// every field the description leaves out.
    pub fn from_palette(palette: &Palette, depth: ColorDepth) -> Self {
        Self::new(depth).with_palette(palette)
    }

    /// Apply a palette's overrides over *this* theme.
    ///
    /// Separate from [`Self::from_palette`] so a config can name a base theme
    /// and adjust it — `Theme::named("nord", depth)?.with_palette(&overrides)` —
    /// without this module knowing that config files exist.
    #[must_use]
    pub fn with_palette(mut self, palette: &Palette) -> Self {
        // A palette cannot resurrect colour the user refused. NO_COLOR is a
        // request about the terminal, not a preference about themes, so a
        // config file must not be able to route around it.
        if self.depth == ColorDepth::None {
            return self;
        }
        macro_rules! apply {
            ($($role:ident),* $(,)?) => {
                $(if let Some(hex) = palette.$role {
                    self.$role = hex.at(self.depth);
                })*
            };
        }
        apply!(
            surface, on_accent, user, assistant, reasoning, tool, success, error,
            warning, dim, accent, code, rail, added, removed,
        );
        self
    }

    /// Secondary text.
    ///
    /// Falls back to the DIM attribute when there is no colour, so the hierarchy
    /// survives on a monochrome terminal instead of flattening.
    pub fn dim(&self) -> Style {
        match self.depth {
            ColorDepth::None => Style::default().add_modifier(Modifier::DIM),
            _ => Style::default().fg(self.dim),
        }
    }

    /// Base style for every cell in the alternate screen.
    ///
    /// The background is **reset**, not painted. Filling the screen with our own
    /// near-black overrode whatever the terminal was set to — it defeated
    /// transparency and blur, ignored the user's own theme, and left a visible
    /// rectangle of not-quite-black wherever our idea of black disagreed with
    /// theirs, which on most terminals it does.
    ///
    /// `Reset` rather than simply leaving the background unset: unset would
    /// inherit whatever a previous frame left in the cell, and a stale
    /// background is the thing this is trying to avoid.
    pub fn canvas(&self) -> Style {
        match self.depth {
            ColorDepth::None => Style::default(),
            _ => Style::default().fg(self.assistant).bg(Color::Reset),
        }
    }

    /// Raised surface used by modal lists.
    ///
    /// This one *does* paint. A picker floats over the transcript, and a modal
    /// you can read the transcript through is not a modal — it is the one place
    /// an explicit background earns its keep.
    pub fn panel(&self) -> Style {
        match self.depth {
            ColorDepth::None => Style::default(),
            _ => Style::default().fg(self.assistant).bg(self.surface),
        }
    }

    /// High-contrast system signal strip.
    ///
    /// Reversed and bold under `NO_COLOR`, preserving the same hierarchy when
    /// foreground and background colours are unavailable.
    pub fn signal(&self, color: Color) -> Style {
        match self.depth {
            ColorDepth::None => Style::default()
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            _ => Style::default()
                .fg(self.on_accent)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        }
    }

    pub fn label(&self, color: Color) -> Style {
        match self.depth {
            ColorDepth::None => Style::default().add_modifier(Modifier::BOLD),
            _ => Style::default().fg(color).add_modifier(Modifier::BOLD),
        }
    }

    /// Reasoning: dim *and* italic, so it is unmistakably the model thinking
    /// rather than something it is telling the user.
    pub fn reasoning(&self) -> Style {
        match self.depth {
            ColorDepth::None => {
                Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC)
            }
            _ => Style::default().fg(self.reasoning).add_modifier(Modifier::ITALIC),
        }
    }

    /// Brand accent, bold.
    pub fn accent(&self) -> Style {
        self.label(self.accent)
    }

    /// Whether colour is being emitted at all.
    pub fn is_colored(&self) -> bool {
        self.depth != ColorDepth::None
    }
}

/// A 24-bit colour, written `"#b8f500"` (or `"b8f500"`) in a config file.
///
/// One value per role rather than one per role *per tier*: asking a user to
/// hand-pick xterm indices is asking them not to write a theme at all, so the
/// 256-colour tier is approximated from the hex by [`Self::at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Hex(pub u8, pub u8, pub u8);

impl Hex {
    /// Parse `#rrggbb` or `rrggbb`. Case-insensitive.
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |range: std::ops::Range<usize>| {
            u8::from_str_radix(digits.get(range)?, 16).ok()
        };
        Some(Self(byte(0..2)?, byte(2..4)?, byte(4..6)?))
    }

    /// This colour as the given tier emits it.
    pub fn at(self, depth: ColorDepth) -> Color {
        match depth {
            ColorDepth::TrueColor => Color::Rgb(self.0, self.1, self.2),
            ColorDepth::Ansi256 => Color::Indexed(self.ansi256()),
            ColorDepth::None => Color::Reset,
        }
    }

    /// Nearest xterm-256 index.
    ///
    /// The 6×6×6 cube and the 24-step grey ramp are both searched and the
    /// closer one wins: a near-grey landing on the cube would be visibly tinted,
    /// and the ramp is four times finer than the cube in that region. The first
    /// 16 indices are excluded on purpose — they are whatever the user's
    /// terminal says they are, so matching against them matches against a guess.
    pub fn ansi256(self) -> u8 {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        fn nearest_level(value: u8) -> usize {
            let mut best = 0;
            let mut best_distance = i32::MAX;
            for (index, level) in LEVELS.iter().enumerate() {
                if distance1(*level, value) < best_distance {
                    best_distance = distance1(*level, value);
                    best = index;
                }
            }
            best
        }
        fn distance1(a: u8, b: u8) -> i32 {
            (i32::from(a) - i32::from(b)).abs()
        }

        let Self(r, g, b) = self;
        let (ri, gi, bi) = (nearest_level(r), nearest_level(g), nearest_level(b));
        let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);

        // The ramp runs 8, 18, .. 238 at indices 232..=255.
        let mean = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
        let step = (mean.saturating_sub(3) / 10).min(23);
        let grey = 8 + step * 10;
        let grey = u8::try_from(grey).unwrap_or(u8::MAX);

        if distance3(self, cube) <= distance3(self, (grey, grey, grey)) {
            u8::try_from(16 + 36 * ri + 6 * gi + bi).unwrap_or(u8::MAX)
        } else {
            u8::try_from(232 + step).unwrap_or(u8::MAX)
        }
    }
}

fn distance3(Hex(r, g, b): Hex, (r2, g2, b2): (u8, u8, u8)) -> i32 {
    let d = |a: u8, b: u8| {
        let d = i32::from(a) - i32::from(b);
        d * d
    };
    // Weighted toward green, which is where the eye resolves difference. Plain
    // Euclidean RGB puts Solarized's green and yellow on the same cube point.
    2 * d(r, r2) + 4 * d(g, g2) + d(b, b2)
}

impl TryFrom<String> for Hex {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
            .ok_or_else(|| format!("expected a hex colour like \"#b8f500\", got {text:?}"))
    }
}

/// A theme as data: every role optional, every omission inherited.
///
/// This is what a user writes in a config file, and also what every built-in
/// scheme in [`crate::themes`] is — there is deliberately no richer internal
/// format, so a hand-written theme can express everything a shipped one can.
/// Unknown fields are rejected: silently ignoring `warnings:` because the field
/// is `warning:` is how a config file lies to its author.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Palette {
    pub surface: Option<Hex>,
    pub on_accent: Option<Hex>,
    pub user: Option<Hex>,
    pub assistant: Option<Hex>,
    pub reasoning: Option<Hex>,
    pub tool: Option<Hex>,
    pub success: Option<Hex>,
    pub error: Option<Hex>,
    pub warning: Option<Hex>,
    pub dim: Option<Hex>,
    pub accent: Option<Hex>,
    pub code: Option<Hex>,
    pub rail: Option<Hex>,
    pub added: Option<Hex>,
    pub removed: Option<Hex>,
}

/// Case-insensitive name match treating `-`, `_` and ` ` as the same character.
fn same_name(key: &str, name: &str) -> bool {
    let normalize = |b: u8| match b {
        b'_' | b' ' => b'-',
        other => other.to_ascii_lowercase(),
    };
    key.len() == name.len()
        && key
            .bytes()
            .zip(name.bytes())
            .all(|(k, n)| normalize(k) == normalize(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_uses_the_brand_hexes() {
        let theme = Theme::new(ColorDepth::TrueColor);
        assert_eq!(theme.accent, ACID);
        assert_eq!(theme.accent, Color::Rgb(0xB8, 0xF5, 0x00));
        assert_eq!(theme.assistant, Color::Rgb(0xE9, 0xE6, 0xDC));
    }

    #[test]
    fn ansi256_stays_indexed() {
        let theme = Theme::new(ColorDepth::Ansi256);
        // An Rgb colour on a 256-colour terminal is emitted as an escape the
        // terminal does not understand, which shows up as literal garbage.
        for color in [
            theme.surface,
            theme.accent,
            theme.error,
            theme.warning,
            theme.dim,
        ] {
            assert!(matches!(color, Color::Indexed(_)), "{color:?} must be indexed");
        }
    }

    #[test]
    fn no_color_emits_no_color() {
        let theme = Theme::new(ColorDepth::None);
        for color in [
            theme.surface,
            theme.user,
            theme.accent,
            theme.error,
            theme.rail,
            theme.added,
            theme.removed,
        ] {
            assert_eq!(color, Color::Reset);
        }
        assert!(!theme.is_colored());
    }

    #[test]
    fn hierarchy_survives_without_color() {
        let theme = Theme::new(ColorDepth::None);
        // Dim and bold still separate the layers when colour cannot.
        assert!(theme.dim().add_modifier.contains(Modifier::DIM));
        assert!(theme.label(Color::Reset).add_modifier.contains(Modifier::BOLD));
        assert!(theme.reasoning().add_modifier.contains(Modifier::ITALIC));
        assert!(
            theme
                .signal(Color::Reset)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn colored_themes_carry_color_not_just_attributes() {
        let theme = Theme::new(ColorDepth::TrueColor);
        assert_eq!(theme.dim().fg, Some(ASH));
        assert_eq!(theme.accent().fg, Some(ACID));
        // Reset, not INK: the terminal's own background shows through, so
        // transparency and the user's theme survive.
        assert_eq!(theme.canvas().bg, Some(Color::Reset));
        assert_eq!(theme.panel().bg, Some(SURFACE));
        assert_eq!(theme.signal(ACID).fg, Some(INK));
        assert_eq!(theme.signal(ACID).bg, Some(ACID));
    }

    #[test]
    fn errors_and_success_are_distinguishable_in_every_tier() {
        for depth in [ColorDepth::TrueColor, ColorDepth::Ansi256] {
            let theme = Theme::new(depth);
            assert_ne!(theme.error, theme.success, "{depth:?}");
            assert_ne!(theme.error, theme.warning, "{depth:?}");
        }
    }

    // -- built-in schemes ---------------------------------------------------

    /// Relative luminance, WCAG 2.x. Only meaningful for `Color::Rgb`, which is
    /// why the contrast checks below run on the truecolor tier.
    fn luminance(color: Color) -> f64 {
        let channel = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        match color {
            Color::Rgb(r, g, b) => {
                0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
            }
            other => panic!("{other:?} is not an rgb colour"),
        }
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (a, b) = (luminance(a), luminance(b));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn every_built_in_name_resolves() {
        for name in Theme::built_in_names() {
            assert!(
                Theme::named(name, ColorDepth::TrueColor).is_some(),
                "{name} is offered by the picker but does not resolve"
            );
        }
        // And the reverse: nothing ships that the picker will not offer.
        for (key, _) in crate::themes::PALETTES {
            assert!(Theme::built_in_names().contains(key), "{key} is not listed");
        }
    }

    #[test]
    fn a_name_matches_however_it_was_typed() {
        for name in ["tokyo-night", "Tokyo Night", "TOKYO_NIGHT"] {
            assert!(Theme::named(name, ColorDepth::TrueColor).is_some(), "{name}");
        }
        // An unknown name is an error the caller can report, not a silent
        // fallback to the default.
        assert!(Theme::named("draculaa", ColorDepth::TrueColor).is_none());
        assert!(Theme::named("", ColorDepth::TrueColor).is_none());
    }

    #[test]
    fn every_built_in_answers_all_three_depths() {
        for name in Theme::built_in_names() {
            let truecolor = Theme::named(name, ColorDepth::TrueColor).expect(name);
            let indexed = Theme::named(name, ColorDepth::Ansi256).expect(name);
            let plain = Theme::named(name, ColorDepth::None).expect(name);
            for role in roles(&truecolor) {
                assert!(matches!(role, Color::Rgb(..)), "{name}: {role:?}");
            }
            for role in roles(&indexed) {
                // An Rgb escape on a 256-colour terminal prints as garbage.
                assert!(matches!(role, Color::Indexed(_)), "{name}: {role:?}");
            }
            for role in roles(&plain) {
                assert_eq!(role, Color::Reset, "{name}");
            }
        }
    }

    fn roles(theme: &Theme) -> [Color; 15] {
        [
            theme.surface,
            theme.on_accent,
            theme.user,
            theme.assistant,
            theme.reasoning,
            theme.tool,
            theme.success,
            theme.error,
            theme.warning,
            theme.dim,
            theme.accent,
            theme.code,
            theme.rail,
            theme.added,
            theme.removed,
        ]
    }

    #[test]
    fn no_built_in_confuses_failure_with_success() {
        for name in Theme::built_in_names() {
            for depth in [ColorDepth::TrueColor, ColorDepth::Ansi256] {
                let theme = Theme::named(name, depth).expect(name);
                assert_ne!(theme.error, theme.success, "{name} at {depth:?}");
                assert_ne!(theme.error, theme.warning, "{name} at {depth:?}");
                assert_ne!(theme.removed, theme.added, "{name} at {depth:?}");
            }
        }
    }

    #[test]
    fn every_built_in_stays_legible_on_its_own_surface() {
        for name in Theme::built_in_names() {
            let theme = Theme::named(name, ColorDepth::TrueColor).expect(name);
            // The panel background is the only one we paint, so it is the only
            // one we can check. `reasoning` and `rail` are excluded: they are
            // meant to recede, and they carry italics and glyphs respectively.
            assert!(
                contrast(theme.assistant, theme.surface) >= 4.0,
                "{name}: body text on the panel is {:.2}:1",
                contrast(theme.assistant, theme.surface)
            );
            assert!(
                contrast(theme.dim, theme.surface) >= 3.0,
                "{name}: dim text on the panel is {:.2}:1",
                contrast(theme.dim, theme.surface)
            );
            // The signal strip paints `accent` and writes `on_accent` on it.
            assert!(
                contrast(theme.on_accent, theme.accent) >= 3.0,
                "{name}: the signal strip is {:.2}:1",
                contrast(theme.on_accent, theme.accent)
            );
        }
    }

    #[test]
    fn a_light_scheme_recedes_upward_not_downward() {
        // The assumption a light theme exists to catch: `rail` and `reasoning`
        // are described as *darker* than the text, which is only true on a dark
        // background. On Solarized Light they must be lighter, or the structure
        // shouts and the body text whispers.
        let light = Theme::named("solarized-light", ColorDepth::TrueColor).expect("light");
        assert!(luminance(light.rail) > luminance(light.assistant));
        assert!(luminance(light.reasoning) > luminance(light.assistant));
        assert!(luminance(light.surface) > luminance(light.dim));

        let dark = Theme::named("nord", ColorDepth::TrueColor).expect("nord");
        assert!(luminance(dark.rail) < luminance(dark.assistant));
    }

    // -- custom palettes ----------------------------------------------------

    #[test]
    fn a_palette_naming_three_colors_changes_three_colors() {
        let palette: Palette = serde_json::from_str(
            r##"{ "accent": "#ff00ff", "error": "0000ff", "rail": "#123456" }"##,
        )
        .expect("parses");
        let theme = Theme::from_palette(&palette, ColorDepth::TrueColor);
        assert_eq!(theme.accent, Color::Rgb(0xFF, 0x00, 0xFF));
        assert_eq!(theme.error, Color::Rgb(0x00, 0x00, 0xFF));
        assert_eq!(theme.rail, Color::Rgb(0x12, 0x34, 0x56));
        // Everything unnamed is still Octane.
        assert_eq!(theme.assistant, BONE);
        assert_eq!(theme.success, LIME);
        assert_eq!(theme.surface, SURFACE);
    }

    #[test]
    fn a_palette_can_be_layered_over_a_named_theme() {
        let palette = Palette { accent: Hex::parse("#ff0000"), ..Palette::default() };
        let theme = Theme::named("nord", ColorDepth::TrueColor)
            .expect("nord")
            .with_palette(&palette);
        assert_eq!(theme.accent, Color::Rgb(0xFF, 0, 0));
        assert_eq!(theme.assistant, Color::Rgb(0xD8, 0xDE, 0xE9), "nord's foreground");
    }

    #[test]
    fn a_custom_palette_is_still_indexed_on_a_256_color_terminal() {
        let palette = Palette { accent: Hex::parse("#b8f500"), ..Palette::default() };
        let theme = Theme::from_palette(&palette, ColorDepth::Ansi256);
        assert!(matches!(theme.accent, Color::Indexed(_)), "{:?}", theme.accent);
    }

    #[test]
    fn a_custom_palette_cannot_override_no_color() {
        let palette = Palette { accent: Hex::parse("#b8f500"), ..Palette::default() };
        let theme = Theme::from_palette(&palette, ColorDepth::None);
        assert_eq!(theme.accent, Color::Reset);
    }

    #[test]
    fn a_misspelled_role_is_reported_rather_than_ignored() {
        assert!(serde_json::from_str::<Palette>(r##"{ "warnings": "#ff0000" }"##).is_err());
        assert!(serde_json::from_str::<Palette>(r#"{ "accent": "octarine" }"#).is_err());
        assert!(serde_json::from_str::<Palette>(r##"{ "accent": "#fff" }"##).is_err());
        assert_eq!(serde_json::from_str::<Palette>("{}").expect("empty"), Palette::default());
    }

    #[test]
    fn on_accent_is_camel_case_in_config() {
        let palette: Palette =
            serde_json::from_str(r##"{ "onAccent": "#101010" }"##).expect("parses");
        assert_eq!(palette.on_accent, Some(Hex(0x10, 0x10, 0x10)));
    }

    #[test]
    fn hex_approximation_lands_on_the_expected_index() {
        assert_eq!(Hex(0x00, 0x00, 0x00).ansi256(), 16);
        assert_eq!(Hex(0xFF, 0xFF, 0xFF).ansi256(), 231);
        assert_eq!(Hex(0xFF, 0x00, 0x00).ansi256(), 196);
        // A near-grey takes the ramp, which is finer there than the cube.
        assert_eq!(Hex(0x80, 0x80, 0x80).ansi256(), 244);
        // ... and a saturated colour does not.
        assert!(Hex(0xB8, 0xF5, 0x00).ansi256() < 232);
    }
}
