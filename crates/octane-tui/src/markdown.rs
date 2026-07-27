//! Markdown rendering.
//!
//! Models answer in markdown whether or not you ask them to, so a transcript
//! that shows the raw source is showing the wrong thing. This is not a CommonMark
//! implementation and does not try to be — a terminal has no images and no
//! nested inline HTML. It handles what models actually emit, and models emit
//! tables constantly, so those are here.
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
use unicode_width::UnicodeWidthStr;

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

    // Indexed rather than a `for` over the lines because a table is the one
    // construct here that needs to look ahead: a header row only becomes a
    // header once the delimiter row after it confirms it.
    let lines: Vec<&str> = source.split('\n').collect();
    let mut index = 0;
    while index < lines.len() {
        let raw = lines[index];
        index += 1;

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
                    // The first line of the block comes with it, so an untagged
                    // fence can still be identified by a shebang or a doctype.
                    let first_line = lines.get(index).copied().unwrap_or_default();
                    highlighter =
                        crate::highlight::Highlighter::detect(&language, first_line, theme);
                    fence = Some(language);
                }
            }
            continue;
        }

        if fence.is_some() {
            out.push(code_line(raw, glyphs, theme, highlighter.as_mut()));
            continue;
        }

        if let Some(consumed) = table(&lines[index - 1..], theme, glyphs, &mut out) {
            index += consumed - 1;
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

    // Block quote. One border per level, so a quote inside a quote reads as
    // deeper rather than identical — which is the whole reason the source
    // bothered to nest it.
    if trimmed.starts_with('>') {
        let mut rest = trimmed;
        let mut depth = 0;
        while let Some(after) = rest.strip_prefix('>') {
            depth += 1;
            rest = after.strip_prefix(' ').unwrap_or(after);
        }
        let border = format!("{} ", glyphs.code_bar()).repeat(depth);
        let mut spans = vec![Span::styled(border, theme.dim())];
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

/// Where a column's content sits in its cell.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A pipe table beginning at `lines[0]`, if these lines are one.
///
/// Returns how many source lines it consumed, having pushed exactly one rendered
/// line for each — the transcript counts lines to do its scroll maths, so a
/// block that expands or contracts would have to be threaded through that too.
/// It falls out naturally here: the delimiter row becomes the rule under the
/// header.
///
/// The presentation — cells padded to a measured column width, separated by a
/// two-space gutter, with a rule under the header instead of drawn borders —
/// follows `codex-rs/tui/src/markdown_render.rs`. Its width-allocation
/// machinery is deliberately not taken: [`render`] is not given a viewport
/// width, so columns keep their natural width and an over-wide table is wrapped
/// by the transcript like any other long line. Guessing a width here would be
/// worse than wrapping, because the guess would be wrong on every resize.
fn table(
    lines: &[&str],
    theme: &Theme,
    glyphs: &Glyphs,
    out: &mut Vec<Line<'static>>,
) -> Option<usize> {
    let header = cells(lines.first()?)?;
    let alignments = alignment_row(lines.get(1)?)?;
    // GFM requires the two rows to agree. When they do not, this is prose that
    // happens to contain pipes, and it has to render as written.
    if header.is_empty() || alignments.len() != header.len() {
        return None;
    }

    let mut rows = vec![header];
    let mut consumed = 2;
    while let Some(row) = lines.get(consumed).and_then(|line| cells(line)) {
        // A row whose cells are all empty draws as blank padding, so the
        // characters the author typed — `| | |`, or a bare `|||` — appear
        // nowhere. Every other renderer does the same, but this module promises
        // never to lose text, and a promise with an exception for "rows other
        // renderers also drop" is not one. The table stops here and the rest
        // renders as written.
        if row.iter().all(|cell| cell.trim().is_empty()) {
            break;
        }
        rows.push(row);
        consumed += 1;
    }

    // Widened to the longest row rather than truncated to the header's count.
    // A row with a cell too many is malformed, but dropping the cell would lose
    // text, and a ragged last column is a far smaller lie.
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let rendered: Vec<Vec<Vec<Span<'static>>>> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let style = if index == 0 {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            (0..columns)
                .map(|column| inline(row.get(column).map_or("", String::as_str), style, theme))
                .collect()
        })
        .collect();

    // Display columns, not chars: a CJK cell is twice as wide as its length and
    // measuring it wrong shears every column to its right.
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rendered.iter().map(|row| span_width(&row[column])).max().unwrap_or(1).max(1)
        })
        .collect();

    out.push(Line::from(table_row(&rendered[0], &widths, &alignments)));
    let rule = widths.iter().map(|width| glyphs.rule(*width)).collect::<Vec<_>>().join("  ");
    out.push(Line::styled(rule, theme.dim()));
    for row in &rendered[1..] {
        out.push(Line::from(table_row(row, &widths, &alignments)));
    }
    Some(consumed)
}

