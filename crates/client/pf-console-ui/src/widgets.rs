//! Immediate-mode focus widgets: the settings menu list, the section tab strip,
//! the controller keyboard, and the scroll of a read-only page.
//!
//! The widget owns cursor, springs, and scroll. The screen owns row content and
//! what an activation means; every frame it hands the widget a fresh `RowSpec`
//! slice.

mod flick;
mod keyboard;
mod list;
mod tabs;

pub use flick::*;
pub use keyboard::*;
pub use list::*;
pub use tabs::*;
