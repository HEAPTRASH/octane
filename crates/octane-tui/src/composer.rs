//! The input box.
//!
//! Pure state: characters in, lines out. No terminal, no I/O, so every editing
//! rule below is testable by calling a method.
//!
//! Three prefixes are recognized, following opencode (`RESEARCH.md` §I). Each
//! exists to avoid spending an inference round trip on something the user already
//! knows they want:
//!
//! | Prefix | Meaning |
//! |---|---|
//! | `/` | slash command — expands to a prompt, or runs a client action |
//! | `!` | shell command — runs directly, output attached as a tool result |
//! | `@` | file reference — content pulled into the message |
//!
//! The word-boundary rule and its separator set are derived from Codex CLI's
//! `codex-rs/tui/src/bottom_pane/textarea.rs` (`is_word_separator`,
//! `split_word_pieces`), reimplemented here without its dependency on
//! `unicode-segmentation`.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// What the user asked for when they pressed enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Ordinary prompt. `file_references` are the `@paths` mentioned in it.
    Prompt { text: String, file_references: Vec<String> },
    /// `/name args`
    Command { name: String, args: String },
    /// `!command`
    Shell { command: String },
}

#[derive(Debug, Default)]
pub struct Composer {
    /// Current text. Multi-line: shift+enter inserts a newline, enter submits.
    text: String,
    /// Caret position, a byte offset into `text`.
    cursor: usize,
    /// Previously submitted entries, newest last.
    history: Vec<String>,
    /// Position while browsing history; `None` means editing fresh text.
    history_index: Option<usize>,
    /// Text set aside when history browsing started, so it can be restored.
    stashed: Option<String>,
    /// The last killed text, for ctrl+y. Deliberately survives [`Composer::submit`]:
    /// a kill is the only copy of what it removed, and losing it to Enter is how
    /// people lose a paragraph.
    kill_buffer: String,
    /// The previous action was a kill, so the next one accumulates rather than
    /// replacing. Two ctrl+k presses must yank back as two lines.
    killing: bool,
    /// States to undo to, oldest first, each a `(text, cursor)` pair.
    undo_stack: Vec<(String, usize)>,
    redo_stack: Vec<(String, usize)>,
    /// The previous edit was a single-character insert, so the next one joins its
    /// undo step. Without this every keystroke is a step and undo is useless.
    coalescing: bool,
}

/// Undo steps kept. A draft is small and coalescing keeps typing to one step per
/// word-ish run, so this is far more than anyone reaches for.
const UNDO_DEPTH: usize = 100;

/// Characters that form a word of their own rather than joining the one beside
/// them, from Codex's `WORD_SEPARATORS`.
///
/// This is what makes deleting through `foo::bar()` behave: `()`, then `bar`,
/// then `::`, then `foo`. Treating a run of punctuation as part of the adjacent
/// word instead swallows the whole token on one press.
const WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Space,
    Separator,
    Word,
}

fn class(ch: char) -> Class {
    if ch.is_whitespace() {
        Class::Space
    } else if WORD_SEPARATORS.contains(ch) {
        Class::Separator
    } else {
        Class::Word
    }
}

