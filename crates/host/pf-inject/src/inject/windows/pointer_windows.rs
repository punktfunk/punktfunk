//! Windows synthetic-pointer injection (`design/pen-tablet-input.md`): a per-session
//! `PT_PEN` device with the pen plane's full fidelity — pressure (Windows 0..1024),
//! polar tilt → tiltX/tiltY, barrel roll on `rotation` (0..359), barrel button, eraser
//! (`PEN_FLAG_INVERTED`/`ERASER`), hover — plus a `PT_TOUCH` device for wire touches
//! (`SendInput` touch is a no-op).
//!
//! Synthetic pointer state goes stale in ~100 ms (`design/apollo-comparison.md`), so
//! each device re-asserts the last frame every [`REFRESH_MS`] while in range / held.
//! `CreateSyntheticPointerDevice` needs Win10 1809+; [`crate::pen_supported`] probes it.
//!
//! Frame grouping matches Linux uinput: proximity-enter lands with its position, tip
//! edges own DOWN/UP frames, range-leave is a final frame without `INRANGE`.

use anyhow::{Context, Result};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{
    PenSample, PenTool, PenTransition, PEN_ANGLE_UNKNOWN, PEN_BARREL1, PEN_TILT_UNKNOWN,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::POINT;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, HDESK,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_NEW, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_FLAG_NONE, PEN_MASK_PRESSURE,
    PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH, TOUCH_FLAG_NONE,
    TOUCH_MASK_NONE,
};

/// Re-inject cadence while state is held (in-range pen / touching contacts). Apollo uses
/// 50 ms against the ~100 ms staleness window; 40 ms keeps two refreshes inside it.
const REFRESH_MS: u64 = 40;

/// `GENERIC_ALL` for the desktop open — the `windows` crate models desktop rights as their own
/// flag type and doesn't re-export the generic ones (same constant `sendinput.rs` uses).
const DESKTOP_GENERIC_ALL: u32 = 0x1000_0000;

/// This thread's binding to the input desktop, restored on drop.
///
/// `SendInput` binds a dedicated injector thread once. Here `inject_pen` /
/// `inject_touch_frame` run on the batch caller *and* the refresh threads; the
/// batch thread must not stay parked on a `Winlogon` desktop that vanishes when
/// the prompt dismisses. Scope the bind to the retry: `GetThreadDesktop` is
/// borrowed (never closed); only the `OpenInputDesktop` handle is closed after
/// the thread moves back off it.
struct InputDesktopBinding {
    previous: HDESK,
    input: HDESK,
}

impl InputDesktopBinding {
    fn bind() -> Option<Self> {
        // SAFETY: FFI calls taking by-value args only. `OpenInputDesktop` yields an owned `HDESK`
        // solely on `Ok`; it is either installed by `SetThreadDesktop` (then owned by this guard,
        // closed exactly once in `Drop`) or closed here on failure — closed once on every path,
        // never used after. `SetThreadDesktop` rebinds only the calling thread, which owns no
        // windows or hooks, so it cannot fail on that account.
        unsafe {
            let previous = GetThreadDesktop(GetCurrentThreadId()).ok()?;
            let input = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(DESKTOP_GENERIC_ALL),
            )
            .ok()?;
            if SetThreadDesktop(input).is_err() {
                let _ = CloseDesktop(input);
                return None;
            }
            Some(Self { previous, input })
        }
    }
}

impl Drop for InputDesktopBinding {
    fn drop(&mut self) {
        // SAFETY: `previous` is the borrowed desktop this thread started on, `input` the handle
        // this guard uniquely owns. The thread moves back FIRST, so the handle is not the thread's
        // desktop when closed — closed exactly once, never used after.
        unsafe {
            let _ = SetThreadDesktop(self.previous);
            let _ = CloseDesktop(self.input);
        }
    }
}

