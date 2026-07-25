//! The runtime loop.
//!
//! Everything else in this crate is pure. This module is where the terminal,
//! async, and I/O live, deliberately concentrated in one place.
//!
//! # Full screen
//!
//! The alternate screen, with a fixed layout: brand header at the top, transcript
//! filling the middle, composer and status pinned to the bottom.
//!
//! This trades away the terminal's own scrollback, search, and selection, so
//! [`crate::transcript`] provides scrolling in their place. It is a real cost —
//! `cmd+F` and mouse selection stop meaning what they did — and it is the reason
//! the transcript pane keeps a generous line budget and a visible scrollbar.
//!
//! # Flicker
//!
//! Two rules, both of which are invisible until violated because their absence
//! still renders correctly:
//!
//! - Redraw only when something changed. The caller polls on a tick, so an
//!   unconditional draw repaints ~12 times a second forever.
//! - Wrap every frame in synchronized output (`CSI ?2026h` / `l`) so the terminal
//!   presents it atomically instead of tearing.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyEvent, KeyEventKind,
    MouseEventKind,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use octane_permission::PermissionMode;
use octane_protocol::Event;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::approval::{ApprovalPrompt, ApprovalReply};
use crate::completion::{Candidate, Completion};
use crate::composer::{Composer, Submission};
use crate::glyphs::Glyphs;
use crate::keymap::{self, KeyAction, KeyContext};
use crate::render::{RenderOptions, render_event};
use crate::status::StatusLine;
use crate::transcript::Transcript;

/// Composer rows before it stops growing and scrolls internally.
///
/// Generous, because shift+enter exists so people can write real paragraphs.
const MAX_COMPOSER_ROWS: u16 = 12;

/// Rows the transcript keeps for itself no matter how large the draft grows.
///
/// Without a floor, a long draft squeezes the transcript to nothing and the user
/// loses sight of what they are replying to.
const MIN_TRANSCRIPT_ROWS: u16 = 3;

/// Spinner tick. ~12fps: alive without being visible in a CPU profile.
const TICK: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    Submit(Submission),
    /// A picker row was chosen.
    Picked { kind: crate::picker::PickerKind, key: String },
    ModeChanged(PermissionMode),
    Interrupt,
    Exit,
}

pub struct App {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    transcript: Transcript,
    composer: Composer,
    completion: Completion,
    status: StatusLine,
    options: RenderOptions,
    glyphs: Glyphs,

    /// Candidate universes for completion, supplied by the caller.
    commands: Vec<Candidate>,
    files: Vec<String>,

    pending_approval: Option<(ApprovalPrompt, tokio::sync::oneshot::Sender<ApprovalReply>)>,
    spinner_frame: usize,
    started: Instant,

    workspace: String,
    sandboxed: bool,

    raw_mode: bool,
    dirty: bool,
    /// The item currently streaming: its id, accumulated text, and whether it is
    /// reasoning rather than prose.
    streaming: Option<(octane_protocol::ItemId, String, bool)>,
    /// An open selection overlay.
    picker: Option<crate::picker::Picker>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("mode", &self.status.mode)
            .field("transcript_lines", &self.transcript.len())
            .field("completing", &self.completion.is_active())
            .finish_non_exhaustive()
    }
}

impl App {
    /// Enter the alternate screen and take over the terminal.
    pub fn new(status: StatusLine, workspace: String, sandboxed: bool) -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        crossterm::execute!(
            stdout,
            EnterAlternateScreen,
            // Bracketed paste, so a multi-line paste arrives whole rather than as
            // a burst of keystrokes whose newlines would each submit.
            EnableBracketedPaste,
            crossterm::event::EnableMouseCapture,
        )?;

