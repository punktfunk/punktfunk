//! Virtual DualSense on Windows via the UMDF minidriver (`packaging/windows/drivers/pf-gamepad`).
//!
//! Same [`DsState`] and report codec as the Linux UHID backend ([`super::dualsense`],
//! [`super::dualsense_proto`]). Transport is an unnamed `PadShm` DATA section reached over the
//! sealed channel ([`PadChannel`], `design/gamepad-channel-sealing.md`): the host duplicates the
//! section handle into the driver's WUDFHost through `Global\pfds-boot-<idx>`. hidclass owns the
//! device stack, so a UMDF minidriver has no control device — this IPC is the only channel
//! (`windows-dualsense-scoping.md`).
//!
//! Each pad `SwDeviceCreate`s a `pf_pad_<index>` software devnode (hwid `pf_dualsense`, enumerator
//! `punktfunk`) on open and `SwDeviceClose`s it on drop. The driver package must already be
//! installed.

use super::dualsense_proto::{
    parse_ds_output, serialize_state, DsFeedback, DsState, DsTriggers, DS_INPUT_REPORT_LEN,
    DS_TOUCH_H, DS_TOUCH_W,
};
use super::gamepad_raii::{sw_create_cb, PadChannel, SwCreateCtx};
use crate::sensor_clock::SensorClock;
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{anyhow, Result};
use punktfunk_core::quic::RichInput;
use std::ffi::c_void;
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::time::{Duration, Instant};
use windows::core::{w, GUID, PCWSTR};
use windows::Win32::Devices::Enumeration::Pnp::{
    SwDeviceClose, SwDeviceCreate, HSWDEVICE, SW_DEVICE_CREATE_INFO,
};
use windows::Win32::Foundation::{CloseHandle, E_FAIL, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Byte size of [`pf_driver_proto::gamepad::PadShm`]. Offsets and magic come from the same struct
/// so a layout change is a compile error; the driver maps that type too.
pub(super) const SHM_SIZE: usize = core::mem::size_of::<pf_driver_proto::gamepad::PadShm>();
pub(super) const SHM_MAGIC: u32 = pf_driver_proto::gamepad::PAD_MAGIC; // "PFDS"
pub(super) const OFF_INPUT: usize = core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, input);
pub(super) const OFF_OUT_SEQ: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, out_seq);
pub(super) const OFF_OUTPUT: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, output);
/// 0 DualSense (section is zeroed), 1 DualShock 4 — the driver picks HID identity from this.
pub(super) const OFF_DEVTYPE: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, device_type);
pub(super) const OFF_DRIVER_PROTO: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, driver_proto);
pub(super) const OFF_DRIVER_REV: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, driver_rev);

/// `(driver_proto, driver_rev)` from a pad section. The driver stamps the revision first and the
/// protocol with Release, so a revision read after a nonzero protocol is the driver's.
///
/// # Safety
/// `base` points at a live, mapped [`PadShm`].
pub(super) unsafe fn driver_marks(base: *mut u8) -> (u32, u32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    // SAFETY: the caller's contract; both offsets are 4-aligned fields inside the section.
    unsafe {
        let proto = (*(base.add(OFF_DRIVER_PROTO) as *const AtomicU32)).load(Ordering::Acquire);
        (
            proto,
            std::ptr::read_volatile(base.add(OFF_DRIVER_REV) as *const u32),
        )
    }
}
pub(super) const OFF_PAD_INDEX: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, pad_index);
pub(super) const DEVTYPE_DUALSHOCK4: u8 = pf_driver_proto::gamepad::DEVTYPE_DUALSHOCK4;
pub(super) const DEVTYPE_DUALSENSE_EDGE: u8 = pf_driver_proto::gamepad::DEVTYPE_DUALSENSE_EDGE;
pub(super) const OFF_OUT_RING_VER: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, out_ring_ver);
pub(super) const OFF_RING_HEAD: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, ring_head);
pub(super) const OFF_OUT_RING_LEN: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, out_ring_len);
pub(super) const OFF_OUT_RING: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, out_ring);
pub(super) const OUT_SLOT_SIZE: usize = core::mem::size_of::<pf_driver_proto::gamepad::OutSlot>();
pub(super) const OUT_RING_LEN: u32 = pf_driver_proto::gamepad::OUT_RING_LEN;
pub(super) const OUT_RING_LEN_V22: u32 = pf_driver_proto::gamepad::OUT_RING_LEN_V22;
/// v2.3 input seqlock — see [`publish_input`].
pub(super) const OFF_INPUT_GEN: usize =
    core::mem::offset_of!(pf_driver_proto::gamepad::PadShm, input_gen);

/// Publish one HID input report into the section's input slot under the v2.3 seqlock.
///
/// The slot is a single unqueued buffer. The driver's timer can copy 64 bytes out of it mid-write,
/// which for gyro is a spike a game will integrate as aim.
///
/// `generation` goes odd before the body and even after. The driver samples either side of its
/// read and retries on disagreement. The `Release` fence keeps body stores below the odd marker;
/// the `Release` store publishes them ahead of even. Both are no-ops on x86-TSO, load-bearing on ARM64.
///
/// # Safety
/// `base` must point at a live mapped pad section of at least `PAD_SHM_SIZE` bytes, and `report`
/// must be no longer than the 64-byte input slot.
pub(super) unsafe fn publish_input(base: *mut u8, generation: &mut u32, report: &[u8]) {
    debug_assert!(report.len() <= 64, "report overruns the input slot");
    // Odd: a report is in flight.
    *generation = generation.wrapping_add(1);
    // SAFETY: the caller guarantees `base` maps the section; `OFF_INPUT_GEN` is 4-aligned off the
    // page-aligned base and sits in the v2 legacy region every driver generation maps.
    unsafe {
        (*(base.add(OFF_INPUT_GEN) as *const AtomicU32)).store(*generation, Ordering::Relaxed)
    };
    // Ordered, not ordering: keeps the body stores below from being hoisted above the odd marker.
    fence(Ordering::Release);
    // SAFETY: the caller guarantees the mapping and that `report` fits the slot at OFF_INPUT.
    unsafe { std::ptr::copy_nonoverlapping(report.as_ptr(), base.add(OFF_INPUT), report.len()) };
    // Even: the slot holds a whole report again.
    *generation = generation.wrapping_add(1);
    // SAFETY: as the first store.
    unsafe {
        (*(base.add(OFF_INPUT_GEN) as *const AtomicU32)).store(*generation, Ordering::Release)
    };
}