/// Inject one synthetic-pointer frame, following the input desktop on ACCESS_DENIED.
///
/// A UAC / lock / logon desktop owns input; injection from `WinSta0\Default`
/// returns `ERROR_ACCESS_DENIED`. The synthetic device is not desktop-affine, so
/// rebinding the thread is enough — recreating the device would drop in-flight
/// contacts and pen in-range state.
fn inject_following_desktop(
    dev: &Device,
    frame: &[POINTER_TYPE_INFO],
) -> windows::core::Result<()> {
    // SAFETY: `dev` owns a live synthetic-pointer device for the borrow; the call only reads
    // `frame`. Best-effort, exactly as the direct call was.
    match unsafe { InjectSyntheticPointerInput(dev.0, frame) } {
        Ok(()) => Ok(()),
        Err(first) => {
            // Only a desktop switch is worth a rebind; anything else would just fail identically.
            let Some(_binding) = InputDesktopBinding::bind() else {
                return Err(first);
            };
            // SAFETY: same live `dev`/`frame`, re-issued with this thread on the input desktop.
            unsafe { InjectSyntheticPointerInput(dev.0, frame) }
        }
    }
}

/// Windows pen pressure is 0..1024 (vs the wire's full-scale u16).
const WIN_PEN_PRESSURE_MAX: u32 = 1024;

/// Map a normalized [0,1] pair onto the STREAMED output's rect ([`crate::stream_target`])
/// — the same surface SendInput absolute mouse uses. Mapping over the whole virtual
/// desktop is only right when they coincide (Exclusive topology; the no-target fallback).
fn to_screen(x: f32, y: f32) -> POINT {
    let (px, py) = crate::stream_target::map_normalized(x as f64, y as f64);
    POINT { x: px, y: py }
}

/// An owned `HSYNTHETICPOINTERDEVICE` (destroyed exactly once on drop).
struct Device(HSYNTHETICPOINTERDEVICE);

// SAFETY: the handle is a plain kernel object identifier with no thread affinity —
// `InjectSyntheticPointerInput`/`DestroySyntheticPointerDevice` are documented callable from
// any thread; ownership transfer/sharing does not alias memory.
unsafe impl Send for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the device this wrapper uniquely owns; destroyed once here.
        unsafe { DestroySyntheticPointerDevice(self.0) };
    }
}

/// Probe: can this Windows build create a synthetic pen device (Win10 1809+)?
pub fn synthetic_pen_available() -> bool {
    // SAFETY: FFI create with by-value args; on success the returned handle is destroyed
    // immediately by the `Device` wrapper, exactly once.
    match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
        Ok(h) => {
            drop(Device(h));
            true
        }
        Err(_) => false,
    }
}

#[derive(Default)]
struct PenState {
    in_range: bool,
    touching: bool,
    barrel: bool,
    eraser: bool,
    x: f32,
    y: f32,
    pressure: u16,
    tilt_deg: u8,
    azimuth_deg: u16,
    roll_deg: u16,
}

struct PenShared {
    dev: Device,
    state: PenState,
    /// First injection failure logs at WARN; repeats stay at trace.
    fail_warned: bool,
}

/// Per-session virtual pen (Linux uinput tablet sibling — same [`PenTransition`] API).
/// Wire barrel button 2 has no Windows equivalent (one barrel + eraser) and is ignored.
pub struct VirtualPen {
    shared: Arc<Mutex<PenShared>>,
    stop: Arc<AtomicBool>,
    refresher: Option<std::thread::JoinHandle<()>>,
    // Batch-local frame grouping (single-threaded within apply_batch).
    edge_down: bool,
    edge_up: bool,
    is_new: bool,
    frame_dirty: bool,
    frame_has_motion: bool,
}

