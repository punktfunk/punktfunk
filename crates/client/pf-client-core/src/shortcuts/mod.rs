//! In-stream shortcuts: a press or hold of controller buttons or keys, bound to a quick
//! action. Design: `design/client-shortcuts.md` (planning repo).
//!
//! - `model.rs`: [`Trigger`], [`Action`], [`Binding`] and the `shortcuts` settings blob
//!   ([`Shortcuts::parse`] never fails), plus the editor's [`Shortcuts::validate`].
//! - `defaults.rs`: each [`Client`]'s rows when the blob is empty ([`resolve`]).
//! - `engine.rs`: the pure state machine that holds back, forwards and fires ([`Engine`]).
//! - `rows.rs`: what the Shortcuts screen and the in-stream hints draw ([`Shortcuts::rows`]).
//!
//! Swift and Kotlin twins replay `clients/shared/shortcut-vectors.json`, as `tests.rs`
//! does; a rule changes there first.

mod defaults;
mod engine;
mod model;
mod rows;
#[cfg(test)]
mod tests;

pub use defaults::{defaults, resolve};
pub use engine::{Engine, Event, Out, Phase, HOLD, KEY_TAP, TAP_PRESS};
pub use model::{
    button_bit, button_name, Action, ActionKind, Binding, Conflict, Device, Shortcuts, Trigger,
    BUTTONS, MODIFIERS,
};
pub use rows::{groups, Gesture, Group, Item, Kind, Press};

/// A client family with its own bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// The Vulkan session binary: Linux, Windows and Steam Deck.
    Desktop,
    Android,
    /// An iPhone, iPad or Apple TV. Its keys are a hardware keyboard's, so the caller lists
    /// them only while one is attached. The Apple About screen lists the remote's rows.
    Apple,
    /// A Mac: the iPad's chords, plus mouse capture, the mouse mode and fullscreen.
    Mac,
    /// The LG TV client: Magic Remote colour keys, and its own dial and escape chord.
    WebOS,
    /// The browser page, with a keyboard, a mouse and a pad.
    Web,
    /// The Samsung TV client: the browser page, with the remote's Back as its menu key.
    Tizen,
    /// The PS5 title: a DualSense, its touchpad click standing in for Create.
    Ps5,
    /// The Switch title: Nintendo's A sits east, so its wire bit is `b`; Minus is Select, Plus
    /// is Start, L/R are the shoulders.
    Switch,
}
