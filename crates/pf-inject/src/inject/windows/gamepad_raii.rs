//! Per-pad Windows RAII and the sealed gamepad channel (DualSense / DualShock 4 / XUSB).
//!
//! Each pad owns three OS objects: an unnamed DATA section (`XusbShm`/`PadShm`), a named
//! [`PadBootstrap`] mailbox, and a `SwDeviceCreate` devnode. [`Shm`] and [`SwDevice`] own the
//! handles; [`PadChannel`] owns both sections and the delivery handshake.
//!
//! The DATA section is unnamed and SYSTEM-only; the driver receives it as a duplicated handle.
//! The mailbox is the only named object because a UMDF HID minidriver has no control device.
//! [`PadChannel::pump`] duplicates into the pid the bound devnode reports ([`channel_proof`]),
//! never into the mailbox's `driver_pid`. A delivery stands until that process exits, judged on
//! a retained `SYNCHRONIZE` handle. No `SwDeviceCreate` instance id → no delivery, unless
//! [`TRUST_MAILBOX_ENV`] is set.
//!
//! Evidence: `design/gamepad-channel-sealing.md`.

use super::channel_proof;
pub(super) use super::channel_proof::ProofTransport;
use crate::pad_slots::PadCreateFault;
use anyhow::{anyhow, Context, Result};
use pf_driver_proto::gamepad::{PadBootstrap, BOOT_MAGIC, GAMEPAD_PROTO_VERSION};
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{w, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_DevNode_Status, CM_Locate_DevNodeW, CM_DEVNODE_STATUS_FLAGS,
    CM_LOCATE_DEVNODE_NORMAL, CM_PROB, CR_SUCCESS, DN_DRIVER_LOADED, DN_HAS_PROBLEM, DN_STARTED,
};
use windows::Win32::Devices::Enumeration::Pnp::{SwDeviceClose, HSWDEVICE};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_HardwareIds, DEVPROPTYPE, DEVPROP_TYPE_STRING_LIST,
};
use windows::Win32::Foundation::{
    CloseHandle, DuplicateHandle, GetLastError, LocalFree, SetLastError, DUPLICATE_HANDLE_OPTIONS,
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{GetCurrentProcess, SetEvent, WaitForSingleObject};

/// `SECTION_MAP_READ | SECTION_MAP_WRITE` — what the pad driver maps. Granted in
/// [`PadChannel::deliver_to`] instead of `DUPLICATE_SAME_ACCESS`, so the remote handle
/// cannot take ownership, change security, or delete the section.
const SECTION_MAP_RW: u32 = 0x0004 | 0x0002;

/// Pagefile-backed section plus its mapped RW view. Drop unmaps the view, then the
/// [`OwnedHandle`] closes the section. Unnamed = sealed DATA; named = bootstrap mailbox.
pub(super) struct Shm {
    /// Duplication source for the sealed channel.
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

/// SDDL `SECURITY_ATTRIBUTES` plus the `LocalAlloc`'d descriptor it points at.
/// Drop `LocalFree`s the descriptor. Must outlive every `CreateFileMappingW` that
/// borrows `sa` — the section copies the DACL at create time, so free after return
/// is safe. [`Shm::create_named`] builds one and reuses it across squat retries.
struct SecAttr {
    sa: SECURITY_ATTRIBUTES,
    psd: PSECURITY_DESCRIPTOR,
}

impl Drop for SecAttr {
    fn drop(&mut self) {
        // SAFETY: `psd` is the descriptor `ConvertStringSecurityDescriptorToSecurityDescriptorW`
        // allocated with `LocalAlloc`; matching `LocalFree`. Every `CreateFileMappingW` that
        // borrowed `self.sa` has returned and copied the DACL into the section object.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.psd.0)));
        }
    }
}

fn sddl_sa(sddl: PCWSTR) -> Result<SecAttr> {
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL literal is valid; `psd` receives a `LocalAlloc`'d descriptor that `SecAttr`'s
    // `Drop` `LocalFree`s once the section create that borrows it has returned.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &mut psd,
            None,
        )?;
    }
    Ok(SecAttr {
        sa: SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        },
        psd,
    })
}

impl Shm {
    /// Unnamed `size`-byte section, mapped RW — the sealed DATA section.
    /// SDDL `D:P(A;;GA;;;SY)`: no name to open or squat; the driver gets a duplicated handle
    /// that carries this process's access without re-checking the DACL (`design/idd-push-security.md`).
    pub(super) fn create_unnamed(size: usize) -> Result<Shm> {
        let sa = sddl_sa(w!("D:P(A;;GA;;;SY)"))?;
        // Unnamed: nothing to collide with, so the flag is always false.
        Self::create_inner(&sa.sa, PCWSTR::null(), size)
            .map(|(shm, _)| shm)
            .context("create unnamed gamepad DATA section")
    }