/// Drain of a pad section's output plane: the lossless report ring when the driver publishes one
/// (8 slots on v2.1, [`OUT_RING_LEN_V22`] after both sides negotiate v2.2 — the driver's
/// `out_ring_len` echo decides), else the legacy latest-report slot. The ring is the only path
/// that cannot coalesce a rumble-STOP behind a following LED/trigger report inside one ~4 ms
/// poll (`design/rumble-root-fix.md`).
pub(super) struct OutputDrain {
    /// Driver `ring_head` value drained up to.
    tail: u32,
    /// Last `out_seq` consumed — single-slot path only.
    last_out_seq: u32,
    /// Latched on first ring activity; the legacy path never re-engages after it (the driver
    /// dual-writes both planes, so consuming both would double-parse every report).
    ring_live: bool,
}

impl OutputDrain {
    pub(super) fn new() -> OutputDrain {
        OutputDrain {
            tail: 0,
            last_out_seq: 0,
            ring_live: false,
        }
    }

    /// Drain every output report published since the last call, oldest → newest.
    ///
    /// `per_report` gets the slot bytes and a `feature` flag: bit 31 of the raw ring length is a
    /// Triton FEATURE set ([`pf_driver_proto::triton::out_is_feature`]).
    /// [`pf_driver_proto::triton::out_len`] masks that bit **before** the 64-byte clamp, so a
    /// tagged slot clamps on payload size, not `raw_len | 0x8000_0000`.
    ///
    /// Returns `true` on overflow (more than the negotiated length landed, or the driver lapped
    /// mid-copy): the pending window is discarded as possibly torn, the untagged latest-report slot
    /// is salvaged into one `per_report` call, and the caller must `PadFeedback::resync` planes that
    /// report did not carry. Overflow salvage and the pre-ring path both read that untagged slot, so
    /// `feature` is always `false` there — a FEATURE that lands on overflow or on an old driver
    /// replays as OUTPUT until the next ring-fed poll.
    pub(super) fn drain_tagged(
        &mut self,
        base: *mut u8,
        mut per_report: impl FnMut(&[u8], bool),
    ) -> bool {
        // SAFETY: base points at SHM_SIZE bytes; `OFF_RING_HEAD` is 4-aligned off the
        // page-aligned base. The driver bumps `ring_head` AFTER writing the slot, so an Acquire
        // load orders the slot copies below.
        let head =
            unsafe { (*(base.add(OFF_RING_HEAD) as *const AtomicU32)).load(Ordering::Acquire) };
        if self.ring_live || head != 0 {
            self.ring_live = true;
            if head == self.tail {
                return false;
            }
            // Driver's slot-math modulo (0 = pre-v2.2, hardcodes 8). Loaded after Acquire on
            // `ring_head`; restamped before every bump. Out-of-range clamps to v2.1 so offsets
            // stay inside the v2.2 ring.
            // SAFETY: `OFF_OUT_RING_LEN` is 4-aligned off the page-aligned base.
            let echo = unsafe {
                (*(base.add(OFF_OUT_RING_LEN) as *const AtomicU32)).load(Ordering::Relaxed)
            };
            let ring_len = if (1..=OUT_RING_LEN_V22).contains(&echo) {
                echo
            } else {
                OUT_RING_LEN
            };
            let pending = head.wrapping_sub(self.tail);
            if pending <= ring_len {
                // Copy slots first, then re-check head: a writer that lapped the window during
                // the copy may have overwritten what we read.
                let n = pending as usize;
                let mut bufs =
                    [([0u8; 64], 0usize, false); pf_driver_proto::gamepad::OUT_RING_LEN_V22_USIZE];
                for (k, buf) in bufs.iter_mut().enumerate().take(n) {
                    let idx = (self.tail.wrapping_add(k as u32) % ring_len) as usize;
                    let slot = OFF_OUT_RING + idx * OUT_SLOT_SIZE;
                    // SAFETY: slot .. slot+OUT_SLOT_SIZE is inside the SHM_SIZE section (idx <
                    // `ring_len` ≤ OUT_RING_LEN_V22, whose last slot ends at 4064 ≤ SHM_SIZE);
                    // the len field is 4-aligned (`OFF_OUT_RING` == 256, `OUT_SLOT_SIZE` == 68).
                    let raw_len = unsafe { std::ptr::read_unaligned(base.add(slot) as *const u32) };
                    buf.2 = pf_driver_proto::triton::out_is_feature(raw_len);
                    buf.1 = (pf_driver_proto::triton::out_len(raw_len) as usize).min(64);
                    // SAFETY: the slot's data region is slot+4 .. slot+4+64, inside the section;
                    // `buf.0` is a live local 64-byte array.
                    unsafe {
                        std::ptr::copy_nonoverlapping(base.add(slot + 4), buf.0.as_mut_ptr(), buf.1)
                    };
                }
                // SAFETY: as the first `ring_head` load above.
                let head2 = unsafe {
                    (*(base.add(OFF_RING_HEAD) as *const AtomicU32)).load(Ordering::Acquire)
                };
                if head2.wrapping_sub(self.tail) <= ring_len {
                    for (data, len, feature) in bufs.iter().take(n) {
                        if *len > 0 {
                            per_report(&data[..*len], *feature);
                        }
                    }
                    self.tail = head;
                    return false;
                }
            }
            // Overflow or lapped mid-copy: skip to the freshest head and salvage the untagged
            // latest-report slot (driver dual-publishes every report there). No seqlock; parser
            // gates drop most tears, caller resync silences planes the salvage does not assert.
            // SAFETY: as the first `ring_head` load above.
            self.tail =
                unsafe { (*(base.add(OFF_RING_HEAD) as *const AtomicU32)).load(Ordering::Acquire) };
            let mut out = [0u8; 64];
            // SAFETY: the legacy output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe { std::ptr::copy_nonoverlapping(base.add(OFF_OUTPUT), out.as_mut_ptr(), 64) };
            per_report(&out, false);
            return true;
        }
        // Pre-ring driver: latest-report slot + seq, coalescing. No feature tag on this slot.
        // SAFETY: `OFF_OUT_SEQ` is 4-aligned off the page-aligned base; Acquire pairs with the
        // driver's publish-then-bump store order.
        let seq = unsafe { (*(base.add(OFF_OUT_SEQ) as *const AtomicU32)).load(Ordering::Acquire) };
        if seq != self.last_out_seq {
            self.last_out_seq = seq;
            let mut out = [0u8; 64];
            // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe { std::ptr::copy_nonoverlapping(base.add(OFF_OUTPUT), out.as_mut_ptr(), 64) };
            per_report(&out, false);
        }
        false
    }

