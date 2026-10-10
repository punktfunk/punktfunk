//! Immediate-mode focus widgets: the settings menu list, the section tab strip,
//! and the controller keyboard.
//!
//! The widget owns cursor, springs, and scroll. The screen owns row content and
//! what an activation means; every frame it hands the widget a fresh `RowSpec`
//! slice. Ports of Apple's `GamepadMenuList` / `GamepadKeyboard`.

mod keyboard;
mod list;
mod tabs;

pub use keyboard::*;
pub use list::*;
pub use tabs::*;
