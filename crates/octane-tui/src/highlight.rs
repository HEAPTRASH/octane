//! Syntax highlighting for fenced code.
//!
//! syntect rather than tree-sitter, deliberately. Tree-sitter's two assets are
//! a real AST and incremental reparse, and this renders short immutable blocks
//! once: there is no buffer to edit, so nothing to be incremental about. It is
//! also the wrong shape for the input — a model emits fragments, a function
//! body with no enclosing item, a shell transcript with a prompt in it, and a
//! parser wants a whole file. syntect's line-oriented lexing degrades into
//! plausible colours where a grammar degrades into one ERROR node.
//!
//! The cost decided it: twelve tree-sitter grammars measured at +7.26 MB on a
//! 7.8 MB binary, against ~2 MB for syntect covering far more languages with
//! no C toolchain. Tree-sitter earns its place the day something wants
//! semantic chunking or jump-to-definition, and highlighting comes free with
//! it then.
//!
//! Scopes map onto [`crate::theme::Theme`] rather than a syntect theme, so all
//! three colour tiers keep working: under `ColorDepth::None` every colour is
//! already `Reset`, and highlighting becomes invisible rather than wrong.

use std::sync::OnceLock;

use ratatui::style::Style;
use ratatui::text::Span;
use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

use crate::theme::{ColorDepth, Theme};

fn syntaxes() -> &'static SyntaxSet {
    // Loaded once. Decompressing the bundle is a few milliseconds and would
    // otherwise be paid per code block.
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_no_newlines)
}

/// Highlighter for one fenced block.
///
/// Held across the block's lines because a lexer is stateful: a string opened
/// on one line and closed on the next is only correct if the parse carries
/// over. Highlighting each line independently mis-colours every multi-line
/// construct.
#[derive(Debug)]
pub struct Highlighter {
    state: ParseState,
    stack: ScopeStack,
}

impl Highlighter {
    /// A highlighter for `language`, or `None` when it is unknown or colour is
    /// off.
    ///
    /// Returning `None` rather than a no-op highlighter keeps the caller on its
    /// existing plain path, which is the one that is already tested.
    pub fn new(language: &str, theme: &Theme) -> Option<Self> {
        if theme.depth == ColorDepth::None {
            return None;
        }
        let set = syntaxes();
        let token = language.trim();
        if token.is_empty() {
            return None;
        }
        let syntax = set
            .find_syntax_by_token(token)
            .or_else(|| set.find_syntax_by_extension(token))?;
        Some(Self { state: ParseState::new(syntax), stack: ScopeStack::new() })
    }

    /// Style one line, advancing the parse.
    ///
    /// Returns one span per styled run. The text is never altered, only split:
    /// highlighting must not change what a line says or how wide it is.
    pub fn line(&mut self, text: &str, theme: &Theme) -> Vec<Span<'static>> {
        let set = syntaxes();
        let Ok(ops) = self.state.parse_line(text, set) else {
            return vec![Span::styled(text.to_string(), Style::default().fg(theme.code))];
        };

        let mut spans = Vec::new();
        let mut cursor = 0usize;

        for (offset, op) in ops {
            if offset > cursor {
                let piece = &text[cursor..offset];
                spans.push(Span::styled(piece.to_string(), self.style(theme)));
                cursor = offset;
            }
            // A failed op would desynchronise the stack from the parse, so the
            // rest of the block is better left plain than mis-coloured.
            if self.stack.apply(&op).is_err() {
                spans.push(Span::styled(
                    text[cursor..].to_string(),
                    Style::default().fg(theme.code),
                ));
                return spans;
            }
        }

        if cursor < text.len() {
            spans.push(Span::styled(text[cursor..].to_string(), self.style(theme)));
        }
        if spans.is_empty() {
            spans.push(Span::styled(text.to_string(), Style::default().fg(theme.code)));
        }
        spans
    }

    /// The colour for the innermost scope currently open.
    fn style(&self, theme: &Theme) -> Style {
        let name = self
            .stack
            .scopes
            .last()
            .map(|scope| scope.build_string())
            .unwrap_or_default();
        Style::default().fg(colour_for(&name, theme))
    }
}

