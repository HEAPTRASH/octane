//! The runtime loop.
//!
//! Everything else in this crate is pure. This module is where the terminal,
//! async, and I/O live, deliberately concentrated in one place.
//!
//! # How the screen is managed
//!
//! Ratatui in [`Viewport::Inline`] mode. Only the bottom few rows are ours; the
//! rest of the terminal is untouched scrollback that the user's terminal owns,
//! along with its scrolling, search, and selection.
//!
//! Finished content is pushed up with [`Terminal::insert_before`], which writes
//! into real scrollback. Once written, it is never redrawn — which is also why
//! the renderer only emits lines for *completed* items. Anything still changing
//! stays in the live region.
//!
//! # Flicker
//!
//! Every frame is wrapped in synchronized-output escapes (`CSI ?2026h` /
//! `CSI ?2026l`) so the terminal presents it atomically. This is the difference
//! between visible flicker and none in most modern terminals, and it costs two
//! escape sequences per frame.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEvent, KeyEventKind,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, disable_raw_mode, enable_raw_mode,
};
use octane_permission::PermissionMode;
use octane_protocol::Event;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{TerminalOptions, Viewport};

use crate::approval::{ApprovalPrompt, ApprovalReply};
use crate::composer::{Composer, Submission};
use crate::keymap::{self, KeyAction, KeyContext};
use crate::render::{RenderOptions, render_event};
use crate::status::StatusLine;

/// Height of the live region: border, one input row, border, status.
///
/// Kept small on purpose. Every row here is a row the user does not get for
/// scrollback, and the composer grows only when a draft needs it.
const BASE_VIEWPORT_HEIGHT: u16 = 4;

/// Composer rows before it stops growing and scrolls internally.
const MAX_COMPOSER_ROWS: u16 = 10;

/// Spinner tick. ~12fps: fast enough to look alive, slow enough to be invisible
/// in a CPU profile.
const TICK: Duration = Duration::from_millis(80);

/// What the loop hands back to its caller.
///
/// The app does not run the agent — it reports what the user asked for and lets
/// the caller drive `octane-core`. That is the boundary that keeps agent logic
/// out of this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    Submit(Submission),
    /// Shift+Tab.
    ModeChanged(PermissionMode),
    /// Esc while working.
    Interrupt,
    Exit,
}

pub struct App {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    composer: Composer,
    status: StatusLine,
    options: RenderOptions,
    pending_approval: Option<(ApprovalPrompt, tokio::sync::oneshot::Sender<ApprovalReply>)>,
    spinner_frame: usize,
    started: Instant,
    raw_mode: bool,
    /// Set when something the live region shows has changed.
    ///
    /// Without this the caller's poll tick repaints the region ~12 times a second
    /// forever, which is visible as flicker on any terminal that is not doing
    /// synchronized updates perfectly.
    dirty: bool,
    /// Last viewport size actually applied, so `resize` is only called when it
    /// genuinely changed — see [`App::draw`].
    viewport_height: u16,
    viewport_width: u16,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("mode", &self.status.mode)
            .field("has_pending_approval", &self.pending_approval.is_some())
            .finish_non_exhaustive()
    }
}

impl App {
    /// Take over the bottom of the terminal, leaving scrollback alone.
    pub fn new(status: StatusLine) -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Bracketed paste, so a multi-line paste arrives as one event instead of
        // as a burst of keystrokes with newlines that would each submit.
        crossterm::execute!(stdout, EnableBracketedPaste)?;

        let terminal = Terminal::with_options(
            CrosstermBackend::new(stdout),
            TerminalOptions { viewport: Viewport::Inline(BASE_VIEWPORT_HEIGHT) },
        )?;

