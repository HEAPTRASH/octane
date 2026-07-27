//! The runtime loop.
//!
//! The terminal, async, and I/O live here and nowhere else. Everything this
//! module draws it draws by asking a [`Pane`](crate::component::Pane) — it owns
//! the layout, not the appearance of anything in it.
//!
//! # Full screen
//!
//! The alternate screen, with a fixed vertical stack: header, transcript,
//! approval, activity, composer, status. Each band's size comes from the pane
//! that fills it, so adding or resizing one is a change in that pane's module.
//!
//! This trades away the terminal's own scrollback, search, and selection, so
//! [`crate::transcript`] provides scrolling in their place.
//!
//! # Flicker
//!
//! Two rules, both invisible until violated because their absence still renders
//! correctly:
//!
//! - Redraw only when something changed. The caller polls on a tick, so an
//!   unconditional draw repaints ~12 times a second forever.
//! - Wrap every frame in synchronized output (`CSI ?2026h` / `l`) so the terminal
//!   presents it atomically instead of tearing.
//!
//! `scripts/tui-smoke.py` measures idle bytes on a pty and is the only thing that
//! catches a regression in either.

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
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear};

use crate::approval::{ApprovalPane, ApprovalPrompt, ApprovalReply};
use crate::banner;
use crate::completion::{self, Candidate, Completion};
use crate::component::{self, Pane};
use crate::composer::{Composer, ComposerPane, Submission};
use crate::picker;
use crate::glyphs::Glyphs;
use crate::keymap::{self, KeyAction, KeyContext};
use crate::render::{RenderOptions, render_event};
use crate::status::{ActivityPane, StatusLine, StatusPane};
use crate::transcript::{Transcript, TranscriptView, MIN_ROWS};

/// Rows the fixed bands claim before the transcript gets what is left: three of
/// header, three of composer at its smallest, one of status.
///
/// Only used to guess the transcript's height for wrapping, which happens before
/// the layout runs. The layout itself asks each pane, so a wrong guess here
/// costs a slightly-off wrap width, never a misplaced pane.
const FIXED_ROWS: u16 = 7;

/// Stands in for a pane that is not showing, so the slot indices stay fixed.
struct Empty;

impl Pane for Empty {
    fn constraint(&self, _width: u16) -> Constraint {
        Constraint::Length(0)
    }
    fn render(&self, _area: Rect, _buf: &mut ratatui::buffer::Buffer) {}
}