impl Composer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Lines as displayed, for the caller to render.
    pub fn lines(&self) -> Vec<&str> {
        if self.text.is_empty() {
            return vec![""];
        }
        self.text.split('\n').collect()
    }

    /// Caret as `(line, column)` in characters, for cursor placement.
    ///
    /// Columns are counted in characters rather than bytes: placing the terminal
    /// cursor by byte offset puts it in the wrong column the moment anyone types
    /// a non-ASCII character.
    pub fn cursor_position(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.matches('\n').count();
        let column = before.rsplit('\n').next().unwrap_or("").chars().count();
        (line, column)
    }

    pub fn insert(&mut self, ch: char) {
        // A newline ends the run, so undo lands on line boundaries rather than
        // rewinding a whole paragraph in one press.
        self.checkpoint(ch != '\n');
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.leave_history();
    }

    pub fn insert_str(&mut self, text: &str) {
        self.checkpoint(false);
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.leave_history();
    }

    /// Insert a literal newline. Bound to shift+enter, so plain enter can submit.
    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.checkpoint(false);
        let previous = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.leave_history();
    }

    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.checkpoint(false);
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
        self.text.replace_range(self.cursor..next, "");
        self.leave_history();
    }

    /// Place the caret, ending both the kill run and the insert run.
    ///
    /// Both are defined by contiguity, exactly as in readline: moving away and
    /// back means the next kill starts a fresh buffer and the next character
    /// typed starts a fresh undo step. Every motion goes through here so none of
    /// them can forget.
    fn set_cursor(&mut self, at: usize) {
        self.cursor = at;
        self.killing = false;
        self.coalescing = false;
    }

    pub fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.set_cursor(index);
        }
    }

    pub fn move_right(&mut self) {
        if let Some((index, ch)) = self.text[self.cursor..].char_indices().next() {
            self.set_cursor(self.cursor + index + ch.len_utf8());
        }
    }

    /// Home: start of the current visual line, not of the whole buffer.
    pub fn move_line_start(&mut self) {
        let start = self.line_start();
        self.set_cursor(start);
    }

    pub fn move_line_end(&mut self) {
        let end = self.line_end();
        self.set_cursor(end);
    }

    /// Alt+Left. A run of punctuation is its own word, so this walks
    /// `foo::bar()` in four steps rather than one.
    pub fn move_word_left(&mut self) {
        let start = self.word_start();
        self.set_cursor(start);
    }

    /// Alt+Right.
    pub fn move_word_right(&mut self) {
        let end = self.word_end();
        self.set_cursor(end);
    }

    /// Ctrl+W. The removed text goes to the kill buffer, so a mis-aimed one is
    /// recoverable with ctrl+y.
    pub fn delete_word_backward(&mut self) {
        let start = self.word_start();
        self.kill(start..self.cursor, true);
    }

    /// Alt+D.
    pub fn delete_word_forward(&mut self) {
        let end = self.word_end();
        self.kill(self.cursor..end, false);
    }

    /// Ctrl+U. Already at the start of a line, it takes the newline before it,
    /// so holding the key keeps making progress instead of stalling.
    pub fn kill_to_line_start(&mut self) {
        let start = self.line_start();
        if start == self.cursor {
            let joined = self.cursor.saturating_sub(1);
            self.kill(joined..self.cursor, true);
            return;
        }
        self.kill(start..self.cursor, true);
    }

    /// Ctrl+K, with the same end-of-line rule in the other direction.
    pub fn kill_to_line_end(&mut self) {
        let end = self.line_end();
        if end == self.cursor {
            let joined = (self.cursor + 1).min(self.text.len());
            self.kill(self.cursor..joined, false);
            return;
        }
        self.kill(self.cursor..end, false);
    }

    /// Ctrl+Y — put the last kill back at the caret.
    pub fn yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let text = self.kill_buffer.clone();
        self.insert_str(&text);
        // `insert_str` is an ordinary edit and clears the kill state; the buffer
        // itself must survive, or yanking twice pastes only once.
        self.kill_buffer = text;
    }

    /// Ctrl+Z.
    pub fn undo(&mut self) -> bool {
        let Some((text, cursor)) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push((std::mem::replace(&mut self.text, text), self.cursor));
        self.set_cursor(cursor.min(self.text.len()));
        self.leave_history();
        true
    }

    /// Ctrl+Shift+Z. Not ctrl+y: that is the yank, which is the older habit.
    pub fn redo(&mut self) -> bool {
        let Some((text, cursor)) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push((std::mem::replace(&mut self.text, text), self.cursor));
        self.set_cursor(cursor.min(self.text.len()));
        self.leave_history();
        true
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|index| index + 1).unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset)
            .unwrap_or(self.text.len())
    }

    /// Start of the word before the caret, skipping any whitespace on the way.
    ///
    /// Byte offsets throughout, and always on a character boundary: the text is
    /// a `String`, and slicing it mid-character panics.
    fn word_start(&self) -> usize {
        let prefix = &self.text[..self.cursor];
        let mut characters = prefix.char_indices().rev().skip_while(|(_, ch)| ch.is_whitespace());
        let Some((index, ch)) = characters.next() else {
            return 0;
        };

        let kind = class(ch);
        let mut start = index;
        for (index, ch) in characters {
            if class(ch) != kind {
                break;
            }
            start = index;
        }
        start
    }

    /// End of the word after the caret, likewise skipping leading whitespace.
    fn word_end(&self) -> usize {
        let suffix = &self.text[self.cursor..];
        let mut characters = suffix.char_indices().skip_while(|(_, ch)| ch.is_whitespace());
        let Some((index, ch)) = characters.next() else {
            return self.text.len();
        };

        let kind = class(ch);
        let mut end = index + ch.len_utf8();
        for (index, ch) in characters {
            if class(ch) != kind {
                break;
            }
            end = index + ch.len_utf8();
        }
        self.cursor + end
    }

    /// Remove a range, saving it for the yank.
    ///
    /// Consecutive kills accumulate in the direction they were made, which is
    /// what makes two ctrl+k presses yank back as two lines rather than one.
    fn kill(&mut self, range: Range<usize>, backwards: bool) {
        if range.start >= range.end {
            return;
        }
        let removed = self.text[range.clone()].to_string();
        // Read before the checkpoint, which ends the run every other edit is
        // supposed to end.
        let accumulate = self.killing;
        self.checkpoint(false);

        if accumulate {
            if backwards {
                self.kill_buffer.insert_str(0, &removed);
            } else {
                self.kill_buffer.push_str(&removed);
            }
        } else {
            self.kill_buffer = removed;
        }

        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.killing = true;
        self.leave_history();
    }

    /// Record the state an undo returns to.
    ///
    /// `coalesce` marks an edit that may join the previous step — a single typed
    /// character. Anything else is its own step.
    fn checkpoint(&mut self, coalesce: bool) {
        // Any edit abandons the redo branch; keeping it would let ctrl+shift+z
        // paste in text that no longer belongs to what is on screen.
        self.redo_stack.clear();
        self.killing = false;

        if !(coalesce && self.coalescing) {
            self.undo_stack.push((self.text.clone(), self.cursor));
            if self.undo_stack.len() > UNDO_DEPTH {
                self.undo_stack.remove(0);
            }
        }
        self.coalescing = coalesce;
    }

    /// Whether the draft ends with a lone backslash.
    ///
    /// The portable newline escape hatch: no terminal support required, and it
    /// is the shell continuation people already know.
    pub fn ends_with_continuation(&self) -> bool {
        self.text.ends_with('\\') && !self.text.ends_with("\\\\")
    }

    /// Turn a trailing backslash into a newline.
    pub fn continue_line(&mut self) {
        if !self.ends_with_continuation() {
            return;
        }
        self.checkpoint(false);
        let backslash = self.text.len() - 1;
        self.text.replace_range(backslash.., "\n");
        self.cursor = self.text.len();
        self.leave_history();
    }

    /// Replace the whole buffer, e.g. when a completion is accepted.
    pub fn set_text(&mut self, text: String, cursor: usize) {
        self.checkpoint(false);
        self.cursor = cursor.min(text.len());
        self.text = text;
        self.leave_history();
    }

    /// Move up one display row, keeping the column where it can.
    ///
    /// By *display* row, not by logical line: a long paragraph with no newlines
    /// in it still occupies several rows on screen, and an up-arrow that skipped
    /// the whole thing made a wrapped draft uneditable above its last row.
    pub fn move_up(&mut self, usable: usize) {
        self.step_row(usable, -1);
    }

    /// Move down one display row. The counterpart to [`Self::move_up`], absent
    /// until now — so a draft could be walked up and not back down.
    pub fn move_down(&mut self, usable: usize) {
        self.step_row(usable, 1);
    }

    fn step_row(&mut self, usable: usize, delta: isize) {
        let rows = display_rows(&self.text, usable);
        let Some(current) = rows.iter().position(|row| self.row_contains(row)) else {
            return;
        };
        let Some(target) = current.checked_add_signed(delta).filter(|i| *i < rows.len()) else {
            return;
        };

        // The column within the current row, in characters.
        let column = self.text[rows[current].start..self.cursor].chars().count();
        let destination = &rows[target];
        let offset = self.text[destination.clone()]
            .char_indices()
            .nth(column)
            .map(|(index, _)| index)
            // Shorter row: land at its end rather than overshooting into the next.
            .unwrap_or(destination.len());
        self.set_cursor(destination.start + offset);
    }

    fn row_contains(&self, row: &std::ops::Range<usize>) -> bool {
        // Inclusive of the end, so a caret at the end of a row is on that row
        // rather than nowhere.
        self.cursor >= row.start && self.cursor <= row.end
    }

    /// Which display row the caret is on, and how many there are.
    ///
    /// The keymap needs both: up browses history only from the first row, and
    /// down leaves history only past the last.
    pub fn row_position(&self, usable: usize) -> (usize, usize) {
        let rows = display_rows(&self.text, usable);
        let at = rows.iter().position(|row| self.row_contains(row)).unwrap_or(0);
        (at, rows.len())
    }

    /// Discard the whole draft.
    pub fn clear(&mut self) {
        self.checkpoint(false);
        self.text.clear();
        self.cursor = 0;
        self.leave_history();
    }

    /// Up arrow. Only browses history when the caret is on the first line, so
    /// arrow keys still navigate a multi-line draft.
    pub fn history_previous(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        if self.history_index.is_none() && self.cursor_position().0 != 0 {
            return false;
        }

        let next_index = match self.history_index {
            None => {
                self.stashed = Some(self.text.clone());
                self.history.len() - 1
            }
            Some(0) => return true, // already at the oldest
            Some(index) => index - 1,
        };

        self.history_index = Some(next_index);
        self.text = self.history[next_index].clone();
        let end = self.text.len();
        self.set_cursor(end);
        true
    }

    /// Down arrow. Walking off the newest entry restores the stashed draft, so
    /// history browsing never destroys what was being typed.
    pub fn history_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };

        if index + 1 >= self.history.len() {
            self.history_index = None;
            self.text = self.stashed.take().unwrap_or_default();
            let end = self.text.len();
            self.set_cursor(end);
            return true;
        }

        self.history_index = Some(index + 1);
        self.text = self.history[index + 1].clone();
        let end = self.text.len();
        self.set_cursor(end);
        true
    }

    /// Consume the buffer and classify it. `None` when there is nothing to send.
    pub fn submit(&mut self) -> Option<Submission> {
        let raw = self.text.trim().to_string();
        if raw.is_empty() {
            return None;
        }

        // Consecutive duplicates are not recorded: re-running the same command
        // twice should not make the user press up twice to reach the one before.
        if self.history.last() != Some(&raw) {
            self.history.push(raw.clone());
        }
        self.text.clear();
        self.set_cursor(0);
        self.leave_history();
        // The sent prompt is gone, not undoable: ctrl+z bringing back a message
        // the model is already answering would be a lie about the state.
        self.undo_stack.clear();
        self.redo_stack.clear();

        Some(classify(&raw))
    }

    fn leave_history(&mut self) {
        self.history_index = None;
        self.stashed = None;
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }
}