    /// Drop the Triton feature flag for DualSense/DS4/Edge/Deck callers.
    pub(super) fn drain(&mut self, base: *mut u8, mut per_report: impl FnMut(&[u8])) -> bool {
        self.drain_tagged(base, |b, _| per_report(b))
    }
}

/// One virtual DualSense: a `SwDeviceCreate`'d `pf_pad_<index>` software devnode plus the sealed
/// shared-memory channel. Drop removes the devnode (`SwDeviceClose`) and closes both sections.
/// Public because it is `PadProto::Pad`.
pub struct DsWinPad {
    /// `None` falls back to an out-of-band `pf_dualsense` devnode (installer/devgen).
    _sw: Option<super::gamepad_raii::SwDevice>,
    channel: PadChannel,
    attach: super::gamepad_raii::DriverAttach,
    seq: u8,
    clock: SensorClock,
    /// v2.3 input-seqlock generation — see [`publish_input`].
    input_gen: u32,
    drain: OutputDrain,
    triggers: DsTriggers,
}

/// PnP identity for a virtual controller devnode, so one [`create_swdevice`] builds DualSense or
/// DualShock 4 (and the Deck / Triton / Xbox siblings).
pub(super) struct SwDeviceProfile<'a> {
    /// Distinct namespaces per type (`pf_pad_<idx>` vs `pf_ds4_<idx>`) so the two never reuse a
    /// devnode shell.
    pub instance: &'a str,
    /// `Data1` of the ContainerId — a per-family tag (`"PFDS"` pads, `"PFMO"` mouse) so two
    /// families at the same index never share a container (Windows would group them as one device).
    pub container_tag: u32,
    /// Also stamped into the devnode Location, which the driver reads as its bootstrap-mailbox index.
    pub container_index: u8,
    /// INF-matched hardware id, listed first so the INF binds.
    pub hwid: &'a str,
    pub usb_vid_pid: &'a str,
    /// Appended as `&MI_xx` on the USB hardware ids. hidclass mirrors the parent's `USB\VID…`
    /// tokens into the HID child; hidapi/SDL/Steam parse `MI_` as `bInterfaceNumber` (0 if absent).
    /// The Steam Deck controller lives on interface 2.
    pub usb_mi: Option<u8>,
    pub description: &'a str,
    /// The `SWD\<enumerator>\<instance>` namespace. hidclass names the HID child after it, so a
    /// pad Steam must recognise carries its VID/PID here (`VID_054C&PID_0CE6&MI_03`,
    /// `VID_045E&PID_0B13`): Steam merges a pad's views by that token in the path, and under
    /// `punktfunk` it listed the same pad twice.
    pub enumerator: &'a str,
}

