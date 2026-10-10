//! Compositor-independent panel darkening over DRM — how a box with no desktop compositor
//! honors [`Topology::Exclusive`](crate::policy::Topology::Exclusive).
//!
//! [`crate::panel_dpms`] is the dispatcher; this is the "none at all" arm. Open `/dev/dri/cardN`
//! (logind uaccess, session user), `SET_MASTER` (decline if another client is master), modeset
//! each lit CRTC off (`SETCRTC` `fb_id=0, mode_valid=0, count_connectors=0`), `DROP_MASTER`,
//! keep the fd. Darkness survives dropping master; the open fd is the hold. Relight is
//! `close(fd)`: kernel last-close restores the console. No saved mode, no journal — a dead
//! host lights the box. Best-effort: missing `/dev/dri`, a foreign master, or nothing lit
//! contributes nothing and the stream proceeds.
//!
//! Ioctl numbers encode payload size; layouts are pinned by the `const` asserts below.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

// Hand-declared from `drm.h` / `drm_mode.h`. `_IO('d', nr)` / `_IOWR('d', nr, T)` — payload
// sizes live in the ioctl numbers; the `const` asserts below pin the structs to those sizes.

/// `_IO('d', 0x1e)`.
const DRM_IOCTL_SET_MASTER: libc::c_ulong = 0x641e;
/// `_IO('d', 0x1f)`.
const DRM_IOCTL_DROP_MASTER: libc::c_ulong = 0x641f;
/// `_IOWR('d', 0xA0, drm_mode_card_res)` — 0x40-byte payload.
const DRM_IOCTL_MODE_GETRESOURCES: libc::c_ulong = 0xC040_64A0;
/// `_IOWR('d', 0xA1, drm_mode_crtc)` — 0x68-byte payload.
const DRM_IOCTL_MODE_GETCRTC: libc::c_ulong = 0xC068_64A1;
/// `_IOWR('d', 0xA2, drm_mode_crtc)` — 0x68-byte payload.
const DRM_IOCTL_MODE_SETCRTC: libc::c_ulong = 0xC068_64A2;

#[repr(C)]
#[derive(Default)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmModeModeinfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DrmModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: DrmModeModeinfo,
}

// Ioctl numbers encode payload size (0x40 = 64, 0x68 = 104). A struct that disagrees is a
// kernel read/write of the wrong length.
const _: () = assert!(std::mem::size_of::<DrmModeCardRes>() == 0x40);
const _: () = assert!(std::mem::size_of::<DrmModeModeinfo>() == 68);
const _: () = assert!(std::mem::size_of::<DrmModeCrtc>() == 0x68);

impl Default for DrmModeCrtc {
    fn default() -> Self {
        // SAFETY: both structs are `repr(C)` plain old data — integers and a `[u8; 32]`, no
        // padding invariants, no pointers that must be valid, and no `Drop`. An all-zero value is
        // a legal instance, and is exactly what the ioctls want for "no connectors, no mode".
        unsafe { std::mem::zeroed() }
    }
}

/// Open fds holding the panels dark. Drop closes them; kernel last-close re-lights.
pub struct DrmDarken {
    /// Kept solely for its `Drop`. The panel stays dark exactly as long as these are open.
    _cards: Vec<File>,
    /// Which `/dev/dri/cardN` we actually turned something off on — logging only.
    pub darkened: Vec<String>,
}

/// Shared `ioctl` so there is one `unsafe` to justify, not one per call site.
fn ioctl<T>(fd: libc::c_int, req: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
    // SAFETY: `fd` is an open DRM node owned by the caller for the whole call; `req` is one of the
    // five `_IO`/`_IOWR` codes declared above, each paired with the `T` its size field names (the
    // const asserts pin that); and `arg` is a live, uniquely-borrowed, `repr(C)` value of that
    // exact type, so the kernel's read/write of `size_of::<T>()` bytes stays inside it.
    let rc = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Turn every lit CRTC on every DRM card off, and hold them off. `None` when nothing was darkened
/// — no cards, none masterable, or none lit — and therefore nothing to restore.
pub fn darken() -> Option<DrmDarken> {
    let mut cards = Vec::new();
    let mut darkened = Vec::new();
    for entry in std::fs::read_dir("/dev/dri").ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // `cardN` only: `renderD*` is the render node (no modesetting at all) and `by-path/` is a
        // directory of symlinks to the same nodes.
        if !name.starts_with("card") {
            continue;
        }
        match darken_card(&path) {
            // Masterable, but nothing on this card was lit. Its fd is dropped here, which is
            // correct: we changed nothing, so there is nothing to hold.
            Ok((_, 0)) => {}
            Ok((card, n)) => {
                tracing::debug!(card = name, crtcs = n, "DRM: CRTCs off");
                darkened.push(name.to_string());
                // This exact fd. Close-then-reopen is last-close: the console restore
                // re-lights, and the fresh fd holds nothing.
                cards.push(card);
            }
            Err(why) => tracing::debug!(card = name, %why, "DRM: not ours to darken"),
        }
    }
    if darkened.is_empty() {
        return None;
    }
    Some(DrmDarken {
        _cards: cards,
        darkened,
    })
}