    /// Named `size`-byte section, mapped RW — the bootstrap mailbox.
    /// SDDL `D:P(A;;GA;;;SY)(A;;GA;;;LS)`: SYSTEM plus LocalService (WUDFHost opens by name).
    ///
    /// `Global\` names are creatable by any `SeCreateGlobalPrivilege` holder. If the name
    /// already exists, `CreateFileMappingW` silently opens it (`ERROR_ALREADY_EXISTS`);
    /// close and retry, then fail rather than handshake through a foreign mailbox.
    /// A DACL we cannot open never reaches that branch: it is `ERROR_ACCESS_DENIED`,
    /// which [`classify_named_create_failure`] splits from "cannot create Global\\".
    pub(super) fn create_named(name: &HSTRING, size: usize) -> Result<Shm> {
        // One descriptor for the squat-retry loop. `D:P` strips inherited ACEs so only
        // SYSTEM + LocalService are granted.
        let sa = sddl_sa(w!("D:P(A;;GA;;;SY)(A;;GA;;;LS)"))?;
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(50));
            }
            // SAFETY: clearing the thread error slot so ERROR_ALREADY_EXISTS below is unambiguous.
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let (shm, existed) = match Self::create_inner(&sa.sa, PCWSTR(name.as_ptr()), size) {
                Ok(pair) => pair,
                Err(e) => return Err(classify_named_create_failure(name, e)),
            };
            // The verdict comes from the create itself, not from a re-read of the error slot
            // after the mapping call — and a section we merely opened was not zeroed.
            if !existed {
                return Ok(shm);
            }
            // `shm` drops here → unmap + close our handle to the foreign object, then retry.
        }
        Err(anyhow!(
            "bootstrap mailbox {name} already exists and stayed alive across retries — another \
             punktfunk-host instance is serving this pad index, or a local service is squatting the \
             name (gamepad DoS attempt?)"
        )
        .context(PadCreateFault::IndexOwnedElsewhere))
    }

    /// `true` in the tuple means `CreateFileMappingW` OPENED an existing section rather than
    /// creating one — the caller decides what to do about it. Read here, immediately after the
    /// create, because the mapping call below can clobber the thread error slot.
    fn create_inner(sa: &SECURITY_ATTRIBUTES, name: PCWSTR, size: usize) -> Result<(Shm, bool)> {
        // SAFETY: an anonymous (pagefile-backed) section of `size` bytes with the caller's SDDL; the
        // descriptor behind `sa` outlives this call (owned by the caller's `SecAttr`, freed only once
        // every create that borrows it has returned).
        let map = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa),
                PAGE_READWRITE,
                0,
                size as u32,
                name,
            )?
        };
        // SAFETY: read before anything else can touch the thread error slot.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: `map` is a fresh section handle we own; take ownership immediately so the early
        // return (and drop) closes it. `map` is `Copy`; `from_raw_handle` only copies the pointer.
        let handle = unsafe { OwnedHandle::from_raw_handle(map.0) };
        // SAFETY: `map` is a valid section handle; map the whole thing read/write.
        let view = unsafe { MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            // `handle` drops here → closes the section. No view to unmap.
            return Err(anyhow!("MapViewOfFile failed"));
        }
        if !existed {
            // Only ours. Windows zero-fills a new pagefile-backed section, so this is belt —
            // but running it on a section we merely OPENED would wipe a live mailbox that
            // belongs to another host instance, which the caller is about to back away from.
            // SAFETY: `view` points at `size` writable bytes (just mapped).
            unsafe { core::ptr::write_bytes(view.Value as *mut u8, 0, size) };
        }
        Ok((Shm { handle, view }, existed))
    }

    /// Mapped base. Stable for this `Shm`'s lifetime — `MapViewOfFile` pins the address.
    pub(super) fn base(&self) -> *mut u8 {
        self.view.Value as *mut u8
    }

    fn raw_handle(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: `view` came from `MapViewOfFile`; unmap it BEFORE the `handle` field closes the
        // section (struct fields drop only after this `Drop::drop` returns).
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

/// Split `CreateFileMappingW`'s `ERROR_ACCESS_DENIED` into two causes:
///
/// * Name taken, DACL excludes us — creating over an existing name is an open.
///   Mailbox SDDL is SYSTEM + LocalService, so a LocalSystem host vs an elevated
///   Administrator console is this case, not `ERROR_ALREADY_EXISTS`.
/// * Name free, we cannot create `Global\` — needs `SeCreateGlobalPrivilege`.
///
/// `OpenFileMappingW` looks up before the access check: missing → `ERROR_FILE_NOT_FOUND`,
/// present-but-denied → `ERROR_ACCESS_DENIED`. The taken case carries
/// [`PadCreateFault`] so the pad manager does not tell the operator to reinstall.
fn classify_named_create_failure(name: &HSTRING, e: anyhow::Error) -> anyhow::Error {
    let denied = e
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|w| w.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0));
    if !denied {
        return e.context(format!("create gamepad bootstrap mailbox {name}"));
    }
    if named_section_exists(name) {
        return e
            .context(PadCreateFault::IndexOwnedElsewhere)
            .context(format!(
            "bootstrap mailbox {name} exists and belongs to a process this one may not open — a \
             live session's pad, held by the LocalSystem host service (its mailboxes grant SYSTEM \
             + LocalService only, so an Administrator console sees ACCESS_DENIED, not \
             ALREADY_EXISTS). Nothing is wrong with the drivers"
        ));
    }
    e.context(format!(
        "create gamepad bootstrap mailbox {name}: access denied although the name is FREE — this \
         process may not create Global\\ objects at all (that needs SeCreateGlobalPrivilege, which \
         SYSTEM and services hold and a user token does not)"
    ))
}