/// Decide what a submitted line means.
fn classify(raw: &str) -> Submission {
    if let Some(command) = raw.strip_prefix('!') {
        return Submission::Shell { command: command.trim().to_string() };
    }

    if let Some(rest) = raw.strip_prefix('/') {
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        return Submission::Command {
            name: name.to_string(),
            args: args.trim().to_string(),
        };
    }

    Submission::Prompt { text: raw.to_string(), file_references: file_references(raw) }
}

/// Extract `@path` references.
///
/// Requires the `@` to start a word, so an email address or a Rust attribute in
/// pasted code does not turn into a file read. The same reasoning as memory
/// imports in `octane-memory`.
pub fn file_references(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();

    for (index, _) in text.match_indices('@') {
        let starts_word = index == 0 || bytes[index - 1].is_ascii_whitespace();
        if !starts_word {
            continue;
        }

        let rest = &text[index + 1..];
        let end = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
        // Trailing sentence punctuation is almost never part of the path.
        let path = rest[..end].trim_end_matches(['.', ',', ';', ':', '?', '!', ')']);

        if !path.is_empty() && !found.iter().any(|existing| existing == path) {
            found.push(path.to_string());
        }
    }
    found
}

/// Composer rows before it stops growing and scrolls internally.
///
/// Generous, because shift+enter exists so people can write real paragraphs.
pub const MAX_ROWS: u16 = 12;

/// Blank rows above and below the draft, inside the filled band.
const PADDING_ROWS: u16 = 1;

/// Columns the `\u{203a}` marker and its trailing space occupy.
const MARKER_COLS: u16 = 2;

