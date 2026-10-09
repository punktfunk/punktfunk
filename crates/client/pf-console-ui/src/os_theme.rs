//! The desktop's own theme, pushed by the embedding binary ("Follow system theme").
//!
//! The console never reads a file. The session binary (or a future Android feed)
//! publishes three sRGB colours here; a process-wide slot plus a revision lets the
//! watcher live on a worker thread and the shell compare once per frame.
//! Colours are [`Rgb`], not Skia types, so the publisher needs no Skia.

pub use pf_client_core::rgb::Rgb;
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OsTheme {
    pub light: bool,
    pub background: Rgb,
    pub foreground: Rgb,
    pub accent: Rgb,
}

static CURRENT: Mutex<(u64, Option<OsTheme>)> = Mutex::new((0, None));

/// The revision moves only on a real change so a 2 s poll is free.
pub fn set_os_theme(t: Option<OsTheme>) {
    let mut cur = CURRENT.lock().unwrap();
    if cur.1 != t {
        cur.0 += 1;
        cur.1 = t;
    }
}

/// Revision for the shell's per-frame rebuild check.
pub(crate) fn os_theme() -> (u64, Option<OsTheme>) {
    *CURRENT.lock().unwrap()
}

/// The settings row keys off this, not the platform.
pub(crate) fn available() -> bool {
    CURRENT.lock().unwrap().1.is_some()
}

static REDUCE_MOTION: Mutex<Option<bool>> = Mutex::new(None);

/// The OS's own reduce-motion switch, when the embedder can read it. An answer wins over
/// the stored setting and hides its row; `None` leaves the row in charge.
pub fn set_os_reduce_motion(reduce: Option<bool>) {
    *REDUCE_MOTION.lock().unwrap() = reduce;
}

pub(crate) fn os_reduce_motion() -> Option<bool> {
    *REDUCE_MOTION.lock().unwrap()
}

/// Held by every test that sets [`set_os_reduce_motion`] or reads the effective value: libtest
/// runs siblings on threads, and the slot is process-wide.
#[cfg(test)]
pub(crate) static REDUCE_MOTION_TEST: Mutex<()> = Mutex::new(());

/// The accent lifted toward the foreground until it reads on the background
/// ([`pf_client_core::rgb::readable`]).
pub(crate) fn readable_accent(t: &OsTheme) -> Rgb {
    pf_client_core::rgb::readable(t.accent, t.background, t.foreground)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::rgb::contrast;

    // Do not touch CURRENT here. libtest is parallel; the one allowed setter is
    // `screens::settings::tests`, where the row that reads it is reachable too.

    #[test]
    fn a_washed_out_accent_is_lifted_and_a_sound_one_is_left_alone() {
        let washed = OsTheme {
            light: true,
            background: Rgb(0.992, 0.965, 0.890),
            foreground: Rgb(0.361, 0.416, 0.447),
            accent: Rgb(0.874, 0.627, 0.0),
        };
        assert!(
            contrast(washed.accent, washed.background) < 3.0,
            "the premise"
        );
        assert!(contrast(readable_accent(&washed), washed.background) >= 3.0);

        let sound = OsTheme {
            light: false,
            background: Rgb(0.102, 0.106, 0.149),
            foreground: Rgb(0.753, 0.792, 0.961),
            accent: Rgb(0.478, 0.635, 0.969),
        };
        assert_eq!(readable_accent(&sound), sound.accent);
    }
}