/// Darken one card. Return the open fd (the hold) and how many CRTCs went off.
/// Closing that fd is last-close and restores the console; count 0 may drop it.
fn darken_card(path: &Path) -> std::io::Result<(File, usize)> {
    let card = File::options().read(true).write(true).open(path)?;
    let fd = card.as_raw_fd();
    // Foreign master (live compositor, other gamescope) ⇒ not ours. Also the Attach
    // guard: do not darken the picture being streamed.
    ioctl(fd, DRM_IOCTL_SET_MASTER, &mut 0u64)?;

    // Count pass: every pointer NULL, the kernel fills in the counts.
    let mut res = DrmModeCardRes::default();
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res)?;
    let n = res.count_crtcs as usize;
    if n == 0 {
        let _ = ioctl(fd, DRM_IOCTL_DROP_MASTER, &mut 0u64);
        return Ok((card, 0));
    }
    // Data pass: hand back a buffer sized by that count and ask again.
    let mut ids = vec![0u32; n];
    let mut res = DrmModeCardRes {
        crtc_id_ptr: ids.as_mut_ptr() as u64,
        count_crtcs: n as u32,
        ..Default::default()
    };
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res)?;
    // The kernel may report FEWER than the count pass promised (a hotplug between the two); it
    // never reports more than the buffer we sized, so trust the second count.
    ids.truncate(res.count_crtcs as usize);

    let mut off = 0usize;
    for id in ids {
        let mut crtc = DrmModeCrtc {
            crtc_id: id,
            ..Default::default()
        };
        if ioctl(fd, DRM_IOCTL_MODE_GETCRTC, &mut crtc).is_err() {
            continue;
        }
        // Skip an already-dark CRTC: disabling it is a no-op, but counting it would log a
        // panel that was never on.
        if crtc.mode_valid == 0 && crtc.fb_id == 0 {
            continue;
        }
        let mut disable = DrmModeCrtc {
            crtc_id: id,
            ..Default::default()
        };
        if ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut disable).is_ok() {
            off += 1;
        }
    }
    // Darkness does not need master. Hand it back so the box's gamescope can take the card
    // on restore; keeping it would leave a session that cannot start.
    let _ = ioctl(fd, DRM_IOCTL_DROP_MASTER, &mut 0u64);
    Ok((card, off))
}

#[cfg(test)]
mod tests {
    use super::{DrmModeCardRes, DrmModeCrtc, DrmModeModeinfo};

    /// Payload sizes the ioctl numbers encode, plus the two offsets the count/data-pass uses.
    /// The `const` asserts fail the build; this restates them so a failure is greppable.
    #[test]
    fn the_abi_structs_match_the_ioctl_payload_sizes() {
        assert_eq!(std::mem::size_of::<DrmModeCardRes>(), 0x40, "_IOWR 0x40");
        assert_eq!(std::mem::size_of::<DrmModeModeinfo>(), 68);
        assert_eq!(std::mem::size_of::<DrmModeCrtc>(), 0x68, "_IOWR 0x68");
        // `crtc_id_ptr` is the second u64 — the field the data pass points at its id buffer. A
        // reorder here would hand the kernel the framebuffer-id pointer instead.
        assert_eq!(std::mem::offset_of!(DrmModeCardRes, crtc_id_ptr), 8);
        assert_eq!(std::mem::offset_of!(DrmModeCardRes, count_crtcs), 36);
        // `mode` must sit right after the seven u32s, or SETCRTC reads a mode we never wrote.
        assert_eq!(std::mem::offset_of!(DrmModeCrtc, mode), 36);
    }

    /// Live: darken this box's panels and read sysfs. Skips when a compositor already
    /// masters the card. The same heads must reach Off, then restore exactly.
    #[test]
    #[ignore = "on glass: needs a connected head and no compositor holding /dev/dri/card*"]
    fn live_the_panels_go_dark_and_come_back() {
        fn connectors() -> Vec<(String, String, String)> {
            let mut v = Vec::new();
            let Ok(rd) = std::fs::read_dir("/sys/class/drm") else {
                return v;
            };
            for e in rd.flatten() {
                let p = e.path();
                let rd = |f: &str| {
                    std::fs::read_to_string(p.join(f))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default()
                };
                if rd("status") == "connected" {
                    v.push((
                        e.file_name().to_string_lossy().into_owned(),
                        rd("enabled"),
                        rd("dpms"),
                    ));
                }
            }
            v.sort();
            v
        }

        fn wait_until(
            pred: impl Fn(&[(String, String, String)]) -> bool,
        ) -> Vec<(String, String, String)> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let now = connectors();
                if pred(&now) || std::time::Instant::now() >= deadline {
                    return now;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        let before = connectors();
        assert!(
            !before.is_empty(),
            "no connected head — this test needs one to mean anything"
        );

        let Some(hold) = super::darken() else {
            return;
        };
        let during = wait_until(|now| {
            now.len() == before.len()
                && now
                    .iter()
                    .zip(&before)
                    .all(|((name, _, dpms), (before_name, _, _))| {
                        name == before_name && dpms == "Off"
                    })
        });

        drop(hold);
        let after = wait_until(|now| now == before);

        assert_eq!(
            during.len(),
            before.len(),
            "connected heads changed while the panels were darkened"
        );
        for ((name, en, dpms), (before_name, _, _)) in during.iter().zip(&before) {
            assert_eq!(name, before_name, "connected heads changed while darkened");
            assert_eq!(dpms, "Off", "{name} should be DPMS-off while held ({en})");
        }
        assert_eq!(
            after, before,
            "dropping the hold must restore exactly the state we found"
        );
    }

    /// A zeroed `DrmModeCrtc` is the disable request — `Default`'s only job here.
    #[test]
    fn the_default_crtc_is_the_disable_request() {
        let c = DrmModeCrtc::default();
        assert_eq!(c.fb_id, 0, "a framebuffer would keep the CRTC lit");
        assert_eq!(
            c.mode_valid, 0,
            "a valid mode would re-modeset, not disable"
        );
        assert_eq!(c.count_connectors, 0);
        assert_eq!(c.set_connectors_ptr, 0);
    }
}
