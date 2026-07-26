//! Markdown rendering.
//!
//! Models answer in markdown whether or not you ask them to, so a transcript
//! that shows the raw source is showing the wrong thing. This is not a CommonMark
//! implementation and does not try to be — a terminal has no images, no tables
//! worth the effort, and no nested inline HTML. It handles what models actually
//! emit.
//!
//! Two rules shape it:
//!
//! **Never lose text.** A construct this does not understand is rendered as
//! written rather than dropped. Losing a line to a parser edge case is far worse
//! than showing one asterisk.
//!
//! **Code is verbatim.** Inside a fence, nothing is interpreted — no bold, no
//! headings, no list bullets. Markdown inside a code block is code, and a
//! renderer that styles it corrupts what the user is about to copy.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::glyphs::Glyphs;
use crate::theme::Theme;

/// Render markdown as styled lines.
pub fn render(source: &str, theme: &Theme, glyphs: &Glyphs) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut fence: Option<String> = None;
    // Held for the whole block, not per line: a lexer is stateful, and a string
    // opened on one line and closed on the next only reads correctly if the
    // parse carries over.
    let mut highlighter: Option<crate::highlight::Highlighter> = None;

    for raw in source.split('\n') {
        // Fences first: everything else is suppressed inside one.
        if let Some(rest) = fence_marker(raw) {
            match &fence {
                Some(_) => {
                    out.push(code_edge(glyphs, theme, false));
                    fence = None;
                    highlighter = None;
                }
                None => {
                    let language = rest.trim().to_string();
                    out.push(code_header(&language, glyphs, theme));
                    highlighter = crate::highlight::Highlighter::new(&language, theme);
                    fence = Some(language);
                }
            }
            continue;
        }

        if fence.is_some() {
            out.push(code_line(raw, glyphs, theme, highlighter.as_mut()));
            continue;
        }

        out.push(block(raw, theme, glyphs));
    }

    // An unterminated fence is common in a truncated response; close it so the
    // block does not visually run into whatever comes next.
    if fence.is_some() {
        out.push(code_edge(glyphs, theme, false));
    }
    out
}

/// The ``` or ~~~ marker, returning the info string after it.
fn fence_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("```")
        .or_else(|| trimmed.strip_prefix("~~~"))
}

fn code_header(language: &str, glyphs: &Glyphs, theme: &Theme) -> Line<'static> {
    let label = if language.is_empty() { String::new() } else { format!(" {language}") };
    Line::from(vec![
        Span::styled(format!("{}{}", glyphs.rule(2), label), theme.dim()),
    ])
}

fn code_edge(glyphs: &Glyphs, theme: &Theme, _open: bool) -> Line<'static> {
    Line::styled(glyphs.rule(2).to_string(), theme.dim())
}

/// A line inside a fence: verbatim, with a margin so it reads as a block.
fn code_line(
    text: &str,
    glyphs: &Glyphs,
    theme: &Theme,
    highlighter: Option<&mut crate::highlight::Highlighter>,
) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("{} ", glyphs.code_bar()), theme.dim())];
    match highlighter {
        // Styled but never altered: the characters are what the user copies,
        // and highlighting only splits them into runs.
        Some(highlighter) => spans.extend(highlighter.line(text, theme)),
        None => spans.push(Span::styled(text.to_string(), Style::default().fg(theme.code))),
    }
    Line::from(spans)
}

/// One non-code line.
fn block(raw: &str, theme: &Theme, glyphs: &Glyphs) -> Line<'static> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();
    let pad = " ".repeat(indent.min(8));

    // Horizontal rule.
    if matches!(trimmed, "---" | "***" | "___" | "- - -") {
        return Line::styled(glyphs.rule(40), theme.dim());
    }

    // Heading.
    if let Some(level) = heading_level(trimmed) {
        let text = trimmed[level..].trim_start();
        let style = if level <= 2 {
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        return Line::from(inline(text, style, theme));
    }

    // Block quote.
    if let Some(rest) = trimmed.strip_prefix("> ").or_else(|| trimmed.strip_prefix(">")) {
        let mut spans = vec![Span::styled(format!("{} ", glyphs.code_bar()), theme.dim())];
        spans.extend(inline(rest, theme.dim(), theme));
        return Line::from(spans);
    }

    // Bullet list. The marker is replaced with a real bullet so nesting reads.
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        let mut spans = vec![
            Span::raw(pad),
            Span::styled(format!("{} ", glyphs.bullet), Style::default().fg(theme.accent)),
        ];
        spans.extend(inline(rest, Style::default(), theme));
        return Line::from(spans);
    }

    // Ordered list: the number is kept, since it carries meaning.
    if let Some((marker, rest)) = ordered_marker(trimmed) {
        let mut spans = vec![
            Span::raw(pad),
            Span::styled(format!("{marker} "), Style::default().fg(theme.accent)),
        ];
        spans.extend(inline(rest, Style::default(), theme));
        return Line::from(spans);
    }

    let mut spans = vec![Span::raw(pad)];
    spans.extend(inline(trimmed, Style::default(), theme));
    Line::from(spans)
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    // A `#` must be followed by a space to be a heading; `#4` is not one.
    ((1..=6).contains(&hashes) && line.chars().nth(hashes) == Some(' ')).then_some(hashes)
}

