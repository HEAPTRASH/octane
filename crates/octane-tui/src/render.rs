//! Turning protocol events into scrollback lines.
//!
//! Pure: [`Event`] in, styled [`Line`]s out. Nothing here touches a terminal, so
//! the rendering rules — which are where the bugs live — are testable by calling
//! a function.
//!
//! Two principles taken from the survey (`RESEARCH.md` §I):
//!
//! **Collapsed but available.** Tool calls render as one line by default. The
//! detail exists and can be expanded, but a transcript where every `read` dumps
//! 200 lines is one nobody can follow.
//!
//! **What the user sees, the model sees.** The collapsed line summarizes the same
//! result the model received. It never shows something the model did not get, and
//! never hides an error.

use octane_protocol::{Event, ItemEvent, ItemKind, ItemStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::highlight::Highlighter;
use crate::theme::Theme;

/// Whether to show model reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reasoning {
    #[default]
    Hidden,
    Shown,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub reasoning: Reasoning,
    pub theme: Theme,
    /// Marker glyphs. Carried here rather than hardcoded so a terminal without
    /// dependable Unicode gets the ASCII set throughout, not just in the banner.
    pub glyphs: crate::glyphs::Glyphs,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            reasoning: Reasoning::default(),
            theme: Theme::default(),
            glyphs: crate::glyphs::UNICODE,
        }
    }
}

/// Render one event as scrollback lines.
///
/// Returns empty for events with nothing to show — deltas, for instance, which
/// are handled by the live region rather than appended to scrollback. An event
/// producing no lines is normal, not an error.
pub fn render_event(event: &Event, options: &RenderOptions) -> Vec<Line<'static>> {
    render_event_expanded(event, options, false)
}

/// As [`render_event`], but showing a tool result in full.
pub fn render_event_expanded(
    event: &Event,
    options: &RenderOptions,
    expanded: bool,
) -> Vec<Line<'static>> {
    match event {
        Event::Item(item_event) => render_item(item_event, options, expanded),

        Event::Compaction { before_tokens, after_tokens, strategy } => {
            // Surfaced deliberately. Silent context loss is the most confusing
            // thing that can happen to a user mid-session — the agent appears to
            // forget, with no explanation anywhere.
            vec![notice(
                options.glyphs.notice,
                format!(
                    "context compacted ({strategy}): {} → {} tokens",
                    thousands(*before_tokens),
                    thousands(*after_tokens)
                ),
                options.theme.warning,
            )]
        }

        // Turn lifecycle and usage drive the status line, not the transcript.
        Event::Turn(_) | Event::Usage(_) | Event::Thread { .. } => Vec::new(),
    }
}

fn render_item(event: &ItemEvent, options: &RenderOptions, expanded: bool) -> Vec<Line<'static>> {
    // Only completed items reach scrollback. Anything in flight belongs in the
    // live region, where it can still change.
    let ItemEvent::Completed { item, .. } = event else {
        return Vec::new();
    };
    let theme = &options.theme;
    let glyphs = &options.glyphs;

    // Sanitized once, here, rather than in each branch. Every string below is
    // somebody else's bytes and there is no branch that would be correct to
    // skip: the model, the tools, `!command` and pastes all land in one of
    // them.
    match &sanitized(&item.kind) {
        ItemKind::UserMessage { text } => {
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{} ", glyphs.prompt), theme.label(theme.user)),
                Span::styled(first_line(text), Style::default().fg(theme.user)),
            ])];
            // Continuation lines are indented to match the marker width, so a
            // pasted block stays visually attached to its prompt.
            for line in text.lines().skip(1) {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(line.to_string(), Style::default().fg(theme.user)),
                ]));
            }
            lines.push(Line::default());
            lines
        }

        ItemKind::AgentMessage { text } => {
            // Models answer in markdown whether or not asked to, so showing the
            // raw source is showing the wrong thing.
            //
            // The margin leads rather than trails, because tool results
            // deliberately do not trail one: a run of calls should stay tight,
            // and the prose that follows is what needs separating from it.
            let mut lines = vec![Line::default()];
            lines.extend(crate::markdown::render(text, theme, glyphs));
            lines.push(Line::default());
            lines
        }

        ItemKind::Reasoning { text } => {
            if options.reasoning == Reasoning::Hidden {
                return Vec::new();
            }
            let mut lines: Vec<Line<'static>> = text
                .lines()
                .map(|line| Line::styled(format!("  {line}"), theme.reasoning()))
                .collect();
            lines.push(Line::default());
            lines
        }

        ItemKind::ToolExecution { name, input, .. } => {
            vec![Line::from(vec![
                // The marker, not only its colour. Under NO_COLOR every colour
                // becomes Reset, and octane's own success and error hues are
                // close to indistinguishable under deuteranopia even at full
                // truecolor, so a failed call must differ in shape.
                Span::styled(
                    format!("{} ", status_marker(item.status, glyphs)),
                    Style::default().fg(status_color(item.status, theme)),
                ),
                // A verb, not a function name. `Ran`, `Read`, `Searched` say
                // what happened; `bash`, `read`, `grep` name the machinery that
                // did it, which the reader is not the one operating. Padded so
                // the argument starts in the same column down a run of calls,
                // which is what makes the column scannable.
                Span::styled(format!("{:<8}", verb_for(name)), theme.label(theme.tool)),
                Span::styled(summarize_input_with(name, input, glyphs), theme.dim()),
            ])]
        }

        ItemKind::ToolResult { name, title, metadata, is_error, body, .. } => {
            // Indented under the call it answers, so the pairing is structural
            // rather than a matter of colour. The marker differs on failure for
            // the same reason: a red line is invisible to a reader who cannot
            // see red, and to anyone under NO_COLOR.
            // No marker when it worked. The row is indented under the call it
            // answers, which already says what it is, and a bullet on every
            // result is a column of noise. A failure keeps its marker, because
            // that is the one outcome that must not be quiet.
            let (marker, colour) = if *is_error {
                (format!("{} ", glyphs.error), theme.error)
            } else {
                (String::new(), theme.tool)
            };
            let _ = &colour;

            // The detail when there is one, else the title. For `read` and
            // `bash` the title is the path or the description, which the call
            // line above already shows, so printing both says the same thing
            // twice on consecutive rows.
            let summary = match summarize_result(name, metadata.as_ref()) {
                Some(detail) => detail,
                None => title.clone(),
            };

            // The name is on the call line directly above; repeating it here
            // said `list` twice on consecutive rows. Parallel calls are the
            // case that motivated naming the result, and the body under it
            // identifies those far better than a repeated tool name does.
            let rendered_body =
                result_body(name, metadata.as_ref(), body, theme, glyphs, expanded);

            // A summary row that only confirms success, above output that
            // already demonstrates it, is a row of nothing. Three shell
            // commands in a row produced three `exit 0` lines between their
            // output. It stays whenever it carries something the body does not
            // — a failure, a truncation count — and whenever there is no body
            // to speak for itself.
            let confirms_only_success = !*is_error && !rendered_body.is_empty();
            if confirms_only_success && says_nothing_new(name, &summary) {
                return rendered_body;
            }

            // The summary heads the same hanging block as the output rather
            // than sitting above it. Outside the gutter it read as a loose line
            // between two calls, which is exactly the ambiguity hanging the
            // output was meant to remove.
            let head = Line::from(vec![
                Span::styled(format!("  {} ", glyphs.elbow), Style::default().fg(theme.rail)),
                Span::styled(marker, Style::default().fg(colour)),
                Span::styled(summary, theme.dim()),
            ]);
            let mut lines = vec![head];
            // The body already carried the elbow; under a summary it continues
            // the block instead of starting a second one.
            lines.extend(rendered_body.into_iter().map(|line| continuation(line, glyphs, theme)));
            lines
        }

        ItemKind::Diff { path, unified } => {
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{} ", glyphs.edit), theme.label(theme.tool)),
                Span::raw(path.clone()),
            ])];
            lines.extend(render_diff(unified, theme));
            lines.push(Line::default());
            lines
        }

        ItemKind::Approval(request) => vec![Line::from(vec![
            Span::styled(format!("{} ", glyphs.question), theme.label(theme.warning)),
            Span::raw(request.summary.clone()),
        ])],

        ItemKind::Error { message } => {
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{} ", glyphs.error), theme.label(theme.error)),
                Span::styled(message.clone(), Style::default().fg(theme.error)),
            ])];
            lines.push(Line::default());
            lines
        }
    }
}

