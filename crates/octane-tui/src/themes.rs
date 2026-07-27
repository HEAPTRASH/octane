//! Built-in colour schemes.
//!
//! Each entry is a [`Palette`] — the same type a user's config file
//! deserialises into — so a built-in scheme has no capability a hand-written
//! one lacks. There is no second, privileged theme format.
//!
//! # Mapping a scheme onto the roles
//!
//! These schemes were designed for syntax highlighting, not for an agent's
//! transcript, so none of them ships a colour called `reasoning`. The mapping
//! below is by *role*, and the same three rules were applied to every scheme:
//!
//! - **Status colours are literal.** `error`/`removed` take the scheme's red,
//!   `success`/`added` its green, `warning` its yellow or orange. A scheme
//!   whose author picked a pink for red gets a pink error — but never a red
//!   `success`, because a transcript where a failure and a success read alike
//!   is worse than one that is off-brand.
//! - **`accent` is the scheme's own signature hue**, and `user` follows it, as
//!   in the Octane palette where both are the acid green. The signature hue is
//!   skipped when it collides with a status colour: Monokai's identity is its
//!   pink, but that pink is also its red, so the accent falls to cyan.
//! - **`dim` must stay legible, `reasoning` and `rail` must not compete.** Most
//!   schemes' "comment" grey is too dark to carry hints, so `reasoning` takes
//!   the comment colour and `dim` takes a lighter secondary text tone. `rail`
//!   is structure: the lowest-contrast tone that is still visible, which on a
//!   light scheme means *lighter* than the text, not darker.
//!
//! Ansi256 tiers are derived from these hexes by nearest-index approximation
//! (see [`Theme::from_palette`](crate::theme::Theme::from_palette)); only the
//! Octane palette hand-picks its indices, because it is the default and worth
//! the eye time.

use crate::theme::Palette;

/// Every built-in scheme, keyed by the name a user writes in config.
///
/// The Octane palette is *not* here: it is the fallback every palette resolves
/// against, so [`Theme::named`](crate::theme::Theme::named) answers it directly
/// from its hand-tuned indices. [`BUILT_IN`] is the list to show a picker.
pub const PALETTES: &[(&str, Palette)] = &[
    ("dracula", DRACULA),
    ("gruvbox-dark", GRUVBOX_DARK),
    ("nord", NORD),
    ("solarized-dark", SOLARIZED_DARK),
    ("solarized-light", SOLARIZED_LIGHT),
    ("catppuccin-mocha", CATPPUCCIN_MOCHA),
    ("tokyo-night", TOKYO_NIGHT),
    ("one-dark", ONE_DARK),
    ("monokai", MONOKAI),
    ("high-contrast", HIGH_CONTRAST),
];

/// Every built-in theme name, for a settings picker to offer.
///
/// `octane` first because it is the default; the rest alphabetical, with the
/// accessibility scheme last so it reads as the deliberate choice it is.
pub const BUILT_IN: &[&str] = &[
    "octane",
    "catppuccin-mocha",
    "dracula",
    "gruvbox-dark",
    "monokai",
    "nord",
    "one-dark",
    "solarized-dark",
    "solarized-light",
    "tokyo-night",
    "high-contrast",
];

/// Whether a built-in scheme expects a light terminal background.
///
/// Exposed because the caller wiring config may want to warn on a light scheme
/// under a dark terminal, and because it documents that exactly one built-in
/// tests the light path.
pub fn is_light(name: &str) -> bool {
    name.eq_ignore_ascii_case("solarized-light")
}

const fn hex(value: u32) -> Option<crate::theme::Hex> {
    Some(crate::theme::Hex(
        (value >> 16) as u8,
        (value >> 8) as u8,
        value as u8,
    ))
}

/// Dracula. Purple is the identity hue; the pink is louder but is spent on
/// `code`, where it marks spans rather than colouring a whole line.
const DRACULA: Palette = Palette {
    surface: hex(0x44475a),
    on_accent: hex(0x282a36),
    user: hex(0xbd93f9),
    assistant: hex(0xf8f8f2),
    reasoning: hex(0x6272a4),
    tool: hex(0x8be9fd),
    success: hex(0x50fa7b),
    error: hex(0xff5555),
    warning: hex(0xffb86c),
    // Dracula names no secondary text tone: its comment blue is too dark to
    // carry hints, so this is that blue lifted toward the foreground.
    dim: hex(0xa5abc4),
    accent: hex(0xbd93f9),
    code: hex(0xff79c6),
    rail: hex(0x6272a4),
    added: hex(0x50fa7b),
    removed: hex(0xff5555),
};