/// `1. ` / `12) ` and the text after it.
fn ordered_marker(line: &str) -> Option<(&str, &str)> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &line[digits..];
    let rest = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))?;
    Some((&line[..digits], rest))
}

/// Inline spans: bold, italic, and code.
///
/// Written as a single pass rather than nested parsers because the interesting
/// case is unbalanced markers — a lone `*` in prose, or a `**` the model never
/// closed. A pass that emits what it cannot match keeps the text.
fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;

    let flush = |buffer: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buffer.is_empty() {
            spans.push(Span::styled(std::mem::take(buffer), base));
        }
    };

    while index < chars.len() {
        // Inline code wins over emphasis: `**` inside backticks is literal.
        if chars[index] == '`' {
            if let Some(end) = find(&chars, index + 1, '`') {
                flush(&mut buffer, &mut spans);
                let code: String = chars[index + 1..end].iter().collect();
                spans.push(Span::styled(code, Style::default().fg(theme.code)));
                index = end + 1;
                continue;
            }
        }

        // Only `*` opens emphasis, never `_`.
        //
        // CommonMark treats `__init__` as strong emphasis and `a_b_c` as partly
        // italic, which is correct for prose and wrong for this. A coding
        // transcript is full of `__init__`, `MAX_SIZE`, and `_private`, and
        // mangling them costs more than losing `_italic_` — which models rarely
        // emit anyway, preferring `*italic*`.
        if chars[index] == '*' {
            let strong = chars.get(index + 1) == Some(&'*');
            let width = if strong { 2 } else { 1 };

            if let Some(end) = find_run(&chars, index + width, '*', width) {
                let inner: String = chars[index + width..end].iter().collect();
                // An empty pair is literal text, not emphasis.
                if !inner.trim().is_empty() {
                    flush(&mut buffer, &mut spans);
                    let modifier = if strong { Modifier::BOLD } else { Modifier::ITALIC };
                    spans.push(Span::styled(inner, base.add_modifier(modifier)));
                    index = end + width;
                    continue;
                }
            }
        }

        buffer.push(chars[index]);
        index += 1;
    }

    flush(&mut buffer, &mut spans);
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

fn find(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|index| chars[*index] == needle)
}