/// Whether a section with this name exists as seen from this process.
/// `true` also when the object is present but closed to us (ACCESS_DENIED on open): a
/// LocalSystem host's mailbox is closed to an Administrator console.
/// Picks a pad slot or an error text, never trust — a squatter can make this say either.
pub(crate) fn named_section_exists(name: &HSTRING) -> bool {
    // SAFETY: `name` is a live NUL-terminated UTF-16 string for the duration of the call. Ask for
    // the least access there is (`FILE_MAP_READ`): the handle is closed immediately and never
    // mapped — we want the lookup's verdict, not the object.
    let opened = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) };
    match opened {
        Ok(h) => {
            // SAFETY: `h` is the handle just opened here and referenced nowhere else.
            unsafe {
                let _ = CloseHandle(h);
            }
            true
        }
        // ERROR_FILE_NOT_FOUND (and anything else) reads as absent; ACCESS_DENIED is presence.
        Err(e) => e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0),
    }
}

/// Host-wide [`PadBootstrap::handle_seq`]. Never 0, so two pads on the same index
/// cannot hand a persistent driver the same seq twice. Starts at 1.
static BOOT_SEQ: AtomicU32 = AtomicU32::new(1);

/// Cap on FAILED deliveries per pad. Each attempt duplicates a handle into a WUDFHost;
/// a flapping mailbox pid must not mint unbounded remote handles. Successes stand
/// until the target exits ([`PadChannel::delivered`]).
const MAX_DELIVERY_ATTEMPTS: u32 = 16;

/// How often an unattached pad may ask the devnode for a channel proof.
/// The pump ticks every few milliseconds; a proof query is a device open + I/O.
/// 250 ms is past a healthy driver's attach (tens of ms) and idle once delivered.
const PROOF_PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// Opt-in fallback when there is no `SwDeviceCreate` instance id (`devgen`/`devcon`).
/// Trusts the mailbox `driver_pid` — the path [`PadChannel::pump`] otherwise refuses.
/// Per-boot, logged loudly. Normal pads and the resident mouse never need it.
const TRUST_MAILBOX_ENV: &str = "PUNKTFUNK_PAD_CHANNEL_TRUST_MAILBOX";

/// Unanswered proof queries before one operator-facing warn.
/// At [`PROOF_PROBE_INTERVAL`] this is ~5 s — past attach and the eager window.
const PROOF_FAILURES_BEFORE_WARN: u32 = 20;

/// Mailbox names this process created and still holds. [`crate::pad_pool`] skips an index
/// only when another process serves it; our own pad inside its unplug grace is not that.
static CREATED_HERE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn created_names() -> MutexGuard<'static, Vec<String>> {
    CREATED_HERE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether this process created the mailbox `name` and still holds it.
pub(crate) fn created_here(name: &str) -> bool {
    created_names().iter().any(|n| n == name)
}

/// One pad's sealed host↔driver channel: unnamed DATA, named mailbox, and the
/// [`Self::pump`] state machine. Drop closes the mailbox; a persistent driver
/// treats the vanished name as "host gone".
pub(super) struct PadChannel {
    data: Shm,
    boot: Shm,
    boot_name: String,
    /// Instance id + transport for [`Self::bind_devnode`]. Stays `None` on the
    /// out-of-band path ([`TRUST_MAILBOX_ENV`]).
    devnode: Option<(String, ProofTransport)>,
    /// Must match the proof so a mis-resolved interface cannot cross-wire two pads.
    pad_index: u32,
    last_probe: Option<Instant>,
    /// Last pid delivered or rejected — never retry the same value within a probe interval
    /// (hot-loop trap). A failed delivery clears it so the next probe tries again.
    last_seen_pid: u32,
    attempts: u32,
    /// WUDFHost that holds the DATA handle. `Some` ⇒ no other process is served while it lives.
    delivered: Option<Delivered>,
    warned_proto: bool,
    warned_cap: bool,
    warned_takeover: bool,
    warned_unproven: bool,
    proof_failures: u32,
}

/// Completed delivery, pinned by a live process handle — never by pid.
/// A recycled pid cannot read as "the process we served has exited".
struct Delivered {
    pid: u32,
    /// `SYNCHRONIZE`; signaled ⇔ exited. Same idiom as the frame channel's `driver_alive`.
    process: OwnedHandle,
}

impl Delivered {
    fn exited(&self) -> bool {
        // SAFETY: `process` is the live `OwnedHandle` this channel owns (borrowed for this
        // synchronous call); a 0 ms wait only reads the handle's signaled state.
        unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) == WAIT_OBJECT_0 }
    }
}