/// Colour a unified diff.
///
/// The line-number column, per-hunk syntax highlighting and the dim overlay on
/// removals are derived from codex-rs/tui/src/diff_render.rs. No code was
/// copied; this reuses octane's own [`Highlighter`] and palette, and tints
/// nothing — see below for why.
pub fn render_diff(unified: &str, theme: &Theme) -> Vec<Line<'static>> {
    // Tracked from the `@@ -old,n +new,n @@` headers. A diff without line
    // numbers makes "which line is this?" — the first question anyone asks of
    // an approval prompt — unanswerable without opening the file. Codex renders
    // a number column for the same reason.
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    let width = number_column_width(unified);

    // Highlighting costs about 0.05ms a line in release, 0.8ms in a debug
    // build — ~22ms for a 400-line diff. `ApprovalPane` used to pay that twice
    // per frame, once to measure and once to draw; it now renders the rows once
    // and hands them in, so this budget is no longer covering for that.
    //
    // It stays as a ceiling on the pathological case: an expanded diff of a few
    // thousand lines would still cost a visible pause on the frame that opens
    // it. Past this size a diff is being skimmed, not read, so the sign colours
    // and the number column carry it alone. Codex guards the same path.
    const HIGHLIGHT_LINE_BUDGET: usize = 200;

    let language = (unified.lines().count() <= HIGHLIGHT_LINE_BUDGET)
        .then(|| diff_language(unified))
        .flatten();
    let open = || language.as_deref().and_then(|token| Highlighter::new(token, theme));
    let mut highlighter = open();

    let mut lines = Vec::new();
    for line in unified.lines() {
        // `+++`/`---` are file headers, not content; colouring them as
        // additions and removals makes every diff look like it rewrote a file.
        if line.starts_with("+++") || line.starts_with("---") {
            // Headers keep the same indent so the content column lines up.
            lines.push(Line::styled(format!("{:width$} {line}", "", width = width), theme.dim()));
            continue;
        }
        if let Some((old, new)) = parse_hunk_header(line) {
            old_line = old;
            new_line = new;
            // A hunk starts somewhere else in the file, so the lexer is restarted
            // with it. Carrying the parse across the gap paints everything after
            // an unterminated string in an earlier hunk as string.
            highlighter = open();
            lines.push(Line::styled(format!("{:width$} {line}", "", width = width), theme.dim()));
            continue;
        }

        // `sign` is the style for the `+`/`-` column, `overlay` what the content
        // carries on top of its syntax colours.
        let (sign, overlay, at) = if line.starts_with('+') {
            let at = new_line;
            new_line += 1;
            (Style::default().fg(theme.added), Style::default(), at)
        } else if line.starts_with('-') {
            let at = old_line;
            old_line += 1;
            // Codex separates the two sides with a background tint. octane does
            // not: `wrap::wrap_line` rebuilds a `Line` from its spans and drops
            // the line style, so a row background would silently vanish on every
            // wrapped row, and putting it on the spans instead stops at the text
            // edge — a ragged block of colour on an ink-black canvas. Dimming the
            // removed side is the same signal without a new colour, and it is an
            // attribute, so it survives NO_COLOR and a 16-colour terminal.
            // The overlay carries the colour, not just DIM. Syntect's string
            // scope is bit-identical to `theme.added`, so a removed line
            // containing a string literal rendered part of itself in the
            // additions green — a deleted line that reads, at a glance, as an
            // addition. Removed code is read to confirm what is leaving, not to
            // study its syntax, so the whole row takes the removal colour.
            // Additions keep full highlighting: that is where attention goes.
            (
                Style::default().fg(theme.removed),
                Style::default().fg(theme.removed).add_modifier(Modifier::DIM),
                at,
            )
        } else {
            let at = new_line;
            // Context advances both sides, or every number after the first
            // hunk drifts by the number of context lines seen.
            new_line += 1;
            old_line += 1;
            (Style::default(), Style::default(), at)
        };

        let mut spans = vec![Span::styled(format!("{at:>width$} "), theme.dim())];
        match highlighter.as_mut() {
            Some(highlighter) => {
                // The marker is one byte in a well-formed diff; splitting on the
                // first char's length keeps a malformed one from panicking
                // mid-codepoint.
                let (marker, content) =
                    line.split_at(line.chars().next().map(char::len_utf8).unwrap_or(0));
                spans.push(Span::styled(marker.to_string(), sign));
                spans.extend(
                    highlighter
                        .line(content, theme)
                        .into_iter()
                        .map(|span| span.patch_style(overlay)),
                );
            }
            // No usable extension, or no colour at all: the whole row keeps the
            // sign's colour, which is the path the plain diff always took.
            None => spans.push(Span::styled(line.to_string(), sign)),
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// The grammar a diff's content is in, from its `+++ b/<path>` header.
///
/// The extension only — [`Highlighter`] resolves it, and declines when it is not
/// a language it knows, which is what makes an extensionless path fall through
/// to the plain renderer rather than being guessed at.
fn diff_language(unified: &str) -> Option<String> {
    let header = unified.lines().find(|line| line.starts_with("+++"))?;
    // git writes `+++ b/src/main.rs\t2026-01-01 ...`; the timestamp is optional
    // and separated by a tab, which `sanitize` has by then turned into spaces.
    let path = header.trim_start_matches('+').split_whitespace().next()?;
    Some(std::path::Path::new(path).extension()?.to_str()?.to_string())
}

/// Columns needed for the largest line number the diff mentions.
///
/// Measured up front so every row in one diff shares a column; computing it per
/// line would make the content edge move as the numbers grow past a power of ten.
fn number_column_width(unified: &str) -> usize {
    let highest = unified
        .lines()
        .filter_map(parse_hunk_header)
        .map(|(old, new)| old.max(new))
        .max()
        .unwrap_or(0)
        // The header names where a hunk starts; its lines run past that.
        + unified.lines().count();
    highest.to_string().len().max(2)
}

/// `@@ -12,7 +12,9 @@` → the first old and new line numbers.
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let body = line.strip_prefix("@@ ")?;
    let (ranges, _) = body.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    let start = |part: &str, sign: char| -> Option<usize> {
        part.strip_prefix(sign)?.split(',').next()?.parse().ok()
    };
    Some((start(old, '-')?, start(new, '+')?))
}

/// One-line gist of a tool call, for the collapsed view.
///
/// Reaches into the arguments per tool because a generic JSON dump is unreadable
/// at a glance, and the point of the collapsed line is to be readable at a glance.
/// Lines of output shown for a tool that is not `edit` or `write`.
/// Re-gutter a hung line so it continues a block rather than starting one.
fn continuation(
    line: Line<'static>,
    glyphs: &crate::glyphs::Glyphs,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = line.spans;
    if let Some(first) = spans.first_mut()
        && first.content.contains(glyphs.elbow)
    {
        *first = Span::styled("    ".to_string(), Style::default().fg(theme.rail));
    }
    Line::from(spans)
}

/// What a tool did, as a verb.
///
/// The transcript is read by someone supervising work, not operating the tool,
/// so the row says what happened rather than which function was called. An
/// unknown tool — an MCP one, say — keeps its own name, because inventing a
/// verb for it would be a guess presented as fact.
fn verb_for(tool: &str) -> String {
    match tool {
        "bash" => "Ran".into(),
        "read" => "Read".into(),
        "write" => "Wrote".into(),
        "edit" | "apply_patch" => "Edited".into(),
        "grep" | "glob" => "Searched".into(),
        "list" => "Listed".into(),
        "task" => "Delegated".into(),
        other => match other.strip_prefix("mcp__") {
            // `mcp__linter__check` reads as `linter/check`, which says where it
            // came from — the part a reader needs in order to trust it.
            Some(rest) => rest.replacen("__", "/", 1),
            None => other.to_string(),
        },
    }
}

/// Hang lines under the call that produced them.
///
/// An elbow on the first row and an aligned indent under it, so a run of calls
/// reads as discrete blocks instead of a stream. The output used to sit at a
/// flat indent with nothing tying it to its call.
fn hang(
    body: Vec<String>,
    theme: &Theme,
    glyphs: &crate::glyphs::Glyphs,
) -> Vec<Line<'static>> {
    body.into_iter()
        .enumerate()
        .map(|(index, text)| {
            let gutter =
                if index == 0 { format!("  {} ", glyphs.elbow) } else { "    ".to_string() };
            Line::from(vec![
                Span::styled(gutter, Style::default().fg(theme.rail)),
                Span::styled(text, theme.dim()),
            ])
        })
        .collect()
}