/// Spawn the per-session virtual controller devnode under `p.enumerator`.
/// The returned `HSWDEVICE` owns it — `SwDeviceClose` removes it on drop.
///
/// Game detection (`design/windows-dualsense-game-detection.md`): `HIDD_ATTRIBUTES` VID/PID
/// satisfies SDL/HIDAPI/RawInput, but a native PS5 path classifies connection type by walking
/// to the parent and matching `"USB"`/`"BTHENUM"` in `DEVPKEY_Device_CompatibleIds`. Set these
/// via `SW_DEVICE_CREATE_INFO` only — a later `DEVPROPERTY` write of bus/identity keys is ignored:
/// - `pszzCompatibleIds` starts with a `USB\` token so the parent walk resolves USB.
/// - `pszzHardwareIds` lists the INF id first, then `USB\VID_…[&REV_0100]`, so hidclass derives
///   `HID\VID_…` child ids a genuine USB DualSense exposes.
/// - a deterministic per-pad `pContainerId` (the null sentinel trips an `xinput1_4` slot skip).
///
/// Enumerator names must not contain `_` (`punktfunk`, not `pf_dualsense`) and `pCallback` is
/// mandatory — either yields `E_INVALIDARG`. The caller must be Administrator (the host runs as
/// LocalSystem).
pub(super) fn create_swdevice(p: &SwDeviceProfile) -> Result<(HSWDEVICE, Option<String>)> {
    let multi_sz = |ids: &[&str]| -> Vec<u16> {
        ids.iter()
            .flat_map(|s| s.encode_utf16().chain(std::iter::once(0)))
            .chain(std::iter::once(0))
            .collect()
    };
    let mi = p.usb_mi.map(|n| format!("&MI_{n:02}")).unwrap_or_default();
    let usb_rev = format!("USB\\{}&REV_0100{mi}", p.usb_vid_pid);
    let usb = format!("USB\\{}{mi}", p.usb_vid_pid);
    let hwids = multi_sz(&[
        p.hwid, // FIRST → the INF binds our UMDF driver on this id
        usb_rev.as_str(),
        usb.as_str(),
    ]);
    let compat = multi_sz(&[
        usb.as_str(), // a `USB\` token → native bus-type detection resolves USB
        "USB\\Class_03&SubClass_00&Prot_00",
        "USB\\Class_03",
    ]);
    let instid: Vec<u16> = p
        .instance
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let desc: Vec<u16> = p
        .description
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // Pad index in Location — the driver polls `pfds-boot-<index>`. The buffer outlives
    // SwDeviceCreate (we wait on the event before return).
    let loc: Vec<u16> = format!("{}", p.container_index)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let container = GUID::from_values(
        p.container_tag,
        0x0000,
        0x0000,
        [0, 0, 0, 0, 0, 0, 0, p.container_index],
    );

    // SAFETY: zeroed then the fields we use are set; cbSize identifies the struct version. The id
    // buffers and `container` outlive SwDeviceCreate (we wait on the event before return).
    let mut info: SW_DEVICE_CREATE_INFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SW_DEVICE_CREATE_INFO>() as u32;
    info.pszInstanceId = PCWSTR(instid.as_ptr());
    info.pszzHardwareIds = PCWSTR(hwids.as_ptr());
    info.pszzCompatibleIds = PCWSTR(compat.as_ptr());
    info.pContainerId = &container;
    info.pszDeviceDescription = PCWSTR(desc.as_ptr());
    info.pszDeviceLocation = PCWSTR(loc.as_ptr());
    info.CapabilityFlags = 0x0000_000B; // DriverRequired | SilentInstall | Removable

    // SAFETY: a manual-reset, initially-unsignaled, unnamed event.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null())? };
    // `result` starts as E_FAIL: a timeout must not read a zeroed HRESULT as success.
    // Heap-allocated: `sw_create_cb` writes through this pointer then `SetEvent`s. The wait is
    // 10 s; on a wedged-PnP timeout the callback may still be pending, so we leak the box and
    // leave the event open rather than let a late write hit recycled stack/handle.
    let ctx = Box::into_raw(Box::new(SwCreateCtx {
        event,
        result: E_FAIL,
        instance_id: [0; 128],
    }));
    let enumerator: Vec<u16> = p
        .enumerator
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: info + the buffers outlive the call; `ctx` is a live heap allocation that outlives
    // every path below (reclaimed only where the callback provably ran). windows-rs returns the
    // HSWDEVICE (the C out-param) as the Result value.
    let hsw = match unsafe {
        SwDeviceCreate(
            PCWSTR(enumerator.as_ptr()),
            w!("HTREE\\ROOT\\0"),
            &info,
            None,
            Some(sw_create_cb),
            Some(ctx as *const c_void),
        )
    } {
        Ok(h) => h,
        Err(e) => {
            // SAFETY: the call failed, so no callback was registered and `ctx` is ours to reclaim;
            // `event` is valid and unreferenced.
            unsafe {
                drop(Box::from_raw(ctx));
                let _ = CloseHandle(event);
            }
            return Err(anyhow!("SwDeviceCreate failed: {e}"));
        }
    };
    // SAFETY: event is valid.
    let wait = unsafe { WaitForSingleObject(event, 10_000) };
    if wait != WAIT_OBJECT_0 {
        // Timed out: leak `ctx` and leave `event` open so a late callback writes live memory.
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate enumeration callback never fired (10s) — PnP may be wedged"
        ));
    }
    // SAFETY: the callback signalled the event, so nothing else will touch `ctx`/`event`.
    // `ctx` came from `Box::into_raw` above and is reclaimed exactly once here; `event` is
    // valid and no longer referenced by a pending callback.
    let ctx = unsafe {
        let _ = CloseHandle(event);
        Box::from_raw(ctx)
    };
    if ctx.result.is_err() {
        // SAFETY: hsw is the handle SwDeviceCreate returned.
        unsafe { SwDeviceClose(hsw) };
        return Err(anyhow!(
            "SwDeviceCreate enumeration failed: {:?}",
            ctx.result
        ));
    }
    Ok((hsw, ctx.instance_id()))
}

/// Identity a [`DsWinPad`] enumerates with. DualSense and Edge share the transport and report
/// codec; only `device_type` and PnP identity differ. DS4 differs in report codec too, so it
/// keeps its own pad type.
pub(super) struct WinDsIdentity {
    /// Stamped into the section; the driver picks its HID identity off it.
    pub devtype: u8,
    /// Distinct namespaces per type (`pf_pad` / `pf_edge`).
    pub instance_prefix: &'static str,
    pub hwid: &'static str,
    pub usb_vid_pid: &'static str,
    pub description: &'static str,
    /// See [`SwDeviceProfile::enumerator`].
    pub enumerator: &'static str,
}

impl WinDsIdentity {
    pub(super) const fn dualsense() -> WinDsIdentity {
        WinDsIdentity {
            devtype: 0,
            instance_prefix: "pf_pad",
            // Hardware id, not the package name. The INF still matches `pf_dualsense`; renaming
            // this to `pf_gamepad` binds inbox `input.inf`/`HidUsb`, which cannot start on a
            // software-enumerated devnode. `hwid_matches_inf` pins it.
            hwid: "pf_dualsense",
            usb_vid_pid: "VID_054C&PID_0CE6",
            description: "Punktfunk Virtual DualSense",
            enumerator: "VID_054C&PID_0CE6&MI_03",
        }
    }

    pub(super) const fn dualsense_edge() -> WinDsIdentity {
        WinDsIdentity {
            devtype: DEVTYPE_DUALSENSE_EDGE,
            instance_prefix: "pf_edge",
            hwid: "pf_dualsenseedge",
            usb_vid_pid: "VID_054C&PID_0DF2",
            description: "Punktfunk Virtual DualSense Edge",
            enumerator: "VID_054C&PID_0DF2&MI_03",
        }
    }
}