impl VirtualPen {
    pub fn create() -> Result<VirtualPen> {
        // SAFETY: FFI create with by-value args; the handle's sole owner becomes `Device`.
        let dev = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
            .context("CreateSyntheticPointerDevice(PT_PEN) — needs Windows 10 1809+")?;
        let shared = Arc::new(Mutex::new(PenShared {
            dev: Device(dev),
            state: PenState::default(),
            fail_warned: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        // Re-assert the last frame while in range so a stationary pen never hovers out
        // (native plane: between 100 ms heartbeats; GameStream: indefinitely).
        let refresher = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("pf-pen-refresh".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS));
                        let s = &mut *shared.lock().unwrap();
                        if s.state.in_range {
                            inject_pen(s, POINTER_FLAG_UPDATE, false);
                        }
                    }
                })
                .context("spawn pen refresh thread")?
        };
        tracing::info!("virtual pen created (Windows synthetic pointer, PT_PEN)");
        Ok(VirtualPen {
            shared,
            stop,
            refresher: Some(refresher),
            edge_down: false,
            edge_up: false,
            is_new: false,
            frame_dirty: false,
            frame_has_motion: false,
        })
    }

    /// Apply one batch of tracker transitions — same grouping as the Linux backend:
    /// `[ProxIn, Motion, TipDown]` is ONE frame, consecutive `Motion`s split, tip
    /// edges own their frames, range-leave is a final no-`INRANGE` frame.
    pub fn apply_batch(&mut self, transitions: &[PenTransition]) {
        for t in transitions {
            match t {
                PenTransition::ProximityIn { tool } => {
                    self.flush();
                    let mut s = self.shared.lock().unwrap();
                    s.state.in_range = true;
                    s.state.eraser = *tool == PenTool::Eraser;
                    drop(s);
                    self.is_new = true;
                    self.frame_dirty = true;
                }
                PenTransition::Motion { sample } => {
                    if self.frame_has_motion {
                        self.flush();
                    }
                    self.set_axes(sample);
                    self.frame_dirty = true;
                    self.frame_has_motion = true;
                }
                PenTransition::TipDown => {
                    self.shared.lock().unwrap().state.touching = true;
                    self.edge_down = true;
                    self.frame_dirty = true;
                }
                PenTransition::ButtonsChanged { pressed, released } => {
                    let mut s = self.shared.lock().unwrap();
                    if pressed & PEN_BARREL1 != 0 {
                        s.state.barrel = true;
                    }
                    if released & PEN_BARREL1 != 0 {
                        s.state.barrel = false;
                    }
                    drop(s);
                    self.frame_dirty = true;
                }
                PenTransition::TipUp => {
                    self.shared.lock().unwrap().state.touching = false;
                    self.edge_up = true;
                    self.frame_dirty = true;
                    self.flush(); // UP owns its frame (still INRANGE)
                }
                PenTransition::ProximityOut => {
                    self.shared.lock().unwrap().state.in_range = false;
                    self.frame_dirty = true;
                    self.flush(); // final frame without INRANGE
                }
            }
        }
        self.flush();
    }

    fn set_axes(&mut self, s: &PenSample) {
        let mut sh = self.shared.lock().unwrap();
        sh.state.x = s.x;
        sh.state.y = s.y;
        sh.state.pressure = s.pressure;
        sh.state.tilt_deg = s.tilt_deg;
        sh.state.azimuth_deg = s.azimuth_deg;
        sh.state.roll_deg = s.roll_deg;
    }

    fn flush(&mut self) {
        if !self.frame_dirty {
            return;
        }
        let edge = if self.edge_down {
            POINTER_FLAG_DOWN
        } else if self.edge_up {
            POINTER_FLAG_UP
        } else {
            POINTER_FLAG_UPDATE
        };
        inject_pen(&mut self.shared.lock().unwrap(), edge, self.is_new);
        self.edge_down = false;
        self.edge_up = false;
        self.is_new = false;
        self.frame_dirty = false;
        self.frame_has_motion = false;
    }
}

impl Drop for VirtualPen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.refresher.take() {
            let _ = h.join();
        }
        // Device dies with `shared` — Windows releases held in-range/contact on destroy.
    }
}

