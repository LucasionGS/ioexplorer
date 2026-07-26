//! Keyboard model for the spotlight window.
//!
//! Kept as a pure function so the whole shortcut table can be tested without a
//! GTK main loop or a real key event.

use gtk::gdk::{Key, ModifierType};

/// How far Page Up / Page Down move the selection.
const PAGE_STEP: i32 = 8;

/// Which surface the window is showing. The key table differs between them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    /// The results list.
    #[default]
    Search,
    /// The chat transcript.
    Chat,
    /// The chat transcript with an approval card awaiting a decision.
    ///
    /// A third mode rather than a flag checked in `on_key`: routing that lives
    /// outside this function is routing the table's tests do not cover, and the
    /// two would drift.
    Approval,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Dismiss the window.
    Close,
    /// Move the selection by a signed number of rows.
    Move(i32),
    /// Activate the selected row. `secondary` requests the alternate action.
    Activate { secondary: bool },
    /// Accept the prefix hint or complete the selected path.
    Complete,
    /// Activate the row at this zero-based index.
    Pick(usize),
    /// Step a paginated `get_results` prefix forward or back. Ignored by every
    /// other kind of query.
    Page(i32),
    /// Chat: send the entry text as a follow-up.
    Send,
    /// Chat: leave the transcript for the results list, cancelling any stream.
    Back,
    /// Chat: stop generating but stay in the transcript.
    Cancel,
    /// Chat: copy the last assistant message to the clipboard.
    CopyLast,
    /// Approval: run the pending tool.
    Approve,
    /// Approval: decline it. The model is told, so it can adapt rather than
    /// waiting for a result that never comes.
    Deny,
    /// Not a spotlight shortcut — let the entry handle it.
    Pass,
}

pub fn resolve(key: Key, state: ModifierType, mode: Mode) -> Action {
    match mode {
        Mode::Search => resolve_search(key, state),
        Mode::Chat => resolve_chat(key, state),
        Mode::Approval => resolve_approval(key),
    }
}

/// While a tool is awaiting a decision, Enter and Escape belong to the card.
///
/// Everything else is passed through rather than swallowed, so the transcript
/// stays scrollable and the entry stays usable while the user reads what is
/// about to run. Ctrl+C still cancels — a pending approval is exactly when
/// someone might want out.
fn resolve_approval(key: Key) -> Action {
    match key {
        Key::Return | Key::KP_Enter => Action::Approve,
        Key::Escape => Action::Deny,
        _ => Action::Pass,
    }
}

/// Chat deliberately passes arrows, paging and Alt+digit through: the entry
/// owns Left/Right for the cursor and the scroller owns Page Up/Down, which is
/// what reading a long reply needs.
fn resolve_chat(key: Key, state: ModifierType) -> Action {
    let ctrl = state.contains(ModifierType::CONTROL_MASK);
    let shift = state.contains(ModifierType::SHIFT_MASK);

    match key {
        // One Escape leaves the transcript; the next closes the window.
        Key::Escape => Action::Back,
        Key::c | Key::C if ctrl => Action::Cancel,
        Key::y | Key::Y if ctrl => Action::CopyLast,
        // `gtk::Entry` is single-line and its buffer rejects `\n`, so
        // Shift+Enter cannot insert a newline — it sends, like plain Enter.
        Key::Return | Key::KP_Enter => {
            let _ = shift;
            Action::Send
        }
        _ => Action::Pass,
    }
}

fn resolve_search(key: Key, state: ModifierType) -> Action {
    let ctrl = state.contains(ModifierType::CONTROL_MASK);
    let alt = state.contains(ModifierType::ALT_MASK);
    let shift = state.contains(ModifierType::SHIFT_MASK);

    if alt && let Some(index) = digit_index(key) {
        return Action::Pick(index);
    }

    match key {
        Key::Escape => Action::Close,
        Key::Down => Action::Move(1),
        Key::Up => Action::Move(-1),
        // Two bindings for the same thing: Alt+arrows read as "forward" and
        // "back", and Ctrl+Page is what a list of pages usually answers to.
        // Both are checked before the plain arms below, which ignore modifiers.
        Key::Right if alt => Action::Page(1),
        Key::Left if alt => Action::Page(-1),
        Key::Page_Down if ctrl => Action::Page(1),
        Key::Page_Up if ctrl => Action::Page(-1),
        Key::Page_Down => Action::Move(PAGE_STEP),
        Key::Page_Up => Action::Move(-PAGE_STEP),
        Key::Home if ctrl => Action::Move(i32::MIN),
        Key::End if ctrl => Action::Move(i32::MAX),
        // Ctrl+N / Ctrl+P, matching both cases since some layouts deliver the
        // uppercase keyval while Ctrl is held.
        Key::n | Key::N if ctrl => Action::Move(1),
        Key::p | Key::P if ctrl => Action::Move(-1),
        Key::Return | Key::KP_Enter => Action::Activate {
            secondary: ctrl || shift,
        },
        // Always consume Tab so it completes rather than moving focus.
        Key::Tab | Key::ISO_Left_Tab => Action::Complete,
        _ => Action::Pass,
    }
}