impl DsWinPad {
    /// Create the sealed channel, stamp device type (visible the moment magic is) then pad index
    /// then a neutral report then magic last, then spawn the devnode. Drop removes it.
    pub(super) fn open(index: u8, id: &WinDsIdentity) -> Result<DsWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = id.devtype;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // Stamped before magic so the driver sees it on attach. `2` = host drains the v2.2
            // long ring; a v2.1 driver treats it as boolean and stays on 8-slot math. Drain
            // follows the driver's `out_ring_len` echo.
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; DS_INPUT_REPORT_LEN], {
                let mut r = [0u8; DS_INPUT_REPORT_LEN];
                serialize_state(&mut r, &DsState::neutral(), 0, 0);
                r
            });
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("{}_{index}", id.instance_prefix);
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: id.hwid,
            usb_vid_pid: id.usb_vid_pid,
            // Composite USB devices: audio on interfaces 0-2, HID on 3. hidapi reads it back.
            usb_mi: Some(3),
            description: id.description,
            enumerator: id.enumerator,
        })?; // `?`: a swallowed fail latched a pad with no devnode; PadSlots never retried.
        let (hsw, instance_id) = (Some(hsw), instance_id);
        // Duplicate into the process this devnode is serving, not the pid the LocalService-writable
        // mailbox names.
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            super::gamepad_raii::ProofTransport::HidFeatureReport,
        );
        let _sw = hsw.map(super::gamepad_raii::SwDevice::new);
        // Driver must hold the DATA section (and can read `device_type`) before hidclass asks
        // for descriptors.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(DsWinPad {
            _sw,
            channel,
            attach: super::gamepad_raii::DriverAttach::new(
                id.hwid,
                "pf_gamepad.inf", // one driver package serves every PS identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
            clock: SensorClock::dualsense(),
            input_gen: 0,
            drain: OutputDrain::new(),
            triggers: DsTriggers::default(),
        })
    }

    pub(super) fn write_state(&mut self, st: &DsState) {
        self.seq = self.seq.wrapping_add(1);
        let ts = self.clock.ds_ticks(Instant::now());
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.seq, ts);
        self.triggers.stamp(&mut r, st.l2, st.r2);
        // No driver-polled change-detect on this plane; the timer copies the whole slot. Seqlock:
        // see `publish_input`.
        // SAFETY: `data_base()` points at a live PAD_SHM_SIZE-byte section and `r` is the 64-byte
        // input report.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    /// Drain the output plane oldest → newest so a stop-then-LED burst yields both, never just
    /// the latest report. Also ticks channel delivery and the attach watcher.
    pub(super) fn service(&mut self, pad: u8) -> DsFeedback {
        self.channel.pump();
        let mut fb = DsFeedback::default();
        // SAFETY: the channel's section is live and SHM_SIZE bytes.
        let (proto, rev) = unsafe { driver_marks(self.channel.data_base()) };
        self.attach.observe_pad(proto, rev);
        let base = self.channel.data_base();
        fb.resync = self
            .drain
            .drain(base, |bytes| parse_ds_output(pad, bytes, &mut fb));
        self.triggers.observe(&fb.hidout);
        fb
    }
}