/// Find a run of `width` copies of `marker`, starting at `from`.
fn find_run(chars: &[char], from: usize, marker: char, width: usize) -> Option<usize> {
    let mut index = from;
    while index + width <= chars.len() {
        if chars[index..index + width].iter().all(|c| *c == marker) {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn render_default(source: &str) -> Vec<Line<'static>> {
        render(source, &Theme::default(), &crate::glyphs::UNICODE)
    }

    fn styles(line: &Line<'_>) -> Vec<Style> {
        line.spans.iter().map(|span| span.style).collect()
    }

    #[test]
    fn plain_prose_survives_unchanged() {
        assert_eq!(text_of(&render_default("just a sentence")), vec!["just a sentence"]);
    }

    #[test]
    fn headings_lose_their_hashes_and_gain_weight() {
        let lines = render_default("## The heading");
        assert_eq!(text_of(&lines), vec!["The heading"]);
        assert!(styles(&lines[0])[0].add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        // `#4` and `#[derive]` are ordinary text.
        assert_eq!(text_of(&render_default("#4 in the list"))[0], "#4 in the list");
        assert_eq!(text_of(&render_default("#[derive(Debug)]"))[0], "#[derive(Debug)]");
    }

    #[test]
    fn bold_and_italic_are_styled_and_their_markers_removed() {
        let lines = render_default("a **bold** and *soft* word");
        assert_eq!(text_of(&lines), vec!["a bold and soft word"]);

        let modifiers: Vec<Modifier> = styles(&lines[0]).iter().map(|s| s.add_modifier).collect();
        assert!(modifiers.iter().any(|m| m.contains(Modifier::BOLD)));
        assert!(modifiers.iter().any(|m| m.contains(Modifier::ITALIC)));
    }

    #[test]
    fn identifiers_with_underscores_survive() {
        // The intraword rule. Without it, most of the interesting text in a
        // coding transcript is mangled.
        for identifier in
            ["a_variable_name", "snake_case_fn", "__init__", "MAX_SIZE", "_private", "__all__"]
        {
            assert_eq!(text_of(&render_default(identifier))[0], identifier);
        }
    }

    #[test]
    fn underscores_are_never_emphasis() {
        // The deliberate divergence from CommonMark. `_soft_` staying literal is
        // the price of `__init__` and `MAX_SIZE` surviving, and it is worth it
        // in a transcript that is mostly about code.
        assert_eq!(text_of(&render_default("a _soft_ word"))[0], "a _soft_ word");
        assert_eq!(text_of(&render_default("__init__"))[0], "__init__");
    }

    #[test]
    fn an_unclosed_marker_keeps_its_text() {
        // Losing a line to a parser edge case is far worse than one asterisk.
        assert_eq!(text_of(&render_default("2 * 3 = 6"))[0], "2 * 3 = 6");
        assert_eq!(text_of(&render_default("**never closed"))[0], "**never closed");
        // Underscores inside an identifier are not emphasis.
    }

    #[test]
    fn inline_code_beats_emphasis_inside_it() {
        // `**` inside backticks is literal, or a snippet is corrupted.
        let lines = render_default("call `a ** b` please");
        assert_eq!(text_of(&lines), vec!["call a ** b please"]);
    }

    #[test]
    fn bullets_become_real_bullets() {
        let lines = render_default("- first\n- second");
        let rendered = text_of(&lines);
        assert!(rendered[0].contains("first"));
        assert!(!rendered[0].contains("- first"));
        assert!(rendered[0].starts_with(crate::glyphs::UNICODE.bullet));
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        // The number carries meaning; a bullet would lose it.
        let rendered = text_of(&render_default("1. first\n2. second"));
        assert!(rendered[0].contains("1 "));
        assert!(rendered[1].contains("2 "));
    }

    #[test]
    fn nested_list_indentation_is_preserved() {
        let rendered = text_of(&render_default("- top\n  - nested"));
        assert!(rendered[1].starts_with("  "));
    }

    #[test]
    fn a_fenced_block_is_verbatim() {
        // Markdown inside a code block is code. Styling it corrupts what the
        // user is about to copy.
        let source = "```rust\nlet x = **not bold**;\n# not a heading\n```";
        let rendered = text_of(&render_default(source));

        assert!(rendered.iter().any(|line| line.contains("let x = **not bold**;")));
        assert!(rendered.iter().any(|line| line.contains("# not a heading")));
    }

    #[test]
    fn a_fence_is_visually_bounded_and_labelled() {
        let rendered = text_of(&render_default("```rust\nfn main() {}\n```"));
        assert!(rendered[0].contains("rust"), "the language should be shown");
        // Opening and closing edges, so the block does not run into the prose.
        assert_eq!(rendered.len(), 3);
    }

    #[test]
    fn an_unterminated_fence_is_closed() {
        // Common in a truncated response; without this the block runs on.
        let rendered = text_of(&render_default("```\ncode here"));
        assert_eq!(rendered.len(), 3, "header, body, and a synthesised edge");
    }

    #[test]
    fn tildes_open_a_fence_too() {
        let rendered = text_of(&render_default("~~~\ncode\n~~~"));
        assert!(rendered.iter().any(|line| line.contains("code")));
    }

    #[test]
    fn block_quotes_are_marked_and_dimmed() {
        let lines = render_default("> quoted thing");
        assert!(text_of(&lines)[0].contains("quoted thing"));
        assert!(text_of(&lines)[0].starts_with(crate::glyphs::UNICODE.code_bar()));
    }

    #[test]
    fn horizontal_rules_become_rules() {
        let rendered = text_of(&render_default("---"));
        assert!(rendered[0].chars().all(|c| c == '─'));
    }

    #[test]
    fn line_count_is_preserved_for_ordinary_prose() {
        // The transcript's scroll maths depends on it.
        let source = "one\n\ntwo\n\nthree";
        assert_eq!(render_default(source).len(), source.split('\n').count());
    }

    #[test]
    fn the_ascii_glyph_set_is_honoured() {
        let lines = render("- item", &Theme::default(), &crate::glyphs::ASCII);
        let rendered = text_of(&lines);
        assert!(rendered[0].is_ascii(), "got {rendered:?}");
    }

    #[test]
    fn an_empty_line_stays_empty() {
        assert_eq!(text_of(&render_default("a\n\nb")), vec!["a", "", "b"]);
    }

    #[test]
    fn a_fenced_block_is_highlighted() {
        // The wiring, not the highlighter: `code_line` has to receive the
        // block's highlighter and use it, or every token renders in one colour
        // and the whole feature is invisible.
        let theme = Theme::new(crate::theme::ColorDepth::TrueColor);
        let lines = render(
            "```rust\nfn main() { let s = \"hi\"; }\n```",
            &theme,
            &crate::glyphs::UNICODE,
        );

        let colours: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|line| &line.spans)
            .filter_map(|span| span.style.fg)
            .collect();

        assert!(
            colours.len() >= 3,
            "expected several token colours, got {colours:?}"
        );
    }

    #[test]
    fn a_fenced_block_keeps_its_text_exactly() {
        // Highlighting splits a line into runs. If it ever alters one, the
        // thing the user copies out of the transcript is wrong.
        let theme = Theme::new(crate::theme::ColorDepth::TrueColor);
        let lines = render("```rust\nlet x = 1;\n```", &theme, &crate::glyphs::UNICODE);
        let body: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(body.ends_with("let x = 1;"), "got {body:?}");
    }
}
