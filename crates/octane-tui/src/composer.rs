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
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.leave_history();
    }

    pub fn insert_str(&mut self, text: &str) {
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
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.text.len());
        self.text.replace_range(self.cursor..next, "");
        self.leave_history();
    }

    pub fn move_left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    pub fn move_right(&mut self) {
        if let Some((index, ch)) = self.text[self.cursor..].char_indices().next() {
            self.cursor += index + ch.len_utf8();
        }
    }

    /// Home: start of the current visual line, not of the whole buffer.
    pub fn move_line_start(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map(|index| index + 1).unwrap_or(0);
    }

    pub fn move_line_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|offset| self.cursor + offset)
            .unwrap_or(self.text.len());
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
        let backslash = self.text.len() - 1;
        self.text.replace_range(backslash.., "\n");
        self.cursor = self.text.len();
        self.leave_history();
    }

    /// Replace the whole buffer, e.g. when a completion is accepted.
    pub fn set_text(&mut self, text: String, cursor: usize) {
        self.cursor = cursor.min(text.len());
        self.text = text;
        self.leave_history();
    }

    /// Move up one visual line, keeping the column where possible.
    ///
    /// Needed because up-arrow browses history only from the first line; without
    /// this a multi-line draft cannot be navigated above its last line.
    pub fn move_up(&mut self) {
        let (line, column) = self.cursor_position();
        if line == 0 {
            return;
        }

        let start_of_current = self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let start_of_previous = self.text[..start_of_current.saturating_sub(1)]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);

        let previous_line = &self.text[start_of_previous..start_of_current.saturating_sub(1)];
        // Clamp to the shorter line rather than overshooting into the next one.
        let target = previous_line
            .char_indices()
            .nth(column)
            .map(|(index, _)| start_of_previous + index)
            .unwrap_or(start_of_previous + previous_line.len());

        self.cursor = target;
    }

    /// Ctrl+U — clear the line, as in readline.
    pub fn clear(&mut self) {
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
        self.cursor = self.text.len();
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
            self.cursor = self.text.len();
            return true;
        }

        self.history_index = Some(index + 1);
        self.text = self.history[index + 1].clone();
        self.cursor = self.text.len();
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
        self.cursor = 0;
        self.leave_history();

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

#[cfg(test)]
mod tests {
    use super::*;

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
        composer.move_up();
        assert_eq!(composer.cursor_position(), (0, 6));
    }

    #[test]
    fn move_up_clamps_to_a_shorter_line_above() {
        let mut composer = Composer::new();
        composer.insert_str("ab");
        composer.newline();
        composer.insert_str("much longer line");

        composer.move_up();
        // Column 16 does not exist on a 2-character line.
        assert_eq!(composer.cursor_position(), (0, 2));
    }

    #[test]
    fn move_up_from_the_first_line_does_nothing() {
        let mut composer = typed("only line");
        let before = composer.cursor();
        composer.move_up();
        assert_eq!(composer.cursor(), before);
    }

    #[test]
    fn clear_empties_the_buffer() {
        let mut composer = typed("half a thought");
        composer.clear();
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
    }
}