const OUTPUT_PREVIEW_LINES: usize = 8;

/// What to show under a tool result, which differs by tool.
///
/// `edit` shows the whole change, because a change is the one thing a reader
/// has to be able to check, and a summary of it is unverifiable. `write` shows
/// what it wrote, for the same reason. Everything else is a lookup whose value
/// is in the answer rather than the transcript, so it gets a preview and a
/// count of what was left out.
fn result_body(
    name: &str,
    metadata: Option<&serde_json::Value>,
    output: &str,
    theme: &Theme,
    glyphs: &crate::glyphs::Glyphs,
    expanded: bool,
) -> Vec<Line<'static>> {
    let text = |key: &str| {
        metadata.and_then(|m| m.get(key)).and_then(|v| v.as_str()).unwrap_or_default().to_string()
    };

    match name {
        "edit" => {
            let (removed, added) = (text("removed"), text("added"));
            if removed.is_empty() && added.is_empty() {
                return Vec::new();
            }
            let mut lines = Vec::new();
            // Marked with - and +, not by colour: a red line and a green line
            // are the same line under NO_COLOR, and the two hues octane uses
            // for them are close under deuteranopia.
            for line in removed.lines() {
                lines.push(Line::styled(
                    format!("      - {}", sanitize(line)),
                    Style::default().fg(theme.error),
                ));
            }
            for line in added.lines() {
                lines.push(Line::styled(
                    format!("      + {}", sanitize(line)),
                    Style::default().fg(theme.success),
                ));
            }
            lines
        }

        "write" => text("content")
            .lines()
            .map(|line| {
                Line::styled(
                    format!("      {} {}", glyphs.bar, sanitize(line)),
                    theme.dim(),
                )
            })
            .collect(),

        _ => {
            // A tool that publishes a `preview` has already stripped the
            // framing its output carries for the model: `read` wraps its body
            // in `<file path=...>`, which is addressed to the model and is
            // noise on screen.
            let source = match text("preview") {
                preview if !preview.is_empty() => preview,
                _ => output.to_string(),
            };
            // Trimmed, or a command whose output begins with a newline hangs an
            // empty row off the elbow and the block starts with nothing in it.
            let all: Vec<&str> = source.trim_matches('\n').lines().collect();
            if all.iter().all(|line| line.trim().is_empty()) {
                return Vec::new();
            }
            let total = all.len();

            // Head *and* tail, not the first N. The head says what the command
            // started doing and the tail says whether it worked; the middle of
            // a long listing is the part nobody reads. Taken from Codex's
            // history cell, which truncates the same way for the same reason.
            let mut body: Vec<String> = Vec::new();
            if expanded || total <= OUTPUT_PREVIEW_LINES {
                body.extend(all.iter().map(|line| sanitize(line)));
            } else {
                let head = OUTPUT_PREVIEW_LINES / 2;
                let tail = OUTPUT_PREVIEW_LINES - head;
                body.extend(all[..head].iter().map(|line| sanitize(line)));
                body.push(format!("{} +{} lines", glyphs.ellipsis, total - head - tail));
                body.extend(all[total - tail..].iter().map(|line| sanitize(line)));
            }
            if expanded && total > OUTPUT_PREVIEW_LINES {
                body.push("[ctrl+o or click to collapse]".to_string());
            }

            hang(body, theme, glyphs)
        }
    }
}