impl PadChannel {
    /// Unnamed DATA (`data_size`, zeroed — caller stamps layout/magic) plus named mailbox.
    /// Stamp `host_proto` first and `BOOT_MAGIC` last so a driver only trusts a complete mailbox.
    pub(super) fn create(boot_name: String, data_size: usize) -> Result<PadChannel> {
        let data = Shm::create_unnamed(data_size)?;
        let boot = Shm::create_named(
            &HSTRING::from(boot_name.as_str()),
            core::mem::size_of::<PadBootstrap>(),
        )?;
        let base = boot.base();
        // SAFETY: `base` is the live, page-aligned mailbox view (>= size_of::<PadBootstrap>()); the
        // field offsets are pinned by the proto's asserts and naturally aligned, so the atomic views
        // are valid. `host_proto` is published BEFORE `magic` (Release) — a driver that observes the
        // magic (Acquire) sees the version.
        unsafe {
            (*(base.add(core::mem::offset_of!(PadBootstrap, host_proto)) as *const AtomicU32))
                .store(GAMEPAD_PROTO_VERSION, Ordering::Relaxed);
            fence(Ordering::Release);
            (*(base.add(core::mem::offset_of!(PadBootstrap, magic)) as *const AtomicU32))
                .store(BOOT_MAGIC, Ordering::Release);
        }
        created_names().push(boot_name.clone());
        Ok(PadChannel {
            data,
            boot,
            boot_name,
            devnode: None,
            pad_index: 0,
            last_probe: None,
            last_seen_pid: 0,
            attempts: 0,
            delivered: None,
            warned_proto: false,
            warned_cap: false,
            warned_takeover: false,
            warned_unproven: false,
            proof_failures: 0,
        })
    }
}

impl Drop for PadChannel {
    fn drop(&mut self) {
        created_names().retain(|n| *n != self.boot_name);
    }
}

impl PadChannel {
    pub(super) fn data_base(&self) -> *mut u8 {
        self.data.base()
    }

    pub(super) fn boot_name(&self) -> &str {
        &self.boot_name
    }

    fn boot_load(&self, off: usize) -> u32 {
        // SAFETY: the mailbox view is live (owned by `self.boot`), page-aligned, and every
        // `PadBootstrap` u32 field offset is 4-aligned (proto asserts), so the atomic view is valid;
        // no reference into the shared region outlives the load.
        unsafe { (*(self.boot.base().add(off) as *const AtomicU32)).load(Ordering::Acquire) }
    }

    /// Bind to the `SwDeviceCreate` instance so [`Self::pump`] can ask for a channel proof.
    /// Call between `create_swdevice` and [`Self::deliver_eager`].
    /// `instance_id` is `None` on the `devgen` fallback: no device to ask, no delivery
    /// unless [`TRUST_MAILBOX_ENV`] is set.
    pub(super) fn bind_devnode(
        &mut self,
        pad_index: u32,
        instance_id: Option<String>,
        transport: ProofTransport,
    ) {
        self.pad_index = pad_index;
        self.devnode = instance_id.map(|id| (id, transport));
    }

    /// One tick of the delivery state machine (pad service pump, ≤4 ms). Idle cost is
    /// an atomic load; once delivered, a 0 ms wait.
    pub(super) fn pump(&mut self) {
        // Driver writes its proto even when it refuses the handshake, so a mismatch is visible.
        let drv_proto = self.boot_load(core::mem::offset_of!(PadBootstrap, driver_proto));
        if drv_proto != 0 && drv_proto != GAMEPAD_PROTO_VERSION && !self.warned_proto {
            self.warned_proto = true;
            crate::note_pad_driver(crate::PadDriverVerdict::ProtocolMismatch {
                driver_proto: drv_proto,
                host_proto: GAMEPAD_PROTO_VERSION,
            });
            tracing::warn!(
                mailbox = %self.boot_name,
                driver_proto = drv_proto,
                host_proto = GAMEPAD_PROTO_VERSION,
                "gamepad driver/host protocol mismatch on the bootstrap mailbox — update the \
                 drivers: punktfunk-host.exe driver install --gamepad"
            );
        }

        // A delivery stands until the target process dies. UMDF restarting a crashed host
        // is the one legitimate re-attach, visible here as an exited handle.
        if let Some(d) = self.delivered.as_ref() {
            if !d.exited() {
                return;
            }
            tracing::info!(
                mailbox = %self.boot_name,
                exited_pid = d.pid,
                "the WUDFHost this channel was delivered to has exited (driver crash / host \
                 restart) — re-attaching to the restarted driver"
            );
            self.delivered = None;
            self.attempts = 0; // a genuine restart earns a fresh budget
            self.last_seen_pid = 0;
            self.last_probe = None;
        }
        if self.attempts >= MAX_DELIVERY_ATTEMPTS {
            if !self.warned_cap {
                self.warned_cap = true;
                tracing::warn!(
                    mailbox = %self.boot_name,
                    attempts = self.attempts,
                    "gamepad channel delivery cap reached — no further handles will be duplicated"
                );
            }
            return;
        }
        if self
            .last_probe
            .is_some_and(|t| t.elapsed() < PROOF_PROBE_INTERVAL)
        {
            return;
        }
        self.last_probe = Some(Instant::now());

        let Some(pid) = self.resolve_driver_pid() else {
            return;
        };
        if pid == self.last_seen_pid {
            return; // already tried this one and it failed — don't spin on it
        }
        self.last_seen_pid = pid;
        self.attempts += 1;
        match self.deliver_to(pid) {
            Ok((seq, process)) => {
                self.delivered = Some(Delivered { pid, process });
                tracing::info!(
                    mailbox = %self.boot_name,
                    wudf_pid = pid,
                    seq,
                    "sealed gamepad channel delivered (DATA handle duplicated into the driver's \
                     WUDFHost)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    mailbox = %self.boot_name,
                    pid,
                    error = %format!("{e:#}"),
                    "sealed gamepad channel delivery failed"
                );
                // `last_seen_pid` was stamped before the attempt, so leaving it set retires
                // this pid for the pad's life and one transient failure strands the pad.
                // Clearing it retries at the next probe; the attempt cap still bounds it.
                self.last_seen_pid = 0;
            }
        }
    }