        Ok(Self {
            terminal,
            composer: Composer::new(),
            status,
            options: RenderOptions::default(),
            pending_approval: None,
            spinner_frame: 0,
            started: Instant::now(),
            raw_mode: true,
            // The first frame must always draw.
            dirty: true,
            viewport_height: BASE_VIEWPORT_HEIGHT,
            viewport_width: 0,
        })
    }

    /// Mutable access to the status line. Marks the region for redraw, since the
    /// caller is about to change something it displays.
    pub fn status_mut(&mut self) -> &mut StatusLine {
        self.dirty = true;
        &mut self.status
    }

    pub fn options_mut(&mut self) -> &mut RenderOptions {
        self.dirty = true;
        &mut self.options
    }

    /// Force a redraw on the next [`App::draw`].
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Append an agent event to scrollback.
    ///
    /// Events that render to nothing are skipped without touching the terminal:
    /// `insert_before` with a zero height still costs a redraw.
    pub fn push_event(&mut self, event: &Event) -> Result<()> {
        let lines = render_event(event, &self.options);
        if lines.is_empty() {
            return Ok(());
        }
        self.push_lines(lines)
    }

    /// Write lines into real scrollback, above the live region.
    pub fn push_lines(&mut self, lines: Vec<Line<'static>>) -> Result<()> {
        let height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        self.terminal.insert_before(height, |buf| {
            let paragraph = Paragraph::new(lines);
            ratatui::widgets::Widget::render(paragraph, buf.area, buf);
        })?;
        // Scrolling the region up leaves it needing a repaint.
        self.dirty = true;
        Ok(())
    }

    /// Raise an approval prompt. The live region grows to show it.
    pub fn set_approval(
        &mut self,
        prompt: ApprovalPrompt,
        responder: tokio::sync::oneshot::Sender<ApprovalReply>,
    ) {
        self.pending_approval = Some((prompt, responder));
        self.dirty = true;
    }

    pub fn has_pending_approval(&self) -> bool {
        self.pending_approval.is_some()
    }

    /// Redraw the live region, if anything changed.
    ///
    /// Both guards below exist because their absence is visible as flicker:
    ///
    /// **Nothing changed, nothing drawn.** The caller polls on a tick, so without
    /// a dirty flag this repaints ~12 times a second forever, including while the
    /// user is reading a still screen.
    ///
    /// **Resize only on an actual size change.** `Terminal::resize` resets both
    /// of ratatui's buffers, which discards the cell diff and forces a full
    /// repaint of the region. Calling it unconditionally turned every frame into
    /// a full repaint and defeated the diffing entirely.
    pub fn draw(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;

        let composer_rows = composer_height(&self.composer);
        let approval_rows = self
            .pending_approval
            .as_ref()
            .map(|(prompt, _)| approval_height(prompt))
            .unwrap_or(0);
        let activity_rows = u16::from(self.status.activity.is_some());

        let needed = BASE_VIEWPORT_HEIGHT + composer_rows.saturating_sub(1) + approval_rows
            + activity_rows;
        let area = inline_area(&self.terminal, needed)?;

        if viewport_changed((self.viewport_width, self.viewport_height), area) {
            self.viewport_height = area.height;
            self.viewport_width = area.width;
            self.terminal.resize(area)?;
        }

        // Synchronized output: the terminal buffers this frame and presents it in
        // one go, which is what keeps the composer from tearing while streaming.
        let _ = crossterm::execute!(io::stdout(), BeginSynchronizedUpdate);

        let status = self.status.clone();
        let spinner_frame = self.spinner_frame;
        let composer = &self.composer;
        let pending = self.pending_approval.as_ref().map(|(prompt, _)| prompt.clone());
        let theme = self.options.theme;

        self.terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(approval_rows),
                    Constraint::Length(activity_rows),
                    Constraint::Length(composer_rows + 2),
                    Constraint::Length(1),
                ])
                .split(frame.area());

            if let Some(prompt) = &pending {
                let mut lines = vec![Line::from(vec![
                    Span::styled("? ", Style::default().fg(theme.warning)),
                    Span::raw(prompt.title()),
                ])];
                if let Some(diff) = &prompt.diff {
                    lines.extend(crate::render::render_diff(diff, &theme));
                }
                lines.push(Line::styled(prompt.options_line(), theme.dim()));
                frame.render_widget(Paragraph::new(lines), chunks[0]);
            }

            if let Some(line) = status.activity_line(spinner_frame) {
                frame.render_widget(
                    Paragraph::new(Line::styled(line, theme.dim())),
                    chunks[1],
                );
            }

            let composer_lines: Vec<Line> =
                composer.lines().iter().map(|line| Line::raw(line.to_string())).collect();
            frame.render_widget(
                Paragraph::new(composer_lines)
                    .block(Block::default().borders(Borders::ALL).border_style(theme.dim())),
                chunks[2],
            );

            // Place the real terminal cursor rather than drawing a fake one, so
            // it blinks as the user's terminal is configured to.
            let (line, column) = composer.cursor_position();
            frame.set_cursor_position((
                chunks[2].x + 1 + column as u16,
                chunks[2].y + 1 + line as u16,
            ));

            frame.render_widget(status_paragraph(&status, &theme), chunks[3]);
        })?;

        let _ = crossterm::execute!(io::stdout(), EndSynchronizedUpdate);
        Ok(())
    }

    /// Poll for input. `Ok(None)` means the tick elapsed with nothing to report.
    pub fn poll(&mut self) -> Result<Option<AppEvent>> {
        if !crossterm::event::poll(TICK)? {
            // An idle tick changes nothing on screen, so it must not schedule a
            // repaint. Only an animating spinner does.
            if let Some(activity) = self.status.activity.as_mut() {
                activity.elapsed_secs = self.started.elapsed().as_secs();
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                self.dirty = true;
            }
            return Ok(None);
        }

        match crossterm::event::read()? {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                // A keypress may or may not change the composer, but working that
                // out per action is not worth the bookkeeping — one repaint per
                // keystroke is exactly the right rate.
                self.dirty = true;
                Ok(self.on_key(key))
            }
            // Bracketed paste arrives whole, so newlines inside it insert rather
            // than submit.
            TermEvent::Paste(text) => {
                self.composer.insert_str(&text);
                self.dirty = true;
                Ok(None)
            }
            TermEvent::Resize(_, _) => {
                // Soft wrapping moved, so nothing on screen can be trusted.
                // Zeroing the recorded width forces the resize path in `draw`.
                self.viewport_width = 0;
                self.dirty = true;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Apply a keypress.
    ///
    /// Routing lives in [`crate::keymap`] as a pure function; this only performs
    /// the resulting action. Keeping the decision separate from the effect is
    /// what makes the bindings testable without a terminal.
    fn on_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        let action = {
            let ctx = KeyContext {
                working: self.status.activity.is_some(),
                composer_empty: self.composer.is_empty(),
                approval: self.pending_approval.as_ref().map(|(prompt, _)| prompt),
            };
            keymap::route(key, &ctx)
        };

        match action {
            KeyAction::None => None,

            KeyAction::Insert(ch) => {
                self.composer.insert(ch);
                None
            }
            KeyAction::InsertNewline => {
                self.composer.newline();
                None
            }
            KeyAction::Backspace => {
                self.composer.backspace();
                None
            }
            KeyAction::DeleteForward => {
                self.composer.delete_forward();
                None
            }
            KeyAction::MoveLeft => {
                self.composer.move_left();
                None
            }
            KeyAction::MoveRight => {
                self.composer.move_right();
                None
            }
            KeyAction::MoveLineStart => {
                self.composer.move_line_start();
                None
            }
            KeyAction::MoveLineEnd => {
                self.composer.move_line_end();
                None
            }
            KeyAction::HistoryPrevious => {
                self.composer.history_previous();
                None
            }
            KeyAction::HistoryNext => {
                self.composer.history_next();
                None
            }
            KeyAction::Clear => {
                self.composer.clear();
                None
            }

            KeyAction::Submit => {
                self.started = Instant::now();
                self.composer.submit().map(AppEvent::Submit)
            }
            KeyAction::CycleMode => {
                self.status.mode = self.status.mode.cycle();
                Some(AppEvent::ModeChanged(self.status.mode))
            }
            KeyAction::Interrupt => Some(AppEvent::Interrupt),
            KeyAction::Exit => Some(AppEvent::Exit),

            KeyAction::Approve(reply) => {
                self.answer(reply);
                None
            }
            KeyAction::RejectWithComposerText => {
                let instructions = self.composer.text().trim().to_string();
                self.composer.clear();
                self.answer(ApprovalReply::RejectWith { instructions });
                None
            }
        }
    }

    fn answer(&mut self, reply: ApprovalReply) {
        // Navigation is not a decision: keep the prompt up.
        if !reply.is_decision() {
            return;
        }
        if let Some((_, responder)) = self.pending_approval.take() {
            let _ = responder.send(reply);
            self.dirty = true;
        }
    }

    /// Restore the terminal. Idempotent, so calling it twice is harmless.
    pub fn restore(&mut self) -> Result<()> {
        if !self.raw_mode {
            return Ok(());
        }
        self.raw_mode = false;
        disable_raw_mode()?;
        crossterm::execute!(io::stdout(), DisableBracketedPaste)?;
        // Leave the cursor below our region so the shell prompt does not land on
        // top of the last frame.
        self.terminal.clear()?;
        println!();
        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // A panic mid-session must not leave the terminal in raw mode with no
        // echo — the user would be left with an apparently dead shell.
        let _ = self.restore();
    }
}

