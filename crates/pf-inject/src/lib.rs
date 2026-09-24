//! Host-side input injection: [`punktfunk_core::input::InputEvent`] → compositor or OS input.
//!
//! Headless Sway sets `WLR_LIBINPUT_NO_DEVICES=1`, so kernel `uinput` is never a seat device.
//! Injection uses wlroots virtual-input (`zwlr_virtual_pointer_manager_v1` +
//! `zwp_virtual_keyboard_manager_v1`) as an ordinary Wayland client on Sway's
//! `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`. Keyboard codes are Linux evdev; the keymap is the
//! box layout (`pf_host_config::layout` — `XKB_DEFAULT_*`, then `localectl`) and modifiers
//! are tracked so the compositor resolves shifted keysyms.
//!
//! Consumes `punktfunk_core::input` and `pf-driver-proto`. Not the orchestrator.

// Per-OS backends are defined before any target uses them.
#![allow(dead_code)]
use anyhow::Result;
use punktfunk_core::input::{InputEvent, InputKind};

#[path = "inject/keymap.rs"]
mod keymap;
#[cfg(target_os = "linux")]
pub(crate) use keymap::gs_button_to_evdev;
pub use keymap::KEY_FLAG_SEMANTIC_VK;
// Linux injectors always call this; Windows only the SendInput mirror test. Keep the re-export.
#[cfg_attr(not(target_os = "linux"), allow(unused_imports))]
pub use keymap::vk_to_evdev;

/// Dedup for HID-output reports (0xCD), shared by [`uhid_manager`].
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/hidout_dedup.rs"]
pub mod hidout_dedup;

/// Normalized scroll ([`InputKind::Scroll`]) → per-backend primitive plans.
/// Pure and ungated so tests on any platform assert the same mapping the
/// injectors execute.
#[path = "inject/scroll.rs"]
pub mod scroll;

/// Host-session injector. Not `Send`: owns compositor resources and stays on the control
/// thread that created it.
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Preferred injection backend. Variants are per-OS so [`open`] cannot name a backend the
/// target lacks — that is a compile error, not `bail!`.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// wlroots virtual pointer + keyboard. Headless Sway.
    WlrVirtual,
    /// KWin `org_kde_kwin_fake_input`. Direct; no RemoteDesktop portal. Authorized by the
    /// host `.desktop`.
    KwinFakeInput,
    /// libei via `reis`. RemoteDesktop portal, or Mutter's direct RemoteDesktop API on GNOME
    /// (`libei_ei_source`).
    Libei,
    /// libei against gamescope's EIS socket (no portal). Nested-game / SteamOS-like session.
    GamescopeEi,
}

/// Preferred injection backend. Windows has only `SendInput`.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    SendInput,
}

/// Preferred injection backend. [`open`] rejects it.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Placeholder so the host still builds.
    Unsupported,
}

/// Open the injector for `backend`. One per-OS body; `backend` can only name a backend this
/// target has.
#[cfg(target_os = "linux")]
pub fn open(backend: Backend) -> Result<Box<dyn InputInjector>> {
    match backend {
        Backend::WlrVirtual => Ok(Box::new(wlr::WlrootsInjector::open()?)),
        Backend::KwinFakeInput => Ok(Box::new(kwin_fake_input::KwinFakeInjector::open()?)),
        Backend::Libei => Ok(Box::new(
            libei::LibeiInjector::open_with(libei_ei_source())?,
        )),
        Backend::GamescopeEi => Ok(Box::new(libei::LibeiInjector::open_with(
            libei::EiSource::SocketPathFile(pf_paths::gamescope_ei_socket_file()),
        )?)),
    }
}

/// libei injector pinned to this session's gamescope EIS relay file.
///
/// Isolated multi-user spawns write `LIBEI_SOCKET` to their own file; this must read that
/// one, never the global file a concurrent spawn may rewrite. Separate from [`Backend`] so
/// the enum stays `Copy` (`SESSION_BACKEND` and the knob parser). The worker polls the
/// relay itself, so calling this before gamescope is up is fine.
#[cfg(target_os = "linux")]
pub fn open_gamescope_at(relay: std::path::PathBuf) -> Result<Box<dyn InputInjector>> {
    Ok(Box::new(libei::LibeiInjector::open_with(
        libei::EiSource::SocketPathFile(relay),
    )?))
}