    /// Pid to receive this pad's DATA section — from the DEVNODE ([`channel_proof`]),
    /// never from the mailbox. Only the driver PnP-bound to the `SwDeviceCreate`'d
    /// device can answer that I/O. Evidence: `design/gamepad-channel-sealing.md`.
    fn resolve_driver_pid(&mut self) -> Option<u32> {
        let Some((instance_id, transport)) = self.devnode.clone() else {
            return self.unproven_mailbox_pid("this pad has no SwDeviceCreate devnode to ask");
        };
        match channel_proof::query(&instance_id, transport, self.pad_index) {
            Ok(pid) => {
                self.proof_failures = 0;
                Some(pid)
            }
            Err(e) => {
                self.proof_failures += 1;
                // Debug while the driver may still be starting; one warn once it is clearly not.
                if self.proof_failures == PROOF_FAILURES_BEFORE_WARN {
                    tracing::warn!(
                        mailbox = %self.boot_name,
                        devnode = %instance_id,
                        error = %format!("{e:#}"),
                        "the pad's devnode has not answered a channel proof — the host will NOT \
                         hand the DATA section to a pid it cannot verify, so this pad stays \
                         unattached (an old driver? reinstall: punktfunk-host.exe driver install \
                         --gamepad)"
                    );
                } else {
                    tracing::debug!(
                        mailbox = %self.boot_name,
                        attempt = self.proof_failures,
                        error = %format!("{e:#}"),
                        "no channel proof yet"
                    );
                }
                None
            }
        }
    }

    /// Mailbox `driver_pid` for the `devgen` path that has no instance id to query.
    /// Refused unless [`TRUST_MAILBOX_ENV`] is set: LocalService can write that field.
    fn unproven_mailbox_pid(&mut self, why: &str) -> Option<u32> {
        if std::env::var_os(TRUST_MAILBOX_ENV).is_none() {
            if !self.warned_unproven {
                self.warned_unproven = true;
                tracing::warn!(
                    mailbox = %self.boot_name,
                    reason = why,
                    "no channel proof for this pad's driver — refusing the DATA section, since \
                     any local service can write the mailbox pid. Set {TRUST_MAILBOX_ENV}=1 to \
                     accept the old, unverified handshake on a driver bring-up box"
                );
            }
            return None;
        }
        let pid = self.boot_load(core::mem::offset_of!(PadBootstrap, driver_pid));
        if pid == 0 {
            return None;
        }
        if !self.warned_unproven {
            self.warned_unproven = true;
            tracing::warn!(
                mailbox = %self.boot_name,
                reason = why,
                "delivering this pad channel on the UNVERIFIED mailbox pid — a local service that \
                 wins the startup race can redirect this pad's input section (forged gamepad input \
                 + a read of pad state). Documented residual; the virtual MOUSE and the XUSB pad are \
                 both proved."
            );
        }
        Some(pid)
    }

