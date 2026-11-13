//! The desktop's own accessibility switches, where it states them. The console follows an
//! answer and keeps its own row for a desktop that gives none.

/// A reader of the OS's reduce-motion switch: `Some(true)` reduce, `Some(false)` animate,
/// `None` when this desktop does not say. A poller calls it repeatedly; on Linux it keeps one
/// D-Bus session connection between calls and reconnects after an answer of `None`.
pub fn reduce_motion_reader() -> impl FnMut() -> Option<bool> + Send {
    imp::reader()
}

#[cfg(target_os = "linux")]
mod imp {
    use zbus::zvariant::OwnedValue;

    pub(super) fn reader() -> impl FnMut() -> Option<bool> + Send {
        let mut conn: Option<zbus::blocking::Connection> = None;
        move || {
            if conn.is_none() {
                conn = zbus::blocking::Connection::session().ok();
            }
            let answer = reduce_motion(conn.as_ref()?);
            if answer.is_none() {
                conn = None;
            }
            answer
        }
    }

    /// The XDG Settings portal: the standard `reduced-motion` (1 = reduce), else the GNOME and
    /// KDE keys their portal backends relay.
    fn reduce_motion(conn: &zbus::blocking::Connection) -> Option<bool> {
        let read = |namespace: &str, key: &str| -> Option<OwnedValue> {
            let reply = conn
                .call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    "/org/freedesktop/portal/desktop",
                    Some("org.freedesktop.portal.Settings"),
                    "ReadOne",
                    &(namespace, key),
                )
                .ok()?;
            reply.body().deserialize::<OwnedValue>().ok()
        };
        if let Some(v) = read("org.freedesktop.appearance", "reduced-motion") {
            if let Ok(n) = u32::try_from(v) {
                return Some(n == 1);
            }
        }
        if let Some(v) = read("org.gnome.desktop.interface", "enable-animations") {
            if let Ok(on) = bool::try_from(v) {
                return Some(!on);
            }
        }
        // kdeglobals carries it as a duration factor, 0 = off; the portal relays it as text.
        let v = read("org.kde.kdeglobals.KDE", "AnimationDurationFactor")?;
        let factor = f64::try_from(&v)
            .ok()
            .or_else(|| String::try_from(v).ok()?.trim().parse().ok())?;
        Some(factor == 0.0)
    }
}

#[cfg(windows)]
mod imp {
    use windows::Win32::winuser::{SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION};

    pub(super) fn reader() -> impl FnMut() -> Option<bool> + Send {
        reduce_motion
    }

    /// Settings → Accessibility → Visual effects → Animation effects.
    fn reduce_motion() -> Option<bool> {
        let mut on: i32 = 1;
        // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL (4 bytes) through `pvparam`, and
        // `on` is a live i32 local; no update flag, so nothing is written back to the profile.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION as u32,
                0,
                std::ptr::from_mut(&mut on).cast(),
                0,
            )
        };
        ok.as_bool().then_some(on == 0)
    }
}
