//! Compositor-independent panel darkening over DRM — how a box with no desktop compositor
//! honors [`Topology::Exclusive`](crate::policy::Topology::Exclusive).
//!
//! [`crate::panel_dpms`] is the dispatcher; this is the "none at all" arm. Open `/dev/dri/cardN`
//! (logind uaccess, session user), `SET_MASTER` (decline if another client is master), modeset
//! each lit CRTC off (`SETCRTC` `fb_id=0, mode_valid=0, count_connectors=0`), `DROP_MASTER`,
//! keep the fd. Darkness survives dropping master; the open fd is the hold. Relight is
//! `close(fd)`: kernel last-close restores the console, so no saved mode and no journal — but
//! any other fd on the GPU, a render node included, defers that close, and [`release`] reads
//! the heads back from sysfs to name the ones still dark. Best-effort: missing `/dev/dri`, a
//! foreign master, or nothing lit contributes nothing and the stream proceeds.
//!
//! [`lit_mode`] reads a lit CRTC's mode over the same ioctls; that needs no master.
//!
//! Ioctl numbers encode payload size; layouts are pinned by the `const` asserts below.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

/// Where the kernel lists each connector as `cardN-<connector>`, with its `enabled` state.
const SYSFS_DRM: &str = "/sys/class/drm";
/// How long [`release`] waits for last close to re-light the heads it darkened.
const RELIGHT_BUDGET: Duration = Duration::from_secs(2);

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

/// Open fds holding the panels dark. [`release`] closes them; kernel last-close re-lights.
pub struct DrmDarken {
    /// The panel stays dark exactly as long as these are open.
    cards: Vec<File>,
    /// Which `/dev/dri/cardN` we actually turned something off on — logging only.
    pub darkened: Vec<String>,
    /// The connectors lit on those cards before the darken, by sysfs name (`card1-DP-1`).
    heads: Vec<String>,
}

/// Close the hold, then read its heads back for up to [`RELIGHT_BUDGET`]: last close re-lights
/// only once nothing else holds the GPU open. Logs the heads still dark; an unplugged one is not.
pub fn release(d: DrmDarken) {
    drop(d.cards);
    let dark = still_dark(Path::new(SYSFS_DRM), &d.heads, RELIGHT_BUDGET);
    if dark.is_empty() {
        tracing::info!(cards = ?d.darkened, "DRM: the box's own CRTCs released — panel back on");
    } else {
        tracing::warn!(
            cards = ?d.darkened, heads = ?dark,
            "DRM: heads still dark after the release — the kernel re-lights them once nothing \
             holds the GPU open; `fuser -v /dev/dri/*` names what does"
        );
    }
}

/// The connectors of `card` that `sysfs` reports `enabled`, sorted.
fn lit_connectors(sysfs: &Path, card: &str) -> Vec<String> {
    let prefix = format!("{card}-");
    let Ok(dir) = std::fs::read_dir(sysfs) else {
        return Vec::new();
    };
    let mut lit: Vec<String> = dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix) && enabled(sysfs, name) == Some(true))
        .collect();
    lit.sort();
    lit
}

