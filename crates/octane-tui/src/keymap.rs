//! Key routing.
//!
//! Split out of [`crate::app`] because keybindings are behaviour worth pinning,
//! and the version living inside the runtime loop could only be tested by driving
//! a real terminal. This is a pure function: key plus a little state in, an
//! action out.
//!
//! The bindings themselves follow the survey (`RESEARCH.md` §I): readline habits
//! where they exist, and opencode's conventions where they do not.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::approval::{ApprovalPrompt, ApprovalReply};

/// What a keypress should cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    None,

    Insert(char),
    InsertNewline,
    Backspace,
    DeleteForward,
    MoveLeft,
    MoveRight,
    MoveLineStart,
    MoveLineEnd,
    MoveUp,
    HistoryPrevious,
    HistoryNext,
    Clear,

    Submit,
    /// Replace a trailing backslash with a newline.
    ContinueLine,
    CycleMode,
    Interrupt,
    Exit,

    /// Move the completion highlight.
    CompletionNext,
    CompletionPrevious,
    /// Insert the highlighted candidate.
    CompletionAccept,
    CompletionDismiss,

    /// Move the picker highlight.
    PickerNext,
    PickerPrevious,
    /// Choose the highlighted row.
    PickerChoose,
    PickerCancel,
    /// Narrow the picker.
    PickerFilter(char),
    PickerUnfilter,

    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,

    /// Settle a pending approval.
    Approve(ApprovalReply),
    /// Reject, handing the composer contents back as course correction.
    RejectWithComposerText,
}

/// The state a keypress is interpreted against.
#[derive(Debug, Clone, Copy)]
pub struct KeyContext<'a> {
    /// The agent is mid-turn.
    pub working: bool,
    pub composer_empty: bool,
    /// Set when an approval is on screen.
    pub approval: Option<&'a ApprovalPrompt>,
    /// The completion popup is open.
    pub completing: bool,
    /// The caret is on the first line of the draft.
    ///
    /// Distinguishes "browse history" from "move up a line" — without it, the up
    /// arrow makes multi-line drafts uneditable.
    pub on_first_line: bool,
    /// The text ends with a backslash, the portable newline escape hatch.
    pub ends_with_continuation: bool,
    /// A selection overlay is open.
    pub picking: bool,
}