/// Windows DualSense [`PadProto`]: sealed-channel open, the same [`DsState`] mappers as
/// `linux/dualsense.rs`, section feedback poll. Lifecycle lives in [`UhidManager`].
pub struct DsWinProto {
    /// Steam back grips have no DualSense HID slot. `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for DsWinProto {
    fn default() -> DsWinProto {
        DsWinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for DsWinProto {
    type Pad = DsWinPad;
    type State = DsState;
    const LABEL: &'static str = "DualSense/Windows";
    const DEVICE: &'static str = "DualSense";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DsWinPad> {
        let p = DsWinPad::open(idx, &WinDsIdentity::dualsense())?;
        tracing::info!(
            index = idx,
            "virtual DualSense created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Preserve touch + motion + pad clicks across a button-only frame, as `linux/dualsense.rs`.
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        let mut s = DsState::from_gamepad(
            buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.touch = prev.touch;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.touch_click = prev.touch_click;
        s
    }

    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
    }

    fn neutralize_gyro(&self, st: &mut DsState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut DsState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut DsWinPad, st: &DsState) {
        pad.write_state(st);
    }

    fn service(&self, pad: &mut DsWinPad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            // Only a report that asserted vibration counts — an LED/trigger stream must not feed
            // the abandoned-rumble force-off clock.
            rumble_drove: Some(fb.rumble.is_some()),
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb.hidout,
            resync: fb.resync,
        }
    }
}

/// Hold a software-devnode HID Steam Deck (`device_type = 3`, `VID_28DE&PID_1205`) for `secs`,
/// streaming the neutral Deck frame. Wired to `deck-windows-spike`; never used by a session.
/// Watch Steam's `logs/controller.txt` / controller settings: does Steam Input promote a
/// software-devnode HID Deck, or does it require a real USB bus identity?
pub fn deck_spike_hold(index: u8, secs: u64) -> Result<()> {
    let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
    let mut channel = PadChannel::create(boot_name, SHM_SIZE)?;
    let base = channel.data_base();
    let neutral = super::steam_proto::neutral_deck_report();
    // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range. Device-type
    // FIRST, magic LAST — the same publish order the session pads use.
    unsafe {
        *base.add(OFF_DEVTYPE) = pf_driver_proto::gamepad::DEVTYPE_STEAMDECK;
        std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
        std::ptr::write_unaligned(base.add(OFF_INPUT) as *mut [u8; 64], neutral);
        std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
    }
    let inst = format!("pf_deckspike_{index}");
    let (hsw, spike_instance_id) = create_swdevice(&SwDeviceProfile {
        instance: &inst,
        container_tag: 0x5046_4453, // "PFDS"
        container_index: index,
        hwid: super::steam_deck_windows::DECK_HWID,
        usb_vid_pid: "VID_28DE&PID_1205",
        // hidapi parses MI_ from the child hwids; absent = interface 0, Steam wants 2.
        usb_mi: Some(2),
        description: "Punktfunk Virtual Steam Deck (spike)",
        enumerator: "punktfunk",
    })?;
    // Same devnode-proved delivery as a session pad — a bring-up tool must not fall back
    // to the mailbox.
    channel.bind_devnode(
        index as u32,
        spike_instance_id,
        super::gamepad_raii::ProofTransport::HidFeatureReport,
    );
    let _sw = super::gamepad_raii::SwDevice::new(hsw);
    channel.deliver_eager(std::time::Duration::from_millis(1500));
    println!(
        "virtual Steam Deck devnode up (28DE:1205, device_type 3) — holding {secs}s.\n\
         Observe: Get-PnpDevice -PresentOnly | findstr 1205; Steam logs\\controller.txt for a\n\
         detect/promote line; Steam Settings > Controller for a 'Steam Deck' entry.\n\
         GO = Steam lists/promotes it; NO-GO = it never appears (the Linux `Interface: -1` gap\n\
         applies verbatim — document and keep the SteamDeck->DualSense Windows fold)."
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last_out_seq = 0u32;
    while std::time::Instant::now() < deadline {
        channel.pump();
        // SAFETY: base points at SHM_SIZE bytes; OFF_OUT_SEQ is in range.
        let seq =
            unsafe { std::ptr::read_unaligned(channel.data_base().add(OFF_OUT_SEQ) as *const u32) };
        if seq != last_out_seq {
            last_out_seq = seq;
            let mut out = [0u8; 16];
            // SAFETY: output slot is OFF_OUTPUT..OFF_OUTPUT+64 within the section.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    channel.data_base().add(OFF_OUTPUT),
                    out.as_mut_ptr(),
                    16,
                )
            };
            println!("  output report from a client (Steam?): {out:02x?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("deck-windows-spike: done (devnode removed on exit)");
    Ok(())
}

/// Session DualSense pads — Windows analogue of
/// [`DualSenseManager`](super::dualsense::DualSenseManager). Heartbeat keeps the section fresh
/// (the driver's timer streams whatever is in it).
pub type DualSenseWindowsManager = UhidManager<DsWinProto>;

#[cfg(test)]
mod drain_tests {
    use super::*;

    fn section() -> Vec<u32> {
        vec![0u32; SHM_SIZE / 4]
    }

    fn base(buf: &mut [u32]) -> *mut u8 {
        buf.as_mut_ptr() as *mut u8
    }

    /// v2.1 dual write: legacy slot + seq, then ring slot (8-slot math, no length echo), then head.
    fn publish(buf: &mut [u32], bytes: &[u8]) {
        ring_publish(buf, bytes, OUT_RING_LEN, false);
    }

    /// v2.1 dual write with `OUT_FEATURE_BIT` ORed into the slot length — the tag `drain_tagged`
    /// must strip and surface as `feature`.
    fn publish_tagged(buf: &mut [u32], bytes: &[u8]) {
        legacy_publish(buf, bytes);
        let head = read32(buf, OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % OUT_RING_LEN) as usize * OUT_SLOT_SIZE;
        write32(
            buf,
            slot,
            bytes.len() as u32 | pf_driver_proto::triton::OUT_FEATURE_BIT,
        );
        let b = bytes_mut(buf);
        b[slot + 4..slot + 4 + bytes.len()].copy_from_slice(bytes);
        write32(buf, OFF_RING_HEAD, head.wrapping_add(1));
    }

    /// v2.2 dual write: long-ring slot math, `out_ring_len` echo stamped before the head bump.
    fn v22_publish(buf: &mut [u32], bytes: &[u8]) {
        ring_publish(buf, bytes, OUT_RING_LEN_V22, true);
    }

    fn ring_publish(buf: &mut [u32], bytes: &[u8], len: u32, echo: bool) {
        legacy_publish(buf, bytes);
        let head = read32(buf, OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % len) as usize * OUT_SLOT_SIZE;
        write32(buf, slot, bytes.len() as u32);
        let b = bytes_mut(buf);
        b[slot + 4..slot + 4 + bytes.len()].copy_from_slice(bytes);
        if echo {
            write32(buf, OFF_OUT_RING_LEN, len);
        }
        write32(buf, OFF_RING_HEAD, head.wrapping_add(1));
    }

    /// Pre-ring driver: latest-report slot + seq only.
    fn legacy_publish(buf: &mut [u32], bytes: &[u8]) {
        let b = bytes_mut(buf);
        b[OFF_OUTPUT..OFF_OUTPUT + bytes.len()].copy_from_slice(bytes);
        let seq = read32(buf, OFF_OUT_SEQ).wrapping_add(1);
        write32(buf, OFF_OUT_SEQ, seq);
    }

    /// Byte view of the source slice's allocation, including short test buffers.
    fn bytes_mut(buf: &mut [u32]) -> &mut [u8] {
        let byte_len = buf
            .len()
            .checked_mul(size_of::<u32>())
            .expect("u32 slice byte length overflow");
        // SAFETY: `byte_len` is exactly the source slice's allocation range; u8 needs less alignment.
        unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), byte_len) }
    }

    #[test]
    fn byte_view_stays_within_the_source_slice() {
        let mut buf = [0u32; 2];
        assert_eq!(bytes_mut(&mut buf).len(), 2 * size_of::<u32>());
    }

    fn read32(buf: &mut [u32], off: usize) -> u32 {
        u32::from_ne_bytes(bytes_mut(buf)[off..off + 4].try_into().unwrap())
    }