/// Gruvbox dark (medium). Orange over yellow for the accent: yellow is spoken
/// for by `warning`, and gruvbox's orange is the hue people picture.
const GRUVBOX_DARK: Palette = Palette {
    surface: hex(0x3c3836),
    on_accent: hex(0x282828),
    user: hex(0xfe8019),
    assistant: hex(0xebdbb2),
    reasoning: hex(0x928374),
    tool: hex(0x8ec07c),
    success: hex(0xb8bb26),
    error: hex(0xfb4934),
    warning: hex(0xfabd2f),
    dim: hex(0xa89984),
    accent: hex(0xfe8019),
    code: hex(0xd3869b),
    rail: hex(0x665c54),
    added: hex(0xb8bb26),
    removed: hex(0xfb4934),
};

/// Nord. Frost blue is the signature; aurora carries every status colour.
const NORD: Palette = Palette {
    surface: hex(0x3b4252),
    on_accent: hex(0x2e3440),
    user: hex(0x88c0d0),
    assistant: hex(0xd8dee9),
    // nord3 (#4c566a) is Nord's comment colour and is barely visible on
    // nord1; reasoning is italic as well, so it can afford this, but `dim`
    // cannot — hence the lifted tone below.
    reasoning: hex(0x6f7c96),
    tool: hex(0x8fbcbb),
    success: hex(0xa3be8c),
    error: hex(0xbf616a),
    warning: hex(0xebcb8b),
    dim: hex(0xa7b0c0),
    accent: hex(0x88c0d0),
    code: hex(0xb48ead),
    rail: hex(0x4c566a),
    added: hex(0xa3be8c),
    removed: hex(0xbf616a),
};

/// Solarized Dark. The scheme is built as a luminance ladder, so the roles map
/// almost mechanically: base01 structure, base00 secondary, base1 body.
const SOLARIZED_DARK: Palette = Palette {
    surface: hex(0x073642),
    on_accent: hex(0x002b36),
    user: hex(0x268bd2),
    assistant: hex(0x93a1a1),
    reasoning: hex(0x586e75),
    tool: hex(0x2aa198),
    success: hex(0x859900),
    error: hex(0xdc322f),
    warning: hex(0xb58900),
    dim: hex(0x839496),
    accent: hex(0x268bd2),
    code: hex(0xd33682),
    rail: hex(0x586e75),
    added: hex(0x859900),
    removed: hex(0xdc322f),
};

/// Solarized Light — the one built-in that assumes a light terminal.
///
/// Solarized inverts its ladder rather than its hues, and so does this mapping:
/// body text is base01, secondary is base00, and `rail` moves *up* to base1.
/// "Receded" means low contrast against the background, which on a light
/// background is lighter — the assumption `dim`-is-darker is exactly what a
/// light scheme exists to catch.
const SOLARIZED_LIGHT: Palette = Palette {
    surface: hex(0xeee8d5),
    // Paper, not ink: the signal strip paints its background with `accent`
    // (blue in every tier), so its text stays light even here.
    on_accent: hex(0xfdf6e3),
    user: hex(0x268bd2),
    assistant: hex(0x586e75),
    reasoning: hex(0x93a1a1),
    tool: hex(0x2aa198),
    success: hex(0x859900),
    error: hex(0xdc322f),
    // Solarized's yellow, not its orange: orange sits close enough to the red
    // to blur warning into error on a bright background.
    warning: hex(0xb58900),
    dim: hex(0x657b83),
    accent: hex(0x268bd2),
    code: hex(0xd33682),
    rail: hex(0x93a1a1),
    added: hex(0x859900),
    removed: hex(0xdc322f),
};