        // Ask for disambiguated key reporting, which is the only way shift+enter
        // is distinguishable from Enter — without it terminals send a bare CR for
        // both. Supported by Kitty, Ghostty, WezTerm, foot, and recent iTerm2;
        // ignored elsewhere, which is why alt+enter and trailing-backslash exist
        // as portable fallbacks.
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::event::PushKeyboardEnhancementFlags(
                crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
            ),
        );

        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        let glyphs = Glyphs::detect();

        Ok(Self {
            terminal,
            transcript: Transcript::new(),
            composer: Composer::new(),
            completion: Completion::default(),
            status,
            options: RenderOptions { glyphs, ..Default::default() },
            glyphs,
            commands: Vec::new(),
            files: Vec::new(),
            pending_approval: None,
            spinner_frame: 0,
            started: Instant::now(),
            workspace,
            sandboxed,
            raw_mode: true,
            dirty: true,
            streaming: None,
            picker: None,
        })
    }

    pub fn status_mut(&mut self) -> &mut StatusLine {
        self.dirty = true;
        &mut self.status
    }

    pub fn options_mut(&mut self) -> &mut RenderOptions {
        self.dirty = true;
        &mut self.options
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Slash commands offered by completion.
    pub fn set_commands(&mut self, commands: Vec<Candidate>) {
        self.commands = commands;
    }

    /// Workspace files offered by `@` completion.
    ///
    /// Supplied by the caller rather than walked here, so the UI crate does not
    /// grow a filesystem dependency.
    pub fn set_files(&mut self, files: Vec<String>) {
        self.files = files;
    }

    /// Apply an agent event.
    ///
    /// Streaming items are held in the transcript's pending region and rewritten
    /// on each delta; only completed items are committed. Appending each delta
    /// instead would grow the transcript by a line per token.
    pub fn push_event(&mut self, event: &Event) -> Result<()> {
        use octane_protocol::ItemEvent;

        match event {
            Event::Item(ItemEvent::Started { item, .. }) => {
                self.streaming = Some((item.id.clone(), String::new(), is_reasoning(&item.kind)));
                self.refresh_pending();
            }
            Event::Item(ItemEvent::Delta { item_id, text, .. }) => {
                if let Some((id, buffer, _)) = self.streaming.as_mut() {
                    if id == item_id {
                        buffer.push_str(text);
                    }
                }
                self.refresh_pending();
            }
            Event::Item(ItemEvent::Completed { item, .. }) => {
                // The completed form supersedes whatever was streaming.
                if self.streaming.as_ref().is_some_and(|(id, _, _)| *id == item.id) {
                    self.streaming = None;
                    self.transcript.clear_pending();
                }
                let lines = render_event(event, &self.options);
                if !lines.is_empty() {
                    self.push_lines(lines);
                }
            }
            _ => {
                let lines = render_event(event, &self.options);
                if !lines.is_empty() {
                    self.push_lines(lines);
                }
            }
        }

        // Reasoning is hidden by default; an item whose kind is filtered out
        // must not leave a stale pending region behind.
        if matches!(event, Event::Item(ItemEvent::Started { item, .. }) if is_reasoning(&item.kind))
            && self.options.reasoning == crate::render::Reasoning::Hidden
        {
            self.streaming = None;
            self.transcript.clear_pending();
        }

        self.dirty = true;
        Ok(())
    }

    /// Re-render the streaming region from the accumulated text.
    fn refresh_pending(&mut self) {
        let Some((_, text, reasoning)) = &self.streaming else {
            self.transcript.clear_pending();
            return;
        };
        if *reasoning && self.options.reasoning == crate::render::Reasoning::Hidden {
            self.transcript.clear_pending();
            return;
        }

        let style =
            if *reasoning { self.options.theme.reasoning() } else { Style::default() };
        let lines: Vec<Line<'static>> = text
            .split('\n')
            .map(|line| {
                let text = if *reasoning { format!("  {line}") } else { line.to_string() };
                Line::styled(text, style)
            })
            .collect();

        self.transcript.set_pending(lines);
        self.dirty = true;
    }

    pub fn push_lines(&mut self, lines: Vec<Line<'static>>) {
        self.transcript.push(lines);
        self.dirty = true;
    }

    pub fn set_approval(
        &mut self,
        prompt: ApprovalPrompt,
        responder: tokio::sync::oneshot::Sender<ApprovalReply>,
    ) {
        self.pending_approval = Some((prompt, responder));
        self.dirty = true;
    }

    /// Empty the transcript, returning to the quiet start screen.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.streaming = None;
        self.dirty = true;
    }

    /// Open a selection overlay.
    pub fn set_picker(&mut self, picker: crate::picker::Picker) {
        self.picker = Some(picker);
        self.dirty = true;
    }

    pub fn has_picker(&self) -> bool {
        self.picker.is_some()
    }

    pub fn has_pending_approval(&self) -> bool {
        self.pending_approval.is_some()
    }

    /// Redraw, if anything changed.
    pub fn draw(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;

        let _ = crossterm::execute!(io::stdout(), BeginSynchronizedUpdate);

        let width = self.terminal.size().map(|size| size.width).unwrap_or(80);
        let composer_rows = composer_height(&self.composer, width);
        let approval_rows = self
            .pending_approval
            .as_ref()
            .map(|(prompt, _)| approval_height(prompt))
            .unwrap_or(0);
        let activity_rows = u16::from(self.status.activity.is_some());

        let status = self.status.clone();
        let spinner_frame = self.spinner_frame;
        let theme = self.options.theme;
        let glyphs = self.glyphs;
        let pending = self.pending_approval.as_ref().map(|(prompt, _)| prompt.clone());
        let picker_state = self.picker.clone();
        let workspace = self.workspace.clone();
        let sandboxed = self.sandboxed;

        let transcript = &mut self.transcript;
        let composer = &self.composer;
        let completion = &self.completion;

        self.terminal.draw(|frame| {
            let area = frame.area();

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),                  // header
                    Constraint::Min(MIN_TRANSCRIPT_ROWS),   // transcript
                    Constraint::Length(approval_rows),
                    Constraint::Length(activity_rows),
                    Constraint::Length(composer_rows + 2),  // composer + border
                    Constraint::Length(1),                  // status
                ])
                .split(area);

            frame.render_widget(header(&workspace, sandboxed, &theme, &glyphs), chunks[0]);

            // Transcript, or the empty state.
            let body = chunks[1];
            // Cleared first: `Paragraph` writes only the cells it covers, so a
            // line shorter than the one it replaces leaves the old tail behind.
            // That is the stray-character bleed at the end of scrolled lines.
            frame.render_widget(Clear, body);
            if transcript.is_empty() {
                frame.render_widget(empty_state(&theme, &glyphs, body.width), body);
            } else {
                let visible = transcript.visible(body.height as usize);
                frame.render_widget(Paragraph::new(visible), body);
            }

            if let Some(prompt) = &pending {
                let mut lines = vec![Line::from(vec![
                    Span::styled(format!("{} ", glyphs.question), theme.label(theme.warning)),
                    Span::raw(prompt.title()),
                ])];
                if let Some(diff) = &prompt.diff {
                    lines.extend(crate::render::render_diff(diff, &theme));
                }
                lines.push(Line::styled(prompt.options_line(), theme.dim()));
                frame.render_widget(Paragraph::new(lines), chunks[2]);
            }

            if let Some(line) = status.activity_line(spinner_frame) {
                frame.render_widget(
                    Paragraph::new(Line::styled(line, theme.dim())),
                    chunks[3],
                );
            }

            let composer_area = chunks[4];
            let composer_lines: Vec<Line> = composer
                .lines()
                .iter()
                .map(|line| Line::raw(line.to_string()))
                .collect();
            frame.render_widget(Clear, composer_area);
            frame.render_widget(
                Paragraph::new(composer_lines)
                    // Wrapped, or a long line is simply cut off at the border.
                    .wrap(ratatui::widgets::Wrap { trim: false })
                    .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme.accent))
                        .title(Span::styled(
                            format!(" {} ", glyphs.prompt),
                            theme.label(theme.accent),
                        )),
                ),
                composer_area,
            );

            // The caret follows the wrap too, or it sits at the end of a row the
            // text has already flowed past.
            let usable = usize::from(composer_area.width.saturating_sub(3)).max(1);
            let (line, column) = composer.cursor_position();
            let wrapped_before: usize = composer
                .lines()
                .iter()
                .take(line)
                .map(|text| text.chars().count().div_ceil(usable).max(1))
                .sum();
            let row = wrapped_before + column / usable;
            frame.set_cursor_position((
                composer_area.x + 1 + (column % usable) as u16,
                composer_area.y + 1 + u16::try_from(row).unwrap_or(0),
            ));

            frame.render_widget(status_paragraph(&status, &theme, &glyphs), chunks[5]);

            if let Some(picker) = &picker_state {
                let area = centred(frame.area(), 66, picker_height(picker));
                frame.render_widget(Clear, area);
                frame.render_widget(picker_widget(picker, &theme, &glyphs), area);
            }

            // The popup floats above the composer, so it never displaces the
            // input the user is typing into.
            if completion.is_active() {
                let popup = popup_area(composer_area, completion, area);
                frame.render_widget(Clear, popup);
                frame.render_widget(completion_widget(completion, &theme), popup);
            }
        })?;

        let _ = crossterm::execute!(io::stdout(), EndSynchronizedUpdate);
        Ok(())
    }

    /// Poll for input. `Ok(None)` means the tick elapsed with nothing to report.
    pub fn poll(&mut self) -> Result<Option<AppEvent>> {
        self.poll_for(TICK)
    }

    /// Poll with an explicit timeout.
    ///
    /// The idle loop can afford to block for a full tick. A turn in flight
    /// cannot: this runs inside a `select!` alongside the event channel, and
    /// blocking the runtime for 80ms at a time would stall the stream it is
    /// meant to be rendering.
    pub fn poll_for(&mut self, timeout: Duration) -> Result<Option<AppEvent>> {
        if !crossterm::event::poll(timeout)? {
            // An idle tick changes nothing, so it must not schedule a repaint.
            // Only an animating spinner does.
            if let Some(activity) = self.status.activity.as_mut() {
                activity.elapsed_secs = self.started.elapsed().as_secs();
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                self.dirty = true;
            }
            return Ok(None);
        }

        match crossterm::event::read()? {
            TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                self.dirty = true;
                let event = self.on_key(key);
                self.refresh_completion();
                Ok(event)
            }
            TermEvent::Paste(text) => {
                self.composer.insert_str(&text);
                self.refresh_completion();
                self.dirty = true;
                Ok(None)
            }
            // Mouse wheel scrolls the transcript, which is the one piece of
            // native behaviour worth reproducing after taking the screen.
            TermEvent::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.transcript.scroll_up(3),
                    MouseEventKind::ScrollDown => self.transcript.scroll_down(3),
                    _ => return Ok(None),
                }
                self.dirty = true;
                Ok(None)
            }
            TermEvent::Resize(_, _) => {
                self.dirty = true;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn refresh_completion(&mut self) {
        self.completion.update(
            self.composer.text(),
            self.composer.cursor(),
            &self.commands,
            &self.files,
        );
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        let action = {
            let ctx = KeyContext {
                working: self.status.activity.is_some(),
                composer_empty: self.composer.is_empty(),
                approval: self.pending_approval.as_ref().map(|(prompt, _)| prompt),
                // A fully typed command must not swallow Enter.
                completing: self.completion.is_active() && !self.completion.is_exhausted(),
                on_first_line: self.composer.cursor_position().0 == 0,
                ends_with_continuation: self.composer.ends_with_continuation(),
                picking: self.picker.is_some(),
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
            KeyAction::MoveUp => {
                self.composer.move_up();
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

            KeyAction::CompletionNext => {
                self.completion.select_next();
                None
            }
            KeyAction::CompletionPrevious => {
                self.completion.select_previous();
                None
            }
            KeyAction::CompletionAccept => {
                if let Some((text, cursor)) = self.completion.accept(self.composer.text()) {
                    self.composer.set_text(text, cursor);
                }
                self.completion.dismiss();
                None
            }
            KeyAction::CompletionDismiss => {
                self.completion.dismiss();
                None
            }

            KeyAction::ScrollUp => {
                self.transcript.scroll_up(3);
                None
            }
            KeyAction::ScrollDown => {
                self.transcript.scroll_down(3);
                None
            }
            KeyAction::PageUp => {
                self.transcript.page_up();
                None
            }
            KeyAction::PageDown => {
                self.transcript.page_down();
                None
            }
            KeyAction::ScrollTop => {
                self.transcript.scroll_to_top();
                None
            }
            KeyAction::ScrollBottom => {
                self.transcript.scroll_to_bottom();
                None
            }

            KeyAction::ContinueLine => {
                self.composer.continue_line();
                None
            }

            KeyAction::Submit => {
                self.started = Instant::now();
                // Submitting jumps to the bottom: the reply is what the user
                // wants to see next, wherever they had scrolled to.
                self.transcript.scroll_to_bottom();
                self.composer.submit().map(AppEvent::Submit)
            }
            KeyAction::CycleMode => {
                self.status.mode = self.status.mode.cycle();
                Some(AppEvent::ModeChanged(self.status.mode))
            }
            KeyAction::Interrupt => Some(AppEvent::Interrupt),
            KeyAction::Exit => Some(AppEvent::Exit),

            KeyAction::PickerNext => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.select_next();
                }
                None
            }
            KeyAction::PickerPrevious => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.select_previous();
                }
                None
            }
            KeyAction::PickerFilter(ch) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.push_filter(ch);
                }
                None
            }
            KeyAction::PickerUnfilter => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.pop_filter();
                }
                None
            }
            KeyAction::PickerCancel => {
                self.picker = None;
                None
            }
            KeyAction::PickerChoose => {
                let chosen = self
                    .picker
                    .as_ref()
                    .and_then(|picker| picker.choose().map(ToString::to_string));
                match chosen {
                    Some(key) => {
                        let kind = self.picker.as_ref().map(|picker| picker.kind);
                        self.picker = None;
                        kind.map(|kind| AppEvent::Picked { kind, key })
                    }
                    // A disabled row keeps the overlay open rather than
                    // silently doing nothing to a screen that looks live.
                    None => None,
                }
            }

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
        if !reply.is_decision() {
            return;
        }
        if let Some((_, responder)) = self.pending_approval.take() {
            let _ = responder.send(reply);
            self.dirty = true;
        }
    }

    /// Restore the terminal. Idempotent.
    pub fn restore(&mut self) -> Result<()> {
        if !self.raw_mode {
            return Ok(());
        }
        self.raw_mode = false;
        disable_raw_mode()?;
        // Popped first, and unconditionally: leaving a terminal in enhanced
        // reporting mode breaks the user's next program.
        let _ = crossterm::execute!(io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
        crossterm::execute!(
            io::stdout(),
            crossterm::event::DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
        )?;
        Ok(())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // A panic must not leave the user in the alternate screen with no echo,
        // which looks like a dead shell.
        let _ = self.restore();
    }
}