fn status_paragraph<'a>(status: &StatusLine, theme: &crate::theme::Theme) -> Paragraph<'a> {
    let mut spans = Vec::new();
    for (index, segment) in status.segments().into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", theme.dim()));
        }
        spans.push(Span::styled(segment.text.clone(), Style::default().fg(segment.color(theme))));
    }
    spans.push(Span::styled("   ", theme.dim()));
    spans.push(Span::styled(status.hints(), theme.dim()));
    Paragraph::new(Line::from(spans))
}

fn composer_height(composer: &Composer) -> u16 {
    u16::try_from(composer.lines().len()).unwrap_or(1).clamp(1, MAX_COMPOSER_ROWS)
}

fn approval_height(prompt: &ApprovalPrompt) -> u16 {
    let diff_rows = prompt
        .diff
        .as_ref()
        // Capped: a 500-line diff must not swallow the terminal. `f` opens the
        // full-screen view for anything larger.
        .map(|diff| u16::try_from(diff.lines().count()).unwrap_or(u16::MAX).min(12))
        .unwrap_or(0);
    // Title + diff + options.
    2 + diff_rows
}

/// Clamp the requested viewport height to something the terminal can give.
fn inline_area(
    terminal: &Terminal<CrosstermBackend<Stdout>>,
    requested: u16,
) -> Result<ratatui::layout::Rect> {
    let size = terminal.size()?;
    Ok(ratatui::layout::Rect::new(0, 0, size.width, clamp_height(requested, size.height)))
}

