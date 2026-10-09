//! ScreenCast cursor mode. The ladder is [`pf_portal::cursor_mode`], shared with
//! `pf-capture`'s portal monitor; the Linux negotiation against
//! `AvailableCursorModes` is `pf_portal::negotiate_cursor_mode`.

pub use pf_portal::cursor_mode::Mode;
#[cfg(target_os = "linux")]
pub(crate) use pf_portal::negotiate_cursor_mode as negotiate;