/// Brand header: wordmark, workspace, sandbox state.
fn header<'a>(
    workspace: &str,
    sandboxed: bool,
    theme: &crate::theme::Theme,
    glyphs: &Glyphs,
) -> Paragraph<'a> {
    let mark = if *glyphs == crate::glyphs::UNICODE { "\u{2588}\u{2584}\u{2588}" } else { "[#]" };

    let mut spans = vec![
        Span::styled(format!(" {mark} OCTANE"), theme.label(theme.accent)),
        Span::styled(format!("  {}  ", glyphs.separator), theme.dim()),
        Span::styled(workspace.to_string(), theme.dim()),
    ];
    if !sandboxed {
        // Running unconfined is not something to learn by surprise.
        spans.push(Span::styled("  sandbox OFF", theme.label(theme.error)));
    }

    Paragraph::new(vec![Line::from(spans), Line::default()])
}

/// Shown when there are no messages yet.
///
/// The negative space is deliberate: an empty session should look calm and
/// finished, not like a screen waiting for content that failed to load.
fn empty_state<'a>(
    theme: &crate::theme::Theme,
    glyphs: &Glyphs,
    width: u16,
) -> Paragraph<'a> {
    let hints: &[(&str, &str)] = &[
        ("type a message", "ask octane to do something"),
        ("!command", "run a shell command"),
        ("@path", "attach a file"),
        ("/", "commands"),
        ("shift/alt+enter", "newline"),
        ("shift+tab", "cycle mode"),
    ];

    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(format!("  {}", glyphs.claw_mark()), theme.label(theme.accent)),
            Span::styled(" an agent that codes in your terminal", theme.dim()),
        ]),
        Line::default(),
    ];

    for (key, description) in hints {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<16}"), Style::default().fg(theme.accent)),
            Span::styled(description.to_string(), theme.dim()),
        ]));
    }

    if width >= 40 {
        lines.push(Line::default());
        lines.push(Line::styled(
            format!("  {}", glyphs.rule((width as usize).min(58).saturating_sub(2))),
            theme.dim(),
        ));
    }

    Paragraph::new(lines)
}

