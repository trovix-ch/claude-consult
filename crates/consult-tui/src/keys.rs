//! Keys, as every widget here takes them.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// One key press, reduced to what the widgets act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// Up arrow.
    Up,
    /// Down arrow.
    Down,
    /// Left arrow.
    Left,
    /// Right arrow.
    Right,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Home.
    Home,
    /// End.
    End,
    /// The space bar. In the picker it ticks; in a text input it types a space.
    Space,
    /// Enter.
    Enter,
    /// Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Tab.
    Tab,
    /// Shift+Tab.
    BackTab,
    /// Ctrl+R: the recommended panel.
    CtrlR,
    /// Ctrl+B: the budget panel.
    CtrlB,
    /// Ctrl+C: quit. Read as a key in raw mode, so it acts at once.
    CtrlC,
    /// Any other Ctrl+letter (without Alt), lower case.
    Ctrl(char),
    /// A typed character; never a control character.
    Char(char),
}

/// The [`Key`] a crossterm key event stands for, or `None` for one to ignore.
///
/// Only presses (and repeats) count: Windows reports a release for every key as well.
/// Ctrl without Alt is a shortcut. AltGr arrives as Ctrl+Alt, and on many layouts
/// AltGr+C, +B or +R types a character (`&`, `{`, `ć`, `®`), so Ctrl+Alt with a
/// character is typing, or typing one would quit the install.
///
/// Ctrl+Alt on a key that produces no character is not typing either. crossterm then
/// reports the key's plain layout character (it asks the layout for it when the console
/// gives none), so Ctrl+Alt+C arrives exactly like a typed `c` with both modifiers. No
/// layout's AltGr produces the key's own unshifted letter or digit, so Ctrl+Alt with an
/// ASCII letter or digit is dropped, as the PowerShell picker dropped a key with no
/// character.
pub fn map_key(event: KeyEvent) -> Option<Key> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
    let alt = event.modifiers.contains(KeyModifiers::ALT);
    if ctrl && !alt {
        // Ctrl+<named key> was never a shortcut here; the PowerShell picker ignored it.
        let KeyCode::Char(c) = event.code else {
            return None;
        };
        return Some(match c.to_ascii_lowercase() {
            'c' => Key::CtrlC,
            'r' => Key::CtrlR,
            'b' => Key::CtrlB,
            other => Key::Ctrl(other),
        });
    }
    Some(match event.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Char(' ') => Key::Space,
        KeyCode::Char(c) if c.is_control() => return None,
        KeyCode::Char(c) if ctrl && alt && c.is_ascii_alphanumeric() => return None,
        KeyCode::Char(c) => Key::Char(c),
        _ => return None,
    })
}

/// Blocks for the next terminal event and maps it. `Ok(None)` for anything that is not
/// a key to act on, including a resize, after which the caller simply redraws.
///
/// Needs raw mode (see [`crate::Terminal`]), or keys arrive only after Enter.
pub fn read_key() -> io::Result<Option<Key>> {
    match event::read()? {
        Event::Key(k) => Ok(map_key(k)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventState;

    fn ev(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> Option<Key> {
        map_key(ev(code, modifiers, KeyEventKind::Press))
    }

    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const ALTGR: KeyModifiers = KeyModifiers::CONTROL.union(KeyModifiers::ALT);

    // Ported from test_picker.py's KEYS: what the picker must make of a console key.
    #[test]
    fn altgr_characters_type_and_only_ctrl_without_alt_is_a_shortcut() {
        assert_eq!(press(KeyCode::Char('c'), CTRL), Some(Key::CtrlC));
        assert_eq!(press(KeyCode::Char('b'), CTRL), Some(Key::CtrlB));
        assert_eq!(press(KeyCode::Char('r'), CTRL), Some(Key::CtrlR));
        // Hungarian AltGr+C and AltGr+B, Polish AltGr+C, US-International AltGr+R.
        assert_eq!(press(KeyCode::Char('&'), ALTGR), Some(Key::Char('&')));
        assert_eq!(press(KeyCode::Char('{'), ALTGR), Some(Key::Char('{')));
        assert_eq!(press(KeyCode::Char('ć'), ALTGR), Some(Key::Char('ć')));
        assert_eq!(press(KeyCode::Char('®'), ALTGR), Some(Key::Char('®')));
        // Ctrl+Alt+C that types nothing.
        assert_eq!(press(KeyCode::Char('c'), ALTGR), None);
        assert_eq!(press(KeyCode::Up, KeyModifiers::NONE), Some(Key::Up));
        assert_eq!(
            press(KeyCode::Char('a'), KeyModifiers::NONE),
            Some(Key::Char('a'))
        );
        assert_eq!(
            press(KeyCode::Char('A'), KeyModifiers::SHIFT),
            Some(Key::Char('A'))
        );
    }

    #[test]
    fn releases_and_control_characters_are_dropped() {
        assert_eq!(
            map_key(ev(KeyCode::Char('c'), CTRL, KeyEventKind::Release)),
            None
        );
        assert_eq!(
            map_key(ev(KeyCode::Down, KeyModifiers::NONE, KeyEventKind::Repeat)),
            Some(Key::Down)
        );
        assert_eq!(press(KeyCode::Char('\u{3}'), KeyModifiers::NONE), None);
        assert_eq!(
            press(KeyCode::Char(' '), KeyModifiers::NONE),
            Some(Key::Space)
        );
        assert_eq!(press(KeyCode::Char('R'), CTRL), Some(Key::CtrlR));
        assert_eq!(press(KeyCode::Char('x'), CTRL), Some(Key::Ctrl('x')));
        assert_eq!(press(KeyCode::Up, CTRL), None);
        assert_eq!(press(KeyCode::F(1), KeyModifiers::NONE), None);
    }
}
