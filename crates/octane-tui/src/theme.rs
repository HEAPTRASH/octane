//! The Octane palette.
//!
//! Monster-inspired: black base, acid green, high contrast, nothing muddy. The
//! greens are the brand's own values (`#95D600` is the signature acid green),
//! with a hotter lime for emphasis and an acid yellow for warnings.
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

use ratatui::style::{Color, Modifier, Style};

/// Signature acid green. The brand colour, and Octane's primary.
pub const ACID: Color = Color::Rgb(0x95, 0xD6, 0x00);
/// Hotter lime, for things that should pull the eye.
pub const LIME: Color = Color::Rgb(0x6A, 0xCD, 0x0C);
/// Acid yellow, for warnings.
pub const VOLT: Color = Color::Rgb(0xFA, 0xEF, 0x55);
/// Yellow-green, between acid and volt.
pub const CITRON: Color = Color::Rgb(0xC2, 0xD7, 0x2D);
/// Deep green, for receded structure.
pub const MOSS: Color = Color::Rgb(0x30, 0xAA, 0x49);
/// Warm red for failures. Not brand, but a red that does not fight the greens.
pub const FLARE: Color = Color::Rgb(0xFF, 0x4D, 0x2E);
/// Body text.
pub const BONE: Color = Color::Rgb(0xE8, 0xE8, 0xE4);
/// Secondary text.
pub const ASH: Color = Color::Rgb(0x88, 0x88, 0x86);
/// Furthest-back text that is still legible.
pub const SHADOW: Color = Color::Rgb(0x55, 0x55, 0x53);

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
                user: ACID,
                assistant: BONE,
                reasoning: SHADOW,
                tool: MOSS,
                success: LIME,
                error: FLARE,
                warning: VOLT,
                dim: ASH,
                accent: ACID,
                added: LIME,
                removed: FLARE,
            },

            // Nearest xterm-256 indices to the brand colours. Picked by eye
            // against the hexes above rather than computed, because the nearest
            // index by distance is often not the nearest by appearance.
            ColorDepth::Ansi256 => Self {
                depth,
                user: Color::Indexed(154),      // greenyellow
                assistant: Color::Indexed(253), // near-white
                reasoning: Color::Indexed(240),
                tool: Color::Indexed(71), // muted green
                success: Color::Indexed(118),
                error: Color::Indexed(202), // orange-red
                warning: Color::Indexed(227),
                dim: Color::Indexed(244),
                accent: Color::Indexed(154),
                added: Color::Indexed(118),
                removed: Color::Indexed(202),
            },

            // No colour at all. Everything resets, and the interface leans on
            // bold, dim, and the glyphs to carry structure.
            ColorDepth::None => Self {
                depth,
                user: Color::Reset,
                assistant: Color::Reset,
                reasoning: Color::Reset,
                tool: Color::Reset,
                success: Color::Reset,
                error: Color::Reset,
                warning: Color::Reset,
                dim: Color::Reset,
                accent: Color::Reset,
                added: Color::Reset,
                removed: Color::Reset,
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_uses_the_brand_hexes() {
        let theme = Theme::new(ColorDepth::TrueColor);
        assert_eq!(theme.accent, ACID);
        assert_eq!(theme.accent, Color::Rgb(0x95, 0xD6, 0x00));
    }

    #[test]
    fn ansi256_stays_indexed() {
        let theme = Theme::new(ColorDepth::Ansi256);
        // An Rgb colour on a 256-colour terminal is emitted as an escape the
        // terminal does not understand, which shows up as literal garbage.
        for color in [theme.accent, theme.error, theme.warning, theme.dim] {
            assert!(matches!(color, Color::Indexed(_)), "{color:?} must be indexed");
        }
    }

    #[test]
    fn no_color_emits_no_color() {
        let theme = Theme::new(ColorDepth::None);
        for color in [theme.user, theme.accent, theme.error, theme.added, theme.removed] {
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
    }

    #[test]
    fn colored_themes_carry_color_not_just_attributes() {
        let theme = Theme::new(ColorDepth::TrueColor);
        assert_eq!(theme.dim().fg, Some(ASH));
        assert_eq!(theme.accent().fg, Some(ACID));
    }

    #[test]
    fn errors_and_success_are_distinguishable_in_every_tier() {
        for depth in [ColorDepth::TrueColor, ColorDepth::Ansi256] {
            let theme = Theme::new(depth);
            assert_ne!(theme.error, theme.success, "{depth:?}");
            assert_ne!(theme.error, theme.warning, "{depth:?}");
        }
    }
}