#[cfg(target_os = "windows")]
pub fn open(backend: Backend) -> Result<Box<dyn InputInjector>> {
    match backend {
        Backend::SendInput => Ok(Box::new(sendinput::SendInputInjector::open()?)),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open(_backend: Backend) -> Result<Box<dyn InputInjector>> {
    anyhow::bail!("no input-injection backend on this platform")
}

/// Which output absolute coordinates belong to, by identity rather than size.
///
/// libei regions have no output name. Matching on size is a coin flip when two heads share
/// a mode. `mapping_id` is the protocol's correlate; origin is unique because two outputs
/// cannot share a top-left. An unmatched anchor warns and falls back to the size ladder —
/// the region set is the truth.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AbsoluteAnchor {
    /// Target output's top-left in compositor global logical space.
    pub origin: Option<(i32, i32)>,
    /// EI `mapping_id` of the target output. Wins over origin when both are set.
    pub mapping_id: Option<String>,
}

impl AbsoluteAnchor {
    /// Nothing to match on — treated as "no anchor" so callers can build one unconditionally.
    pub fn is_empty(&self) -> bool {
        self.origin.is_none() && self.mapping_id.is_none()
    }
}

/// Current absolute-coordinate anchor. `RwLock` not env: the injector is host-lifetime behind
/// a channel, so a session reaches it only through typed process state. See `SESSION_BACKEND`.
static ABSOLUTE_ANCHOR: std::sync::RwLock<Option<AbsoluteAnchor>> = std::sync::RwLock::new(None);

/// Capture bring-ups so far — every publish of an aim slot (`set_stream_output`,
/// `set_stream_target`) bumps it, unchanged name included: a new session owes a warp too.
static AIM_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Called by both aim slots. Cross-platform: the debt is a pointer position, not a backend.
pub(crate) fn bump_aim_gen() {
    AIM_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn aim_gen() -> u64 {
    AIM_GEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Streamed head's mode, published beside the aim at capture bring-up.
static STREAM_EXTENT: std::sync::RwLock<Option<(u16, u16)>> = std::sync::RwLock::new(None);

/// Extent the pointer warp sends its centre at. A backend that resolves an absolute sample by
/// SIZE (libei regions) only recognizes the streamed head at the head's own mode; the others
/// normalize and take any extent. `None` leaves the warp on its neutral extent.
pub fn set_stream_extent(mode: Option<(u16, u16)>) {
    *STREAM_EXTENT.write().unwrap_or_else(|e| e.into_inner()) = mode;
}

pub(crate) fn stream_extent() -> Option<(u16, u16)> {
    *STREAM_EXTENT.read().unwrap_or_else(|e| e.into_inner())
}

/// Pin absolute coordinates to a specific output. `None` keeps size-matching.
///
/// Host-level, not per-session: the injector is host-lifetime and every session shares it, so
/// a per-session write would re-aim every other pointer. Fine for `PUNKTFUNK_CAPTURE_MONITOR`
/// (host-wide pin; `design/per-monitor-portal-capture.md`). Do not call from a session path.
///
/// The wlroots backend ignores this and aims via `stream_output::set_stream_output`, which
/// the host publishes per session (last-bring-up-wins, same as Windows `stream_target`).
/// Separate slots: this is the operator's host-wide capture pin, recomputed from policy;
/// stream output is the head the session's capture actually attached to.
pub fn set_absolute_anchor(anchor: Option<AbsoluteAnchor>) {
    let anchor = anchor.filter(|a| !a.is_empty());
    tracing::debug!(?anchor, "input: absolute-coordinate anchor set");
    *ABSOLUTE_ANCHOR.write().unwrap_or_else(|e| e.into_inner()) = anchor;
}

pub fn absolute_anchor() -> Option<AbsoluteAnchor> {
    ABSOLUTE_ANCHOR
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Backend the live session resolved to. Host writes this from `pf_vdisplay::input_backend_id`
/// on connect and on mid-stream Desktop↔Game switch.
///
/// `RwLock` not env, same reason as [`ABSOLUTE_ANCHOR`]. [`default_backend`] runs once per
/// input batch; a `getenv` racing a per-session `setenv` is an `environ` data race with no
/// attacker needed. Host-lifetime, last-write-wins; nothing clears it when a session ends.
#[cfg(target_os = "linux")]
static SESSION_BACKEND: std::sync::RwLock<Option<Backend>> = std::sync::RwLock::new(None);

#[cfg(target_os = "linux")]
fn session_backend() -> Option<Backend> {
    *SESSION_BACKEND.read().unwrap_or_else(|e| e.into_inner())
}

/// The [`Backend`] an id names, in every spelling `PUNKTFUNK_INPUT_BACKEND` accepts.
/// Shared by that knob and [`set_backend_id`], so the two cannot drift.
#[cfg(target_os = "linux")]
fn backend_from_id(id: &str) -> Option<Backend> {
    Some(match id.trim().to_ascii_lowercase().as_str() {
        "wlr" | "wlroots" | "wlrvirtual" => Backend::WlrVirtual,
        "kwin" | "fakeinput" | "fake_input" | "kwin-fake-input" => Backend::KwinFakeInput,
        "libei" | "ei" | "portal" => Backend::Libei,
        "gamescope" | "gamescope-ei" => Backend::GamescopeEi,
        _ => return None,
    })
}

/// Point input at the backend this session's video resolved to; the two must not diverge.
/// `id` is `pf_vdisplay::input_backend_id` (`gamescope`/`kwin`/`libei`/`wlr`).
///
/// Host-threaded, not `PUNKTFUNK_INPUT_BACKEND`. Call wherever a session compositor is
/// decided. The operator-pinned `PUNKTFUNK_COMPOSITOR` path deliberately does not, so that
/// knob stays in charge.
#[cfg(target_os = "linux")]
pub fn set_backend_id(id: &str) {
    let Some(backend) = backend_from_id(id) else {
        tracing::warn!(
            value = id,
            "unknown input backend id — leaving input routing alone"
        );
        return;
    };
    tracing::debug!(?backend, "input: session backend set");
    *SESSION_BACKEND.write().unwrap_or_else(|e| e.into_inner()) = Some(backend);
}

/// The host routes input on every platform; only Linux has a backend to choose between.
#[cfg(not(target_os = "linux"))]
pub fn set_backend_id(_id: &str) {}

/// Pick the injection backend for the current session.
///
/// gamescope hosts its own EIS (no portal) — inject there. Sway implements ScreenCast only,
/// so libei cannot run — wlr virtual-input. KWin `org_kde_kwin_fake_input` is direct (host
/// `.desktop`, no portal). GNOME has neither; libei via Mutter's direct
/// `org.gnome.Mutter.RemoteDesktop` (`libei_ei_source`), so headless-capable.
///
/// Order: live [`set_backend_id`] (from `pf_vdisplay::input_backend_id`), then
/// `PUNKTFUNK_INPUT_BACKEND`, then `PUNKTFUNK_COMPOSITOR`, then `XDG_CURRENT_DESKTOP`.
/// Env rungs run only before a session publishes (and on the operator-pinned path, which
/// publishes nothing), so a live stream never `getenv`s — see [`SESSION_BACKEND`].
#[cfg(target_os = "linux")]
pub fn default_backend() -> Backend {
    if let Some(b) = session_backend() {
        return b;
    }
    if let Ok(v) = std::env::var("PUNKTFUNK_INPUT_BACKEND") {
        match backend_from_id(&v) {
            Some(b) => return b,
            None => tracing::warn!(
                value = v.trim(),
                "unknown PUNKTFUNK_INPUT_BACKEND — auto-detecting"
            ),
        }
    }
    // Explicit compositor pick (per connect / mid-stream) outranks the desktop sniff.
    let compositor = pf_host_config::config().compositor.clone();
    if let Some(c) = compositor.as_deref() {
        let c = c.trim();
        if c.eq_ignore_ascii_case("gamescope") {
            return Backend::GamescopeEi;
        }
        if c.eq_ignore_ascii_case("kwin") {
            return Backend::KwinFakeInput;
        }
        if c.eq_ignore_ascii_case("wlroots")
            || c.eq_ignore_ascii_case("sway")
            // Hyprland kept the wlr virtual-input protocols; same backend as sway/river.
            || c.eq_ignore_ascii_case("hyprland")
        {
            return Backend::WlrVirtual;
        }
        // mutter (GNOME) falls through to the XDG_CURRENT_DESKTOP check below.
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let d = desktop.to_ascii_uppercase();
    if d.contains("KDE") {
        Backend::KwinFakeInput
    } else if d.contains("GNOME") {
        Backend::Libei
    } else {
        Backend::WlrVirtual
    }
}

#[cfg(target_os = "windows")]
pub fn default_backend() -> Backend {
    Backend::SendInput
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn default_backend() -> Backend {
    Backend::Unsupported
}

/// Whether this backend can type committed text ([`InputKind::TextInput`], `HOST_CAP_TEXT_INPUT`).
/// Windows always (`KEYEVENTF_UNICODE`). Linux only on wlroots (Unicode virtual keyboard);
/// KWin/libei/gamescope can only press host-layout keycodes. Welcome-time cap; a mid-session
/// switch away from a capable backend drops text events (input is lossy by design).
#[cfg(target_os = "windows")]
pub fn text_input_supported() -> bool {
    true
}

/// Linux types text only through the wlroots virtual-keyboard backend.
#[cfg(target_os = "linux")]
pub fn text_input_supported() -> bool {
    matches!(default_backend(), Backend::WlrVirtual)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn text_input_supported() -> bool {
    false
}

/// Whether wire touch contacts land on the desktop (`HOST_CAP2_TOUCH`).
/// libei and KWin carry touch; wlroots virtual-pointer has no touch protocol and drops
/// contacts. Welcome-time so a client can fall back from passthrough.
#[cfg(target_os = "linux")]
pub fn touch_supported() -> bool {
    !matches!(default_backend(), Backend::WlrVirtual)
}

/// `PT_TOUCH` synthetic pointer, same 1809 probe as [`pen_supported`], without the pen
/// kill-switch.
#[cfg(target_os = "windows")]
pub fn touch_supported() -> bool {
    pen::synthetic_pen_available()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn touch_supported() -> bool {
    false
}

/// Whether this backend consumes normalized scroll ([`InputKind::Scroll`],
/// `HOST_CAP2_SCROLL`). Every shipped backend does — the mapper lowers it onto
/// each one's own primitives. Welcome-time; a host without the bit only sees
/// the legacy `MouseScroll` wire.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn scroll_supported() -> bool {
    true
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn scroll_supported() -> bool {
    false
}

/// Full-fidelity stylus (`HOST_CAP_PEN`). Linux only: [`pen::VirtualPen`] uinput tablet.
/// Probe is "can we open /dev/uinput" (same permission as virtual gamepads) plus
/// `PUNKTFUNK_PEN=0`. Welcome-time; clients without the bit fold pen into touch/pointer.
#[cfg(target_os = "linux")]
pub fn pen_supported() -> bool {
    if pf_host_config::knob("PUNKTFUNK_PEN").as_deref() == Some("0") {
        return false;
    }
    // SAFETY: 'static NUL-terminated path literal; `open` returns a fresh fd (or -1) and
    // retains nothing.
    let fd = unsafe {
        libc::open(
            c"/dev/uinput".as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return false;
    }
    // SAFETY: `fd >= 0` is the fd opened above, owned by no one else; closed exactly once here.
    unsafe { libc::close(fd) };
    true
}

/// Synthetic PT_PEN/PT_TOUCH on Win10 1809+. Probe creates then destroys a PT_PEN device.
/// Same `PUNKTFUNK_PEN=0` kill-switch. Result also stands in for PT_TOUCH (both APIs arrived
/// in 1809).
#[cfg(target_os = "windows")]
pub fn pen_supported() -> bool {
    if pf_host_config::knob("PUNKTFUNK_PEN").as_deref() == Some("0") {
        return false;
    }
    pen::synthetic_pen_available()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn pen_supported() -> bool {
    false
}

/// Result of [`uinput_probe`].
///
/// [`pen_supported`] returns a bool, so "module missing" and "not in `input` group" look the
/// same and need different remedies. This keeps the errno. A verdict enum on purpose — this
/// crate must not learn the host's wire types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UinputVerdict {
    /// Virtual gamepads and the pen can be created.
    Ok,
    /// `EACCES`/`EPERM`: node present, this process may not open it. User not in `input`, or
    /// the udev rule granting that group was never installed.
    PermissionDenied { path: &'static str },
    /// `ENOENT` and friends: the node does not exist — module or rule missing.
    Missing { path: &'static str },
    /// Some other errno; carried verbatim rather than guessed at.
    Error { path: &'static str, message: String },
    /// No uinput/uhid injection on this platform.
    Inapplicable,
}

/// Nodes every virtual input device needs, in report order: `/dev/uinput` kills pen and
/// evdev pads; `/dev/uhid` kills DualSense/Switch Pro HID.
#[cfg(target_os = "linux")]
const INPUT_NODES: &[(&std::ffi::CStr, &str)] =
    &[(c"/dev/uinput", "/dev/uinput"), (c"/dev/uhid", "/dev/uhid")];

/// Probe `/dev/uinput` and `/dev/uhid` as the backends will, keeping the errno. Two
/// `open()`s; diagnostics can re-run on demand.
#[cfg(target_os = "linux")]
pub fn uinput_probe() -> UinputVerdict {
    for &(c_path, path) in INPUT_NODES {
        // SAFETY: 'static NUL-terminated path literal; `open` returns a fresh fd (or -1) and
        // retains nothing.
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd >= 0` is the fd opened above, owned by no one else; closed exactly once.
            unsafe { libc::close(fd) };
            continue;
        }
        // Read errno immediately: any further libc call (including the close above) clobbers it.
        let err = std::io::Error::last_os_error();
        return match err.raw_os_error() {
            Some(libc::EACCES) | Some(libc::EPERM) => UinputVerdict::PermissionDenied { path },
            Some(libc::ENOENT) | Some(libc::ENXIO) | Some(libc::ENODEV) => {
                UinputVerdict::Missing { path }
            }
            _ => UinputVerdict::Error {
                path,
                message: err.to_string(),
            },
        };
    }
    UinputVerdict::Ok
}

/// uinput/uhid are Linux; Windows injects through its own driver stack.
#[cfg(not(target_os = "linux"))]
pub fn uinput_probe() -> UinputVerdict {
    UinputVerdict::Inapplicable
}

/// usbip/vhci attach node as seen from here — [`vhci_probe`].
///
/// Device facts only: module present, process can write the node. Does not reason about
/// group membership; in-group-on-disk vs in-group-in-this-process needs the user database,
/// which is the host's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VhciVerdict {
    /// Module loaded and this process can write `attach`.
    Ok,
    /// No `/sys/devices/platform/vhci_hcd*/status`: module not loaded.
    ModuleMissing,
    /// Node present; this process cannot write it. Why is the host's question.
    NotWritable { path: String },
    /// Virtual-Deck-over-usbip does not apply here.
    Inapplicable { why: &'static str },
}

/// Probe the vhci attach node: module present, writable by this process.
///
/// Writability is what attach will attempt. `60-punktfunk.rules` `chgrp punktfunk` +
/// `chmod 0660`, so only a process that actually carries the group gets `W_OK`.
#[cfg(target_os = "linux")]
pub fn vhci_probe() -> VhciVerdict {
    use std::os::unix::ffi::OsStrExt;

    if !steam_usbip::usbip_preferred() {
        return VhciVerdict::Inapplicable {
            why: "the virtual Steam Deck's usbip transport is disabled (PUNKTFUNK_STEAM_USBIP=0)",
        };
    }
    let Some(base) = steam_usbip::vhci_base() else {
        return VhciVerdict::ModuleMissing;
    };
    let attach = base.join("attach");
    let Ok(c_path) = std::ffi::CString::new(attach.as_os_str().as_bytes()) else {
        return VhciVerdict::NotWritable {
            path: attach.display().to_string(),
        };
    };
    // SAFETY: `c_path` is a NUL-terminated path owned by this frame and outlives the call;
    // `access` only reads it and retains nothing.
    let writable = unsafe { libc::access(c_path.as_ptr(), libc::W_OK) } == 0;
    if writable {
        VhciVerdict::Ok
    } else {
        VhciVerdict::NotWritable {
            path: attach.display().to_string(),
        }
    }
}

/// usbip/vhci is Linux-only.
#[cfg(not(target_os = "linux"))]
pub fn vhci_probe() -> VhciVerdict {
    VhciVerdict::Inapplicable {
        why: "the virtual Steam Deck's usbip transport is Linux-only",
    }
}

/// The Windows virtual-gamepad driver as the last pad that met it saw it — [`pad_driver_probe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PadDriverVerdict {
    /// No virtual pad has met its driver this run.
    Unseen,
    /// The driver attached at this host's revision.
    Current,
    /// The driver attached with an older revision: an old package still serves the pads.
    Stale { driver_rev: u32, host_rev: u32 },
    /// The driver speaks another channel protocol, so it refuses every pad.
    ProtocolMismatch { driver_proto: u32, host_proto: u32 },
    /// The pad reports another controller's VID/PID than its hardware id names.
    WrongIdentity { want: (u16, u16), got: (u16, u16) },
    /// No driver attached to the pad within the grace period.
    NotAttached,
}

/// Verdict for a pad whose driver just attached. `devtype` is the identity its hardware id
/// names, `driver_rev` what the driver stamped, `vid_pid` what its HID collection reports.
pub fn pad_attach_verdict(
    devtype: u8,
    driver_rev: u32,
    vid_pid: Option<(u16, u16)>,
) -> PadDriverVerdict {
    use pf_driver_proto::gamepad::{identity_vid_pid, GAMEPAD_DRIVER_REV};
    if let (Some(want), Some(got)) = (identity_vid_pid(devtype), vid_pid)
        && want != got
    {
        return PadDriverVerdict::WrongIdentity { want, got };
    }
    if driver_rev < GAMEPAD_DRIVER_REV {
        return PadDriverVerdict::Stale {
            driver_rev,
            host_rev: GAMEPAD_DRIVER_REV,
        };
    }
    PadDriverVerdict::Current
}

/// Whether a pad devnode that failed to start (`CM_PROB_FAILED_START`, 10) is one the driver
/// refused: its lowercase hardware ids name no identity. UMDF reports that refusal as
/// `STATUS_DEVICE_DATA_ERROR`, not the driver's own status, so the ids are the evidence.
pub fn pad_refused(problem: u32, hwids: &str) -> bool {
    problem == 10 && pf_driver_proto::gamepad::devtype_from_hwids(hwids).is_none()
}

#[cfg(target_os = "windows")]
static PAD_DRIVER: std::sync::Mutex<PadDriverVerdict> =
    std::sync::Mutex::new(PadDriverVerdict::Unseen);

/// The latest verdict a pad recorded this run; every pad binds the same driver package.
#[cfg(target_os = "windows")]
pub fn pad_driver_probe() -> PadDriverVerdict {
    PAD_DRIVER
        .lock()
        .map_or(PadDriverVerdict::Unseen, |v| v.clone())
}

#[cfg(target_os = "windows")]
pub(crate) fn note_pad_driver(verdict: PadDriverVerdict) {
    if let Ok(mut v) = PAD_DRIVER.lock() {
        *v = verdict;
    }
}

#[cfg(test)]
mod pad_driver_verdict_tests {
    use super::*;
    use pf_driver_proto::gamepad::{DEVTYPE_DUALSENSE, DEVTYPE_XBOX_ELITE, GAMEPAD_DRIVER_REV};

    /// An attached driver below this host's revision still serves pads, with old behaviour.
    /// Only the revision tells; the channel protocol matches.
    #[test]
    fn an_older_driver_revision_reads_as_stale() {
        let ds = Some((0x054C, 0x0CE6));
        assert_eq!(
            pad_attach_verdict(DEVTYPE_DUALSENSE, GAMEPAD_DRIVER_REV, ds),
            PadDriverVerdict::Current
        );
        assert_eq!(
            pad_attach_verdict(DEVTYPE_DUALSENSE, 0, ds),
            PadDriverVerdict::Stale {
                driver_rev: 0,
                host_rev: GAMEPAD_DRIVER_REV
            },
            "a driver older than the revision field stamps 0"
        );
        assert_eq!(
            pad_attach_verdict(DEVTYPE_DUALSENSE, GAMEPAD_DRIVER_REV, None),
            PadDriverVerdict::Current,
            "an unread VID/PID is no evidence against the pad"
        );
    }

    /// A failed start on a devnode whose ids carry no pf_* token is the driver's refusal; the
    /// ids as the devnode reported them on .173.
    #[test]
    fn a_failed_pad_with_no_pf_id_reads_as_refused() {
        let unknown =
            "zz_unknown_pad;usb\\vid_054c&pid_0ce6&rev_0100&mi_03;usb\\vid_054c&pid_0ce6&mi_03;";
        assert!(pad_refused(10, unknown));
        assert!(!pad_refused(
            10,
            "pf_dualsense;usb\\vid_054c&pid_0ce6&mi_03;"
        ));
        assert!(
            !pad_refused(28, unknown),
            "no driver bound is not a refusal"
        );
    }

    /// An old driver that does not know a hardware id answers as a DualSense.
    #[test]
    fn a_pad_answering_as_another_controller_is_named() {
        assert_eq!(
            pad_attach_verdict(DEVTYPE_XBOX_ELITE, 0, Some((0x054C, 0x0CE6))),
            PadDriverVerdict::WrongIdentity {
                want: (0x045E, 0x0B22),
                got: (0x054C, 0x0CE6)
            }
        );
    }
}

#[path = "inject/service.rs"]
mod service;
pub use service::InjectorService;

/// How libei reaches EIS. KWin uses the RemoteDesktop portal (pre-seeded grant). GNOME
/// portal `Start()` needs interactive approval a headless host cannot answer, so GNOME uses
/// Mutter's direct `org.gnome.Mutter.RemoteDesktop` — same API as the Mutter video backend.
#[cfg(target_os = "linux")]
fn libei_ei_source() -> libei::EiSource {
    let gnome = pf_host_config::config()
        .compositor
        .as_deref()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("mutter"))
        || std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_uppercase()
            .contains("GNOME");
    if gnome {
        libei::EiSource::MutterEis
    } else {
        libei::EiSource::Portal
    }
}

/// Which process serves a Windows devnode (`pf_driver_proto::gamepad::ChannelProof`).
/// Unforgeable; the sealed pad channel duplicates DATA from it instead of a
/// LocalService-writable mailbox.
#[cfg(target_os = "windows")]
#[path = "inject/windows/channel_proof.rs"]
pub mod channel_proof;
#[cfg(target_os = "linux")]
#[path = "inject/linux/dualsense.rs"]
pub mod dualsense;
/// Virtual DualSense Edge via UMDF + shm (device-type 2). Wire back grips land on the Edge
/// native back/Fn buttons.
#[cfg(target_os = "windows")]
#[path = "inject/windows/dualsense_edge_windows.rs"]
pub mod dualsense_edge_windows;
/// DualSense HID contract, shared by Linux UHID ([`dualsense`]) and Windows UMDF
/// ([`dualsense_windows`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/dualsense_proto.rs"]
pub mod dualsense_proto;
/// Virtual DualSense over USB/IP (`vhci_hcd`) with its own USB Audio Class card — real USB
/// topology so wine can derive a ContainerId and GE-Proton finds an ALSA card. [`dualsense`]
/// uhid can do neither.
#[cfg(target_os = "linux")]
#[path = "inject/linux/dualsense_usbip.rs"]
pub mod dualsense_usbip;
/// Virtual DualSense via UMDF minidriver + shm host channel.
#[cfg(target_os = "windows")]
#[path = "inject/windows/dualsense_windows.rs"]
pub mod dualsense_windows;
#[cfg(target_os = "linux")]
#[path = "inject/linux/dualshock4.rs"]
pub mod dualshock4;
/// DualShock 4 HID codec, shared by Linux UHID ([`dualshock4`]) and Windows UMDF
/// ([`dualshock4_windows`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/dualshock4_proto.rs"]
pub mod dualshock4_proto;
/// Virtual DualShock 4 via UMDF + shm (device-type 1).
#[cfg(target_os = "windows")]
#[path = "inject/windows/dualshock4_windows.rs"]
pub mod dualshock4_windows;
#[cfg(target_os = "linux")]
#[path = "inject/linux/gamepad.rs"]
pub mod gamepad;
/// Virtual Xbox 360 pads via the in-tree XUSB companion UMDF driver (classic XInput).
#[cfg(target_os = "windows")]
#[path = "inject/windows/gamepad_windows.rs"]
pub mod gamepad;
/// RAII wrappers (`Shm` section+view, `SwDevice` devnode) shared by DualSense / DualShock 4
/// / XUSB so each resource closes on drop.
#[cfg(target_os = "windows")]
#[path = "inject/windows/gamepad_raii.rs"]
mod gamepad_raii;
/// Resident virtual HID mouse via pf-mouse UMDF. Keeps `SM_MOUSEPRESENT` true on headless
/// hosts so DWM composites a cursor into the IDD frame — `SendInput` alone moves an
/// invisible pointer with no physical mouse.
#[cfg(target_os = "windows")]
#[path = "inject/windows/mouse_windows.rs"]
pub mod mouse_windows;
/// Virtual-pad creation-retry ([`pad_gate::PadGate`]), driven by [`pad_slots`]. Replaces a
/// per-backend permanent `broken` latch with capped-backoff retry.
///
/// Built on every target: pure `std::time` arithmetic. Gating it meant its tests — and
/// [`pad_slots`]' — could not run off a host box.
#[path = "inject/pad_gate.rs"]
pub mod pad_gate;
/// Host-wide OS pad slots ([`pad_pool::PadSlotPool`]) and per-session wire-index → slot map
/// ([`pad_pool::PadSlotMap`]).
///
/// Every OS name a virtual pad needs (mailboxes, `SwDeviceCreate` ids, DualSense MAC, Deck
/// serial, Switch MAC) is derived from a pad index, while every client numbers its first
/// pad 0 and the host serves several sessions. This is what keeps those from colliding.
///
/// Built on every target like [`pad_gate`]: a bitmap and an array, no OS pad API. The
/// collision only a multi-session host can show, so the policy has tests everywhere or
/// nowhere.
#[path = "inject/pad_pool.rs"]
pub mod pad_pool;
/// Virtual-pad slot table + create lifecycle ([`pad_slots::PadSlots`]): `Vec<Option<Pad>>`,
/// `active_mask` unplug sweep, gate-checked create.
///
/// Built on every target like [`pad_gate`]: the backend supplies the pad type and `open`
/// closure, so a platform gate only blocked tests. [`pad_slots::PadCreateFault`] describes
/// a `cfg(windows)` failure only a Windows box produces — classification tests run
/// everywhere or nowhere.
#[path = "inject/pad_slots.rs"]
pub mod pad_slots;
/// Per-seat device visibility ([`seat_dev::SeatDev`]): the symlinks a sandboxed seat's Steam
/// resolves its pads through, and the host-wide lock every create takes.
///
/// Built on every target like [`pad_gate`]: only a Linux seat ever sets a directory, but the
/// window arithmetic and the cross-seat rule are what a test pins, everywhere or nowhere.
#[path = "inject/seat_dev.rs"]
pub mod seat_dev;
/// `sensor_timestamp` every virtual Sony pad stamps into its input reports
/// ([`sensor_clock::SensorClock`]) — elapsed time in DualSense 1/3 µs and DualShock 4
/// 5.33 µs units, shared by all four backends.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/sensor_clock.rs"]
pub mod sensor_clock;
/// Virtual Steam Deck via UHID — kernel `hid-steam` binds it as a real Deck.
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_controller.rs"]
pub mod steam_controller;
/// Virtual Steam Controller 2 (Triton, `28DE:1302`) via UHID — raw passthrough of a
/// client-captured physical pad; Steam Input drives hidraw (no kernel driver binds it).
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_controller2.rs"]
pub mod steam_controller2;
/// Virtual Steam Deck via UMDF + shm (device-type 3). Steam Input promotes it because of
/// the `&MI_02` hardware-id synthesis.
#[cfg(target_os = "windows")]
#[path = "inject/windows/steam_deck_windows.rs"]
pub mod steam_deck_windows;
/// Virtual Steam Deck via USB gadget (`raw_gadget` + `dummy_hcd`). Only virtual-Deck
/// transport Steam Input promotes (controller on USB interface 2). SteamOS-host only.
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_gadget.rs"]
pub mod steam_gadget;
/// Steam Controller / Steam Deck HID contract (descriptor, byte-exact Deck serializer,
/// XInput/rich mappers, rumble parser). Linux UHID ([`steam_controller`]) and Windows UMDF
/// ([`steam_deck_windows`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/steam_proto.rs"]
pub mod steam_proto;
/// Fallback remap of Steam-only inputs onto a non-Steam backend, plus Deck motion rescale.
/// Shared by DualSense/DS4 (slot-less pads that must fold Steam back grips). Deck rescale
/// is Linux-only but harmless to compile on Windows.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/proto/steam_remap.rs"]
pub mod steam_remap;
/// Virtual Steam Deck over USB/IP (`vhci_hcd`). Steam-Input-promotable on non-SteamOS hosts
/// where `dummy_hcd`/`raw_gadget` are not built. In-tree and signed; no MOK.
#[cfg(target_os = "linux")]
#[path = "inject/linux/steam_usbip.rs"]
pub mod steam_usbip;
/// Virtual Switch Pro via UHID (kernel `hid-nintendo`).
#[cfg(target_os = "linux")]
#[path = "inject/linux/switch_pro.rs"]
pub mod switch_pro;
/// Switch Pro codec + canned `hid-nintendo` handshake replies, used by [`switch_pro`].
/// Not cfg-gated (same reason as `triton_proto`): pure byte-packing, so layout tests and
/// the IMU unit contract in `tests/motion_contract.rs` run on any host, Windows included.
#[path = "inject/proto/switch_proto.rs"]
pub mod switch_proto;
/// Sysfs/procfs poll helpers shared by the Linux backends' `#[ignore]`d device tests.
#[cfg(all(test, target_os = "linux"))]
#[path = "inject/linux/test_sysfs.rs"]
mod test_sysfs;
/// Steam Controller 2 (Triton) contract: state layout, feature query-dance, rumble parse.
/// Not cfg-gated like `steam_controller2`: pure byte-packing, so layout tests run on any
/// host — consumers are Linux uhid/usbip and Windows `triton_windows`.
#[path = "inject/proto/triton_proto.rs"]
pub mod triton_proto;
/// Virtual Steam Controller 2 over USB/IP — USB device byte-matched to the physical wired
/// pad's captured descriptors, so Steam lists it (UHID is invisible to Steam). Preferred
/// transport of [`steam_controller2`].
#[cfg(target_os = "linux")]
#[path = "inject/linux/triton_usbip.rs"]
pub mod triton_usbip;
/// Virtual Steam Controller 2 (Triton, `28DE:1302`) via UMDF + shm (device-type 7). Raw
/// passthrough of a client-captured physical pad; Steam feature/output writes drain back
/// kind-tagged (FEATURE vs OUTPUT) for replay on the client's real controller.
#[cfg(target_os = "windows")]
#[path = "inject/windows/triton_windows.rs"]
pub mod triton_windows;
/// `/dev/uhid` event ABI shared by every UHID gamepad backend — constants each used to
/// transcribe, plus field accessors that read a payload's real length.
#[cfg(target_os = "linux")]
#[path = "inject/linux/uhid_abi.rs"]
pub mod uhid_abi;
/// Stateful virtual-pad manager ([`uhid_manager::UhidManager`]) — event routing, frame
/// merge, heartbeat, and feedback pump shared by the five UHID/UMDF backends; each supplies
/// only its protocol via [`uhid_manager::PadProto`].
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "inject/uhid_manager.rs"]
pub mod uhid_manager;
/// Byte-level tracing of the USB/IP socket (`PUNKTFUNK_USBIP_TRACE`). A framing bug in that
/// stream is only visible as damage the kernel notices later, so the wire itself has to be
/// recoverable.
#[cfg(target_os = "linux")]
#[path = "inject/linux/usbip_trace.rs"]
pub mod usbip_trace;
/// Xbox HID codec — report `pf-gamepad` serves as device-types 4/5/6 (Wireless / One S /
/// Elite Series 2). Same descriptor, VID/PID differ. Gives HID footing `pf-xusb` never had:
/// Steam / WGI / GameInput / DirectInput cannot see an XUSB-interface-only device.
///
/// Not cfg-gated: pure byte-packing, so layout tests run on any host — including machines
/// that cannot build the Windows backends.
#[path = "inject/proto/xbox_proto.rs"]
pub mod xbox_proto;
/// Virtual Xbox pads via UMDF — Wireless (device-type 4), One S (5), Elite Series 2 (6).
/// HID-visible alternative to [`gamepad`]'s XUSB companion, which Steam / WGI / GameInput
/// / DirectInput cannot enumerate (XUSB interface only). Three identities share one
/// descriptor; VID/PID, product string, and INF model line differ.
#[cfg(target_os = "windows")]
#[path = "inject/windows/xbox_windows.rs"]
pub mod xbox_windows;
/// Stub — virtual gamepads need Linux uinput or Windows UMDF; events are dropped elsewhere.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub mod gamepad {
    #[derive(Default)]
    pub struct GamepadManager;
    impl GamepadManager {
        pub fn new() -> Self {
            GamepadManager
        }
        pub fn handle(&mut self, _ev: &punktfunk_core::input::GamepadEvent) {}
        pub fn pump_rumble(&mut self, _send: impl FnMut(u16, u16, u16, u16, u16)) {}
        /// No pad is ever made here, so there is nothing to show a seat.
        pub fn expose_in(&mut self, _dir: Option<std::path::PathBuf>) {}
    }
}
/// "Punktfunk Pen" uinput virtual tablet — per-session stylus the native pen plane injects
/// through.
#[cfg(target_os = "linux")]
#[path = "inject/linux/pen.rs"]
pub mod pen;
/// PT_PEN/PT_TOUCH synthetic pointer devices. `pen::VirtualPen` is the PT_PEN device;
/// `pen::SyntheticTouch` backs the SendInput injector's wire-touch path.
#[cfg(target_os = "windows")]
#[path = "inject/windows/pointer_windows.rs"]
pub mod pen;
/// Streamed output's desktop rect that every absolute coordinate (pen, touch, absolute
/// mouse) maps into — published at capture bring-up, resolved through the CCD source rect
/// (cursor-readback poller's resolver, so both directions agree). Mapping over the whole
/// virtual desktop is the Extend-topology offset bug.
#[cfg(target_os = "windows")]
#[path = "inject/windows/stream_target.rs"]
pub mod stream_target;
#[cfg(target_os = "windows")]
pub use stream_target::set_stream_target;
/// Streamed compositor output (by name) that absolute coordinates map into — counterpart
/// of Windows `stream_target`. Published at capture bring-up; wlroots binds its pointer to
/// that `wl_output`.
#[cfg(target_os = "linux")]
#[path = "inject/linux/stream_output.rs"]
pub mod stream_output;
#[cfg(target_os = "linux")]
pub use stream_output::{set_stream_output, stream_output};
/// Stub — pen injection needs the Linux uinput tablet or Windows synthetic pointers.
/// `pen_supported()` is false here, so no host advertises the cap and no batches arrive.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub mod pen {
    use anyhow::{bail, Result};
    pub struct VirtualPen;
    impl VirtualPen {
        pub fn create() -> Result<VirtualPen> {
            bail!("no pen injection backend on this platform")
        }
        pub fn apply_batch(&mut self, _transitions: &[punktfunk_core::quic::PenTransition]) {}
    }
}
#[cfg(target_os = "linux")]
#[path = "inject/linux/kwin_fake_input.rs"]
mod kwin_fake_input;
#[cfg(target_os = "linux")]
#[path = "inject/linux/libei.rs"]
mod libei;
#[cfg(target_os = "windows")]
#[path = "inject/windows/sendinput.rs"]
mod sendinput;
#[cfg(target_os = "linux")]
#[path = "inject/linux/wlr.rs"]
mod wlr;

/// `CLOCK_MONOTONIC` in µs: the clock libinput and the compositor stamp input with.
/// Clients compare our stamps with the compositor's synthesised events; GTK closes a
/// menu whose popup-grab release looks more than 500 ms after our press.
#[cfg(target_os = "linux")]
fn monotonic_us() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `CLOCK_MONOTONIC` is a valid clock id and `ts` is a live, writable timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

#[cfg(all(test, target_os = "linux"))]
mod backend_select_tests {
    use super::*;

    /// Session backend is a published value ([`set_backend_id`]) and outranks every env
    /// rung. [`default_backend`] runs per input batch; a `getenv` there raced connect-path
    /// `setenv`. Makes no claim about the environment: nothing needs to write it, so the
    /// test does not.
    #[test]
    fn the_session_backend_threads_through_instead_of_the_process_env() {
        set_backend_id("gamescope");
        assert_eq!(default_backend(), Backend::GamescopeEi);
        // A mid-stream Game→Desktop switch re-publishes; input follows, with no env write.
        set_backend_id("kwin");
        assert_eq!(default_backend(), Backend::KwinFakeInput);
        // An unrecognised id must leave routing where it was, not retarget a backend the
        // video side did not choose.
        set_backend_id("not-a-backend");
        assert_eq!(default_backend(), Backend::KwinFakeInput);
    }

    /// Every id `pf_vdisplay::input_backend_id` can emit must be one this crate accepts.
    /// The two are halves of one contract across a crate boundary the compiler cannot
    /// check — pf-vdisplay must not depend on pf-inject, so it emits `&'static str` and
    /// this pins the receiving end. Counterpart:
    /// `every_compositor_names_the_injector_backend_that_matches_it` in pf-vdisplay.
    #[test]
    fn every_id_the_video_side_emits_maps_to_a_backend() {
        for (id, want) in [
            ("gamescope", Backend::GamescopeEi),
            ("kwin", Backend::KwinFakeInput),
            ("libei", Backend::Libei),
            ("wlr", Backend::WlrVirtual),
        ] {
            assert_eq!(backend_from_id(id), Some(want), "{id}");
        }
        assert_eq!(backend_from_id("not-a-backend"), None);
    }
}
