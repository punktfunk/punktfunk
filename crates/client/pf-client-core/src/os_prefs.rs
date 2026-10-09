//! The desktop's own accessibility switches, where it states them. The console follows an
//! answer and keeps its own row for a desktop that gives none.

/// The OS asked for less motion: `Some(true)` reduce, `Some(false)` animate, `None` when this
/// desktop does not say.
pub fn reduce_motion() -> Option<bool> {
    imp::reduce_motion()
}

#[cfg(target_os = "linux")]
mod imp {
    use zbus::zvariant::OwnedValue;

    /// The XDG Settings portal: the standard `reduced-motion` (1 = reduce), else the GNOME and
    /// KDE keys their portal backends relay.
    pub(super) fn reduce_motion() -> Option<bool> {
        let conn = zbus::blocking::Connection::session().ok()?;
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

    /// Settings → Accessibility → Visual effects → Animation effects.
    pub(super) fn reduce_motion() -> Option<bool> {
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