/// Maps a digit key to a zero-based row index, covering layouts where the
/// number row reports a shifted keyval.
fn digit_index(key: Key) -> Option<usize> {
    let digit = match key {
        Key::_1 | Key::KP_1 => Some(1),
        Key::_2 | Key::KP_2 => Some(2),
        Key::_3 | Key::KP_3 => Some(3),
        Key::_4 | Key::KP_4 => Some(4),
        Key::_5 | Key::KP_5 => Some(5),
        Key::_6 | Key::KP_6 => Some(6),
        Key::_7 | Key::KP_7 => Some(7),
        Key::_8 | Key::KP_8 => Some(8),
        Key::_9 | Key::KP_9 => Some(9),
        _ => key
            .to_unicode()
            .and_then(|ch| ch.to_digit(10))
            .filter(|digit| *digit > 0)
            .map(|digit| digit as usize),
    }?;

    Some(digit - 1)
}

#[cfg(test)]
mod tests_approval {
    use super::*;

    fn approval(key: Key) -> Action {
        resolve(key, ModifierType::empty(), Mode::Approval)
    }

    /// The whole point of the third mode: Enter must not send a follow-up and
    /// Escape must not leave the transcript while a tool is waiting.
    #[test]
    fn enter_and_escape_reach_the_card_not_send_and_back() {
        assert_eq!(approval(Key::Return), Action::Approve);
        assert_eq!(approval(Key::KP_Enter), Action::Approve);
        assert_eq!(approval(Key::Escape), Action::Deny);

        // The same keys in plain chat mode still do the chat thing.
        assert_eq!(
            resolve(Key::Return, ModifierType::empty(), Mode::Chat),
            Action::Send
        );
        assert_eq!(
            resolve(Key::Escape, ModifierType::empty(), Mode::Chat),
            Action::Back
        );
    }

    /// Reading the command is the point of the card, so scrolling and typing
    /// must keep working while it is up.
    #[test]
    fn everything_else_passes_through_while_approving() {
        for key in [Key::Down, Key::Up, Key::Page_Down, Key::Tab, Key::a] {
            assert_eq!(approval(key), Action::Pass, "{key:?}");
        }
    }