/// Map a TextMate scope onto the palette octane already has.
///
/// Reusing the semantic colours rather than adding a syntax palette is what
/// makes the colour tiers work without a second implementation: they are
/// already chosen per tier and already collapse to `Reset` under `NO_COLOR`.
fn colour_for(scope: &str, theme: &Theme) -> ratatui::style::Color {
    // Longest-prefix first: `constant.numeric` must not be caught by a rule
    // for `constant` if both are listed.
    const TABLE: &[(&str, u8)] = &[
        ("comment", 0),
        ("string", 1),
        ("constant", 2),
        ("keyword", 3),
        ("storage", 3),
        ("entity.name.function", 4),
        ("support.function", 4),
        ("entity.name", 5),
        ("support.type", 5),
        ("support.class", 5),
        ("variable", 6),
        ("punctuation", 0),
    ];

    for (prefix, slot) in TABLE {
        if scope.starts_with(prefix) {
            return match slot {
                0 => theme.dim,
                1 => theme.success,
                2 => theme.warning,
                3 => theme.accent,
                4 => theme.tool,
                5 => theme.assistant,
                _ => theme.code,
            };
        }
    }
    theme.code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::new(ColorDepth::TrueColor)
    }

    fn text_of(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn highlighting_never_changes_the_text() {
        // It splits a line into styled runs and must not add, drop or reorder a
        // single character: this is what the user copies.
        let source = "fn main() { let x = \"hi\"; // note\n}";
        let mut highlighter = Highlighter::new("rust", &theme()).expect("rust is known");
        for line in source.lines() {
            let spans = highlighter.line(line, &theme());
            assert_eq!(text_of(&spans), line, "line was altered");
        }
    }

    #[test]
    fn a_string_spanning_lines_stays_a_string() {
        // The reason a highlighter is held across a block rather than built per
        // line: a lexer is stateful, and per-line parsing mis-colours every
        // multi-line construct.
        let mut highlighter = Highlighter::new("rust", &theme()).unwrap();
        let first = highlighter.line("let s = \"open", &theme());
        let second = highlighter.line("still inside\";", &theme());

        assert_eq!(text_of(&first), "let s = \"open");
        assert_eq!(text_of(&second), "still inside\";");
        assert!(
            second.iter().any(|s| s.style.fg == Some(theme().success)),
            "the continuation should still be inside the string"
        );
    }

    #[test]
    fn an_unknown_language_declines_rather_than_guessing() {
        // Falling back to a default grammar would colour prose as if it were
        // code, which is worse than leaving it alone.
        assert!(Highlighter::new("", &theme()).is_none());
        assert!(Highlighter::new("not-a-language-at-all", &theme()).is_none());
    }

    #[test]
    fn colour_is_declined_entirely_when_there_is_none() {
        // Under NO_COLOR every colour is already Reset, so highlighting would
        // be pure cost for an identical screen.
        let plain = Theme::new(ColorDepth::None);
        assert!(Highlighter::new("rust", &plain).is_none());
    }

    #[test]
    fn typescript_is_available() {
        // syntect's own syntax set does not ship it, which is why the bundle is
        // worth its size for a coding agent.
        assert!(Highlighter::new("typescript", &theme()).is_some());
        assert!(Highlighter::new("tsx", &theme()).is_some());
    }

    #[test]
    fn a_scope_maps_to_a_colour_from_the_existing_palette() {
        // Reusing the semantic colours is what makes the three colour tiers
        // work without a second implementation.
        let theme = theme();
        assert_eq!(colour_for("comment.line.double-slash.rust", &theme), theme.dim);
        assert_eq!(colour_for("string.quoted.double.rust", &theme), theme.success);
        assert_eq!(colour_for("keyword.control.rust", &theme), theme.accent);
        assert_eq!(colour_for("something.unmapped", &theme), theme.code);
    }
}
