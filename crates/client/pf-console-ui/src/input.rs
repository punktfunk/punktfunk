//! Toolkit-free keys the shell acts on. Hosts map their codes onto this
//! (SDL scancodes, Android `KeyEvent`); tests construct it directly.
//! Typed characters never travel here — that is
//! [`crate::shell::Shell::text_input`].

/// Hosts need not forward a key that is not in this set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Left,
    Right,
    Up,
    Down,
    /// Enter and the keypad Enter — one key to the console.
    Return,
    Space,
    Escape,
    Backspace,
    PageUp,
    PageDown,
    Tab,
    /// Keyboard stand-in for the pad's Secondary (Y) while not editing.
    Y,
    /// Keyboard stand-in for the pad's Tertiary (X) while not editing.
    X,
}