    /// Duplicate the DATA section into a verified WUDFHost `pid`, then publish handle
    /// value + owning pid, bumping `handle_seq` last. An unconsumed duplicate dies with
    /// the target (nothing to reap after the duplication).
    ///
    /// Returns `(handle_seq, process)` — caller retains the handle so [`Self::pump`]
    /// can tell a UMDF host restart from a different claimant without trusting a pid.
    /// The `SYNCHRONIZE` right that makes that probe work comes with the shared open.
    fn deliver_to(&self, pid: u32) -> Result<(u32, OwnedHandle)> {
        let process = pf_capture::open_wudfhost(pid, "gamepad-channel")?;
        let mut remote = HANDLE::default();
        // SAFETY: `self.data.raw_handle()` is the live section handle this channel owns;
        // `process` is the live PROCESS_DUP_HANDLE target; `&mut remote` is a valid out-param.
        // Grant `SECTION_MAP_RW` only — not `DUPLICATE_SAME_ACCESS`. A compromised driver's
        // handle then cannot `WRITE_DAC`/`DELETE` the unnamed section.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.data.raw_handle(),
                HANDLE(process.as_raw_handle()),
                &mut remote,
                SECTION_MAP_RW,
                false,
                DUPLICATE_HANDLE_OPTIONS(0),
            )
            .context("DuplicateHandle(gamepad DATA section) into the driver's WUDFHost")?;
        }
        let value = remote.0 as usize as u64;
        let base = self.boot.base();
        let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);
        // SAFETY: live, page-aligned mailbox view; `data_handle` is 8-aligned and `handle_pid`/
        // `handle_seq` 4-aligned (proto asserts). The handle value + owning pid are published BEFORE
        // the seq (Release) — a driver that observes the new seq (Acquire) sees a complete delivery.
        unsafe {
            (*(base.add(core::mem::offset_of!(PadBootstrap, data_handle)) as *const AtomicU64))
                .store(value, Ordering::Relaxed);
            (*(base.add(core::mem::offset_of!(PadBootstrap, handle_pid)) as *const AtomicU32))
                .store(pid, Ordering::Relaxed);
            fence(Ordering::Release);
            (*(base.add(core::mem::offset_of!(PadBootstrap, handle_seq)) as *const AtomicU32))
                .store(seq, Ordering::Release);
        }
        Ok((seq, process))
    }

    /// Pump until a pid is acted on or `timeout` passes. Closes the DualShock 4 identity
    /// race: the driver reads `device_type` from DATA when hidclass asks for descriptors.
    pub(super) fn deliver_eager(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            if self.last_seen_pid != 0 || Instant::now() >= deadline {
                if self.delivered.is_none() {
                    tracing::debug!(
                        mailbox = %self.boot_name,
                        "eager gamepad-channel delivery window passed without an attach — the \
                         service pump keeps polling (driver-attach diagnosis follows if it stays \
                         silent)"
                    );
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// `SwDeviceCreate` completion context: event, HRESULT, and PnP instance id.
/// Shared by every Windows companion backend; the creator blocks on the event.
#[repr(C)]
pub(super) struct SwCreateCtx {
    pub(super) event: HANDLE,
    pub(super) result: HRESULT,
    pub(super) instance_id: [u16; 128],
}

/// `SwDeviceCreate` callback: stash result + instance id and wake the creator.
/// The creator blocks on the event, so there is no concurrent access to `*ctx`.
pub(super) unsafe extern "system" fn sw_create_cb(
    _dev: HSWDEVICE,
    result: HRESULT,
    ctx: *const c_void,
    id: PCWSTR,
) {
    if !ctx.is_null() {
        // SAFETY: ctx is the &mut SwCreateCtx the creator passed; it outlives this callback (the
        // creator blocks on the event). `id` is a NUL-terminated string for the callback's duration.
        unsafe {
            let c = ctx as *mut SwCreateCtx;
            (*c).result = result;
            if !id.is_null() {
                for i in 0..(*c).instance_id.len() - 1 {
                    let ch = *id.0.add(i);
                    (*c).instance_id[i] = ch;
                    if ch == 0 {
                        break;
                    }
                }
            }
            let _ = SetEvent((*c).event);
        }
    }
}

impl SwCreateCtx {
    pub(super) fn instance_id(&self) -> Option<String> {
        let len = self.instance_id.iter().position(|&c| c == 0)?;
        (len > 0).then(|| String::from_utf16_lossy(&self.instance_id[..len]))
    }
}

pub(super) struct SwDevice(HSWDEVICE);

impl SwDevice {
    pub(super) fn new(hsw: HSWDEVICE) -> Self {
        SwDevice(hsw)
    }
}

impl Drop for SwDevice {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the handle `SwDeviceCreate` returned; `SwDeviceClose` removes the devnode.
        unsafe { SwDeviceClose(self.0) };
    }
}

/// PnP bind + driver stamp window before a diagnosis warn.
const ATTACH_GRACE: Duration = Duration::from_secs(3);

/// Per-pad attach watcher. Feed `driver_proto` every service tick; logs attach,
/// version mismatch, a pad that enumerated as another controller, or — after
/// [`ATTACH_GRACE`] of silence — one diagnosis. States never repeat a log line, so the
/// pump can call this at full rate.
pub(super) struct DriverAttach {
    driver: &'static str,
    inf: &'static str,
    driver_log: &'static str,
    shm_name: String,
    /// `None` on the out-of-band fallback path.
    instance_id: Option<String>,
    /// The `device_type` the pad's hardware id names; `None` for the XUSB pad and the mouse.
    identity: Option<u8>,
    created: Instant,
    state: AttachState,
}

enum AttachState {
    Waiting,
    /// Diagnosis logged; still watching so a late attach gets its INFO line.
    Warned,
    Attached,
}

impl DriverAttach {
    pub(super) fn new(
        driver: &'static str,
        inf: &'static str,
        driver_log: &'static str,
        shm_name: String,
        instance_id: Option<String>,
    ) -> DriverAttach {
        DriverAttach {
            driver,
            inf,
            driver_log,
            shm_name,
            instance_id,
            identity: pf_driver_proto::gamepad::devtype_from_hwids(driver),
            created: Instant::now(),
            state: AttachState::Waiting,
        }
    }

    /// For the XUSB pad and the mouse, whose sections carry no driver revision.
    pub(super) fn observe(&mut self, driver_proto: u32) {
        self.observe_pad(driver_proto, 0);
    }

    /// `driver_rev` is read after `driver_proto` ([`crate::dualsense_windows::driver_marks`]);
    /// only the attach tick uses it.
    pub(super) fn observe_pad(&mut self, driver_proto: u32, driver_rev: u32) {
        match self.state {
            AttachState::Attached => {}
            AttachState::Waiting | AttachState::Warned if driver_proto != 0 => {
                let late = matches!(self.state, AttachState::Warned);
                tracing::info!(
                    driver = self.driver,
                    shm = %self.shm_name,
                    proto = driver_proto,
                    late,
                    "gamepad driver attached to the shared section"
                );
                if driver_proto != pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION {
                    tracing::warn!(
                        driver = self.driver,
                        driver_proto,
                        host_proto = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION,
                        "gamepad driver/host protocol mismatch — update the drivers: punktfunk-host.exe driver install --gamepad"
                    );
                }
                self.check_pad(driver_rev);
                self.state = AttachState::Attached;
            }
            AttachState::Waiting if self.created.elapsed() >= ATTACH_GRACE => {
                if self.identity.is_some() {
                    crate::note_pad_driver(crate::PadDriverVerdict::NotAttached);
                }
                self.diagnose();
                self.state = AttachState::Warned;
            }
            _ => {}
        }
    }

    /// Judge a freshly attached pad's driver ([`crate::pad_attach_verdict`]) for the diagnostics
    /// row, and WARN on an old revision or on a pad that reports another controller's VID/PID
    /// than its hardware id names (an older driver that did not know the id).
    fn check_pad(&self, driver_rev: u32) {
        let Some(devtype) = self.identity else {
            return;
        };
        let got = self
            .instance_id
            .as_deref()
            .and_then(channel_proof::hid_vid_pid);
        let verdict = crate::pad_attach_verdict(devtype, driver_rev, got);
        let fix = "reinstall the host with its controller drivers";
        match &verdict {
            crate::PadDriverVerdict::WrongIdentity { want, got } => tracing::warn!(
                driver = self.driver,
                want = %format!("VID_{:04X}&PID_{:04X}", want.0, want.1),
                got = %format!("VID_{:04X}&PID_{:04X}", got.0, got.1),
                fix,
                "virtual pad enumerated as another controller; games see the wrong pad"
            ),
            crate::PadDriverVerdict::Stale {
                driver_rev,
                host_rev,
            } => tracing::warn!(
                driver = self.driver,
                driver_rev,
                host_rev,
                fix,
                "gamepad driver is older than this host; pads run with its old behaviour"
            ),
            _ => {}
        }
        crate::note_pad_driver(verdict);
    }

    /// One-shot WARN: driver-store presence, devnode PnP problem, where to look next.
    ///
    /// Spawns a thread and returns. The caller is the pad service thread (input + rumble);
    /// `pnputil` can block for tens of seconds and a deadline wait would stall every
    /// unattached pad on that same wait.
    fn diagnose(&self) {
        let (driver, inf, driver_log) = (self.driver, self.inf, self.driver_log);
        let shm_name = self.shm_name.clone();
        let instance_id = self.instance_id.clone();
        let pad = self.identity.is_some();
        std::thread::Builder::new()
            .name("pf-driver-diagnose".into())
            .spawn(move || diagnose_blocking(driver, inf, driver_log, &shm_name, instance_id, pad))
            .ok();
    }
}

/// Split out of [`DriverAttach::diagnose`] so the blocking wait is visible as blocking.
fn diagnose_blocking(
    driver: &'static str,
    inf: &'static str,
    driver_log: &'static str,
    shm_name: &str,
    instance_id: Option<String>,
    pad: bool,
) {
    let store = match driver_store_has(inf) {
        Some(true) => "driver package present in the driver store",
        Some(false) => {
            "driver package NOT in the driver store — run: punktfunk-host.exe driver install --gamepad"
        }
        None => "driver store could not be queried (pnputil failed or still enumerating)",
    };
    let devnode = match &instance_id {
        Some(id) => devnode_status_line(id, pad),
        None => "no per-session devnode (SwDeviceCreate failed earlier — see the warning above)"
            .to_string(),
    };
    tracing::warn!(
        driver,
        shm = %shm_name,
        grace_secs = ATTACH_GRACE.as_secs(),
        store,
        devnode = %devnode,
        driver_log,
        "gamepad driver has not attached to the shared section — the virtual pad exists but no \
         driver is serving it (games will not see it); an old (pre-sealed-channel) driver also \
         reads as not-attached: update with punktfunk-host.exe driver install --gamepad \
         (driver_log is only written by debug driver builds, or with the PFXUSB_DEBUG_LOG / \
         PFGAMEPAD_DEBUG_LOG / PFMOUSE_DEBUG_LOG system env var set + the device restarted)"
    );
}

/// How long [`driver_store_inventory`] waits for pnputil. Only [`diagnose_blocking`]
/// waits, on its own thread. pnputil routinely exceeds a couple of seconds on a
/// busy driver store; 30 s is enough to report the inventory instead of "still enumerating".
const INVENTORY_WAIT: Duration = Duration::from_secs(30);

/// `pnputil /enum-drivers`, lower-cased, once per process. Failure-path only.
/// Query runs on its own thread so a wedged pnputil is not re-run per pad.
/// `None` = still running past [`INVENTORY_WAIT`], or failed; a late result still caches.
fn driver_store_inventory() -> Option<&'static str> {
    static INV: OnceLock<String> = OnceLock::new();
    static SPAWN: std::sync::Once = std::sync::Once::new();
    SPAWN.call_once(|| {
        std::thread::spawn(|| {
            // Resolve via `%SystemRoot%\System32\pnputil.exe`. SYSTEM must not search PATH /
            // the EXE directory — a planted `pnputil.exe` beside the host would run elevated.
            let pnputil = std::env::var("SystemRoot")
                .map(|r| format!(r"{r}\System32\pnputil.exe"))
                .unwrap_or_else(|_| "pnputil.exe".to_string());
            let inv = std::process::Command::new(&pnputil)
                .arg("/enum-drivers")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase())
                .unwrap_or_default();
            let _ = INV.set(inv);
        });
    });
    let deadline = Instant::now() + INVENTORY_WAIT;
    loop {
        if let Some(inv) = INV.get() {
            return Some(inv);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `None` = pnputil unavailable, failed, or still enumerating.
fn driver_store_has(inf: &str) -> Option<bool> {
    let inv = driver_store_inventory()?;
    if inv.is_empty() {
        return None;
    }
    Some(inv.contains(&inf.to_ascii_lowercase()))
}

fn devnode_status_line(instance_id: &str, pad: bool) -> String {
    let wide: Vec<u16> = instance_id
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut devinst = 0u32;
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 instance id; `devinst` receives the handle.
    let cr = unsafe {
        CM_Locate_DevNodeW(
            &mut devinst,
            PCWSTR(wide.as_ptr()),
            CM_LOCATE_DEVNODE_NORMAL,
        )
    };
    if cr != CR_SUCCESS {
        return format!(
            "devnode {instance_id} not found (CM_Locate_DevNodeW CR={})",
            cr.0
        );
    }
    let mut status = CM_DEVNODE_STATUS_FLAGS(0);
    let mut problem = CM_PROB(0);
    // SAFETY: devinst is the devnode located above; the two out-params receive status + problem.
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    if cr != CR_SUCCESS {
        return format!("devnode {instance_id}: status query failed (CR={})", cr.0);
    }
    if status.0 & DN_HAS_PROBLEM.0 != 0 {
        let refused =
            pad && hardware_ids(devinst).is_some_and(|ids| crate::pad_refused(problem.0, &ids));
        let hint = if refused {
            "the gamepad driver refused it: no pf_* hardware id names this pad; the host and \
             driver disagree on the controller list — reinstall both"
        } else {
            cm_problem_hint(problem.0)
        };
        return format!(
            "devnode {instance_id} has PnP problem code {} ({hint}) [status 0x{:08x}]",
            problem.0, status.0
        );
    }
    format!(
        "devnode {instance_id} status 0x{:08x} (driver_loaded={} started={})",
        status.0,
        status.0 & DN_DRIVER_LOADED.0 != 0,
        status.0 & DN_STARTED.0 != 0,
    )
}

/// A devnode's hardware ids, lowercase and `;`-terminated, the form the driver matches on.
fn hardware_ids(devinst: u32) -> Option<String> {
    let mut ty = DEVPROPTYPE(0);
    let mut buf = [0u16; 512];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: `devinst` is a located devnode; the key is a static const; `buf` holds `size`
    // bytes and `ty` / `size` are valid out-params.
    let cr = unsafe {
        CM_Get_DevNode_PropertyW(
            devinst,
            &DEVPKEY_Device_HardwareIds,
            &mut ty,
            Some(buf.as_mut_ptr().cast()),
            &mut size,
            0,
        )
    };
    if cr != CR_SUCCESS || ty != DEVPROP_TYPE_STRING_LIST {
        return None;
    }
    let len = (size as usize / 2).min(buf.len());
    Some(
        buf[..len]
            .split(|&c| c == 0)
            .filter(|id| !id.is_empty())
            .map(|id| String::from_utf16_lossy(id).to_ascii_lowercase() + ";")
            .collect(),
    )
}

fn cm_problem_hint(problem: u32) -> &'static str {
    match problem {
        1 => "not configured — no driver bound; install the drivers",
        10 => "device did not start — driver bound but the start failed; check the driver log",
        18 => "reinstall required — re-run driver install",
        24 => "device not present/working — PnP could not start the virtual devnode",
        28 => "drivers not installed — the pf driver package is missing from the store or its certificate is not trusted",
        31 => "driver did not load — the package was found but the load failed",
        39 => "driver corrupt or missing — reinstall the drivers",
        43 => "reported failure after start — check the driver log",
        52 => "driver signature rejected — certificate not in Root/TrustedPublisher, or blocked by Memory Integrity",
        _ => "see Device Manager for this code",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    /// Pin [`Delivered::exited`] to the process object, not the pid.
    /// Alive refuses a takeover; exited still lets UMDF restart re-deliver.
    /// Neither half is observable from the pad path without a real WUDFHost.
    #[test]
    fn delivered_exited_tracks_the_process_object_not_the_pid() {
        // A child that parks long enough to be observed alive (no console needed, unlike `pause`).
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a parked child");
        let pid = child.id();
        // SAFETY: plain FFI on our own child's pid; the returned handle is owned solely by the
        // `OwnedHandle` built from it (single owner, closes on drop).
        let process = unsafe {
            let h = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
            .expect("OpenProcess(SYNCHRONIZE) on our own child");
            OwnedHandle::from_raw_handle(h.0 as _)
        };
        let delivered = Delivered { pid, process };
        assert!(
            !delivered.exited(),
            "a running target must read as ALIVE — this is what refuses a channel takeover"
        );
        child.kill().expect("kill the child");
        // `wait` reaps, and only returns once the process has really exited — so the handle is
        // signaled by the time we look.
        child.wait().expect("reap the child");
        assert!(
            delivered.exited(),
            "an exited target must read as GONE — this is what lets a crashed driver re-attach"
        );
    }
}
