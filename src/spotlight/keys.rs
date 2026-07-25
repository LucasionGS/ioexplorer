//! Keyboard model for the spotlight window.
//!
//! Kept as a pure function so the whole shortcut table can be tested without a
//! GTK main loop or a real key event.

use gtk::gdk::{Key, ModifierType};

/// How far Page Up / Page Down move the selection.
const PAGE_STEP: i32 = 8;

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
    /// Not a spotlight shortcut — let the entry handle it.
    Pass,
}

pub fn resolve(key: Key, state: ModifierType) -> Action {
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
mod tests {
    use super::*;

    const NONE: ModifierType = ModifierType::empty();

    #[test]
    fn escape_closes() {
        assert_eq!(resolve(Key::Escape, NONE), Action::Close);
    }

    #[test]
    fn arrows_move_the_selection() {
        assert_eq!(resolve(Key::Down, NONE), Action::Move(1));
        assert_eq!(resolve(Key::Up, NONE), Action::Move(-1));
    }

    #[test]
    fn emacs_bindings_move_the_selection() {
        assert_eq!(resolve(Key::n, ModifierType::CONTROL_MASK), Action::Move(1));
        assert_eq!(resolve(Key::N, ModifierType::CONTROL_MASK), Action::Move(1));
        assert_eq!(
            resolve(Key::p, ModifierType::CONTROL_MASK),
            Action::Move(-1)
        );
        assert_eq!(
            resolve(Key::P, ModifierType::CONTROL_MASK),
            Action::Move(-1)
        );
    }

    #[test]
    fn plain_letters_reach_the_entry() {
        assert_eq!(resolve(Key::n, NONE), Action::Pass);
        assert_eq!(resolve(Key::p, NONE), Action::Pass);
    }

    #[test]
    fn paging_and_extremes() {
        assert_eq!(resolve(Key::Page_Down, NONE), Action::Move(PAGE_STEP));
        assert_eq!(resolve(Key::Page_Up, NONE), Action::Move(-PAGE_STEP));
        assert_eq!(
            resolve(Key::Home, ModifierType::CONTROL_MASK),
            Action::Move(i32::MIN)
        );
        assert_eq!(
            resolve(Key::End, ModifierType::CONTROL_MASK),
            Action::Move(i32::MAX)
        );
        assert_eq!(resolve(Key::Home, NONE), Action::Pass);
    }

    #[test]
    fn enter_activates_and_modifiers_request_the_secondary_action() {
        assert_eq!(
            resolve(Key::Return, NONE),
            Action::Activate { secondary: false }
        );
        assert_eq!(
            resolve(Key::KP_Enter, NONE),
            Action::Activate { secondary: false }
        );
        assert_eq!(
            resolve(Key::Return, ModifierType::CONTROL_MASK),
            Action::Activate { secondary: true }
        );
        assert_eq!(
            resolve(Key::Return, ModifierType::SHIFT_MASK),
            Action::Activate { secondary: true }
        );
    }

    #[test]
    fn tab_always_completes() {
        assert_eq!(resolve(Key::Tab, NONE), Action::Complete);
        assert_eq!(
            resolve(Key::ISO_Left_Tab, ModifierType::SHIFT_MASK),
            Action::Complete
        );
    }

    #[test]
    fn alt_digits_pick_rows() {
        assert_eq!(resolve(Key::_1, ModifierType::ALT_MASK), Action::Pick(0));
        assert_eq!(resolve(Key::_3, ModifierType::ALT_MASK), Action::Pick(2));
        assert_eq!(resolve(Key::_9, ModifierType::ALT_MASK), Action::Pick(8));
    }

    #[test]
    fn digits_without_alt_reach_the_entry() {
        assert_eq!(resolve(Key::_1, NONE), Action::Pass);
    }

    #[test]
    fn alt_zero_is_not_a_row_pick() {
        assert_eq!(resolve(Key::_0, ModifierType::ALT_MASK), Action::Pass);
    }
}