/// A short, human-facing account of what a tool produced.
///
/// Reads the metadata the tool already reports rather than measuring its
/// output, because the output is not shown and the tool knows better than the
/// renderer what mattered about it.
/// Whether a successful result's summary adds nothing to the body below it.
///
/// Only `bash`'s `exit 0`. Everything else counts something the body does not
/// state — `12 of 400 lines` says what was withheld, `3 replacements` says how
/// much changed — so those rows stay.
fn says_nothing_new(tool: &str, summary: &str) -> bool {
    tool == "bash" && summary == "exit 0"
}

pub fn summarize_result(tool: &str, metadata: Option<&serde_json::Value>) -> Option<String> {
    let metadata = metadata?;
    let number = |key: &str| metadata.get(key).and_then(|value| value.as_u64());

    match tool {
        "read" => match (number("lines_shown"), number("lines_total")) {
            (Some(shown), Some(total)) if shown < total => {
                Some(format!("{shown} of {total} lines"))
            }
            (_, Some(total)) => Some(format!("{total} lines")),
            _ => None,
        },
        "bash" => {
            let code = number("exit_code")?;
            // Stated even when zero: "did that command actually work?" is the
            // question a collapsed line has to answer on its own.
            Some(if code == 0 { "exit 0".into() } else { format!("exit {code}") })
        }
        "write" => number("lines").map(|lines| format!("{lines} lines")),
        "edit" => number("replacements")
            .map(|n| if n == 1 { "1 replacement".into() } else { format!("{n} replacements") }),
        _ => None,
    }
}

/// Apply [`sanitize`] to every user-visible string in an item.
///
/// Cloning the item is the price of doing this in one place instead of at a
/// dozen call sites where the next branch added would forget.
fn sanitized(kind: &ItemKind) -> ItemKind {
    let mut kind = kind.clone();
    match &mut kind {
        ItemKind::UserMessage { text }
        | ItemKind::AgentMessage { text }
        | ItemKind::Reasoning { text } => *text = sanitize(text),
        ItemKind::Error { message } => *message = sanitize(message),
        ItemKind::ToolExecution { input, .. } => *input = sanitize(input),
        ItemKind::ToolResult { title, body, .. } => {
            *title = sanitize(title);
            *body = sanitize(body);
        }
        ItemKind::Diff { unified, path } => {
            *unified = sanitize(unified);
            *path = sanitize(path);
        }
        ItemKind::Approval(request) => request.summary = sanitize(&request.summary),
    }
    kind
}

/// Tab stop, in columns. The usual terminal default.
const TAB_WIDTH: usize = 8;

/// Make arbitrary text safe to put in a `Line`.
///
/// Everything in the transcript is somebody else's bytes: tool output, shell
/// output, model text, pastes. Two things go wrong if they arrive raw, and
/// neither is obvious from reading ratatui's API.
///
/// `Paragraph` filters graphemes by *display width*, not by control-ness. An
/// ESC measures zero and is dropped, so `\x1b[39m` loses only the ESC and the
/// remaining `[39m` is written into cells as ordinary text. That is where a
/// stray `39m` in a transcript comes from, and why stripping at the tool is not
/// enough: escapes also arrive from the model and from `!command`.
///
/// A tab measures zero too, so it vanishes rather than aligning anything. Line
/// numbers separated from their content by a tab collapse into the content.
/// Expanding here rather than at the source keeps every producer free to use
/// tabs normally.
///
/// Note `Buffer::set_stringn` *does* filter control characters, so block titles
/// were never affected. Only the transcript, which is the one place untrusted
/// bytes land.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut column = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => {
                // CSI and OSC run to a terminator; anything else is a short
                // sequence whose second byte is consumed here.
                match chars.next() {
                    Some('[') => {
                        for next in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&next) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        for next in chars.by_ref() {
                            if next == '\u{7}' || next == '\u{1b}' {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            '\t' => {
                let spaces = TAB_WIDTH - (column % TAB_WIDTH);
                out.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            '\n' => {
                out.push('\n');
                column = 0;
            }
            // A bare carriage return rewrites the line it is on, which is how
            // progress bars work. Keeping the text after it is what the user
            // would have seen.
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                let start = out.rfind('\n').map(|index| index + 1).unwrap_or(0);
                out.truncate(start);
                column = 0;
            }
            _ if ch.is_control() => {}
            _ => {
                out.push(ch);
                column += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
    }
    out
}

/// One-line gist of a tool call, elided with the active set's ellipsis.
pub fn summarize_input_with(tool: &str, input: &str, glyphs: &crate::glyphs::Glyphs) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return truncate(input, 60, glyphs.ellipsis);
    };
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).unwrap_or_default();

    let summary = match tool {
        "read" | "write" | "edit" | "list" => field("path").to_string(),
        // The description exists precisely so there is something human to show.
        "bash" => {
            let described = field("description");
            if described.is_empty() { field("command").to_string() } else { described.to_string() }
        }
        "glob" => field("pattern").to_string(),
        "grep" => {
            let pattern = field("pattern");
            match value.get("glob").and_then(|v| v.as_str()) {
                Some(glob) => format!("{pattern}  in {glob}"),
                None => pattern.to_string(),
            }
        }
        _ => input.to_string(),
    };

    truncate(&summary, 60, glyphs.ellipsis)
}