fn table_row(
    row: &[Vec<Span<'static>>],
    widths: &[usize],
    alignments: &[Align],
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            spans.push(Span::raw("  "));
        }
        let cell = &row[column];
        let fill = width.saturating_sub(span_width(cell));
        // A column past the alignment row exists only because some body row was
        // too long; left is the only defensible guess for it.
        let (before, after) = match alignments.get(column).copied().unwrap_or(Align::Left) {
            Align::Left => (0, fill),
            Align::Center => (fill / 2, fill - fill / 2),
            Align::Right => (fill, 0),
        };
        if before > 0 {
            spans.push(Span::raw(" ".repeat(before)));
        }
        spans.extend(cell.iter().cloned());
        // No padding after the last column: trailing spaces are invisible until
        // someone copies the line out of the transcript.
        if after > 0 && column + 1 < widths.len() {
            spans.push(Span::raw(" ".repeat(after)));
        }
    }
    spans
}

fn span_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|span| UnicodeWidthStr::width(span.content.as_ref())).sum()
}

/// Split `| a | b |` into its cells.
///
/// The leading pipe is required, matching `is_table_row`, which is what tells
/// the streaming path to hold a table back. GFM also accepts `a | b`, but
/// recognising a form the two disagree about would let half a table commit.
fn cells(line: &str) -> Option<Vec<String>> {
    let mut rest = line.trim().strip_prefix('|')?.chars();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_code = false;

    while let Some(ch) = rest.next() {
        match ch {
            // `\|` is a literal pipe, and `|` inside backticks is part of the
            // snippet — a table of shell or regex syntax is full of both.
            '\\' => match rest.next() {
                Some('|') => cell.push('|'),
                Some(other) => {
                    cell.push('\\');
                    cell.push(other);
                }
                None => cell.push('\\'),
            },
            '`' => {
                in_code = !in_code;
                cell.push('`');
            }
            '|' if !in_code => cells.push(std::mem::take(&mut cell)),
            _ => cell.push(ch),
        }
    }
    // The trailing pipe is optional: what is left over is a cell unless the row
    // was closed with one.
    if !cell.trim().is_empty() {
        cells.push(cell);
    }
    Some(cells.into_iter().map(|cell| cell.trim().to_string()).collect())
}

/// The `|:---|---:|` row, and the alignment each cell declares.
fn alignment_row(line: &str) -> Option<Vec<Align>> {
    let cells = cells(line)?;
    if cells.is_empty() {
        return None;
    }
    cells
        .iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            let dashes = cell.trim_start_matches(':').trim_end_matches(':');
            let is_delimiter = !dashes.is_empty() && dashes.chars().all(|ch| ch == '-');
            is_delimiter.then_some(match (left, right) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            })
        })
        .collect()
}