/// Catppuccin Mocha. Mauve is the flavour's accent by its own spec.
const CATPPUCCIN_MOCHA: Palette = Palette {
    surface: hex(0x313244),
    on_accent: hex(0x1e1e2e),
    user: hex(0xcba6f7),
    assistant: hex(0xcdd6f4),
    reasoning: hex(0x6c7086),
    tool: hex(0x89b4fa),
    success: hex(0xa6e3a1),
    error: hex(0xf38ba8),
    warning: hex(0xf9e2af),
    dim: hex(0xa6adc8),
    accent: hex(0xcba6f7),
    code: hex(0xf5c2e7),
    rail: hex(0x45475a),
    added: hex(0xa6e3a1),
    removed: hex(0xf38ba8),
};

/// Tokyo Night (storm-adjacent dark). Blue is the signature; magenta is spent
/// on `code`.
const TOKYO_NIGHT: Palette = Palette {
    surface: hex(0x292e42),
    on_accent: hex(0x1a1b26),
    user: hex(0x7aa2f7),
    assistant: hex(0xc0caf5),
    reasoning: hex(0x565f89),
    tool: hex(0x7dcfff),
    success: hex(0x9ece6a),
    error: hex(0xf7768e),
    warning: hex(0xe0af68),
    dim: hex(0xa9b1d6),
    accent: hex(0x7aa2f7),
    code: hex(0xbb9af7),
    rail: hex(0x414868),
    added: hex(0x9ece6a),
    removed: hex(0xf7768e),
};

/// One Dark (Atom). Blue accent, magenta for code spans.
const ONE_DARK: Palette = Palette {
    surface: hex(0x3e4451),
    on_accent: hex(0x282c34),
    user: hex(0x61afef),
    assistant: hex(0xabb2bf),
    reasoning: hex(0x5c6370),
    tool: hex(0x56b6c2),
    success: hex(0x98c379),
    error: hex(0xe06c75),
    warning: hex(0xe5c07b),
    // Between One Dark's foreground and its comment grey: the comment grey
    // itself falls under 3:1 on the panel surface.
    dim: hex(0x9aa2b0),
    accent: hex(0x61afef),
    code: hex(0xc678dd),
    rail: hex(0x4b5263),
    added: hex(0x98c379),
    removed: hex(0xe06c75),
};

/// Monokai. The identity hue is the pink — but the pink *is* Monokai's red, and
/// a logo the same colour as every failure trains people to ignore failures. So
/// the pink stays on `error`/`removed` and the accent falls to the cyan.
const MONOKAI: Palette = Palette {
    surface: hex(0x3e3d32),
    on_accent: hex(0x272822),
    user: hex(0x66d9ef),
    assistant: hex(0xf8f8f2),
    reasoning: hex(0x75715e),
    tool: hex(0xae81ff),
    success: hex(0xa6e22e),
    error: hex(0xf92672),
    warning: hex(0xe6db74),
    dim: hex(0xa59f85),
    accent: hex(0x66d9ef),
    code: hex(0xfd971f),
    rail: hex(0x49483e),
    added: hex(0xa6e22e),
    removed: hex(0xf92672),
};

/// High contrast on black, in the spirit of the platform high-contrast themes.
///
/// The palette is deliberately *small*: pure hues at full luminance are the
/// only ones that clear a high contrast ratio on black, and there are about six
/// of them. Roles that never appear in the same glance therefore share a hue
/// (`code` with `user`, `warning` with nothing) rather than being pushed to
/// muddy in-between colours that would defeat the point of the scheme. Status
/// colours stay mutually distinct, which is the one thing that may not give.
const HIGH_CONTRAST: Palette = Palette {
    surface: hex(0x000000),
    on_accent: hex(0x000000),
    user: hex(0x00ffff),
    assistant: hex(0xffffff),
    // Still ~10:1 on black. "Receded" here means a step down from white, not
    // the near-invisible grey a normal scheme can afford.
    reasoning: hex(0xc0c0c0),
    tool: hex(0xff00ff),
    success: hex(0x00ff00),
    // Pure #ff0000 is only ~5:1 on black and the weakest thing on screen;
    // lightening it buys contrast without reading as anything but red.
    error: hex(0xff5555),
    warning: hex(0xffaa00),
    dim: hex(0xe0e0e0),
    accent: hex(0xffff00),
    code: hex(0x00ffff),
    rail: hex(0x808080),
    added: hex(0x00ff00),
    removed: hex(0xff5555),
};