/// The marker for a call's outcome.
///
/// Paired with [`status_color`] rather than replaced by it: colour is the fast
/// signal for those who can use it, and the glyph is what survives NO_COLOR,
/// a monochrome terminal, and colour blindness.
fn status_marker(status: ItemStatus, glyphs: &crate::glyphs::Glyphs) -> &'static str {
    match status {
        ItemStatus::Completed => glyphs.ok,
        ItemStatus::Failed => glyphs.error,
        ItemStatus::Canceled => glyphs.notice,
        ItemStatus::Started | ItemStatus::Streaming => glyphs.tool,
    }
}

fn status_color(status: ItemStatus, theme: &Theme) -> ratatui::style::Color {
    match status {
        ItemStatus::Completed => theme.success,
        ItemStatus::Failed => theme.error,
        ItemStatus::Canceled => theme.dim,
        ItemStatus::Started | ItemStatus::Streaming => theme.tool,
    }
}

fn notice(marker: &str, text: String, color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(color)),
        Span::styled(text, Style::default().fg(color)),
    ])
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// One line, at most `limit` columns, with an ellipsis when it did not fit.
///
/// The marker is passed in rather than hardcoded, because it is the one glyph
/// whose two forms differ in width: `...` is three columns where `…` is one. A
/// marker appended without subtracting its width overflows the box that clipped
/// the line, which is the failure the width rules in [`crate::glyphs`] exist to
/// prevent — so the room for it comes out of `limit`, not out of the caller's.
pub fn truncate(text: &str, limit: usize, ellipsis: &str) -> String {
    use unicode_width::UnicodeWidthChar;

    // Columns, not characters. A CJK glyph is one `char` and two cells, so
    // counting characters lets a "clipped" line overrun its box by its own
    // width in wide glyphs — which is the failure this crate's width rules
    // exist to prevent. It also matters for the marker itself: the ASCII
    // fallback `...` is three columns where `…` is one.
    let width = |text: &str| text.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();

    let cleaned = text.replace('\n', " ");
    if width(&cleaned) <= limit {
        return cleaned;
    }

    // Take up to `room`, never splitting a glyph across the boundary.
    let take = |room: usize| {
        let mut used = 0;
        cleaned
            .chars()
            .take_while(|c| {
                used += c.width().unwrap_or(0);
                used <= room
            })
            .collect::<String>()
    };

    // A limit too narrow to hold the marker clips bare. Emitting the marker
    // anyway would be wider than what was asked for, and `limit` is a promise
    // to whoever sized the box.
    match limit.checked_sub(width(ellipsis)) {
        Some(room) => format!("{}{ellipsis}", take(room)),
        None => take(limit),
    }
}