/// The draft box: bordered, growing with the text, caret tracking the wrap.
#[derive(Debug, Clone, Copy)]
pub struct ComposerPane<'a> {
    pub composer: &'a Composer,
    pub options: &'a crate::render::RenderOptions,
}

impl ComposerPane<'_> {
    /// Where the caret sits inside `area`, following the same wrap the box was
    /// sized with. Not on [`Pane`]: the composer is the only pane with a caret,
    /// and a trait method with one implementor is a method on that type.
    ///
    /// [`Pane`]: crate::component::Pane
    pub fn caret(&self, area: Rect) -> (u16, u16) {
        let usable = usable_width(area.width);
        let text = self.composer.text();
        let rows = display_rows(text, usable);

        // The same rows the render just drew, so the caret cannot land on a
        // different break than the text did.
        let cursor = self.composer.cursor();
        let row = rows
            .iter()
            .position(|row| cursor >= row.start && cursor <= row.end)
            .unwrap_or(rows.len().saturating_sub(1));
        let column = text[rows[row].start..cursor].chars().count();

        (
            area.x + MARKER_COLS + u16::try_from(column).unwrap_or(0),
            area.y + PADDING_ROWS + u16::try_from(row).unwrap_or(0),
        )
    }
}

impl crate::component::Pane for ComposerPane<'_> {
    /// Counts *wrapped* rows, plus a row of padding above and below.
    ///
    /// The padding is why the band reads as an input rather than as another
    /// line of transcript: it is the only thing separating the caret from the
    /// output above it now that the border is gone. Codex spends the same two
    /// rows for the same reason.
    fn constraint(&self, width: u16) -> Constraint {
        let rows = wrapped_rows(self.composer.lines().into_iter(), usable_width(width));
        let rows = u16::try_from(rows).unwrap_or(MAX_ROWS).clamp(1, MAX_ROWS);
        Constraint::Length(rows + PADDING_ROWS * 2)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let (theme, glyphs) = (&self.options.theme, &self.options.glyphs);

        // The band is filled edge to edge and lifted off the canvas. The canvas
        // itself no longer paints — it resets to the terminal's own background —
        // so this fill is what makes the input a distinct surface rather than
        // more transcript, and it is the one place a background earns its keep.
        let band = theme.panel();
        buf.set_style(area, band);

        // The text gets its own column range, starting past the marker. Wrapping
        // the marker *inside* the paragraph made every continuation row wrap
        // back to column 0 instead of aligning under the text — and the caret,
        // which assumes each row starts past the marker, then drifted a
        // character further left on every wrapped row.
        let text_area = Rect {
            x: area.x + MARKER_COLS,
            y: area.y + PADDING_ROWS,
            width: area.width.saturating_sub(MARKER_COLS),
            height: area.height.saturating_sub(PADDING_ROWS * 2),
        };
        if text_area.height == 0 || text_area.width == 0 {
            return;
        }

        // Drawn once, beside the text rather than in it, so it cannot take part
        // in the wrap.
        buf.set_span(
            area.x,
            text_area.y,
            &Span::styled(
                format!("{} ", glyphs.prompt),
                band.fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            MARKER_COLS,
        );

        let lines: Vec<Line> = if self.composer.is_empty() {
            // A placeholder rather than a bare marker. An empty input is the one
            // state where a new user cannot tell whether the program is waiting
            // for them or busy.
            vec![Line::from(Span::styled("ask octane to do something", band.fg(theme.dim)))]
        } else {
            // Rows from the shared layout, not from `Paragraph`'s own wrap.
            // Letting ratatui wrap meant it broke at words while the caret
            // counted characters, and the cursor drifted a word out of place
            // the moment a line wrapped at a space.
            let text = self.composer.text();
            display_rows(text, usable_width(area.width))
                .into_iter()
                .map(|row| Line::from(Span::styled(text[row].to_string(), band)))
                .collect()
        };

        Paragraph::new(lines).style(band).render(text_area, buf);
    }
}

/// Byte ranges of the display rows the draft occupies at `usable` columns.
///
/// **The single source of truth for the composer's geometry.** Rendering, the
/// caret, and up/down motion all read it, which is the only way they can agree.
///
/// They did not, and it showed: the box was drawn with ratatui's `Wrap`, which
/// breaks at *word* boundaries, while the caret divided the cursor offset by the
/// row width — a *character* break. The two coincide only until a line wraps at
/// a space, after which the cursor sat a word away from the text it was editing.
/// Hard-wrapping here rather than word-wrapping is deliberate: it keeps the row
/// a caret lands on computable, and a draft is being typed rather than read.
pub fn display_rows(text: &str, usable: usize) -> Vec<std::ops::Range<usize>> {
    let usable = usable.max(1);
    let mut rows = Vec::new();

    for logical in text.split_inclusive('\n') {
        let start = logical.as_ptr() as usize - text.as_ptr() as usize;
        let body = logical.strip_suffix('\n').unwrap_or(logical);

        let mut taken = 0usize;
        let mut offset = 0usize;
        let mut row_start = 0usize;
        for ch in body.chars() {
            if taken == usable {
                rows.push(start + row_start..start + offset);
                row_start = offset;
                taken = 0;
            }
            taken += 1;
            offset += ch.len_utf8();
        }
        // Always at least one row, so an empty line still has somewhere to put
        // the caret.
        rows.push(start + row_start..start + body.len());
    }

    if rows.is_empty() {
        rows.push(0..0);
    }
    rows
}

/// Columns of the composer that hold text: two of border plus one of padding
/// do not. Shared with the caret, or changing it here moves the box without
/// moving the cursor.
pub fn usable_width(width: u16) -> usize {
    // Exactly the columns the text paragraph is given, so the caret's idea of
    // where a row ends is the same as the wrap's.
    usize::from(width.saturating_sub(MARKER_COLS)).max(1)
}