fn completion_widget<'a>(
    completion: &Completion,
    theme: &crate::theme::Theme,
) -> Paragraph<'a> {
    let (visible, highlight) = completion.visible();

    let lines: Vec<Line> = visible
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let selected = index == highlight;
            let marker = if selected { "\u{25b8} " } else { "  " };
            let style = if selected {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.assistant)
            };
            Line::from(vec![
                Span::styled(marker, Style::default().fg(theme.accent)),
                Span::styled(candidate.value.clone(), style),
                Span::styled(format!("  {}", candidate.detail), theme.dim()),
            ])
        })
        .collect();

    Paragraph::new(lines).block(
        Block::default().borders(Borders::ALL).border_style(theme.dim()),
    )
}

/// Place the popup directly above the composer, clamped to the screen.
fn popup_area(composer: Rect, completion: &Completion, screen: Rect) -> Rect {
    let rows = (completion.visible().0.len() as u16).min(crate::completion::MAX_VISIBLE as u16) + 2;
    let height = rows.min(screen.height.saturating_sub(composer.height).max(3));
    let width = composer.width;

    Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width,
        height,
    }
}

fn status_paragraph<'a>(
    status: &StatusLine,
    theme: &crate::theme::Theme,
    glyphs: &Glyphs,
) -> Paragraph<'a> {
    let mut spans = vec![Span::raw(" ")];
    for (index, segment) in status.segments().into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(format!(" {} ", glyphs.separator), theme.dim()));
        }
        spans.push(Span::styled(segment.text.clone(), Style::default().fg(segment.color(theme))));
    }
    spans.push(Span::styled("   ", theme.dim()));
    spans.push(Span::styled(status.hints(), theme.dim()));
    Paragraph::new(Line::from(spans))
}