/// Build + inject one pen frame from tracked state. `edge` is DOWN/UP/UPDATE;
/// `INRANGE`/`INCONTACT` derive from the state itself.
fn inject_pen(sh: &mut PenShared, edge: POINTER_FLAGS, is_new: bool) {
    let st = &sh.state;
    let mut flags = edge;
    if st.in_range {
        flags |= POINTER_FLAG_INRANGE;
    }
    if st.touching {
        flags |= POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
    }
    if is_new {
        flags |= POINTER_FLAG_NEW;
    }
    let mut pen_flags = PEN_FLAG_NONE;
    if st.barrel {
        pen_flags |= PEN_FLAG_BARREL;
    }
    if st.eraser {
        pen_flags |= PEN_FLAG_INVERTED;
        if st.touching {
            pen_flags |= PEN_FLAG_ERASER;
        }
    }
    let mut pen_mask = PEN_MASK_PRESSURE;
    // Contact needs a nonzero pressure to ink; hover reports 0 (Windows convention).
    let pressure = if st.touching {
        ((st.pressure as u32 * WIN_PEN_PRESSURE_MAX) / u16::MAX as u32).max(1)
    } else {
        0
    };
    let (mut tilt_x, mut tilt_y) = (0i32, 0i32);
    if st.tilt_deg != PEN_TILT_UNKNOWN && st.azimuth_deg != PEN_ANGLE_UNKNOWN {
        let az = (st.azimuth_deg as f32).to_radians();
        let tilt = st.tilt_deg as f32;
        tilt_x = (tilt * az.sin()).round() as i32;
        tilt_y = (-tilt * az.cos()).round() as i32;
        pen_mask |= PEN_MASK_TILT_X | PEN_MASK_TILT_Y;
    }
    let mut rotation = 0u32;
    if st.roll_deg != PEN_ANGLE_UNKNOWN {
        rotation = (st.roll_deg % 360) as u32;
        pen_mask |= PEN_MASK_ROTATION;
    }
    let pt = to_screen(st.x, st.y);
    let info = POINTER_TYPE_INFO {
        r#type: PT_PEN,
        Anonymous: POINTER_TYPE_INFO_0 {
            penInfo: POINTER_PEN_INFO {
                pointerInfo: POINTER_INFO {
                    pointerType: PT_PEN,
                    pointerId: 0,
                    pointerFlags: flags,
                    ptPixelLocation: pt,
                    ptPixelLocationRaw: pt,
                    ..Default::default()
                },
                penFlags: pen_flags,
                penMask: pen_mask,
                pressure,
                rotation,
                tiltX: tilt_x,
                tiltY: tilt_y,
            },
        },
    };
    // Best-effort like every injector write — a transient failure (desktop switch) is healed
    // by the next refresh tick re-asserting state.
    if let Err(e) = inject_following_desktop(&sh.dev, &[info]) {
        if !sh.fail_warned {
            sh.fail_warned = true;
            tracing::warn!(
                error = %e,
                flags = format!("{:#x}", flags.0),
                pen_flags = format!("{pen_flags:#x}"),
                pen_mask = format!("{pen_mask:#x}"),
                pressure,
                rotation,
                tilt_x,
                tilt_y,
                x = pt.x,
                y = pt.y,
                "pen inject failed"
            );
        } else {
            tracing::trace!(error = %e, "pen inject failed (transient)");
        }
    } else {
        sh.fail_warned = false;
    }
}

/// One live wire-touch contact. `slot` is the SMALL, DENSE pointer id handed to Windows —
/// synthetic-pointer injection rejects arbitrary large ids, and clients (Moonlight's
/// `pointerId` especially) send exactly those, so wire ids compact into the lowest free slot
/// for the contact's lifetime.
#[derive(Clone, Copy)]
struct Contact {
    id: u32,
    slot: u32,
    x: f32,
    y: f32,
}

fn free_slot(contacts: &[Contact]) -> u32 {
    let mut slot = 0u32;
    while contacts.iter().any(|c| c.slot == slot) {
        slot += 1;
    }
    slot
}

/// Windows can inject at most this many simultaneous synthetic touch contacts.
const MAX_CONTACTS: usize = 10;

struct TouchShared {
    dev: Device,
    contacts: Vec<Contact>,
    /// First injection failure logs at WARN; repeats stay at trace.
    fail_warned: bool,
}

/// `PT_TOUCH` for wire `TouchDown/Move/Up` — `SendInput` touch is a no-op. Contacts
/// keyed by wire finger id; every frame re-injects the FULL active set, and a refresh
/// thread re-asserts held contacts against the ~100 ms staleness auto-lift.
pub struct SyntheticTouch {
    shared: Arc<Mutex<TouchShared>>,
    stop: Arc<AtomicBool>,
    refresher: Option<std::thread::JoinHandle<()>>,
}