    #[test]
    fn a_search_mode_key_never_approves() {
        for mode in [Mode::Search, Mode::Chat] {
            for key in [Key::Return, Key::Escape] {
                assert_ne!(resolve(key, ModifierType::empty(), mode), Action::Approve);
                assert_ne!(resolve(key, ModifierType::empty(), mode), Action::Deny);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifierType = ModifierType::empty();

    #[test]
    fn escape_closes() {
        assert_eq!(resolve(Key::Escape, NONE, Mode::Search), Action::Close);
    }

    #[test]
    fn arrows_move_the_selection() {
        assert_eq!(resolve(Key::Down, NONE, Mode::Search), Action::Move(1));
        assert_eq!(resolve(Key::Up, NONE, Mode::Search), Action::Move(-1));
    }

    #[test]
    fn emacs_bindings_move_the_selection() {
        assert_eq!(
            resolve(Key::n, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Move(1)
        );
        assert_eq!(
            resolve(Key::N, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Move(1)
        );
        assert_eq!(
            resolve(Key::p, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Move(-1)
        );
        assert_eq!(
            resolve(Key::P, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Move(-1)
        );
    }

    #[test]
    fn plain_letters_reach_the_entry() {
        assert_eq!(resolve(Key::n, NONE, Mode::Search), Action::Pass);
        assert_eq!(resolve(Key::p, NONE, Mode::Search), Action::Pass);
    }

    #[test]
    fn paging_and_extremes() {
        assert_eq!(
            resolve(Key::Page_Down, NONE, Mode::Search),
            Action::Move(PAGE_STEP)
        );
        assert_eq!(
            resolve(Key::Page_Up, NONE, Mode::Search),
            Action::Move(-PAGE_STEP)
        );
        assert_eq!(
            resolve(Key::Home, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Move(i32::MIN)
        );
        assert_eq!(
            resolve(Key::End, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Move(i32::MAX)
        );
        assert_eq!(resolve(Key::Home, NONE, Mode::Search), Action::Pass);
    }

    #[test]
    fn enter_activates_and_modifiers_request_the_secondary_action() {
        assert_eq!(
            resolve(Key::Return, NONE, Mode::Search),
            Action::Activate { secondary: false }
        );
        assert_eq!(
            resolve(Key::KP_Enter, NONE, Mode::Search),
            Action::Activate { secondary: false }
        );
        assert_eq!(
            resolve(Key::Return, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Activate { secondary: true }
        );
        assert_eq!(
            resolve(Key::Return, ModifierType::SHIFT_MASK, Mode::Search),
            Action::Activate { secondary: true }
        );
    }

    #[test]
    fn tab_always_completes() {
        assert_eq!(resolve(Key::Tab, NONE, Mode::Search), Action::Complete);
        assert_eq!(
            resolve(Key::ISO_Left_Tab, ModifierType::SHIFT_MASK, Mode::Search),
            Action::Complete
        );
    }

    #[test]
    fn alt_digits_pick_rows() {
        assert_eq!(
            resolve(Key::_1, ModifierType::ALT_MASK, Mode::Search),
            Action::Pick(0)
        );
        assert_eq!(
            resolve(Key::_3, ModifierType::ALT_MASK, Mode::Search),
            Action::Pick(2)
        );
        assert_eq!(
            resolve(Key::_9, ModifierType::ALT_MASK, Mode::Search),
            Action::Pick(8)
        );
    }

    #[test]
    fn alt_arrows_and_ctrl_paging_step_through_pages() {
        assert_eq!(
            resolve(Key::Right, ModifierType::ALT_MASK, Mode::Search),
            Action::Page(1)
        );
        assert_eq!(
            resolve(Key::Left, ModifierType::ALT_MASK, Mode::Search),
            Action::Page(-1)
        );
        assert_eq!(
            resolve(Key::Page_Down, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Page(1)
        );
        assert_eq!(
            resolve(Key::Page_Up, ModifierType::CONTROL_MASK, Mode::Search),
            Action::Page(-1)
        );
    }

    #[test]
    fn unmodified_arrows_and_paging_still_move_the_selection() {
        // The paging arms come first in the match and must not swallow these.
        assert_eq!(resolve(Key::Right, NONE, Mode::Search), Action::Pass);
        assert_eq!(resolve(Key::Left, NONE, Mode::Search), Action::Pass);
        assert_eq!(
            resolve(Key::Page_Down, NONE, Mode::Search),
            Action::Move(PAGE_STEP)
        );
    }

    #[test]
    fn chat_never_pages() {
        for key in [Key::Right, Key::Left, Key::Page_Down, Key::Page_Up] {
            assert_eq!(
                resolve(key, ModifierType::ALT_MASK, Mode::Chat),
                Action::Pass,
                "{key:?}"
            );
        }
    }

    #[test]
    fn chat_escape_steps_back_rather_than_closing() {
        assert_eq!(resolve(Key::Escape, NONE, Mode::Chat), Action::Back);
    }

    #[test]
    fn chat_enter_sends_a_follow_up() {
        assert_eq!(resolve(Key::Return, NONE, Mode::Chat), Action::Send);
        assert_eq!(resolve(Key::KP_Enter, NONE, Mode::Chat), Action::Send);
        // gtk::Entry cannot hold a newline, so Shift+Enter sends too.
        assert_eq!(
            resolve(Key::Return, ModifierType::SHIFT_MASK, Mode::Chat),
            Action::Send
        );
    }

    #[test]
    fn chat_ctrl_c_cancels_and_ctrl_y_copies() {
        assert_eq!(
            resolve(Key::c, ModifierType::CONTROL_MASK, Mode::Chat),
            Action::Cancel
        );
        assert_eq!(
            resolve(Key::C, ModifierType::CONTROL_MASK, Mode::Chat),
            Action::Cancel
        );
        assert_eq!(
            resolve(Key::y, ModifierType::CONTROL_MASK, Mode::Chat),
            Action::CopyLast
        );
    }

    #[test]
    fn chat_passes_navigation_through_for_reading() {
        for key in [Key::Up, Key::Down, Key::Page_Up, Key::Page_Down, Key::Tab] {
            assert_eq!(resolve(key, NONE, Mode::Chat), Action::Pass, "{key:?}");
        }
        assert_eq!(
            resolve(Key::n, ModifierType::CONTROL_MASK, Mode::Chat),
            Action::Pass
        );
        assert_eq!(
            resolve(Key::_1, ModifierType::ALT_MASK, Mode::Chat),
            Action::Pass
        );
    }

    #[test]
    fn chat_plain_letters_still_reach_the_entry() {
        assert_eq!(resolve(Key::c, NONE, Mode::Chat), Action::Pass);
        assert_eq!(resolve(Key::y, NONE, Mode::Chat), Action::Pass);
    }

    #[test]
    fn chat_never_resolves_to_close() {
        // Back must step out of the transcript; reaching Close would quit the
        // app outright in one-shot mode.
        for key in [Key::Escape, Key::Return, Key::Tab, Key::Up] {
            assert_ne!(resolve(key, NONE, Mode::Chat), Action::Close, "{key:?}");
        }
    }

    #[test]
    fn digits_without_alt_reach_the_entry() {
        assert_eq!(resolve(Key::_1, NONE, Mode::Search), Action::Pass);
    }

    #[test]
    fn alt_zero_is_not_a_row_pick() {
        assert_eq!(
            resolve(Key::_0, ModifierType::ALT_MASK, Mode::Search),
            Action::Pass
        );
    }
}