/// Inline spans: bold, italic, strikethrough, and code.
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

        // Strikethrough. `~` is safe to treat as a marker where `_` was not:
        // it appears in prose and code far more rarely, and a lone pair with
        // nothing closing it falls through to the literal text below.
        if chars[index] == '~' && chars.get(index + 1) == Some(&'~') {
            if let Some(end) = find_run(&chars, index + 2, '~', 2) {
                let inner: String = chars[index + 2..end].iter().collect();
                if !inner.trim().is_empty() {
                    flush(&mut buffer, &mut spans);
                    spans.push(Span::styled(inner, base.add_modifier(Modifier::CROSSED_OUT)));
                    index = end + 2;
                    continue;
                }
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

/// How much of `source` can be rendered now and never re-rendered.
///
/// Returns a byte offset. Everything before it is whole markdown blocks, so
/// rendering that prefix on its own gives the same lines as rendering it as
/// part of the finished document — which is the property that makes streaming
/// incremental instead of quadratic.
///
/// # Why a blank line outside a fence
///
/// A paragraph break is markdown's natural block boundary. An open ``` fence is
/// the obvious counter-example: blank lines inside one mean nothing, and
/// committing there would render half a code block as prose and then have to
/// take it back.
///
/// # Why lists are held back
///
/// A blank line inside a list makes it *loose*, which changes how every item
/// before it renders. Committing at that blank line would fix the earlier items
/// as tight and the finished document would disagree. Rather than model that,
/// any boundary with a list on either side is refused — the text simply stays
/// in the mutable tail, which costs a re-render and is never wrong.
///
/// Tables are held back for the same reason by the same rule: a new row can
/// change every column's width, and the header sits in the same block.
pub fn stable_prefix(source: &str) -> usize {
    let mut in_fence = false;
    let mut commit = 0;
    let mut offset = 0;
    let mut previous_block_is_list = false;
    let mut pending: Option<usize> = None;

    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();

        if !in_fence && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            // A fence opening is a block start like any other, so it settles
            // the boundary before it. Clearing instead would strand every
            // paragraph written before the first code block in the tail.
            if let Some(candidate) = pending.take() {
                commit = candidate;
            }
            previous_block_is_list = false;
            in_fence = true;
        } else if in_fence && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            in_fence = false;
        } else if trimmed.is_empty() && !in_fence {
            // A candidate boundary: everything up to and including this blank
            // line is complete, unless what follows turns out to be a list.
            pending = Some(offset + line.len());
        } else if !in_fence && !trimmed.is_empty() {
            let is_list = is_list_item(trimmed) || is_table_row(trimmed);
            if let Some(candidate) = pending.take() {
                // Held back only when the boundary sits *inside* a list or
                // table — that is the case where the blank line restyles what
                // came before. Prose followed by a list is an ordinary block
                // break, and refusing it would strand every message that ends
                // in a bulleted list.
                if !(is_list && previous_block_is_list) {
                    commit = candidate;
                }
            }
            previous_block_is_list = is_list;
        }
        offset += line.len();
    }

    // A trailing boundary with nothing after it yet: the next chunk decides, so
    // it is not committed here.
    commit
}

/// Render a slice that is about to be committed and never redrawn.
///
/// Same as [`render`] but with the single trailing blank line dropped.
///
/// [`render`] closes its output with one blank after the last block. Committed
/// chunks are concatenated, so keeping it would open a growing gap — one extra
/// blank per paragraph the model streams. Dropping *more* than one is equally
/// wrong in the other direction: the remaining blank is the separator between
/// this chunk's last block and the next chunk's first, and without it the
/// paragraphs run together.
pub fn render_committed(source: &str, theme: &Theme, glyphs: &Glyphs) -> Vec<Line<'static>> {
    let mut lines = render(source, theme, glyphs);
    if lines.last().is_some_and(is_blank) {
        lines.pop();
    }
    lines
}