/// Screen rows those lines occupy once wrapped. An empty line is still a row.
pub fn wrapped_rows<'a>(lines: impl Iterator<Item = &'a str>, usable: usize) -> usize {
    lines.map(|line| line.chars().count().div_ceil(usable).max(1)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Pane;

    /// The bug this exists for: the box was drawn with ratatui's word wrap while
    /// the caret divided the cursor offset by the row width. They agree until a
    /// line breaks at a space — after which the cursor sat a word away from the
    /// character it was actually editing.
    #[test]
    fn the_caret_follows_the_same_break_the_text_was_drawn_with() {
        let width = 30u16;
        let mut composer = Composer::new();
        // A long unbroken token, then a space, then a short word: the exact
        // shape that makes a word wrap and a character wrap disagree.
        composer.insert_str(&format!("{} hello", "x".repeat(40)));

        let options = crate::render::RenderOptions::default();
        let pane = ComposerPane { composer: &composer, options: &options };
        let Constraint::Length(height) = pane.constraint(width) else { panic!("fixed") };
        let area = ratatui::layout::Rect::new(0, 0, width, height);

        let mut buf = ratatui::buffer::Buffer::empty(area);
        pane.render(area, &mut buf);
        let (cx, cy) = pane.caret(area);

        // The caret sits immediately after the last character drawn, which is
        // the only definition of "correct" that does not restate the algorithm.
        let row: String = (0..width).map(|x| buf[(x, cy)].symbol()).collect();
        let drawn = row.trim_end().chars().count() as u16;
        assert_eq!(
            cx,
            MARKER_COLS + drawn - MARKER_COLS.min(drawn),
            "caret at {cx} but row {cy} holds {drawn} columns: {row:?}",
        );
    }

    /// Up and down move by display row, so a wrapped draft is navigable inside
    /// itself. Down did not exist at all: a draft could be walked up and never
    /// back down.
    #[test]
    fn up_and_down_walk_the_display_rows_of_a_wrapped_draft() {
        let usable = 20usize;
        let mut composer = Composer::new();
        composer.insert_str(&"y".repeat(55)); // three rows at this width

        let (start, total) = composer.row_position(usable);
        assert!(total >= 3, "the draft should wrap to several rows, got {total}");
        assert_eq!(start, total - 1, "the caret begins on the last row");

        composer.move_up(usable);
        assert_eq!(composer.row_position(usable).0, total - 2);
        composer.move_up(usable);
        assert_eq!(composer.row_position(usable).0, total - 3);

        composer.move_down(usable);
        assert_eq!(composer.row_position(usable).0, total - 2, "and back down again");
    }

    /// Motion stops at the edges rather than wrapping around or panicking.
    #[test]
    fn motion_at_the_edges_of_the_draft_does_nothing() {
        let usable = 20usize;
        let mut composer = Composer::new();
        composer.insert_str("one line");

        composer.move_up(usable);
        assert_eq!(composer.row_position(usable).0, 0);
        composer.move_down(usable);
        assert_eq!(composer.row_position(usable).0, 0);
    }

    /// The bug this fixes: the marker used to wrap *with* the text, so the
    /// second row of a long draft started at column 0 while the caret still
    /// counted from past the marker — the cursor drifted further left on every
    /// wrapped row.
    #[test]
    fn a_wrapped_draft_keeps_its_rows_aligned_and_its_caret_on_them() {
        let width = 24u16;
        let mut composer = Composer::new();
        // Comfortably past one row at this width.
        composer.insert_str(&"x".repeat(40));

        let options = crate::render::RenderOptions::default();
        let pane = ComposerPane { composer: &composer, options: &options };
        let Constraint::Length(height) = pane.constraint(width) else { panic!("fixed") };

        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        pane.render(area, &mut buf);

        let row = |y: u16| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>();

        // The marker is on the first text row only, and nothing but padding
        // sits to the left of the text on the rows below it.
        assert!(row(1).starts_with('\u{203a}'), "{:?}", row(1));
        for y in 2..height - PADDING_ROWS {
            let text = row(y);
            assert!(
                text.starts_with("  "),
                "row {y} must align under the text, got {text:?}",
            );
            assert!(!text.trim().is_empty(), "row {y} should carry wrapped text");
        }

        // And the caret lands inside the band, on a text row, never in the
        // marker columns.
        let (x, y) = pane.caret(area);
        assert!(x >= area.x + MARKER_COLS, "caret {x} is in the marker column");
        assert!(x < area.right(), "caret {x} ran past the band");
        assert!(y >= area.y + PADDING_ROWS && y < area.bottom() - PADDING_ROWS, "caret row {y}");
    }

    /// The band is what makes the input read as an input now that the border is
    /// gone: filled edge to edge, lifted off the canvas, and padded so the caret
    /// is not flush against the transcript above it.
    #[test]
    fn the_input_is_a_filled_band_with_a_row_of_padding_each_side() {
        let composer = Composer::new();
        let options = crate::render::RenderOptions::default();
        let pane = ComposerPane { composer: &composer, options: &options };

        let Constraint::Length(height) = pane.constraint(40) else { panic!("fixed height") };
        assert_eq!(height, 3, "one text row between two of padding");

        let area = ratatui::layout::Rect::new(0, 0, 40, height);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        pane.render(area, &mut buf);

        // Every cell, including the padding rows and the full width of them.
        for y in 0..height {
            for x in 0..40 {
                assert_eq!(
                    buf[(x, y)].style().bg,
                    Some(options.theme.surface),
                    "cell ({x},{y}) is not part of the band",
                );
            }
        }

        let row = |y: u16| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert!(row(0).trim().is_empty(), "top row is padding: {:?}", row(0));
        assert!(row(1).starts_with('\u{203a}'), "the marker leads the text row: {:?}", row(1));
        assert!(row(2).trim().is_empty(), "bottom row is padding: {:?}", row(2));
    }

    /// The caret has to land on the text row, not in the padding above it.
    #[test]
    fn the_caret_sits_on_the_text_row_inside_the_band() {
        let mut composer = Composer::new();
        composer.insert_str("hi");
        let options = crate::render::RenderOptions::default();
        let pane = ComposerPane { composer: &composer, options: &options };

        let Constraint::Length(height) = pane.constraint(40) else { panic!("fixed height") };
        let area = ratatui::layout::Rect::new(0, 0, 40, height);
        let (x, y) = pane.caret(area);

        assert_eq!(y, area.y + 1, "past the top padding");
        assert_eq!(x, area.x + 2 + 2, "past the marker, after the two typed chars");
    }
    use ratatui::layout::Constraint;

    /// Heights go through the pane, because the pane is what the layout asks.
    /// A helper that measured the same thing a second way would pass while the
    /// screen was wrong.
    fn rows(composer: &Composer, width: u16) -> u16 {
        let options = crate::render::RenderOptions::default();
        match (ComposerPane { composer, options: &options }).constraint(width) {
            // Less the band's padding, so these assertions stay about how the
            // text wraps rather than about the chrome around it.
            Constraint::Length(rows) => rows - PADDING_ROWS * 2,
            other => panic!("composer must ask for a fixed height, got {other:?}"),
        }
    }

    #[test]
    fn the_composer_grows_with_the_draft() {
        let mut composer = Composer::new();
        assert_eq!(rows(&composer, 80), 1);

        // This is what shift+enter does, and the box must follow it.
        for expected in 2..=6 {
            composer.newline();
            assert_eq!(rows(&composer, 80), expected);
        }
    }

    #[test]
    fn a_long_line_grows_the_box_by_wrapping() {
        // The bug this fixes: sizing by newline count leaves the box one row
        // tall while a pasted paragraph runs off the end of it.
        let mut composer = Composer::new();
        composer.insert_str(&"x".repeat(200));
        assert!(rows(&composer, 40) > 1, "a wrapped line must grow the box");
    }

    #[test]
    fn wrapping_is_measured_against_the_usable_width() {
        let mut composer = Composer::new();
        composer.insert_str(&"x".repeat(78));
        // 80 wide, less the rail and the space after it.
        assert_eq!(rows(&composer, 80), 1);

        composer.insert_str("x");
        assert_eq!(rows(&composer, 80), 2);
    }

    #[test]
    fn a_narrow_terminal_does_not_divide_by_zero() {
        let mut composer = Composer::new();
        composer.insert_str("some text");
        for width in 0..=4 {
            assert!(rows(&composer, width) >= 1);
        }
    }

    #[test]
    fn a_runaway_draft_is_capped() {
        let mut composer = Composer::new();
        for _ in 0..100 {
            composer.newline();
        }
        assert_eq!(rows(&composer, 80), MAX_ROWS, "the transcript must keep some rows");
    }

    /// The caret must land inside the box the constraint asked for. When the two
    /// were separate functions this was the drift nobody saw until a wrapped
    /// draft put the cursor a row below its own border.
    #[test]
    fn the_caret_stays_inside_the_box_the_pane_asked_for() {
        let mut composer = Composer::new();
        composer.insert_str(&"x".repeat(200));
        let options = crate::render::RenderOptions::default();
        let pane = ComposerPane { composer: &composer, options: &options };

        let Constraint::Length(height) = pane.constraint(40) else { panic!("fixed height") };
        let area = Rect::new(0, 0, 40, height);
        let (x, y) = pane.caret(area);

        assert!(x < area.right(), "caret {x} past the right border");
        assert!(y < area.bottom(), "caret row {y} past the bottom border");
    }

    fn typed(text: &str) -> Composer {
        let mut composer = Composer::new();
        composer.insert_str(text);
        composer
    }

    #[test]
    fn typing_and_submitting_yields_a_prompt() {
        let mut composer = typed("fix the bug");
        assert_eq!(
            composer.submit(),
            Some(Submission::Prompt { text: "fix the bug".into(), file_references: vec![] })
        );
        assert!(composer.is_empty());
    }

    #[test]
    fn an_empty_submission_does_nothing() {
        assert_eq!(Composer::new().submit(), None);
        assert_eq!(typed("   \n  ").submit(), None);
    }

    #[test]
    fn slash_prefix_is_a_command() {
        assert_eq!(
            typed("/review 42 --verbose").submit(),
            Some(Submission::Command { name: "review".into(), args: "42 --verbose".into() })
        );
        assert_eq!(
            typed("/help").submit(),
            Some(Submission::Command { name: "help".into(), args: String::new() })
        );
    }

    #[test]
    fn bang_prefix_is_a_shell_command() {
        assert_eq!(
            typed("!git status").submit(),
            Some(Submission::Shell { command: "git status".into() })
        );
    }

    #[test]
    fn file_references_are_extracted() {
        let Some(Submission::Prompt { file_references, .. }) =
            typed("how does @src/main.rs call @src/lib.rs?").submit()
        else {
            panic!("expected a prompt");
        };
        assert_eq!(file_references, vec!["src/main.rs", "src/lib.rs"]);
    }

    #[test]
    fn mid_word_at_signs_are_not_file_references() {
        // Email addresses and pasted attributes must not trigger file reads.
        assert!(file_references("mail me at bob@example.com").is_empty());
        assert!(file_references("#[serde(rename@x)]").is_empty());
    }

    #[test]
    fn duplicate_references_are_collapsed() {
        assert_eq!(file_references("@a.rs and again @a.rs"), vec!["a.rs"]);
    }

    #[test]
    fn shift_enter_inserts_a_newline_without_submitting() {
        let mut composer = typed("first");
        composer.newline();
        composer.insert_str("second");
        assert_eq!(composer.lines(), vec!["first", "second"]);
    }

    #[test]
    fn cursor_position_is_measured_in_characters() {
        let mut composer = Composer::new();
        composer.insert_str("héllo");
        // Five characters, six bytes. A byte-based column would misplace the caret.
        assert_eq!(composer.cursor_position(), (0, 5));

        composer.newline();
        composer.insert_str("ab");
        assert_eq!(composer.cursor_position(), (1, 2));
    }

    #[test]
    fn backspace_deletes_whole_characters() {
        let mut composer = typed("héllo");
        composer.backspace();
        assert_eq!(composer.text(), "héll");
        composer.backspace();
        composer.backspace();
        // Must not split the two-byte é.
        assert_eq!(composer.text(), "hé");
    }

    #[test]
    fn arrows_move_by_character_not_byte() {
        let mut composer = typed("aéb");
        composer.move_left();
        composer.move_left();
        assert_eq!(composer.cursor_position(), (0, 1));
        composer.move_right();
        assert_eq!(composer.cursor_position(), (0, 2));
    }

    #[test]
    fn home_and_end_act_on_the_current_line() {
        let mut composer = Composer::new();
        composer.insert_str("first\nsecond");
        composer.move_line_start();
        assert_eq!(composer.cursor_position(), (1, 0));
        composer.move_line_end();
        assert_eq!(composer.cursor_position(), (1, 6));
    }

    #[test]
    fn history_walks_back_and_forward() {
        let mut composer = Composer::new();
        for entry in ["first", "second"] {
            composer.insert_str(entry);
            composer.submit();
        }

        composer.history_previous();
        assert_eq!(composer.text(), "second");
        composer.history_previous();
        assert_eq!(composer.text(), "first");
        composer.history_next();
        assert_eq!(composer.text(), "second");
    }

    #[test]
    fn walking_past_the_newest_entry_restores_the_draft() {
        let mut composer = Composer::new();
        composer.insert_str("submitted");
        composer.submit();

        composer.insert_str("a draft I was writing");
        composer.history_previous();
        assert_eq!(composer.text(), "submitted");

        composer.history_next();
        // The draft must come back; losing it is the classic shell annoyance.
        assert_eq!(composer.text(), "a draft I was writing");
    }

    #[test]
    fn history_does_not_hijack_arrows_in_a_multiline_draft() {
        let mut composer = Composer::new();
        composer.insert_str("old");
        composer.submit();

        composer.insert_str("line one");
        composer.newline();
        composer.insert_str("line two");

        // Caret is on line 2, so up should move within the draft, not browse.
        assert!(!composer.history_previous());
        assert!(composer.text().starts_with("line one"));
    }

    #[test]
    fn consecutive_duplicates_are_not_recorded_twice() {
        let mut composer = Composer::new();
        for _ in 0..3 {
            composer.insert_str("cargo test");
            composer.submit();
        }
        assert_eq!(composer.history(), &["cargo test".to_string()]);
    }

    #[test]
    fn a_trailing_backslash_becomes_a_newline() {
        let mut composer = typed("first line\\");
        assert!(composer.ends_with_continuation());

        composer.continue_line();
        assert_eq!(composer.lines(), vec!["first line", ""]);
        // The backslash itself must not survive into the message.
        assert!(!composer.text().contains('\\'));
    }

    #[test]
    fn an_escaped_backslash_is_not_a_continuation() {
        // `C:\\path\\\\` should send, not continue.
        assert!(!typed("path\\\\").ends_with_continuation());
        assert!(!typed("no trailing slash").ends_with_continuation());
    }

    #[test]
    fn set_text_replaces_the_buffer_and_places_the_caret() {
        let mut composer = typed("@ma");
        composer.set_text("@src/main.rs ".into(), 13);
        assert_eq!(composer.text(), "@src/main.rs ");
        assert_eq!(composer.cursor(), 13);
    }

    #[test]
    fn set_text_clamps_an_out_of_range_caret() {
        let mut composer = Composer::new();
        composer.set_text("short".into(), 999);
        assert_eq!(composer.cursor(), 5);
    }

    #[test]
    fn move_up_navigates_a_multiline_draft() {
        let mut composer = Composer::new();
        composer.insert_str("first line");
        composer.newline();
        composer.insert_str("second");

        assert_eq!(composer.cursor_position(), (1, 6));
        composer.move_up(20);
        assert_eq!(composer.cursor_position(), (0, 6));
    }

    #[test]
    fn move_up_clamps_to_a_shorter_line_above() {
        let mut composer = Composer::new();
        composer.insert_str("ab");
        composer.newline();
        composer.insert_str("much longer line");

        composer.move_up(20);
        // Column 16 does not exist on a 2-character line.
        assert_eq!(composer.cursor_position(), (0, 2));
    }

    #[test]
    fn move_up_from_the_first_line_does_nothing() {
        let mut composer = typed("only line");
        let before = composer.cursor();
        composer.move_up(20);
        assert_eq!(composer.cursor(), before);
    }

    #[test]
    fn a_run_of_punctuation_is_its_own_word() {
        // The rule that matters: without it, one ctrl+w on `foo::bar()` eats the
        // whole token, which is never what was meant.
        let mut composer = typed("call foo::bar()");
        composer.delete_word_backward();
        assert_eq!(composer.text(), "call foo::bar");
        composer.delete_word_backward();
        assert_eq!(composer.text(), "call foo::");
        composer.delete_word_backward();
        assert_eq!(composer.text(), "call foo");
        composer.delete_word_backward();
        assert_eq!(composer.text(), "call ");
    }

    #[test]
    fn a_word_deletion_takes_the_whitespace_before_it() {
        let mut composer = typed("one two   ");
        composer.delete_word_backward();
        assert_eq!(composer.text(), "one ");
    }

    #[test]
    fn word_motions_step_over_whole_characters() {
        // Byte offsets are not character offsets; landing mid-character panics
        // the moment anything slices the text.
        let mut composer = typed("héllo wörld");
        composer.move_word_left();
        assert_eq!(composer.cursor_position(), (0, 6));
        composer.move_word_left();
        assert_eq!(composer.cursor_position(), (0, 0));
        composer.move_word_right();
        assert_eq!(composer.cursor_position(), (0, 5));
    }

    #[test]
    fn deleting_forward_by_word_survives_multibyte_text() {
        let mut composer = typed("héllo wörld");
        composer.move_line_start();
        composer.delete_word_forward();
        assert_eq!(composer.text(), " wörld");
        composer.delete_word_forward();
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn a_word_motion_at_the_edges_does_not_panic() {
        let mut composer = Composer::new();
        composer.move_word_left();
        composer.move_word_right();
        composer.delete_word_backward();
        composer.delete_word_forward();
        assert_eq!(composer.text(), "");

        let mut composer = typed("   ");
        composer.delete_word_backward();
        assert_eq!(composer.text(), "");
    }

    #[test]
    fn a_kill_can_be_yanked_back() {
        let mut composer = typed("keep this");
        composer.move_line_start();
        composer.kill_to_line_end();
        assert!(composer.is_empty());

        composer.yank();
        assert_eq!(composer.text(), "keep this");
        // Yanking twice pastes twice: the buffer is not consumed.
        composer.yank();
        assert_eq!(composer.text(), "keep thiskeep this");
    }

    #[test]
    fn consecutive_kills_accumulate_in_the_order_they_were_made() {
        // The property that makes a kill buffer useful: two ctrl+k presses come
        // back as both lines, not just the last one.
        let mut composer = typed("alpha beta");
        composer.move_line_start();
        composer.kill_to_line_end();
        composer.kill_to_line_end();
        composer.clear();
        composer.yank();
        assert_eq!(composer.text(), "alpha beta");

        // Backwards kills prepend, or the text comes back inside out.
        let mut composer = typed("alpha beta");
        composer.delete_word_backward();
        composer.delete_word_backward();
        composer.yank();
        assert_eq!(composer.text(), "alpha beta");
    }

    #[test]
    fn moving_the_caret_starts_a_fresh_kill() {
        let mut composer = typed("alpha beta");
        composer.delete_word_backward();
        composer.move_left();
        composer.delete_word_backward();
        composer.yank();
        // Only the second kill, "alpha", came back — not "alpha beta", which is
        // what accumulating across the motion would have given.
        assert_eq!(composer.text(), "alpha ");
    }

    #[test]
    fn a_kill_at_the_end_of_a_line_joins_the_next_one() {
        // Otherwise repeated presses stall on an empty range and the key looks
        // broken.
        let mut composer = Composer::new();
        composer.insert_str("first");
        composer.newline();
        composer.insert_str("second");
        composer.move_up(20);
        composer.move_line_end();

        composer.kill_to_line_end();
        assert_eq!(composer.lines(), vec!["firstsecond"]);
    }

    #[test]
    fn typing_undoes_as_a_word_not_a_keystroke() {
        let mut composer = Composer::new();
        for ch in "hello".chars() {
            composer.insert(ch);
        }
        composer.undo();
        assert_eq!(composer.text(), "", "consecutive inserts are one step");
        composer.redo();
        assert_eq!(composer.text(), "hello");
    }

    #[test]
    fn undo_steps_back_through_distinct_edits() {
        let mut composer = Composer::new();
        composer.insert_str("first ");
        for ch in "second".chars() {
            composer.insert(ch);
        }
        composer.delete_word_backward();
        assert_eq!(composer.text(), "first ");

        composer.undo();
        assert_eq!(composer.text(), "first second");
        composer.undo();
        assert_eq!(composer.text(), "first ");
        composer.undo();
        assert_eq!(composer.text(), "");
        // Nothing left to undo, and asking again must not panic.
        assert!(!composer.undo());
    }

    #[test]
    fn an_edit_after_an_undo_abandons_the_redo() {
        // Redoing onto a changed buffer would splice in text belonging to a
        // draft that no longer exists.
        let mut composer = typed("original");
        composer.undo();
        composer.insert_str("different");
        assert!(!composer.redo());
        assert_eq!(composer.text(), "different");
    }

    #[test]
    fn undo_places_the_caret_inside_the_text_it_restored() {
        let mut composer = typed("a longer draft");
        composer.clear();
        composer.insert_str("hi");
        composer.undo();
        assert!(composer.cursor() <= composer.text().len());
    }

    #[test]
    fn a_submitted_prompt_cannot_be_undone_back() {
        let mut composer = typed("send this");
        composer.submit();
        assert!(!composer.undo(), "the model is already answering it");
        assert!(composer.is_empty());
    }

    #[test]
    fn the_undo_history_is_bounded() {
        let mut composer = Composer::new();
        for _ in 0..(UNDO_DEPTH * 2) {
            composer.insert_str("x");
        }
        assert!(composer.undo_stack.len() <= UNDO_DEPTH);
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut composer = typed("half a thought");
        composer.clear();
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
    }
}