const EMPTY: Empty = Empty;

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

    /// Candidate universes for completion, supplied by the caller.
    commands: Vec<Candidate>,
    files: Vec<String>,

    pending_approval: Option<(ApprovalPrompt, tokio::sync::oneshot::Sender<ApprovalReply>)>,
    /// The activity held aside while an approval waits, restored when it is
    /// answered. A spinner means work is happening; while the turn is blocked
    /// on the user, none is, and the one thing that should draw the eye is the
    /// prompt.
    parked_activity: Option<crate::status::Activity>,
    /// Whether the pending approval's diff is shown past its cap.
    approval_expanded: bool,
    /// Tool results that can be shown in full, kept so a toggle can re-render
    /// them. Only these are retained: everything else in the transcript is
    /// already whole.
    expandable: std::collections::HashMap<octane_protocol::ItemId, Event>,
    expanded: std::collections::HashSet<octane_protocol::ItemId>,
    /// The transcript's rectangle at the last draw, for mapping a click.
    body_area: ratatui::layout::Rect,
    /// Terminal width at the last draw.
    ///
    /// Up and down move by *display* row, so they need to know where the text
    /// wraps — which is a property of the width it was drawn at, not of the
    /// text. Captured here rather than queried at keypress so motion and the
    /// frame the user is looking at agree.
    drawn_width: u16,
    spinner_frame: usize,
    started: Instant,

    workspace: String,
    sandboxed: bool,

    raw_mode: bool,
    /// True once the session has a real exchange, as opposed to startup
    /// notices. Guidance stays on screen until then.
    conversing: bool,
    dirty: bool,
    /// The item currently streaming: its id, accumulated text, and whether it is
    /// reasoning rather than prose.
    streaming: Option<(octane_protocol::ItemId, String, bool)>,
    /// Whole markdown blocks of the streaming message, already rendered.
    ///
    /// Kept so a delta re-renders only the unfinished tail. Reset whenever
    /// `streaming` is, which is the one thing that must stay true: a stale
    /// prefix here would prepend the previous message to the current one.
    stream_stable: Vec<Line<'static>>,
    /// Byte offset in the streaming text that `stream_stable` covers.
    stream_committed: usize,
    /// An open selection overlay.
    /// Open pickers, innermost last.
    ///
    /// A stack rather than one slot, because `/settings` opens a value picker
    /// from a setting picker: with a single slot the first is destroyed the
    /// moment the second opens, so there is nothing to go back to and Esc can
    /// only abandon the whole flow.
    pickers: Vec<crate::picker::Picker>,
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
            commands: Vec::new(),
            files: Vec::new(),
            pending_approval: None,
            parked_activity: None,
            approval_expanded: false,
            expandable: std::collections::HashMap::new(),
            expanded: std::collections::HashSet::new(),
            body_area: ratatui::layout::Rect::default(),
            drawn_width: 80,
            spinner_frame: 0,
            started: Instant::now(),
            workspace,
            sandboxed,
            raw_mode: true,
            conversing: false,
            dirty: true,
            streaming: None,
            stream_stable: Vec::new(),
            stream_committed: 0,
            pickers: Vec::new(),
        })
    }

    pub fn status_mut(&mut self) -> &mut StatusLine {
        self.dirty = true;
        &mut self.status
    }

    /// The active render options, including the glyph set.
    ///
    /// Read-only, so a caller rendering its own line — the CLI builds the status
    /// line's activity label — uses the same set as everything else. Without it
    /// that caller had to assume Unicode, and the ASCII fallback stopped short
    /// of the status line.
    pub fn options(&self) -> &RenderOptions {
        &self.options
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

        // An error is a notice, not a conversation. Everything else that
        // reaches the transcript means work has started and the guidance has
        // served its purpose.
        if let Event::Item(
            ItemEvent::Started { item, .. } | ItemEvent::Completed { item, .. },
        ) = event
        {
            if !matches!(item.kind, octane_protocol::ItemKind::Error { .. }) {
                self.conversing = true;
                // The status line's hints recede once the session is real, so
                // it has to learn the same fact at the same moment.
                self.status.conversing = true;
            }
        }

        match event {
            Event::Item(ItemEvent::Started { item, .. }) => {
                self.streaming = Some((item.id.clone(), String::new(), is_reasoning(&item.kind)));
                // Reset together with the text they describe. Carried over,
                // the previous message's committed blocks would be prepended
                // to this one.
                self.stream_stable.clear();
                self.stream_committed = 0;
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
                self.push_rendered(event);
            }
            _ => {
                self.push_rendered(event);
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
    /// Rebuild the streaming region from the accumulated text.
    ///
    /// Split in two, following Codex's streaming controller: whole markdown
    /// blocks are rendered once into `stream_stable` and never touched again,
    /// and only the unfinished tail after them is re-rendered per delta. Before
    /// this the whole message was rebuilt on every token — quadratic in its
    /// length, and visibly so on a long answer.
    ///
    /// It also means the streaming text is *markdown* rather than raw source,
    /// so a message no longer reflows and restyles the instant it completes.
    fn refresh_pending(&mut self) {
        let Some((_, text, reasoning)) = &self.streaming else {
            self.transcript.clear_pending();
            self.stream_stable.clear();
            self.stream_committed = 0;
            return;
        };
        if *reasoning && self.options.reasoning == crate::render::Reasoning::Hidden {
            self.transcript.clear_pending();
            return;
        }

        // Reasoning is a thought, not a document: rendering it as markdown
        // would style whatever punctuation the model happened to emit.
        if *reasoning {
            let style = self.options.theme.reasoning();
            let lines = text
                .split('\n')
                .map(|line| Line::styled(format!("  {line}"), style))
                .collect();
            self.transcript.set_pending(lines);
            self.dirty = true;
            return;
        }

        let (theme, glyphs) = (&self.options.theme, &self.options.glyphs);
        let boundary = crate::markdown::stable_prefix(text);
        if boundary > self.stream_committed {
            let settled = &text[self.stream_committed..boundary];
            self.stream_stable.extend(crate::markdown::render_committed(settled, theme, glyphs));
            self.stream_committed = boundary;
        }

        let mut lines = self.stream_stable.clone();
        lines.extend(crate::markdown::render(&text[self.stream_committed..], theme, glyphs));

        self.transcript.set_pending(lines);
        self.dirty = true;
    }

    /// Render an event into the transcript, remembering it if it can expand.
    ///
    /// A tool result whose body was clipped is the only thing worth keeping:
    /// re-rendering needs the event, and holding every event would grow
    /// without bound in a long session.
    fn push_rendered(&mut self, event: &Event) {
        let lines = render_event(event, &self.options);
        if lines.is_empty() {
            return;
        }
        match expandable_id(event) {
            Some(id) => {
                self.expandable.insert(id.clone(), event.clone());
                self.transcript.push_owned(id, lines);
                self.dirty = true;
            }
            None => self.push_lines(lines),
        }
    }

    /// Show or clip one tool result, re-rendering it in place.
    fn toggle_expanded(&mut self, id: &octane_protocol::ItemId) {
        let Some(event) = self.expandable.get(id).cloned() else { return };
        let now = !self.expanded.contains(id);
        if now {
            self.expanded.insert(id.clone());
        } else {
            self.expanded.remove(id);
        }
        let lines = crate::render::render_event_expanded(&event, &self.options, now);
        self.transcript.replace_region(id, lines);
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
        self.approval_expanded = false;
        self.parked_activity = self.status.activity.take();
        self.dirty = true;
    }

    /// Write a line into the transcript that no model produced.
    ///
    /// Command output, a report, a refusal: they belong in the transcript, but
    /// there is no turn or item to attach them to. Synthesising the completed
    /// item is one line here and was twenty-odd copies of it at the call sites.
    pub fn note(&mut self, kind: octane_protocol::ItemKind) -> Result<()> {
        use octane_protocol::{Item, ItemEvent, ItemId, ItemStatus, TurnId};
        self.push_event(&Event::Item(ItemEvent::Completed {
            turn_id: TurnId::new(),
            item: Item { id: ItemId::new(), kind, status: ItemStatus::Completed },
        }))
    }

    /// A note carrying prose, as an agent message.
    pub fn say(&mut self, text: impl Into<String>) -> Result<()> {
        self.note(octane_protocol::ItemKind::AgentMessage { text: text.into() })
    }

    /// A note carrying a failure.
    pub fn report_error(&mut self, message: impl Into<String>) -> Result<()> {
        self.note(octane_protocol::ItemKind::Error { message: message.into() })
    }

    /// Columns of the composer that hold text, at the width last drawn.
    fn composer_usable(&self) -> usize {
        crate::composer::usable_width(self.drawn_width)
    }

    /// Drop a pending approval without answering it.
    ///
    /// For a caller that can no longer service the prompt it installed. Leaving
    /// it installed is worse than declining: a prompt whose responder is gone
    /// still owns every keystroke, so the next thing typed disappears into it.
    pub fn dismiss_approval(&mut self) {
        if self.pending_approval.take().is_some() {
            self.status.activity = self.parked_activity.take();
            self.dirty = true;
        }
    }

    /// Empty the transcript, returning to the quiet start screen.
    pub fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.streaming = None;
        self.conversing = false;
        self.dirty = true;
    }

    /// Close every open picker.
    ///
    /// For a choice that ends the flow. A choice that opens a further level
    /// calls [`Self::set_picker`] instead, which pushes.
    pub fn close_pickers(&mut self) {
        if !self.pickers.is_empty() {
            self.pickers.clear();
            self.dirty = true;
        }
    }

    /// Open a selection overlay on top of any already showing.
    pub fn set_picker(&mut self, picker: crate::picker::Picker) {
        self.pickers.push(picker);
        self.dirty = true;
    }

    /// Redraw, if anything changed.
    ///
    /// The whole frame is: build a pane per band, ask each how tall it wants to
    /// be, split once, draw each into what it got. Nothing here knows what a
    /// composer or a status line looks like — that lives with the state it
    /// draws, which is the point of [`Pane`].
    pub fn draw(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.dirty = false;

        let _ = crossterm::execute!(io::stdout(), BeginSynchronizedUpdate);

        let size = self.terminal.size().unwrap_or(ratatui::layout::Size { width: 80, height: 24 });
        self.drawn_width = size.width;

        // The transcript is wrapped before the frame rather than inside it,
        // because wrapping mutates its cache and a pane draws from `&self`.
        // Keyed on whether a conversation has started, not on whether the
        // transcript has lines: startup notices are lines, and an unconfigured
        // provider prints two errors, which replaced the guidance for exactly
        // the user who had never seen it.
        let body_height = size.height.saturating_sub(FIXED_ROWS).max(MIN_ROWS);
        let mut lines =
            self.transcript.visible(size.width as usize, body_height as usize);
        if !self.conversing {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(banner::empty_state_lines(
                &self.options.theme,
                &self.options.glyphs,
                size.width,
                body_height,
            ));
        }

        // Borrowed field by field, not cloned. These used to be eight `.clone()`
        // calls a frame purely so the draw closure could see them past the
        // `&mut self.terminal`; disjoint field borrows say the same thing for
        // nothing.
        let options = &self.options;
        let header =
            banner::Header { workspace: &self.workspace, sandboxed: self.sandboxed, options };
        let transcript = TranscriptView { lines, style: options.theme.canvas() };
        // Rendered once, before the frame. Both halves of the pane need these
        // rows and highlighting a diff is expensive, so computing them inside
        // the pane paid for it twice a frame.
        let approval_diff = self
            .pending_approval
            .as_ref()
            .map(|(prompt, _)| {
                ApprovalPane::diff_rows(prompt, self.approval_expanded, &self.options)
            })
            .unwrap_or_default();
        let approval = self
            .pending_approval
            .as_ref()
            .map(|(prompt, _)| ApprovalPane { prompt, diff: &approval_diff, options });
        let activity =
            ActivityPane { status: &self.status, spinner_frame: self.spinner_frame, options };
        let composer = ComposerPane { composer: &self.composer, options };
        let status = StatusPane { status: &self.status, options };

        // Order is top to bottom. An absent approval still occupies a slot so
        // the indices below stay put; its constraint is then zero rows.
        let panes: [&dyn Pane; 6] = [
            &header,
            &transcript,
            approval.as_ref().map_or(&EMPTY as &dyn Pane, |a| a),
            &activity,
            &composer,
            &status,
        ];
        let constraints: Vec<Constraint> =
            panes.iter().map(|pane| pane.constraint(size.width)).collect();

        let mut drawn_body = Rect::default();
        let pickers = &self.pickers;
        let completion = &self.completion;
        let terminal = &mut self.terminal;

        terminal.draw(|frame| {
            let screen = frame.area();
            // Terminal themes vary wildly. Painting the canvas makes the
            // website's ink/paper contrast part of Octane rather than an
            // accident of the user's profile.
            frame.render_widget(Block::default().style(options.theme.canvas()), screen);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(screen);

            for (pane, area) in panes.iter().zip(chunks.iter()) {
                pane.render(*area, frame.buffer_mut());
            }

            // Kept for click mapping; leaving it at its default makes every
            // click miss.
            drawn_body = chunks[1];

            // Set on the frame, not on the terminal afterwards: ratatui hides
            // the cursor for any frame that does not ask for it, so a call made
            // after `draw` returns is undone by the `draw` that follows.
            let (x, y) = composer.caret(chunks[4]);
            frame.set_cursor_position((x, y));

            // Overlays float above the stack, so they are drawn last and are
            // not part of the layout.
            if !pickers.is_empty() {
                let (widget, height) = picker::widget(pickers, &options.theme, &options.glyphs);
                let area = component::centred(screen, 66, height);
                frame.render_widget(Clear, area);
                frame.render_widget(widget, area);
            }

            // The completion popup sits directly above the composer, so it never
            // displaces the input the user is typing into.
            if completion.is_active() {
                let popup = completion::popup_area(chunks[4], completion, screen);
                frame.render_widget(Clear, popup);
                frame.render_widget(
                    completion::widget(completion, popup.width, &options.theme, &options.glyphs),
                    popup,
                );
            }
        })?;

        self.body_area = drawn_body;
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
                    // A click inside the transcript expands or clips the tool
                    // result under it. The row is a viewport offset, so the
                    // scroll position turns it back into an absolute line.
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        let area = self.body_area;
                        if mouse.row >= area.y
                            && mouse.row < area.y + area.height
                            && mouse.column >= area.x
                            && mouse.column < area.x + area.width
                        {
                            let row = usize::from(mouse.row - area.y);
                            let line = self.transcript.scroll_offset() + row;
                            if let Some(id) = self.transcript.owner_of(line) {
                                self.toggle_expanded(&id);
                            }
                        }
                    }
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
                // By display row, so a wrapped draft is navigable inside
                // itself before either arrow reaches for history.
                on_first_line: self.composer.row_position(self.composer_usable()).0 == 0,
                on_last_line: {
                    let (at, total) = self.composer.row_position(self.composer_usable());
                    at + 1 >= total
                },
                ends_with_continuation: self.composer.ends_with_continuation(),
                picking: !self.pickers.is_empty(),
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
                self.composer.move_up(self.composer_usable());
                None
            }
            KeyAction::MoveDown => {
                self.composer.move_down(self.composer_usable());
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
            KeyAction::MoveWordLeft => {
                self.composer.move_word_left();
                None
            }
            KeyAction::MoveWordRight => {
                self.composer.move_word_right();
                None
            }
            KeyAction::DeleteWordBackward => {
                self.composer.delete_word_backward();
                None
            }
            KeyAction::DeleteWordForward => {
                self.composer.delete_word_forward();
                None
            }
            KeyAction::KillToLineStart => {
                self.composer.kill_to_line_start();
                None
            }
            KeyAction::KillToLineEnd => {
                self.composer.kill_to_line_end();
                None
            }
            KeyAction::Yank => {
                self.composer.yank();
                None
            }
            KeyAction::Undo => {
                self.composer.undo();
                None
            }
            KeyAction::Redo => {
                self.composer.redo();
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
                if let Some(picker) = self.pickers.last_mut() {
                    picker.select_next();
                }
                None
            }
            KeyAction::PickerPrevious => {
                if let Some(picker) = self.pickers.last_mut() {
                    picker.select_previous();
                }
                None
            }
            KeyAction::PickerFilter(ch) => {
                if let Some(picker) = self.pickers.last_mut() {
                    picker.push_filter(ch);
                }
                None
            }
            KeyAction::PickerUnfilter => {
                if let Some(picker) = self.pickers.last_mut() {
                    picker.pop_filter();
                }
                None
            }
            // Layered, in the order a user undoes things: the filter is the
            // most recent narrowing, then the level, then the overlay. Losing
            // a hard-won position in a long list to one mistyped character and
            // a reflexive Esc is the failure this prevents.
            // Left is unambiguous: it always means "up". Esc is layered and
            // clears a filter first, which is what the user wants from Esc and
            // not what they want from an arrow.
            KeyAction::ToggleLastOutput => {
                // The most recent one, which is what "expand that" means when
                // there is no pointer to say which.
                if let Some(id) = self.transcript.last_owner() {
                    self.toggle_expanded(&id);
                }
                None
            }

            KeyAction::PickerAscend => {
                if self.pickers.len() > 1 {
                    self.pickers.pop();
                }
                None
            }

            KeyAction::PickerCancel => {
                match self.pickers.last_mut() {
                    Some(picker) if !picker.filter().is_empty() => picker.clear_filter(),
                    _ => {
                        self.pickers.pop();
                    }
                }
                None
            }
            KeyAction::PickerChoose => {
                let chosen = self
                    .pickers
                    .last()
                    .and_then(|picker| picker.choose().map(ToString::to_string));
                match chosen {
                    Some(key) => {
                        // The stack is left standing. A choice may open another
                        // level, and the caller is the only thing that knows
                        // which: closing here would throw away the parent the
                        // user needs to go back to.
                        let kind = self.pickers.last().map(|picker| picker.kind.clone());
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
        // `f` is not a decision: it grows the diff and leaves the question
        // standing. It was advertised and did nothing, because this early
        // return swallowed it before anything acted on it.
        if matches!(reply, ApprovalReply::ShowDiff) {
            self.approval_expanded = !self.approval_expanded;
            self.dirty = true;
            return;
        }
        if !reply.is_decision() {
            return;
        }
        if self.pending_approval.is_some() {
            self.status.activity = self.parked_activity.take();
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

/// The item id of a tool result that could be shown in full.
///
/// Only tool results clip anything, so only they can expand.
fn expandable_id(event: &Event) -> Option<octane_protocol::ItemId> {
    let octane_protocol::Event::Item(octane_protocol::ItemEvent::Completed { item, .. }) = event
    else {
        return None;
    };
    matches!(item.kind, octane_protocol::ItemKind::ToolResult { .. }).then(|| item.id.clone())
}

fn is_reasoning(kind: &octane_protocol::ItemKind) -> bool {
    matches!(kind, octane_protocol::ItemKind::Reasoning { .. })
}