/// Interpret a keypress.
pub fn route(key: KeyEvent, ctx: &KeyContext<'_>) -> KeyAction {
    if let Some(prompt) = ctx.approval {
        return route_approval(key, prompt, ctx.composer_empty);
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // A picker is modal: it owns every key until it is answered. Letting a
    // keystroke through would act on a composer the user cannot see.
    if ctx.picking {
        return match key.code {
            KeyCode::Up => KeyAction::PickerPrevious,
            KeyCode::Down => KeyAction::PickerNext,
            KeyCode::Enter | KeyCode::Tab => KeyAction::PickerChoose,
            KeyCode::Esc => KeyAction::PickerCancel,
            KeyCode::Backspace => KeyAction::PickerUnfilter,
            // Ctrl+C still exits: a modal that traps the user is a bug.
            KeyCode::Char('c') if ctrl => KeyAction::Exit,
            KeyCode::Char(ch) if !ctrl => KeyAction::PickerFilter(ch),
            _ => KeyAction::None,
        };
    }

    // The popup owns the arrows, tab, and enter while it is open. Anything else
    // falls through, so typing keeps refining the query rather than being eaten.
    if ctx.completing {
        match key.code {
            KeyCode::Up => return KeyAction::CompletionPrevious,
            KeyCode::Down => return KeyAction::CompletionNext,
            KeyCode::Tab => return KeyAction::CompletionAccept,
            // Only a plain Enter accepts, so the newline bindings still work
            // with the popup open.
            KeyCode::Enter if !shift && !alt => return KeyAction::CompletionAccept,
            KeyCode::Esc => return KeyAction::CompletionDismiss,
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char('c') if ctrl => KeyAction::Exit,
        // Only when empty, so ctrl+d mid-draft does not discard a session.
        KeyCode::Char('d') if ctrl && ctx.composer_empty => KeyAction::Exit,
        KeyCode::Char('u') if ctrl => KeyAction::Clear,
        KeyCode::Char('a') if ctrl => KeyAction::MoveLineStart,
        KeyCode::Char('e') if ctrl => KeyAction::MoveLineEnd,

        // Esc stops the work, it does not quit. Conflating the two loses
        // sessions to a reflex.
        KeyCode::Esc if ctx.working => KeyAction::Interrupt,
        KeyCode::Esc => KeyAction::None,

        KeyCode::BackTab => KeyAction::CycleMode,

        // Three ways to get a newline, because one is not portable.
        //
        // `shift+enter` is what people reach for, but most terminals send a bare
        // CR for it — indistinguishable from Enter — unless the keyboard
        // enhancement protocol is negotiated. It is requested at startup, so this
        // arm fires on Kitty, Ghostty, WezTerm, foot, and recent iTerm2.
        //
        // `alt+enter` arrives as ESC-prefixed CR and works essentially everywhere.
        //
        // A trailing backslash is the last resort: it needs no terminal support
        // at all, and it is the shell continuation people already know.
        KeyCode::Enter if shift || alt => KeyAction::InsertNewline,
        KeyCode::Enter if ctx.ends_with_continuation => KeyAction::ContinueLine,
        KeyCode::Enter => KeyAction::Submit,

        KeyCode::Backspace => KeyAction::Backspace,
        KeyCode::Delete => KeyAction::DeleteForward,
        KeyCode::Left => KeyAction::MoveLeft,
        KeyCode::Right => KeyAction::MoveRight,
        // Guarded arms must precede their unguarded counterparts, or the
        // modifier variant is unreachable. The compiler warns; heed it.
        KeyCode::Home if ctrl => KeyAction::ScrollTop,
        KeyCode::End if ctrl => KeyAction::ScrollBottom,
        KeyCode::Home => KeyAction::MoveLineStart,
        KeyCode::End => KeyAction::MoveLineEnd,
        // Scrolling the transcript, since going full screen gave up the
        // terminal's own scrollback.
        KeyCode::PageUp => KeyAction::PageUp,
        KeyCode::PageDown => KeyAction::PageDown,
        KeyCode::Up if shift => KeyAction::ScrollUp,
        KeyCode::Down if shift => KeyAction::ScrollDown,

        // Up browses history only from the first line, so it still navigates a
        // multi-line draft.
        KeyCode::Up if ctx.on_first_line => KeyAction::HistoryPrevious,
        KeyCode::Up => KeyAction::MoveUp,
        KeyCode::Down => KeyAction::HistoryNext,

        KeyCode::Char(ch) if !ctrl => KeyAction::Insert(ch),
        _ => KeyAction::None,
    }
}

/// Keys while an approval is pending.
///
/// The important rule: a key that is not a shortcut goes into the composer, so
/// typing "no, use the other file" works. If letters were swallowed as unknown
/// shortcuts, the redirect affordance would be unusable — and redirecting is the
/// common case, not a rare one.
fn route_approval(key: KeyEvent, prompt: &ApprovalPrompt, composer_empty: bool) -> KeyAction {
    match key.code {
        // Shortcuts apply only on an empty composer. Once the user has started
        // typing instructions, `n` is a letter in a sentence.
        KeyCode::Char(ch) if composer_empty => match prompt.key(ch) {
            Some(reply) => KeyAction::Approve(reply),
            None => KeyAction::Insert(ch),
        },
        KeyCode::Char(ch) => KeyAction::Insert(ch),

        KeyCode::Backspace => KeyAction::Backspace,
        KeyCode::Left => KeyAction::MoveLeft,
        KeyCode::Right => KeyAction::MoveRight,

        KeyCode::Enter if !composer_empty => KeyAction::RejectWithComposerText,
        // Enter on an empty prompt is ambiguous, so it does nothing rather than
        // guessing between allow and reject.
        KeyCode::Enter => KeyAction::None,

        KeyCode::Esc => KeyAction::Approve(ApprovalReply::Reject),
        _ => KeyAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octane_permission::Resource;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn idle() -> KeyContext<'static> {
        KeyContext { working: false, composer_empty: true, approval: None, completing: false, on_first_line: true, ends_with_continuation: false, picking: false }
    }

    fn drafting() -> KeyContext<'static> {
        KeyContext { working: false, composer_empty: false, approval: None, completing: false, on_first_line: true, ends_with_continuation: false, picking: false }
    }

    fn working() -> KeyContext<'static> {
        KeyContext { working: true, composer_empty: true, approval: None, completing: false, on_first_line: true, ends_with_continuation: false, picking: false }
    }

    fn prompt(diff: Option<&str>) -> ApprovalPrompt {
        ApprovalPrompt {
            resource: Resource::write_file("/p/a.rs"),
            summary: "Write a.rs".into(),
            diff: diff.map(ToString::to_string),
        }
    }

    fn completing() -> KeyContext<'static> {
        KeyContext {
            working: false,
            composer_empty: false,
            approval: None,
            completing: true,
            on_first_line: true,
            ends_with_continuation: false,
            picking: false,
        }
    }

    #[test]
    fn the_popup_owns_the_arrows_tab_and_enter() {
        let ctx = completing();
        assert_eq!(route(key(KeyCode::Down), &ctx), KeyAction::CompletionNext);
        assert_eq!(route(key(KeyCode::Up), &ctx), KeyAction::CompletionPrevious);
        assert_eq!(route(key(KeyCode::Tab), &ctx), KeyAction::CompletionAccept);
        assert_eq!(route(key(KeyCode::Enter), &ctx), KeyAction::CompletionAccept);
        assert_eq!(route(key(KeyCode::Esc), &ctx), KeyAction::CompletionDismiss);
    }

    #[test]
    fn typing_still_refines_the_query_while_completing() {
        // Swallowing letters would make the popup impossible to narrow.
        assert_eq!(route(key(KeyCode::Char('x')), &completing()), KeyAction::Insert('x'));
        assert_eq!(route(key(KeyCode::Backspace), &completing()), KeyAction::Backspace);
    }

    #[test]
    fn shift_enter_still_makes_a_newline_while_completing() {
        let ctx = completing();
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(route(shift_enter, &ctx), KeyAction::InsertNewline);
    }

    #[test]
    fn the_transcript_can_be_scrolled() {
        let ctx = idle();
        assert_eq!(route(key(KeyCode::PageUp), &ctx), KeyAction::PageUp);
        assert_eq!(route(key(KeyCode::PageDown), &ctx), KeyAction::PageDown);
        assert_eq!(
            route(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT), &ctx),
            KeyAction::ScrollUp
        );
        assert_eq!(route(ctrl_key(KeyCode::Home), &ctx), KeyAction::ScrollTop);
        assert_eq!(route(ctrl_key(KeyCode::End), &ctx), KeyAction::ScrollBottom);
    }

    #[test]
    fn up_browses_history_only_from_the_first_line() {
        let first = KeyContext { on_first_line: true, ..drafting() };
        assert_eq!(route(key(KeyCode::Up), &first), KeyAction::HistoryPrevious);

        // Otherwise a multi-line draft cannot be edited above its last line.
        let later = KeyContext { on_first_line: false, ..drafting() };
        assert_eq!(route(key(KeyCode::Up), &later), KeyAction::MoveUp);
    }

    fn picking() -> KeyContext<'static> {
        KeyContext {
            working: false,
            composer_empty: true,
            approval: None,
            completing: false,
            on_first_line: true,
            ends_with_continuation: false,
            picking: true,
        }
    }

    #[test]
    fn a_picker_owns_every_key_until_answered() {
        // Letting a keystroke through would act on a composer the user cannot see.
        let ctx = picking();
        assert_eq!(route(key(KeyCode::Down), &ctx), KeyAction::PickerNext);
        assert_eq!(route(key(KeyCode::Up), &ctx), KeyAction::PickerPrevious);
        assert_eq!(route(key(KeyCode::Enter), &ctx), KeyAction::PickerChoose);
        assert_eq!(route(key(KeyCode::Esc), &ctx), KeyAction::PickerCancel);
        assert_eq!(route(key(KeyCode::Backspace), &ctx), KeyAction::PickerUnfilter);
    }

    #[test]
    fn typing_in_a_picker_filters_it() {
        assert_eq!(route(key(KeyCode::Char('r')), &picking()), KeyAction::PickerFilter('r'));
    }

    #[test]
    fn a_picker_does_not_trap_the_user() {
        // A modal with no way out is a bug, whatever else it does.
        assert_eq!(route(ctrl('c'), &picking()), KeyAction::Exit);
    }

    #[test]
    fn a_picker_suppresses_mode_cycling_and_scrolling() {
        // Both would change state behind an overlay the user is answering.
        let ctx = picking();
        assert_eq!(route(key(KeyCode::BackTab), &ctx), KeyAction::None);
        assert_eq!(route(key(KeyCode::PageUp), &ctx), KeyAction::None);
    }

    #[test]
    fn typing_inserts() {
        assert_eq!(route(key(KeyCode::Char('a')), &idle()), KeyAction::Insert('a'));
    }

    #[test]
    fn enter_submits_and_shift_enter_newlines() {
        assert_eq!(route(key(KeyCode::Enter), &drafting()), KeyAction::Submit);
        assert_eq!(
            route(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &drafting()),
            KeyAction::InsertNewline
        );
    }

    #[test]
    fn alt_enter_is_the_portable_newline() {
        // Most terminals send a bare CR for shift+enter, so this is the binding
        // that actually works without negotiating the enhancement protocol.
        assert_eq!(
            route(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &drafting()),
            KeyAction::InsertNewline
        );
    }

    #[test]
    fn a_trailing_backslash_continues_the_line() {
        let ctx = KeyContext { ends_with_continuation: true, ..drafting() };
        assert_eq!(route(key(KeyCode::Enter), &ctx), KeyAction::ContinueLine);

        // And without one, Enter still sends.
        assert_eq!(route(key(KeyCode::Enter), &drafting()), KeyAction::Submit);
    }

    #[test]
    fn newline_bindings_survive_an_open_popup() {
        let ctx = completing();
        assert_eq!(
            route(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &ctx),
            KeyAction::InsertNewline
        );
        // A plain Enter still accepts the highlighted candidate.
        assert_eq!(route(key(KeyCode::Enter), &ctx), KeyAction::CompletionAccept);
    }

    #[test]
    fn esc_interrupts_while_working_and_does_nothing_when_idle() {
        assert_eq!(route(key(KeyCode::Esc), &working()), KeyAction::Interrupt);
        // Must not exit: losing a session to a reflexive escape is unforgivable.
        assert_eq!(route(key(KeyCode::Esc), &idle()), KeyAction::None);
    }

    #[test]
    fn ctrl_d_exits_only_on_an_empty_composer() {
        assert_eq!(route(ctrl('d'), &idle()), KeyAction::Exit);
        assert_eq!(
            route(ctrl('d'), &drafting()),
            KeyAction::None,
            "ctrl+d mid-draft must not discard the session"
        );
    }

    #[test]
    fn ctrl_c_always_exits() {
        assert_eq!(route(ctrl('c'), &idle()), KeyAction::Exit);
        assert_eq!(route(ctrl('c'), &drafting()), KeyAction::Exit);
        assert_eq!(route(ctrl('c'), &working()), KeyAction::Exit);
    }

    #[test]
    fn readline_habits_are_honoured() {
        assert_eq!(route(ctrl('u'), &drafting()), KeyAction::Clear);
        assert_eq!(route(ctrl('a'), &drafting()), KeyAction::MoveLineStart);
        assert_eq!(route(ctrl('e'), &drafting()), KeyAction::MoveLineEnd);
    }

    #[test]
    fn shift_tab_cycles_mode() {
        assert_eq!(route(key(KeyCode::BackTab), &idle()), KeyAction::CycleMode);
    }

    #[test]
    fn control_chords_do_not_leak_into_the_composer() {
        // Otherwise an unhandled chord types a stray letter into the prompt.
        assert_eq!(route(ctrl('z'), &idle()), KeyAction::None);
        assert_eq!(route(ctrl('k'), &idle()), KeyAction::None);
    }

    #[test]
    fn approval_shortcuts_settle_the_prompt() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };

        assert_eq!(
            route(key(KeyCode::Char('y')), &ctx),
            KeyAction::Approve(ApprovalReply::Allow)
        );
        assert_eq!(
            route(key(KeyCode::Char('n')), &ctx),
            KeyAction::Approve(ApprovalReply::Reject)
        );
        assert_eq!(
            route(key(KeyCode::Esc), &ctx),
            KeyAction::Approve(ApprovalReply::Reject)
        );
    }

    #[test]
    fn unknown_keys_during_an_approval_start_an_instruction() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };

        // "use the other file" begins with 'u', which is not a shortcut.
        assert_eq!(route(key(KeyCode::Char('u')), &ctx), KeyAction::Insert('u'));
    }

    #[test]
    fn once_typing_has_started_shortcut_letters_are_just_letters() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: false, approval: Some(&prompt), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };

        // The 'n' in "run the other one" must not reject the prompt.
        assert_eq!(route(key(KeyCode::Char('n')), &ctx), KeyAction::Insert('n'));
        assert_eq!(route(key(KeyCode::Char('y')), &ctx), KeyAction::Insert('y'));
    }

    #[test]
    fn enter_with_instructions_rejects_with_them() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: false, approval: Some(&prompt), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };
        assert_eq!(route(key(KeyCode::Enter), &ctx), KeyAction::RejectWithComposerText);
    }

    #[test]
    fn enter_on_an_empty_approval_prompt_does_not_guess() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };
        assert_eq!(
            route(key(KeyCode::Enter), &ctx),
            KeyAction::None,
            "neither allow nor reject is a safe guess here"
        );
    }

    #[test]
    fn the_diff_shortcut_is_live_only_with_a_diff() {
        let without = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&without), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };
        assert_eq!(route(key(KeyCode::Char('f')), &ctx), KeyAction::Insert('f'));

        let with = prompt(Some("+ line"));
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&with), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };
        assert_eq!(
            route(key(KeyCode::Char('f')), &ctx),
            KeyAction::Approve(ApprovalReply::ShowDiff)
        );
    }

    #[test]
    fn an_approval_blocks_submission_and_mode_cycling() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt), completing: false, on_first_line: true, ends_with_continuation: false, picking: false };
        // Changing mode or sending a prompt mid-approval would be answering a
        // different question than the one on screen.
        assert_eq!(route(key(KeyCode::BackTab), &ctx), KeyAction::None);
    }
}