impl SyntheticTouch {
    pub fn create() -> Result<SyntheticTouch> {
        // SAFETY: FFI create with by-value args; the handle's sole owner becomes `Device`.
        let dev = unsafe {
            CreateSyntheticPointerDevice(PT_TOUCH, MAX_CONTACTS as u32, POINTER_FEEDBACK_DEFAULT)
        }
        .context("CreateSyntheticPointerDevice(PT_TOUCH) — needs Windows 10 1809+")?;
        let shared = Arc::new(Mutex::new(TouchShared {
            dev: Device(dev),
            contacts: Vec::new(),
            fail_warned: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let refresher = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("pf-touch-refresh".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS));
                        let s = &mut *shared.lock().unwrap();
                        if !s.contacts.is_empty() {
                            inject_touch_frame(s, None);
                        }
                    }
                })
                .context("spawn touch refresh thread")?
        };
        tracing::info!("virtual touchscreen created (Windows synthetic pointer, PT_TOUCH)");
        Ok(SyntheticTouch {
            shared,
            stop,
            refresher: Some(refresher),
        })
    }

    /// Apply one wire touch event (`code` = finger id, pixel x/y against the
    /// `flags = (w << 16) | h` reference extent, exactly like `MouseMoveAbs`).
    pub fn apply(&mut self, ev: &InputEvent) {
        let (w, h) = ((ev.flags >> 16) as f32, (ev.flags & 0xFFFF) as f32);
        if (w < 1.0 || h < 1.0) && ev.kind != InputKind::TouchUp {
            return; // the documented zero-extent drop, as for MouseMoveAbs
        }
        let (x, y) = (ev.x as f32 / w.max(1.0), ev.y as f32 / h.max(1.0));
        let s = &mut *self.shared.lock().unwrap();
        match ev.kind {
            InputKind::TouchDown => {
                match s.contacts.iter().position(|c| c.id == ev.code) {
                    Some(i) => (s.contacts[i].x, s.contacts[i].y) = (x, y),
                    None if s.contacts.len() < MAX_CONTACTS => {
                        let slot = free_slot(&s.contacts);
                        s.contacts.push(Contact {
                            id: ev.code,
                            slot,
                            x,
                            y,
                        })
                    }
                    None => return, // beyond the platform max — drop, never evict a live finger
                }
                inject_touch_frame(s, Some((ev.code, POINTER_FLAG_DOWN)));
            }
            InputKind::TouchMove => {
                match s.contacts.iter().position(|c| c.id == ev.code) {
                    Some(i) => {
                        (s.contacts[i].x, s.contacts[i].y) = (x, y);
                        inject_touch_frame(s, None);
                    }
                    // A move for an unknown id (its DOWN was dropped/lost): synthesize the
                    // contact so the stroke self-heals, like the pen plane does.
                    None if s.contacts.len() < MAX_CONTACTS => {
                        let slot = free_slot(&s.contacts);
                        s.contacts.push(Contact {
                            id: ev.code,
                            slot,
                            x,
                            y,
                        });
                        inject_touch_frame(s, Some((ev.code, POINTER_FLAG_DOWN)));
                    }
                    None => {}
                }
            }
            InputKind::TouchUp => {
                let Some(idx) = s.contacts.iter().position(|c| c.id == ev.code) else {
                    return;
                };
                // The UP frame still carries the lifting contact (with UP flags), then it
                // leaves the active set.
                inject_touch_frame(s, Some((ev.code, POINTER_FLAG_UP)));
                s.contacts.remove(idx);
            }
            _ => {}
        }
    }
}

impl Drop for SyntheticTouch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.refresher.take() {
            let _ = h.join();
        }
    }
}

/// Inject the full active-contact frame; `edge` marks one contact's DOWN/UP transition (by
/// WIRE id — everyone else is a held UPDATE). Windows sees the compacted `slot` ids only.
fn inject_touch_frame(sh: &mut TouchShared, edge: Option<(u32, POINTER_FLAGS)>) {
    let contacts = &sh.contacts;
    if contacts.is_empty() {
        return;
    }
    let mut frame: Vec<POINTER_TYPE_INFO> = Vec::with_capacity(contacts.len());
    for c in contacts {
        let mut flags = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE;
        if let Some((id, e)) = edge {
            if id == c.id {
                if e == POINTER_FLAG_DOWN {
                    flags = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN;
                } else if e == POINTER_FLAG_UP {
                    flags = POINTER_FLAG_UP; // contact + range end together
                }
            }
        }
        let pt = to_screen(c.x, c.y);
        frame.push(POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 {
                touchInfo: POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCH,
                        pointerId: c.slot,
                        pointerFlags: flags,
                        ptPixelLocation: pt,
                        ptPixelLocationRaw: pt,
                        ..Default::default()
                    },
                    touchFlags: TOUCH_FLAG_NONE,
                    touchMask: TOUCH_MASK_NONE,
                    ..Default::default()
                },
            },
        });
    }
    // Best-effort — a transient failure heals on the next event/refresh re-assertion.
    if let Err(e) = inject_following_desktop(&sh.dev, &frame) {
        if !sh.fail_warned {
            sh.fail_warned = true;
            tracing::warn!(error = %e, contacts = frame.len(), "touch inject failed");
        } else {
            tracing::trace!(error = %e, "touch inject failed (transient)");
        }
    } else {
        sh.fail_warned = false;
    }
}