fn is_blank(line: &Line<'static>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

fn is_list_item(line: &str) -> bool {
    let mut chars = line.chars();
    match chars.next() {
        Some('-' | '*' | '+') => chars.next().is_none_or(char::is_whitespace),
        Some(first) if first.is_ascii_digit() => {
            let rest: String = chars.collect();
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            matches!(rest.chars().nth(digits), Some('.' | ')'))
        }
        _ => false,
    }
}

fn is_table_row(line: &str) -> bool {
    line.starts_with('|')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Highlighting is resolved from the tag, then an alias table, then the
    /// code's own first line — and from nothing else. Guessing a language from
    /// arbitrary code and getting it wrong renders the block as garbage, which
    /// is worse than leaving it plain.
    #[test]
    fn a_fence_is_highlighted_from_its_tag_its_alias_or_its_shebang() {
        let theme = Theme::default();
        let glyphs = crate::glyphs::UNICODE;
        let spans = |source: &str, needle: &str| {
            render(source, &theme, &glyphs)
                .iter()
                .find(|line| {
                    line.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .contains(needle)
                })
                .map(|line| line.spans.len())
                .unwrap_or(0)
        };

        assert!(spans("```rust\nfn main() {}\n```\n", "fn main") > 2);
        // What models actually write; no grammar is named either of these.
        assert!(spans("```shell\necho hi\n```\n", "echo") > 2, "shell aliases bash");
        assert!(spans("```yaml\nkey: value\n```\n", "key") > 2, "yaml aliases yml");
        // Untagged, but it says what it is.
        assert!(
            spans("```\n#!/bin/bash\necho hi\n```\n", "echo") > 2,
            "a shebang identifies the block",
        );
        // Untagged and unidentifiable: left plain rather than guessed at.
        assert_eq!(spans("```\njust some prose\n```\n", "prose"), 2);
    }

    /// The module's first rule is that text is never lost. An all-empty table
    /// row draws as blank padding, so the pipes the author typed would appear
    /// nowhere at all — the table ends instead and the row renders as written.
    #[test]
    fn a_table_row_of_empty_cells_keeps_its_source_text() {
        let theme = Theme::default();
        let glyphs = crate::glyphs::UNICODE;
        for source in ["| a | b |\n|---|---|\n| | |\n", "| a | b |\n|---|---|\n|||\n"] {
            let rendered: String = render(source, &theme, &glyphs)
                .iter()
                .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
                .collect();
            assert!(
                rendered.contains('|'),
                "the row's own characters must survive: {source:?} -> {rendered:?}",
            );
        }
    }

    /// And the ordinary table still renders as a table.
    #[test]
    fn a_table_with_content_still_renders_as_one() {
        let theme = Theme::default();
        let glyphs = crate::glyphs::UNICODE;
        let rendered: Vec<String> = render("| a | b |\n|---|---|\n| 1 | 2 |\n", &theme, &glyphs)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(rendered.iter().any(|line| line.contains('1') && line.contains('2')));
        assert!(!rendered.iter().any(|line| line.contains('|')), "{rendered:?}");
    }

    /// The end-to-end property of the streaming split: feeding a message in
    /// chunks and committing whole blocks as they settle must produce exactly
    /// what rendering the finished message produces. If it does not, the
    /// transcript silently changes under the user at the moment a message ends.
    #[test]
    fn streaming_a_message_in_pieces_ends_up_identical_to_rendering_it_whole() {
        let theme = Theme::default();
        let glyphs = crate::glyphs::UNICODE;
        for message in [
            "Here is the plan.\n\n                       ```rust\nfn main() {}\n```\n\n                       That covers it.\n\nAnd a closing note.",
            // A table is the construct most able to break this: a later row can
            // widen every column above it, so nothing inside one may commit.
            "Comparison:\n\n| lang | speed |\n|---|--:|\n| rust | fast |\n| a much longer name | slow |\n\nDone.",
        ] {
            // One character at a time — the worst case, and what a fast model
            // effectively does.
            let mut stable: Vec<Line<'static>> = Vec::new();
            let mut committed = 0usize;
            for end in 1..=message.len() {
                if !message.is_char_boundary(end) {
                    continue;
                }
                let seen = &message[..end];
                let boundary = stable_prefix(seen);
                if boundary > committed {
                    stable.extend(render_committed(&seen[committed..boundary], &theme, &glyphs));
                    committed = boundary;
                }
            }

            let mut streamed = stable;
            streamed.extend(render(&message[committed..], &theme, &glyphs));

            assert_eq!(streamed, render(message, &theme, &glyphs), "streaming {message:?}");
        }
    }

    /// Prose separated by a blank line is two whole blocks, so the first can be
    /// drawn once and left alone.
    #[test]
    fn a_paragraph_break_outside_a_fence_is_a_commit_point() {
        let source = "first para\n\nsecond para";
        let at = stable_prefix(source);
        assert_eq!(&source[..at], "first para\n\n");
    }

    /// The obvious way to get this wrong: a blank line inside a code block is
    /// not a block boundary, and committing there renders half a fence as prose.
    #[test]
    fn a_blank_line_inside_an_open_fence_is_not_a_commit_point() {
        let source = "intro\n\n```rust\nlet a = 1;\n\nlet b = 2;\n";
        let at = stable_prefix(source);
        assert_eq!(&source[..at], "intro\n\n", "only the prose before the fence commits");
    }

    #[test]
    fn a_closed_fence_can_be_committed() {
        let source = "```\ncode\n```\n\nafter";
        let at = stable_prefix(source);
        assert!(at >= "```\ncode\n```\n\n".len(), "the whole closed fence commits");
    }

    /// A blank line inside a list makes it loose, which restyles every earlier
    /// item. Committing there would leave the finished document disagreeing
    /// with what was already drawn.
    #[test]
    fn a_boundary_touching_a_list_is_held_back() {
        assert_eq!(stable_prefix("- one\n\n- two"), 0);
        assert_eq!(stable_prefix("intro\n\n1. one\n\n2. two"), "intro\n\n".len());
    }

    /// A new row can change every column's width, so a table cannot be
    /// committed while it is still growing.
    #[test]
    fn a_table_is_held_back_while_it_grows() {
        assert_eq!(stable_prefix("| a | b |\n\n| 1 | 2 |"), 0);
    }

    /// The property that makes the whole thing safe: rendering the committed
    /// prefix separately must equal rendering it as part of the document.
    #[test]
    fn rendering_the_prefix_separately_matches_rendering_it_whole() {
        let theme = Theme::default();
        let glyphs = crate::glyphs::UNICODE;
        for source in [
            "one\n\ntwo\n\nthree",
            "# Heading\n\nbody text\n\nmore",
            "```\ncode\n```\n\nafter the fence",
        ] {
            let at = stable_prefix(source);
            if at == 0 {
                continue;
            }
            let whole = render(source, &theme, &glyphs);
            let prefix = render_committed(&source[..at], &theme, &glyphs);
            assert_eq!(
                prefix,
                whole[..prefix.len()],
                "committing {:?} changed how it renders",
                &source[..at],
            );
        }
    }

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
    fn a_table_becomes_aligned_columns() {
        let rendered = text_of(&render_default("| name | n |\n|---|---|\n| a | 1 |\n| long | 22 |"));
        assert_eq!(rendered[0], "name  n");
        assert!(rendered[1].chars().all(|c| c == '─' || c == ' '), "got {:?}", rendered[1]);
        assert_eq!(rendered[2], "a     1");
        assert_eq!(rendered[3], "long  22");
    }

    #[test]
    fn a_table_honours_its_alignment_row() {
        let source = "| l | c | r |\n|:--|:-:|--:|\n| xxxxx | yyyyy | zzzzz |";
        let rendered = text_of(&render_default(source));
        // Five-wide columns: the left header sits flush, the centred one has
        // its slack split, the right one is pushed against its column's end.
        assert_eq!(rendered[0], format!("l{}c{}r", " ".repeat(8), " ".repeat(8)));
    }

    #[test]
    fn a_table_renders_one_line_per_source_line() {
        // The transcript's scroll maths counts lines; the delimiter row has to
        // become the rule rather than disappear.
        let source = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        assert_eq!(render_default(source).len(), source.split('\n').count());
    }

    #[test]
    fn a_malformed_table_degrades_to_its_raw_lines() {
        // Never lose text. Each of these fails a different check, and every one
        // must still show what the model wrote.
        for source in [
            "| a | b |",                        // no delimiter row
            "| a | b |\n|---|",                 // the rows disagree on width
            "| a | b |\n| c | d |",             // no delimiter at all
            "| a | b |\n|-x-|---|",             // not a delimiter row
        ] {
            let rendered = text_of(&render_default(source));
            assert_eq!(rendered.len(), source.split('\n').count());
            for (line, raw) in rendered.iter().zip(source.split('\n')) {
                assert_eq!(line, raw, "in {source:?}");
            }
        }
    }

    #[test]
    fn a_table_row_with_an_extra_cell_keeps_it() {
        // Truncating to the header's column count is the obvious move and it
        // silently eats a cell.
        let rendered = text_of(&render_default("| a |\n|---|\n| 1 | surplus |"));
        assert!(rendered[2].contains("surplus"), "got {:?}", rendered[2]);
    }

    #[test]
    fn a_pipe_inside_inline_code_does_not_split_a_cell() {
        let rendered = text_of(&render_default("| shell |\n|---|\n| `a | b` |"));
        assert!(rendered[2].contains("a | b"), "got {:?}", rendered[2]);
    }

    #[test]
    fn an_escaped_pipe_stays_in_its_cell() {
        let rendered = text_of(&render_default("| op |\n|---|\n| a \\| b |"));
        assert!(rendered[2].contains("a | b"), "got {:?}", rendered[2]);
    }

    #[test]
    fn column_widths_are_measured_in_display_columns() {
        // A CJK cell is twice as wide as its char count. Measured in chars, the
        // column to its right is sheared by however many wide characters it has.
        let rendered = text_of(&render_default("| a | b |\n|---|---|\n| \u{4e2d}\u{6587} | x |"));
        let width = |line: &String| UnicodeWidthStr::width(line.as_str());
        assert_eq!(width(&rendered[0]), width(&rendered[2]));
    }

    #[test]
    fn a_table_inside_a_fence_is_not_a_table() {
        // Code is verbatim: the pipes are what the user copies.
        let source = "```\n| a | b |\n|---|---|\n```";
        let rendered = text_of(&render_default(source));
        assert!(rendered.iter().any(|line| line.contains("| a | b |")));
        assert!(rendered.iter().any(|line| line.contains("|---|---|")));
    }

    #[test]
    fn a_table_cell_is_still_styled_inline() {
        let lines = render_default("| a |\n|---|\n| **bold** |");
        assert!(text_of(&lines)[2].contains("bold"));
        assert!(!text_of(&lines)[2].contains('*'));
        assert!(styles(&lines[2]).iter().any(|s| s.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn the_ascii_glyph_set_reaches_the_table_rule() {
        let lines = render("| a |\n|---|\n| 1 |", &Theme::default(), &crate::glyphs::ASCII);
        assert!(text_of(&lines)[1].is_ascii(), "got {:?}", text_of(&lines)[1]);
    }

    #[test]
    fn nested_quotes_get_one_border_each() {
        let rendered = text_of(&render_default("> one\n>> two\n> > two again"));
        let bar = crate::glyphs::UNICODE.code_bar();
        assert_eq!(rendered[0], format!("{bar} one"));
        assert_eq!(rendered[1], format!("{bar} {bar} two"));
        assert_eq!(rendered[2], format!("{bar} {bar} two again"));
    }

    #[test]
    fn strikethrough_is_styled_and_its_markers_removed() {
        let lines = render_default("a ~~gone~~ word");
        assert_eq!(text_of(&lines), vec!["a gone word"]);
        assert!(styles(&lines[0]).iter().any(|s| s.add_modifier.contains(Modifier::CROSSED_OUT)));
    }

    #[test]
    fn an_unclosed_strikethrough_keeps_its_text() {
        assert_eq!(text_of(&render_default("~~never closed"))[0], "~~never closed");
        assert_eq!(text_of(&render_default("approx ~~ 5"))[0], "approx ~~ 5");
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