/// Rows the composer needs at a given width.
///
/// Counts *wrapped* rows, not newlines. A single long line still occupies
/// several rows on screen, and sizing by newline count alone leaves the box one
/// row tall while the text runs off the end of it — which is what happens the
/// first time anyone pastes a paragraph.
fn composer_height(composer: &Composer, width: u16) -> u16 {
    // Two columns of border plus one of padding.
    let usable = usize::from(width.saturating_sub(3)).max(1);

    let rows: usize = composer
        .lines()
        .iter()
        .map(|line| line.chars().count().div_ceil(usable).max(1))
        .sum();

    u16::try_from(rows).unwrap_or(MAX_COMPOSER_ROWS).clamp(1, MAX_COMPOSER_ROWS)
}

fn approval_height(prompt: &ApprovalPrompt) -> u16 {
    let diff_rows = prompt
        .diff
        .as_ref()
        // Capped: a 500-line diff must not swallow the screen. `f` opens the
        // full view for anything larger.
        .map(|diff| u16::try_from(diff.lines().count()).unwrap_or(u16::MAX).min(12))
        .unwrap_or(0);
    2 + diff_rows
}

fn is_reasoning(kind: &octane_protocol::ItemKind) -> bool {
    matches!(kind, octane_protocol::ItemKind::Reasoning { .. })
}