    fn write32(buf: &mut [u32], off: usize, v: u32) {
        bytes_mut(buf)[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }

    fn collect(d: &mut OutputDrain, buf: &mut [u32]) -> (Vec<Vec<u8>>, bool) {
        let mut got = Vec::new();
        let resync = d.drain(base(buf), |b| got.push(b.to_vec()));
        (got, resync)
    }

    /// Bit 31 of ring `len` is FEATURE; the tagged drain must strip it from the length and surface
    /// it as a flag. Untagged slots must come through with `feature == false`.
    #[test]
    fn tagged_drain_separates_feature_frames_from_output_frames() {
        let mut buf = section();
        publish(&mut buf, &[0x80, 0x00, 0xFF]);
        publish_tagged(&mut buf, &[0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        let mut got = Vec::new();
        let mut d = OutputDrain::new();
        d.drain_tagged(base(&mut buf), |bytes, feature| {
            got.push((bytes.to_vec(), feature));
        });
        assert_eq!(got[0], (vec![0x80, 0x00, 0xFF], false));
        assert_eq!(got[1].0, vec![0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        assert!(got[1].1);
    }

    /// A rumble-stop then an LED-only report in one poll must yield both, oldest first
    /// (`design/rumble-root-fix.md`). On the legacy single slot the stop is overwritten.
    #[test]
    fn ring_preserves_a_stop_followed_by_an_led_report() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        publish(&mut buf, &[0x02, 0x03, 0, 0xFF, 0xFF]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 0x03, 0, 0xFF, 0xFF]]);

        publish(&mut buf, &[0x02, 0x03, 0, 0, 0]);
        publish(&mut buf, &[0x02, 0, 0x04, 0, 0]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got,
            vec![vec![0x02, 0x03, 0, 0, 0], vec![0x02, 0, 0x04, 0, 0]],
            "the stop report must survive the burst, oldest first"
        );
        assert_eq!(collect(&mut d, &mut buf).0.len(), 0);
    }

    #[test]
    fn ring_wraps_across_polls() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..6u8 {
            publish(&mut buf, &[0x02, i]);
        }
        assert_eq!(collect(&mut d, &mut buf).0.len(), 6);
        for i in 6..12u8 {
            // 12 wraps past the 8-slot v2.1 ring
            publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            vec![6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn overflow_salvages_the_latest_slot_and_flags_resync_then_recovers() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..12u8 {
            // 12 > OUT_RING_LEN pending — the oldest 4 were overwritten in-ring
            publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(resync, "an overflowed window must be reported");
        assert_eq!(
            got.len(),
            1,
            "the possibly-torn ring window must not be parsed — only the legacy latest slot"
        );
        assert_eq!(
            &got[0][..2],
            &[0x02, 11],
            "the salvage must be the freshest coalesced state, not silence"
        );
        publish(&mut buf, &[0x02, 99]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 99]]);
    }

    /// 40 pending fits in 56 slots and overflows every poll against the 8-slot ring.
    #[test]
    fn v22_ring_absorbs_a_burst_the_v21_ring_could_not() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..40u8 {
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync, "40 pending ≤ 56 slots — no overflow");
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            (0..40).collect::<Vec<_>>()
        );
    }