/// `heads` that `sysfs` still reports `disabled` once `budget` is spent or none is left.
fn still_dark(sysfs: &Path, heads: &[String], budget: Duration) -> Vec<String> {
    let deadline = Instant::now() + budget;
    loop {
        let dark: Vec<String> = heads
            .iter()
            .filter(|h| enabled(sysfs, h) == Some(false))
            .cloned()
            .collect();
        if dark.is_empty() || Instant::now() >= deadline {
            return dark;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A connector's sysfs `enabled`; `None` when it is not listed.
fn enabled(sysfs: &Path, connector: &str) -> Option<bool> {
    let state = std::fs::read_to_string(sysfs.join(connector).join("enabled")).ok()?;
    Some(state.trim() == "enabled")
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
/// — no cards, none masterable, or none lit — and therefore nothing to restore. The connectors
/// lit before are read first: what [`release`] checks.
pub fn darken() -> Option<DrmDarken> {
    let mut cards = Vec::new();
    let mut darkened = Vec::new();
    let mut heads = Vec::new();
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
        let lit = lit_connectors(Path::new(SYSFS_DRM), name);
        match darken_card(&path) {
            // Masterable, but nothing on this card was lit. Its fd is dropped here, which is
            // correct: we changed nothing, so there is nothing to hold.
            Ok((_, 0)) => {}
            Ok((card, n)) => {
                tracing::debug!(card = name, crtcs = n, "DRM: CRTCs off");
                darkened.push(name.to_string());
                heads.extend(lit);
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
        cards,
        darkened,
        heads,
    })
}

/// Every CRTC id on `fd`: a count pass, then a data pass into a buffer of that size.
fn crtc_ids(fd: libc::c_int) -> std::io::Result<Vec<u32>> {
    // Count pass: every pointer NULL, the kernel fills in the counts.
    let mut res = DrmModeCardRes::default();
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res)?;
    let n = res.count_crtcs as usize;
    if n == 0 {
        return Ok(Vec::new());
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
    Ok(ids)
}

/// The mode `card`'s one lit CRTC runs, as `(width, height, mHz)` — what the panel shows now,
/// where its EDID only says what it prefers. `None` when the card does not open, nothing is
/// lit, or more than one CRTC is: which of those is the caller's head cannot be told here.
/// No master is taken; the compositor keeps it, and `GETCRTC` never needs it.
pub(crate) fn lit_mode(card: &str) -> Option<(u32, u32, u32)> {
    let file = File::open(Path::new("/dev/dri").join(card)).ok()?;
    let fd = file.as_raw_fd();
    let mut lit = crtc_ids(fd).ok()?.into_iter().filter_map(|id| {
        let mut crtc = DrmModeCrtc {
            crtc_id: id,
            ..Default::default()
        };
        ioctl(fd, DRM_IOCTL_MODE_GETCRTC, &mut crtc).ok()?;
        (crtc.mode_valid != 0).then_some(crtc.mode)
    });
    let mode = lit.next()?;
    if lit.next().is_some() {
        return None;
    }
    Some((
        u32::from(mode.hdisplay),
        u32::from(mode.vdisplay),
        refresh_mhz(&mode),
    ))
}

/// The kernel's `drm_mode_vrefresh` in mHz rather than rounded Hz: 59.94 stays apart from 60.
fn refresh_mhz(m: &DrmModeModeinfo) -> u32 {
    const INTERLACE: u32 = 1 << 4;
    const DBLSCAN: u32 = 1 << 5;
    let (htotal, vtotal) = (u64::from(m.htotal), u64::from(m.vtotal));
    if htotal == 0 || vtotal == 0 {
        return m.vrefresh.saturating_mul(1000);
    }
    // `clock` is kHz: ×1_000_000 over the pixel count per frame is mHz.
    let mut num = u64::from(m.clock) * 1_000_000;
    let mut den = htotal * vtotal;
    if m.flags & INTERLACE != 0 {
        num *= 2;
    }
    if m.flags & DBLSCAN != 0 {
        den *= 2;
    }
    if m.vscan > 1 {
        den *= u64::from(m.vscan);
    }
    (num / den) as u32
}

/// Darken one card. Return the open fd (the hold) and how many CRTCs went off.
/// Closing that fd is last-close and restores the console; count 0 may drop it.
fn darken_card(path: &Path) -> std::io::Result<(File, usize)> {
    let card = File::options().read(true).write(true).open(path)?;
    let fd = card.as_raw_fd();
    // Foreign master (live compositor, other gamescope) ⇒ not ours. Also the Attach
    // guard: do not darken the picture being streamed.
    ioctl(fd, DRM_IOCTL_SET_MASTER, &mut 0u64)?;
    let ids = crtc_ids(fd)?;
    if ids.is_empty() {
        let _ = ioctl(fd, DRM_IOCTL_DROP_MASTER, &mut 0u64);
        return Ok((card, 0));
    }

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
    use super::{
        lit_connectors, refresh_mhz, still_dark, DrmModeCardRes, DrmModeCrtc, DrmModeModeinfo,
    };
    use std::time::Duration;

    /// sysfs over a temp dir: a card's lit connectors are its own (`card1-` is not `card10-`);
    /// after the release only a head still `disabled` counts as dark, an unplugged one not.
    #[test]
    fn the_release_reads_the_heads_back_from_sysfs() {
        let sysfs = std::env::temp_dir().join(format!("pf-drm-sysfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sysfs);
        let set = |connector: &str, state: &str| {
            std::fs::create_dir_all(sysfs.join(connector)).unwrap();
            std::fs::write(sysfs.join(connector).join("enabled"), format!("{state}\n")).unwrap();
        };
        set("card1-DP-1", "enabled");
        set("card1-HDMI-A-1", "enabled");
        set("card1-DP-2", "disabled");
        set("card10-DP-1", "enabled");
        std::fs::create_dir_all(sysfs.join("card1")).unwrap();
        std::fs::create_dir_all(sysfs.join("renderD128")).unwrap();
        let lit = lit_connectors(&sysfs, "card1");
        assert_eq!(lit, ["card1-DP-1", "card1-HDMI-A-1"]);

        set("card1-DP-1", "disabled");
        std::fs::remove_dir_all(sysfs.join("card1-HDMI-A-1")).unwrap();
        assert_eq!(still_dark(&sysfs, &lit, Duration::ZERO), ["card1-DP-1"]);
        set("card1-DP-1", "enabled");
        assert!(still_dark(&sysfs, &lit, Duration::ZERO).is_empty());
        let _ = std::fs::remove_dir_all(&sysfs);
    }

    /// `drm_mode_vrefresh`'s cases: a plain mode, 59.94 kept apart from 60, interlace doubling
    /// the rate, and a mode with no timing falling back to the kernel's rounded field.
    #[test]
    fn the_lit_mode_rate_follows_the_kernels_formula() {
        let mut m = DrmModeCrtc::default().mode;
        // 4K120: 1188 MHz over 4400×2250.
        (m.clock, m.htotal, m.vtotal) = (1_188_000, 4400, 2250);
        assert_eq!(refresh_mhz(&m), 120_000);
        // 1080p59.94: 148.352 MHz over 2200×1125.
        (m.clock, m.htotal, m.vtotal) = (148_352, 2200, 1125);
        assert_eq!(refresh_mhz(&m), 59_940);
        // 1080i60: the same timing at half the pixel clock, interlaced.
        (m.clock, m.flags) = (74_250, 1 << 4);
        assert_eq!(refresh_mhz(&m), 60_000);
        (m.htotal, m.vrefresh) = (0, 75);
        assert_eq!(refresh_mhz(&m), 75_000);
    }

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