/// Group digits for display: 167000 -> "167,000".
pub fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::UNICODE;

    /// `limit` is columns, and the caller sized a box with it. Counting
    /// characters instead lets a wide glyph overrun that box by its own width,
    /// and the ASCII marker is three columns where the Unicode one is one.
    #[test]
    fn truncate_never_exceeds_the_columns_it_was_given() {
        use unicode_width::UnicodeWidthChar;
        let columns =
            |text: &str| text.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();

        // Written as escapes, not literals: `glyphs.rs` scans this source for
        // codepoints above the width-safe ceiling, and a pasted wide glyph here
        // would trip the very guard it is testing.
        let wide = "\u{306D}\u{3053}\u{304C}\u{3059}\u{304D}"; // five 2-column glyphs
        let mixed = format!("mixed{wide}mixed");
        for text in [wide, "plain ascii text here", mixed.as_str()] {
            for ellipsis in [UNICODE.ellipsis, crate::glyphs::ASCII.ellipsis] {
                for limit in 0..=20 {
                    let out = truncate(text, limit, ellipsis);
                    assert!(
                        columns(&out) <= limit,
                        "{out:?} is {} columns, over a limit of {limit}",
                        columns(&out),
                    );
                }
            }
        }
    }
    use octane_protocol::{ApprovalRequest, Item, ItemId, TurnId};

    fn completed(kind: ItemKind) -> Event {
        Event::Item(ItemEvent::Completed {
            turn_id: TurnId::new(),
            item: Item { id: ItemId::new(), kind, status: ItemStatus::Completed },
        })
    }

    fn text_of(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans.iter().map(|span| span.content.as_ref()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(event: &Event) -> Vec<Line<'static>> {
        render_event(event, &RenderOptions::default())
    }

    #[test]
    fn a_user_message_is_marked_and_indented() {
        let event = completed(ItemKind::UserMessage { text: "line one\nline two".into() });
        let rendered = text_of(&render(&event));
        assert!(rendered.contains("› line one"));
        assert!(rendered.contains("  line two"));
    }

    #[test]
    fn tool_calls_collapse_to_one_line() {
        let event = completed(ItemKind::ToolExecution {
            call_id: octane_protocol::ToolCallId::new(),
            name: "read".into(),
            input: r#"{"path":"src/main.rs"}"#.into(),
        });

        let lines = render(&event);
        assert_eq!(lines.len(), 1, "collapsed means one line");
        // The verb, not the tool name: the row says what happened.
        assert!(text_of(&lines).contains("Read"));
        assert!(text_of(&lines).contains("src/main.rs"));
    }

    #[test]
    fn bash_is_summarized_by_its_description_not_its_command() {
        // The description field exists so there is something human to show.
        let summary = summarize_input_with(
            "bash",
            r#"{"command":"cargo test --lib --all-features -- --nocapture","description":"Runs the test suite"}"#,
            &UNICODE,
        );
        assert_eq!(summary, "Runs the test suite");
    }

    #[test]
    fn bash_falls_back_to_the_command_when_undescribed() {
        assert_eq!(summarize_input_with("bash", r#"{"command":"ls -la"}"#, &UNICODE), "ls -la");
    }

    #[test]
    fn grep_summaries_mention_the_file_filter() {
        let summary =
            summarize_input_with("grep", r#"{"pattern":"TODO","glob":"**/*.rs"}"#, &UNICODE);
        assert!(summary.contains("TODO"));
        assert!(summary.contains("**/*.rs"));
    }

    #[test]
    fn unparseable_input_still_summarizes() {
        assert_eq!(summarize_input_with("read", "not json at all", &UNICODE), "not json at all");
    }

    #[test]
    fn long_summaries_are_truncated_to_one_line() {
        let summary =
            summarize_input_with("read", &format!(r#"{{"path":"{}"}}"#, "x".repeat(200)), &UNICODE);
        assert!(summary.chars().count() <= 60);
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn reasoning_is_hidden_by_default_and_showable() {
        let event = completed(ItemKind::Reasoning { text: "let me think".into() });
        assert!(render(&event).is_empty());

        let options = RenderOptions { reasoning: Reasoning::Shown, ..Default::default() };
        assert!(text_of(&render_event(&event, &options)).contains("let me think"));
    }

    #[test]
    fn errors_are_always_shown() {
        let event = completed(ItemKind::Error { message: "permission denied".into() });
        assert!(text_of(&render(&event)).contains("permission denied"));
    }

    #[test]
    fn compaction_is_announced_rather_than_silent() {
        let event = Event::Compaction {
            before_tokens: 167_000,
            after_tokens: 24_000,
            strategy: "summarize".into(),
        };
        let rendered = text_of(&render(&event));
        assert!(rendered.contains("compacted"));
        assert!(rendered.contains("167,000"));
    }

    #[test]
    fn in_flight_items_do_not_reach_scrollback() {
        // Only completed items are appended; anything still changing belongs in
        // the live region.
        let event = Event::Item(ItemEvent::Started {
            turn_id: TurnId::new(),
            item: Item {
                id: ItemId::new(),
                kind: ItemKind::AgentMessage { text: "partial".into() },
                status: ItemStatus::Streaming,
            },
        });
        assert!(render(&event).is_empty());

        let delta = Event::Item(ItemEvent::Delta {
            turn_id: TurnId::new(),
            item_id: ItemId::new(),
            text: "more".into(),
        });
        assert!(render(&delta).is_empty());
    }

    #[test]
    fn diff_headers_are_not_coloured_as_content() {
        let theme = Theme::default();
        let lines = render_diff("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n", &theme);

        // A numbered row carries the content style on its last span and the
        // dim number on the first; a header carries it on the line. Read
        // whichever is set, so the assertion is about the colour the user sees
        // rather than about which struct field holds it.
        let style_of = |index: usize| {
            let line: &Line<'static> = &lines[index];
            line.spans.last().and_then(|span| span.style.fg).or(line.style.fg)
        };
        assert_eq!(style_of(0), Some(theme.dim), "--- is a header, not a removal");
        assert_eq!(style_of(1), Some(theme.dim), "+++ is a header, not an addition");
        assert_eq!(style_of(3), Some(theme.removed));
        assert_eq!(style_of(4), Some(theme.added));
    }

    /// "Which line is this?" is the first question anyone asks of a diff in an
    /// approval prompt, and it cannot be answered without a number column.
    #[test]
    fn diff_lines_carry_the_line_number_they_have_in_the_file() {
        let theme = Theme::default();
        let lines = render_diff(
            "@@ -10,3 +10,4 @@
 context
-removed
+added
+also added
",
            &theme,
        );
        let text = |index: usize| {
            lines[index].spans.iter().map(|span| span.content.as_ref()).collect::<String>()
        };

        assert!(text(1).trim_start().starts_with("10 "), "context is line 10: {:?}", text(1));
        // The removal is line 11 of the *old* file; the addition that replaces
        // it is line 11 of the new one. Numbering both from one counter is the
        // easy way to get this wrong.
        assert!(text(2).trim_start().starts_with("11 "), "{:?}", text(2));
        assert!(text(3).trim_start().starts_with("11 "), "{:?}", text(3));
        assert!(text(4).trim_start().starts_with("12 "), "{:?}", text(4));
    }

    /// A diff with no hunk header still has to render, and every row must keep
    /// the same content column or the text edge visibly steps.
    #[test]
    fn a_diff_without_a_hunk_header_still_aligns() {
        let theme = Theme::default();
        let lines = render_diff("+just an added line
 context
", &theme);
        let width = |index: usize| {
            lines[index].spans.first().map(|span| span.content.chars().count()).unwrap_or(0)
        };
        assert_eq!(width(0), width(1), "the number column must not change width");
    }

    /// The diff for a `.rs` file should read like Rust, and the marker column
    /// must still say which side each row is on — the syntax palette covers the
    /// content, so a row coloured entirely by its language has lost the one
    /// thing a diff exists to show.
    #[test]
    fn diff_content_is_highlighted_without_losing_the_add_remove_signal() {
        let theme = Theme::new(crate::theme::ColorDepth::TrueColor);
        let lines = render_diff(
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n-let a = \"x\";\n+let b = \"y\";\n",
            &theme,
        );

        let sign = |index: usize| lines[index].spans[1].clone();
        assert_eq!(sign(3).content, "-");
        assert_eq!(sign(3).style.fg, Some(theme.removed));
        assert_eq!(sign(4).content, "+");
        assert_eq!(sign(4).style.fg, Some(theme.added));

        // The string literal is coloured as a string rather than as an addition.
        assert!(
            lines[4].spans[2..].iter().any(|span| span.style.fg == Some(theme.success)),
            "content was not highlighted: {:?}",
            lines[4].spans
        );
        // Removals are the side that is dimmed, which is the tint's stand-in and
        // is an attribute, so it survives a terminal with no colour to tint with.
        assert!(lines[3].spans[2..].iter().all(|s| s.style.add_modifier.contains(Modifier::DIM)));
        assert!(lines[4].spans[2..].iter().all(|s| !s.style.add_modifier.contains(Modifier::DIM)));
    }

    /// Highlighting splits a row into styled runs; it must not add, drop or
    /// reorder a character, and the number column must stay the same width.
    #[test]
    fn highlighting_a_diff_changes_no_text_and_no_column() {
        let source = "+++ b/a.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    let a = 1;\n+    let a = 2;\n";
        let theme = Theme::new(crate::theme::ColorDepth::TrueColor);
        let lines = render_diff(source, &theme);

        for (rendered, original) in lines.iter().zip(source.lines()) {
            let text: String = rendered.spans.iter().map(|s| s.content.as_ref()).collect();
            let (column, content) = text.split_at(3);
            assert_eq!(content, original, "text was altered");
            assert!(column.ends_with(' ') && column.len() == 3, "column moved: {column:?}");
        }
    }

    /// A path the highlighter cannot place must fall through to the plain
    /// renderer rather than being coloured by a guessed grammar.
    #[test]
    fn a_diff_without_a_usable_extension_is_not_highlighted() {
        assert_eq!(diff_language("+++ b/src/main.rs\n").as_deref(), Some("rs"));
        assert_eq!(diff_language("+++ b/Makefile\n"), None);
        assert_eq!(diff_language("+++ /dev/null\n"), None);
        assert_eq!(diff_language("@@ -1 +1 @@\n-a\n+b\n"), None);
    }

    #[test]
    fn approvals_are_rendered_with_their_summary() {
        let event = completed(ItemKind::Approval(ApprovalRequest {
            resource: "command(rm -rf build)".into(),
            summary: "Removes the build directory".into(),
            diff: None,
        }));
        assert!(text_of(&render(&event)).contains("Removes the build directory"));
    }

    #[test]
    fn the_ascii_glyph_set_reaches_the_transcript() {
        // The fallback is worthless if it only applies to the banner.
        let options = RenderOptions { glyphs: crate::glyphs::ASCII, ..Default::default() };
        let event = completed(ItemKind::UserMessage { text: "hello".into() });

        let rendered = text_of(&render_event(&event, &options));
        assert!(rendered.starts_with("> "), "got {rendered:?}");
        assert!(rendered.is_ascii());
    }

    #[test]
    fn a_clipped_line_never_exceeds_the_limit_in_either_glyph_set() {
        // The ASCII marker is three columns where the Unicode one is one. Room
        // for it has to come out of `limit`, or a line clipped to fit its box
        // is drawn wider than the box.
        let long = "x".repeat(200);
        for glyphs in [crate::glyphs::UNICODE, crate::glyphs::ASCII] {
            for limit in 0..12 {
                let clipped = truncate(&long, limit, glyphs.ellipsis);
                assert!(
                    clipped.chars().count() <= limit,
                    "{clipped:?} is wider than the {limit} columns asked for"
                );
            }
        }
        // The exact shape, so "fits" cannot be satisfied by clipping everything.
        assert_eq!(truncate(&long, 8, crate::glyphs::ASCII.ellipsis), "xxxxx...");
        assert_eq!(truncate(&long, 8, crate::glyphs::UNICODE.ellipsis), "xxxxxxx\u{2026}");
    }

    #[test]
    fn a_clipped_summary_carries_the_ascii_ellipsis() {
        // The negative control for the pair below: under the Unicode set the
        // same line does contain U+2026, so the assertion is not passing by
        // never having truncated at all.
        let options = RenderOptions { glyphs: crate::glyphs::ASCII, ..Default::default() };
        let call = |glyphs| {
            let event = completed(ItemKind::ToolExecution {
                call_id: octane_protocol::ToolCallId::new(),
                name: "read".into(),
                input: format!(r#"{{"path":"{}"}}"#, "x".repeat(200)),
            });
            text_of(&render_event(&event, &RenderOptions { glyphs, ..options }))
        };

        let ascii = call(crate::glyphs::ASCII);
        assert!(ascii.is_ascii(), "the fallback must reach the transcript: {ascii:?}");
        assert!(ascii.ends_with("..."), "and say the line was clipped: {ascii:?}");
        assert!(call(crate::glyphs::UNICODE).contains('\u{2026}'));
    }

    #[test]
    fn thousands_separates_correctly() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(167_000), "167,000");
    }

    #[test]
    fn an_ansi_escape_cannot_reach_the_transcript() {
        // ratatui filters graphemes by display width, not by control-ness. An
        // ESC measures zero and is dropped, so `\x1b[31m` loses only the ESC
        // and `[31m` is written into cells as ordinary text.
        let event = completed(ItemKind::AgentMessage {
            text: "\u{1b}[31mRED\u{1b}[0m plain".into(),
        });
        let rendered = text_of(&render(&event));

        assert!(rendered.contains("RED plain"));
        assert!(!rendered.contains("31m"), "escape body leaked: {rendered:?}");
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn a_tab_becomes_spaces_before_it_reaches_a_cell() {
        // A tab measures zero too, so it vanishes rather than aligning. Line
        // numbers separated from their content by one collapse into it.
        let event = completed(ItemKind::UserMessage { text: "62\tlast line".into() });
        let rendered = text_of(&render(&event));

        assert!(!rendered.contains('\t'));
        assert!(rendered.contains("62      last line"), "got {rendered:?}");
    }

    #[test]
    fn a_progress_bar_shows_only_what_it_settled_on() {
        // A bare carriage return rewrites its line, which is how progress bars
        // work. Keeping every intermediate state would show the user something
        // they never saw.
        let event = completed(ItemKind::AgentMessage { text: "10%\r50%\r100%".into() });
        let rendered = text_of(&render(&event));

        assert!(rendered.contains("100%"));
        assert!(!rendered.contains("10%"), "superseded output leaked: {rendered:?}");
    }

    #[test]
    fn a_tool_result_shows_its_summary_not_its_output() {
        // The whole point: `output` goes to the model and costs tokens, while
        // `title` and `metadata` go to the UI. Publishing the output put file
        // bodies and build logs in the transcript under the agent's own voice.
        let event = completed(ItemKind::ToolResult {
            call_id: octane_protocol::ToolCallId::new(),
            name: "read".into(),
            title: "flake.nix".into(),
            metadata: Some(serde_json::json!({ "lines_total": 62, "lines_shown": 62 })),
            is_error: false,
            body: String::new(),
        });
        let rendered = text_of(&render(&event));

        // Reports the size rather than the contents. The tool name is on the
        // call line above and is deliberately not repeated here.
        assert!(rendered.contains("62 lines"));
        assert!(!rendered.contains("<file"), "wire framing must not be shown");
    }

    #[test]
    fn a_failed_tool_is_marked_without_relying_on_colour() {
        // A red line is invisible under NO_COLOR and to a reader who cannot
        // see red, so the marker glyph has to carry it too.
        let failed = completed(ItemKind::ToolResult {
            call_id: octane_protocol::ToolCallId::new(),
            name: "bash".into(),
            title: "run the tests".into(),
            metadata: Some(serde_json::json!({ "exit_code": 101 })),
            is_error: true,
            body: String::new(),
        });
        let ok = completed(ItemKind::ToolResult {
            call_id: octane_protocol::ToolCallId::new(),
            name: "bash".into(),
            title: "run the tests".into(),
            metadata: Some(serde_json::json!({ "exit_code": 0 })),
            is_error: false,
            body: String::new(),
        });

        let failed = text_of(&render(&failed));
        let ok = text_of(&render(&ok));

        // Both hang off the same elbow now, so the distinguishing mark is the
        // failure glyph inside the block rather than the first character of the
        // row. What matters is unchanged: the difference must survive NO_COLOR,
        // so it has to be a character and not a hue.
        assert!(failed.contains(UNICODE.error), "a failure must carry its marker: {failed:?}");
        assert!(!ok.contains(UNICODE.error), "a success must not: {ok:?}");
        assert!(failed.contains("exit 101"), "and the words must say so: {failed:?}");
    }

    #[test]
    fn a_tool_call_outcome_survives_without_colour() {
        // Under NO_COLOR every colour becomes Reset, so anything encoded only
        // in the foreground disappears. octane's own success and error hues are
        // also close under deuteranopia, which affects roughly one in twelve
        // men even on a truecolor terminal.
        let call = |status| {
            Event::Item(ItemEvent::Completed {
                turn_id: TurnId::new(),
                item: Item {
                    id: ItemId::new(),
                    kind: ItemKind::ToolExecution {
                        call_id: octane_protocol::ToolCallId::new(),
                        name: "bash".into(),
                        input: "{\"description\":\"run the tests\"}".into(),
                    },
                    status,
                },
            })
        };

        let completed = text_of(&render(&call(ItemStatus::Completed)));
        let failed = text_of(&render(&call(ItemStatus::Failed)));
        let canceled = text_of(&render(&call(ItemStatus::Canceled)));

        let marker = |text: &str| text.chars().next();
        assert_ne!(marker(&completed), marker(&failed));
        assert_ne!(marker(&completed), marker(&canceled));
        assert_ne!(marker(&failed), marker(&canceled));
    }

    fn tool_result(name: &str, metadata: serde_json::Value, body: &str) -> Event {
        completed(ItemKind::ToolResult {
            call_id: octane_protocol::ToolCallId::new(),
            name: name.into(),
            title: "t".into(),
            metadata: Some(metadata),
            is_error: false,
            body: body.into(),
        })
    }

    #[test]
    fn an_edit_shows_both_sides_of_the_change() {
        // A change is the one thing a reader has to be able to check, and a
        // summary of it is unverifiable. The pair is exact rather than
        // reconstructed: the tool's contract is that `old` became `new`.
        let event = tool_result(
            "edit",
            serde_json::json!({ "removed": "let a = 1;", "added": "let a = 2;" }),
            "",
        );
        let rendered = text_of(&render(&event));
        assert!(rendered.contains("- let a = 1;"));
        assert!(rendered.contains("+ let a = 2;"));
    }

    #[test]
    fn a_diff_is_marked_without_relying_on_colour() {
        // Red and green are the same line under NO_COLOR, and octane's two
        // hues are close under deuteranopia.
        let event = tool_result(
            "edit",
            serde_json::json!({ "removed": "old", "added": "new" }),
            "",
        );
        let rendered = text_of(&render(&event));
        assert!(rendered.lines().any(|l| l.trim_start().starts_with("- ")));
        assert!(rendered.lines().any(|l| l.trim_start().starts_with("+ ")));
    }

    #[test]
    fn other_tools_are_truncated_and_say_how_much_was_left() {
        // A lookup's value is in the answer, not in the transcript. Silent
        // truncation reads as a short result rather than a clipped one.
        let body = (1..=30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let event = tool_result("grep", serde_json::json!({}), &body);
        let rendered = text_of(&render(&event));

        // Head and tail, not the first N: the head says what started and the
        // tail says how it ended. The middle is what gets dropped, and the
        // count says how much.
        assert!(rendered.contains("line 1"), "the head survives: {rendered:?}");
        assert!(rendered.contains("line 30"), "and so does the tail: {rendered:?}");
        assert!(!rendered.contains("line 15"), "the middle is what goes: {rendered:?}");
        assert!(rendered.contains("+22 lines"), "and it says how much: {rendered:?}");
    }

    #[test]
    fn a_read_preview_does_not_show_the_model_facing_framing() {
        // `read` wraps its output in `<file path=...>` for the model. A tool
        // that publishes a preview has already stripped that, so the preview
        // wins over the raw output.
        let event = tool_result(
            "read",
            serde_json::json!({ "preview": "fn main() {}" }),
            "<file path=\"a.rs\">\nfn main() {}\n</file>",
        );
        let rendered = text_of(&render(&event));
        assert!(rendered.contains("fn main()"));
        assert!(!rendered.contains("<file"), "framing must not reach the screen");
    }
}
