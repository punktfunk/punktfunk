//! GDK button codes → GameStream button ids on the input wire, plus the evdev → VK
//! table, which lives in `punktfunk_core::input` so every client replays one file.

pub use punktfunk_core::input::evdev_to_vk;

/// GDK back/forward are 8/9; GameStream wants X1/X2 as 4/5. Other buttons are 1:1.
pub fn gdk_button_to_gs(button: u32) -> Option<u32> {
    Some(match button {
        1 => 1,
        2 => 2,
        3 => 3,
        8 => 4,
        9 => 5,
        _ => return None,
    })
}