    #[test]
    fn v22_ring_wraps_across_polls() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..50u8 {
            v22_publish(&mut buf, &[0x02, i]);
        }
        assert_eq!(collect(&mut d, &mut buf).0.len(), 50);
        for i in 50..100u8 {
            // 100 wraps past the 56-slot v2.2 ring
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            (50..100).collect::<Vec<_>>()
        );
    }

    #[test]
    fn v22_overflow_still_salvages_and_recovers() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..60u8 {
            // 60 > OUT_RING_LEN_V22 pending
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(resync);
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0][..2], &[0x02, 59]);
        v22_publish(&mut buf, &[0x02, 99]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 99]]);
    }

    /// Torn or hostile `out_ring_len` must clamp to the v2.1 length, not index past the ring.
    #[test]
    fn garbage_length_echo_clamps_to_the_v21_length() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        publish(&mut buf, &[0x02, 1]); // 8-slot math, matching the clamp fallback
        write32(&mut buf, OFF_OUT_RING_LEN, 9999);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 1]]);
    }

    /// Every hwid the host puts on a pad devnode must be one the shipped INF declares. Otherwise
    /// PnP falls through to the synthesized USB ids and binds inbox `input.inf`/`HidUsb`, which
    /// cannot start on a software-enumerated devnode. Hardware ids outlive package renames.
    #[test]
    fn hwid_matches_inf() {
        let inx = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/windows/drivers/pf-gamepad/pf_gamepad.inx"
        );
        let inf = std::fs::read_to_string(inx).expect("read pf_gamepad.inx");
        // Match the install section by prefix, not `pfGamepad,` — Xbox installs `pfGamepadXbox`
        // (xinputhid filter) and PlayStation/Deck must not. An exact match went vacuous at that
        // split.
        let declared: Vec<String> = inf
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with(';'))
            .filter_map(|l| l.split_once('='))
            .filter(|(_, rhs)| rhs.trim_start().starts_with("pfGamepad"))
            .flat_map(|(_, rhs)| {
                // `pfGamepad[Suffix], <hwid>[, <hwid>…]` — drop the section name. `AddReg=` lines
                // have no comma and contribute nothing.
                rhs.split(',')
                    .skip(1)
                    .map(|id| id.trim().to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            declared.len() >= 4,
            "parsed {} hardware ids out of {inx} — the [Models] shape changed and this test went \
             vacuous; fix the parse rather than deleting the assert",
            declared.len()
        );
        for hwid in [
            WinDsIdentity::dualsense().hwid,
            WinDsIdentity::dualsense_edge().hwid,
            super::super::dualshock4_windows::DS4_HWID,
            super::super::steam_deck_windows::DECK_HWID,
            super::super::triton_windows::TRITON_HWID,
        ]
        .into_iter()
        // Every Xbox identity, not just the first — a new one without its INF model line never starts.
        .chain(
            super::super::xbox_windows::XBOX_IDENTITIES
                .iter()
                .map(|i| i.hwid),
        )
        // The unfiltered Xbox line the host falls back to where `xinputhid` is not a service.
        .chain(std::iter::once(
            super::super::xbox_windows::XBOX_UNFILTERED_HWID,
        )) {
            let want = hwid.to_ascii_lowercase();
            let rooted = format!("root\\{want}");
            assert!(
                declared
                    .iter()
                    .any(|d| d.as_str() == want || d.as_str() == rooted),
                "the host creates pad devnodes with hardware id {hwid:?}, which pf_gamepad.inx \
                 does not declare (it has {declared:?}) — PnP would bind inbox input.inf/HidUsb \
                 instead and the pad would never start"
            );
        }
    }

    /// Xbox identities install `pfGamepadXbox` (xinputhid upper filter + `BusDevice`); PlayStation
    /// and Deck must not. That filter claims the HID collection exclusively — a DualSense handed
    /// to Microsoft's Xbox translator disappears from Steam and SDL. A new Xbox identity pasted
    /// onto `pfGamepad` enumerates and is never promoted.
    #[test]
    fn only_the_xbox_identity_installs_the_xinputhid_section() {
        let inx = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packaging/windows/drivers/pf-gamepad/pf_gamepad.inx"
        );
        let inf = std::fs::read_to_string(inx).expect("read pf_gamepad.inx");
        let xbox: Vec<String> = super::super::xbox_windows::XBOX_IDENTITIES
            .iter()
            .map(|i| i.hwid.to_ascii_lowercase())
            .collect();

        let mut seen: Vec<&str> = Vec::new();
        for line in inf.lines().map(str::trim).filter(|l| !l.starts_with(';')) {
            let Some((_, rhs)) = line.split_once('=') else {
                continue;
            };
            let rhs = rhs.trim_start();
            let Some((section, ids)) = rhs.split_once(',') else {
                continue;
            };
            if !section.starts_with("pfGamepad") {
                continue;
            }
            let ids: Vec<String> = ids
                .split(',')
                .map(|i| i.trim().to_ascii_lowercase())
                .collect();
            // `contains`, not `==`: model lines carry the bare id and its `root\` twin.
            let matched: Vec<&str> = xbox
                .iter()
                .filter(|x| ids.iter().any(|i| i.contains(x.as_str())))
                .map(String::as_str)
                .collect();
            if matched.is_empty() {
                assert_eq!(
                    section, "pfGamepad",
                    "a non-Xbox model line ({ids:?}) installs {section:?}; if that section carries \
                     the xinputhid filter, this pad is about to be handed to Microsoft's Xbox \
                     translator"
                );
            } else {
                seen.extend(matched);
                assert_ne!(
                    section, "pfGamepad",
                    "an Xbox model line ({ids:?}) installs the SHARED section, so either the \
                     xinputhid filter would be attached to every PlayStation and Deck pad too, or \
                     this Xbox pad silently never gets promoted"
                );
            }
        }
        for want in &xbox {
            assert!(
                seen.contains(&want.as_str()),
                "no [Models] line mentions {want:?} — either the identity has no INF line at all, \
                 or the parse went vacuous; fix that rather than deleting the assert"
            );
        }
    }

    /// The driver picks HID identity from the hardware id at `EvtDeviceAdd`, before the sealed
    /// channel exists, through `pf_driver_proto::gamepad::devtype_from_hwids`. Every host hwid
    /// must name the device_type the host stamps; a Deck frame parsed as DualSense `0x01` pins
    /// the left stick and holds d-pad UP.
    #[test]
    fn hwid_devtype_table_matches_the_driver() {
        for (hwid, devtype) in [
            (WinDsIdentity::dualsense().hwid, 0),
            (
                WinDsIdentity::dualsense_edge().hwid,
                pf_driver_proto::gamepad::DEVTYPE_DUALSENSE_EDGE,
            ),
            (
                super::super::dualshock4_windows::DS4_HWID,
                pf_driver_proto::gamepad::DEVTYPE_DUALSHOCK4,
            ),
            (
                super::super::steam_deck_windows::DECK_HWID,
                pf_driver_proto::gamepad::DEVTYPE_STEAMDECK,
            ),
            (
                super::super::triton_windows::TRITON_HWID,
                pf_driver_proto::gamepad::DEVTYPE_TRITON,
            ),
            // Server's unfiltered Xbox line: any Xbox type, so the pad never enumerates as a
            // DualSense while hidclass asks; the section sets the real one on attach.
            (
                super::super::xbox_windows::XBOX_UNFILTERED_HWID,
                pf_driver_proto::gamepad::DEVTYPE_XBOX,
            ),
        ]
        .into_iter()
        // Xbox identities share a report descriptor, so a hwid→devtype slip is the wrong PID, not
        // a mangled report (an Elite that Steam maps as a Series X|S pad).
        .chain(
            super::super::xbox_windows::XBOX_IDENTITIES
                .iter()
                .map(|i| (i.hwid, i.devtype)),
        ) {
            let got = pf_driver_proto::gamepad::devtype_from_hwids(&hwid.to_ascii_lowercase());
            assert_eq!(
                got,
                Some(devtype),
                "the host stamps device_type={devtype} for hardware id {hwid:?}, but the driver's \
                 table says {got:?} — the pad would enumerate with another controller's report \
                 descriptor"
            );
        }
    }

    #[test]
    fn legacy_driver_still_drains_the_latest_slot() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        legacy_publish(&mut buf, &[0x02, 1]);
        legacy_publish(&mut buf, &[0x02, 2]); // coalesced: latest wins
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0][..2], &[0x02, 2]);
        assert_eq!(collect(&mut d, &mut buf).0.len(), 0);
    }
}