/// Whether the live region actually needs a ratatui `resize`.
///
/// Split out and tested because calling `resize` unconditionally is not a
/// visible mistake — it renders correctly. It just resets both of ratatui's
/// buffers, discarding the cell diff, so every frame becomes a full repaint.
/// Measured at 9.6 KB/s of terminal traffic while completely idle, which is
/// what the flicker was.
fn viewport_changed(current: (u16, u16), area: ratatui::layout::Rect) -> bool {
    current != (area.width, area.height)
}

/// Never take more than half the screen: the transcript is the point, and a live
/// region that fills the window leaves nothing to read.
fn clamp_height(requested: u16, terminal_height: u16) -> u16 {
    let ceiling = (terminal_height / 2).max(BASE_VIEWPORT_HEIGHT);
    requested.clamp(BASE_VIEWPORT_HEIGHT, ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalPrompt;
    use octane_permission::Resource;

    fn prompt(diff: Option<&str>) -> ApprovalPrompt {
        ApprovalPrompt {
            resource: Resource::write_file("/p/a.rs"),
            summary: "Write a.rs".into(),
            diff: diff.map(ToString::to_string),
        }
    }

    #[test]
    fn the_composer_grows_with_the_draft_but_is_capped() {
        let mut composer = Composer::new();
        assert_eq!(composer_height(&composer), 1);

        for _ in 0..3 {
            composer.newline();
        }
        assert_eq!(composer_height(&composer), 4);

        for _ in 0..50 {
            composer.newline();
        }
        assert_eq!(
            composer_height(&composer),
            MAX_COMPOSER_ROWS,
            "a long draft must not swallow the screen"
        );
    }

    #[test]
    fn an_unchanged_viewport_is_not_resized() {
        let area = ratatui::layout::Rect::new(0, 0, 100, 6);
        // The common case, every frame. Resizing here throws away the cell diff
        // and turns each frame into a full repaint.
        assert!(!viewport_changed((100, 6), area));
    }

    #[test]
    fn a_changed_viewport_is_resized() {
        let area = ratatui::layout::Rect::new(0, 0, 100, 6);
        assert!(viewport_changed((100, 5), area), "the composer grew");
        assert!(viewport_changed((90, 6), area), "the window was resized");
        // Zero width is the sentinel `poll` sets on a terminal resize to force
        // the resize path, since soft wrapping has moved.
        assert!(viewport_changed((0, 6), area));
    }

    #[test]
    fn the_live_region_never_takes_more_than_half_the_screen() {
        // On a short terminal a long draft must not leave zero rows of transcript.
        assert_eq!(clamp_height(30, 24), 12);
        assert_eq!(clamp_height(4, 24), 4);
        // And it always gets at least the base rows, however short the terminal.
        assert_eq!(clamp_height(20, 6), BASE_VIEWPORT_HEIGHT);
        assert_eq!(clamp_height(1, 100), BASE_VIEWPORT_HEIGHT);
    }

    #[test]
    fn approval_height_accounts_for_the_diff_and_caps_it() {
        assert_eq!(approval_height(&prompt(None)), 2);

        let small = "+ one\n+ two\n";
        assert_eq!(approval_height(&prompt(Some(small))), 4);

        let huge: String = (0..500).map(|i| format!("+ line {i}\n")).collect();
        assert_eq!(
            approval_height(&prompt(Some(&huge))),
            14,
            "a large diff is capped; `f` opens the full view"
        );
    }
}
