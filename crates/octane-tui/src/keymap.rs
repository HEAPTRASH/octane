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
    HistoryPrevious,
    HistoryNext,
    Clear,

    Submit,
    CycleMode,
    Interrupt,
    Exit,

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
}

/// Interpret a keypress.
pub fn route(key: KeyEvent, ctx: &KeyContext<'_>) -> KeyAction {
    if let Some(prompt) = ctx.approval {
        return route_approval(key, prompt, ctx.composer_empty);
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

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

        // Shift+Enter is the newline; plain Enter sends. The other way round
        // means every multi-line prompt gets sent a line at a time.
        KeyCode::Enter if shift => KeyAction::InsertNewline,
        KeyCode::Enter => KeyAction::Submit,

        KeyCode::Backspace => KeyAction::Backspace,
        KeyCode::Delete => KeyAction::DeleteForward,
        KeyCode::Left => KeyAction::MoveLeft,
        KeyCode::Right => KeyAction::MoveRight,
        KeyCode::Home => KeyAction::MoveLineStart,
        KeyCode::End => KeyAction::MoveLineEnd,
        KeyCode::Up => KeyAction::HistoryPrevious,
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

    fn idle() -> KeyContext<'static> {
        KeyContext { working: false, composer_empty: true, approval: None }
    }

    fn drafting() -> KeyContext<'static> {
        KeyContext { working: false, composer_empty: false, approval: None }
    }

    fn working() -> KeyContext<'static> {
        KeyContext { working: true, composer_empty: true, approval: None }
    }

    fn prompt(diff: Option<&str>) -> ApprovalPrompt {
        ApprovalPrompt {
            resource: Resource::write_file("/p/a.rs"),
            summary: "Write a.rs".into(),
            diff: diff.map(ToString::to_string),
        }
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
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt) };

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
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt) };

        // "use the other file" begins with 'u', which is not a shortcut.
        assert_eq!(route(key(KeyCode::Char('u')), &ctx), KeyAction::Insert('u'));
    }

    #[test]
    fn once_typing_has_started_shortcut_letters_are_just_letters() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: false, approval: Some(&prompt) };

        // The 'n' in "run the other one" must not reject the prompt.
        assert_eq!(route(key(KeyCode::Char('n')), &ctx), KeyAction::Insert('n'));
        assert_eq!(route(key(KeyCode::Char('y')), &ctx), KeyAction::Insert('y'));
    }

    #[test]
    fn enter_with_instructions_rejects_with_them() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: false, approval: Some(&prompt) };
        assert_eq!(route(key(KeyCode::Enter), &ctx), KeyAction::RejectWithComposerText);
    }

    #[test]
    fn enter_on_an_empty_approval_prompt_does_not_guess() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt) };
        assert_eq!(
            route(key(KeyCode::Enter), &ctx),
            KeyAction::None,
            "neither allow nor reject is a safe guess here"
        );
    }

    #[test]
    fn the_diff_shortcut_is_live_only_with_a_diff() {
        let without = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&without) };
        assert_eq!(route(key(KeyCode::Char('f')), &ctx), KeyAction::Insert('f'));

        let with = prompt(Some("+ line"));
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&with) };
        assert_eq!(
            route(key(KeyCode::Char('f')), &ctx),
            KeyAction::Approve(ApprovalReply::ShowDiff)
        );
    }

    #[test]
    fn an_approval_blocks_submission_and_mode_cycling() {
        let prompt = prompt(None);
        let ctx = KeyContext { working: true, composer_empty: true, approval: Some(&prompt) };
        // Changing mode or sending a prompt mid-approval would be answering a
        // different question than the one on screen.
        assert_eq!(route(key(KeyCode::BackTab), &ctx), KeyAction::None);
    }
}