/// Rows a picker needs: border, title, filter, and its visible rows.
fn picker_height(picker: &crate::picker::Picker) -> u16 {
    let rows = picker.visible().0.len().max(1);
    u16::try_from(rows).unwrap_or(10) + 4
}

/// A box of the given width and height, centred in `area`.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    // Clamped, so a small terminal gets a smaller box rather than a box that
    // starts off-screen.
    let width = width.min(area.width.saturating_sub(2)).max(20);
    let height = height.min(area.height.saturating_sub(2)).max(5);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn picker_widget<'a>(
    picker: &crate::picker::Picker,
    theme: &crate::theme::Theme,
    glyphs: &Glyphs,
) -> Paragraph<'a> {
    let (visible, highlight) = picker.visible();

    // Padded past the longest label rather than to a guessed width, or a long
    // one runs straight into its state with no gap — `Ollama (local)ready`.
    let column = visible
        .iter()
        .map(|item| item.label.chars().count())
        .max()
        .unwrap_or(0)
        + 2;

    let mut lines = vec![Line::from(vec![
        Span::styled(
            if picker.filter().is_empty() {
                "type to filter".to_string()
            } else {
                format!("{} {}", glyphs.prompt, picker.filter())
            },
            theme.dim(),
        ),
    ])];
    lines.push(Line::default());

    if visible.is_empty() {
        lines.push(Line::styled("  nothing matches", theme.dim()));
    }

    for (index, item) in visible.iter().enumerate() {
        let selected = index == highlight;
        let marker = if selected { glyphs.edit } else { " " };

        let label_style = if !item.enabled {
            theme.dim()
        } else if selected {
            Style::default().fg(theme.accent).add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            Style::default().fg(theme.assistant)
        };

        let mut spans = vec![
            Span::styled(format!("{marker} "), Style::default().fg(theme.accent)),
            Span::styled(format!("{:<column$}", item.label), label_style),
        ];
        if let Some(state) = &item.state {
            // The state is why a row is or is not usable, so it earns colour.
            let style = if item.enabled { theme.dim() } else { theme.label(theme.warning) };
            spans.push(Span::styled(state.clone(), style));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());
    lines.push(Line::styled(
        "  ↑↓ move   enter choose   esc cancel",
        theme.dim(),
    ));

    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(Span::styled(
                format!(" {} ", picker.title),
                theme.label(theme.accent),
            )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_permission::Resource;

    fn prompt(diff: Option<&str>) -> ApprovalPrompt {
        ApprovalPrompt {
            resource: Resource::write_file("/p/a.rs"),
            summary: "Write a.rs".into(),
            diff: diff.map(ToString::to_string),
        }
    }

    #[test]
    fn the_composer_grows_with_the_draft() {
        let mut composer = Composer::new();
        assert_eq!(composer_height(&composer, 80), 1);

        // This is what shift+enter does, and the box must follow it.
        for expected in 2..=6 {
            composer.newline();
            assert_eq!(composer_height(&composer, 80), expected);
        }
    }

    #[test]
    fn a_long_line_grows_the_box_by_wrapping() {
        // The bug this fixes: sizing by newline count leaves the box one row
        // tall while a pasted paragraph runs off the end of it.
        let mut composer = Composer::new();
        composer.insert_str(&"x".repeat(200));
        assert!(
            composer_height(&composer, 40) > 1,
            "a wrapped line must grow the box"
        );
    }

    #[test]
    fn wrapping_is_measured_against_the_usable_width() {
        let mut composer = Composer::new();
        composer.insert_str(&"x".repeat(77));
        // 80 wide minus two borders and a column of padding.
        assert_eq!(composer_height(&composer, 80), 1);

        composer.insert_str("xx");
        assert_eq!(composer_height(&composer, 80), 2);
    }

    #[test]
    fn a_narrow_terminal_does_not_divide_by_zero() {
        let mut composer = Composer::new();
        composer.insert_str("some text");
        for width in 0..=4 {
            assert!(composer_height(&composer, width) >= 1);
        }
    }

    #[test]
    fn a_runaway_draft_is_capped() {
        let mut composer = Composer::new();
        for _ in 0..100 {
            composer.newline();
        }
        assert_eq!(
            composer_height(&composer, 80),
            MAX_COMPOSER_ROWS,
            "the transcript must keep some rows"
        );
    }

    #[test]
    fn approval_height_accounts_for_the_diff_and_caps_it() {
        assert_eq!(approval_height(&prompt(None)), 2);
        assert_eq!(approval_height(&prompt(Some("+ one\n+ two\n"))), 4);

        let huge: String = (0..500).map(|i| format!("+ line {i}\n")).collect();
        assert_eq!(approval_height(&prompt(Some(&huge))), 14);
    }

    #[test]
    fn the_popup_sits_above_the_composer() {
        let screen = Rect::new(0, 0, 80, 30);
        let composer = Rect::new(0, 24, 80, 4);

        let mut completion = Completion::default();
        let files: Vec<String> = (0..20).map(|i| format!("src/f{i}.rs")).collect();
        completion.update("@f", 2, &[], &files);

        let popup = popup_area(composer, &completion, screen);

        assert!(popup.y + popup.height <= composer.y, "popup must not cover the input");
        assert_eq!(popup.x, composer.x);
        assert_eq!(popup.width, composer.width);
    }

    #[test]
    fn the_popup_never_runs_off_the_top() {
        // A short terminal with a tall draft is the case that overflows.
        let screen = Rect::new(0, 0, 80, 10);
        let composer = Rect::new(0, 2, 80, 6);

        let mut completion = Completion::default();
        let files: Vec<String> = (0..50).map(|i| format!("src/f{i}.rs")).collect();
        completion.update("@f", 2, &[], &files);

        let popup = popup_area(composer, &completion, screen);
        assert!(popup.y < screen.height);
        assert!(popup.height > 0);
    }
}

