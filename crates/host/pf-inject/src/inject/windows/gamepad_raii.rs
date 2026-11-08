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
use crate::pad_shm_ring::SectionView;
use anyhow::{Context, Result};
use pf_driver_proto::gamepad::{PadBootstrap, BOOT_MAGIC, GAMEPAD_PROTO_VERSION};
use pf_driver_proto::pad_delivery::{
    Action, Delivery, Observation, Phase, PidAnswer, MAX_DELIVERY_ATTEMPTS,
};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::{fence, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_HANDLE_OPTIONS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{GetCurrentProcess, WaitForSingleObject};

#[path = "pad_section.rs"]
mod section;
pub(crate) use section::{named_section_exists, Shm};
#[path = "pad_swdevice.rs"]
mod swdevice;
pub use swdevice::set_pad_session;
pub(super) use swdevice::{create_swdevice, verify_fence, FenceAt, SwDevice, SwDeviceProfile};
#[path = "pad_attach.rs"]
mod attach;
#[path = "pad_driver_store.rs"]
mod driver_store;
pub(super) use attach::DriverAttach;

/// `SECTION_MAP_READ | SECTION_MAP_WRITE` — what the pad driver maps. Granted in
/// [`PadChannel::deliver_to`] instead of `DUPLICATE_SAME_ACCESS`, so the remote handle
/// cannot take ownership, change security, or delete the section.
const SECTION_MAP_RW: u32 = 0x0004 | 0x0002;

/// Host-wide [`PadBootstrap::handle_seq`]. Never 0, so two pads on the same index
/// cannot hand a persistent driver the same seq twice. Starts at 1.
static BOOT_SEQ: AtomicU32 = AtomicU32::new(1);

/// Opt-in fallback when there is no `SwDeviceCreate` instance id (`devgen`/`devcon`).
/// Trusts the mailbox `driver_pid` — the path [`PadChannel::pump`] otherwise refuses.
/// Per-boot, logged loudly. Normal pads and the resident mouse never need it.
pub(super) const TRUST_MAILBOX_ENV: &str = "PUNKTFUNK_PAD_CHANNEL_TRUST_MAILBOX";

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

/// One pad's sealed host↔driver channel: unnamed DATA, named mailbox, and the OS side of its
/// [`Delivery`]. Drop closes the mailbox; a persistent driver treats the vanished name as
/// "host gone".
pub(super) struct PadChannel {
    data: Shm,
    boot: Shm,
    boot_name: String,
    /// Instance id + transport for [`Self::bind_devnode`]. Stays `None` on the
    /// out-of-band path ([`TRUST_MAILBOX_ENV`]).
    devnode: Option<(String, ProofTransport)>,
    /// Must match the proof so a mis-resolved interface cannot cross-wire two pads.
    pad_index: u32,
    delivery: Delivery,
    /// The WUDFHost holding the DATA handle: `Some` exactly while `delivery` is delivered.
    target: Option<Delivered>,
    /// Zero of the clock [`Observation::Tick`] carries.
    born: Instant,
}

/// Completed delivery, pinned by a live process handle — never by pid.
/// A recycled pid cannot read as "the process we served has exited".
struct Delivered {
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
        // `host_proto` before `magic` (Release): a driver that sees the magic sees the version.
        let mailbox = boot.view();
        mailbox.store_u32(
            core::mem::offset_of!(PadBootstrap, host_proto),
            GAMEPAD_PROTO_VERSION,
            Ordering::Relaxed,
        );
        fence(Ordering::Release);
        mailbox.store_u32(
            core::mem::offset_of!(PadBootstrap, magic),
            BOOT_MAGIC,
            Ordering::Release,
        );
        created_names().push(boot_name.clone());
        Ok(PadChannel {
            data,
            boot,
            boot_name,
            devnode: None,
            pad_index: 0,
            delivery: Delivery::default(),
            target: None,
            born: Instant::now(),
        })
    }
}

impl Drop for PadChannel {
    fn drop(&mut self) {
        created_names().retain(|n| *n != self.boot_name);
    }
}

impl PadChannel {
    /// The DATA section this channel delivers.
    pub(super) fn data(&self) -> SectionView<'_> {
        self.data.view()
    }

    pub(super) fn boot_name(&self) -> &str {
        &self.boot_name
    }

    fn boot_load(&self, off: usize) -> u32 {
        self.boot.view().load_u32(off, Ordering::Acquire)
    }

    /// Bind to the `SwDeviceCreate` instance so [`Self::pump`] can ask for a channel proof.
    /// Call between [`create_swdevice`] and [`Self::deliver_eager`].
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

    /// One tick of the delivery (pad service pump, ≤4 ms): tell [`Delivery::step`] what the
    /// mailbox and the delivered process say, and carry out its answer. Idle cost is an atomic
    /// load and a clock read; once delivered, a 0 ms wait.
    pub(super) fn pump(&mut self) {
        let proto = self.boot_load(core::mem::offset_of!(PadBootstrap, driver_proto));
        if let Action::WarnProtoMismatch(driver_proto) =
            self.delivery.step(Observation::DriverProto(proto))
        {
            crate::note_pad_driver(crate::PadDriverVerdict::ProtocolMismatch {
                driver_proto,
                host_proto: GAMEPAD_PROTO_VERSION,
            });
            tracing::warn!(
                mailbox = %self.boot_name,
                driver_proto,
                host_proto = GAMEPAD_PROTO_VERSION,
                "gamepad driver/host protocol mismatch on the bootstrap mailbox — update the \
                 drivers: run the Punktfunk installer again"
            );
        }

        let exited = self.target.as_ref().is_some_and(Delivered::exited);
        let now = self.born.elapsed();
        match self.delivery.step(Observation::Tick { now, exited }) {
            Action::WarnCapped => tracing::warn!(
                mailbox = %self.boot_name,
                attempts = MAX_DELIVERY_ATTEMPTS,
                "gamepad channel delivery cap reached — no further handles will be duplicated"
            ),
            Action::Resolve { reattach } => {
                if let Some(exited_pid) = reattach {
                    tracing::info!(
                        mailbox = %self.boot_name,
                        exited_pid,
                        "the WUDFHost this channel was delivered to has exited (driver crash / \
                         host restart) — re-attaching to the restarted driver"
                    );
                    self.target = None;
                }
                self.resolve();
            }
            _ => {}
        }
    }

    /// Ask who the driver is and act on the answer. The pid comes from the DEVNODE
    /// ([`channel_proof`]), never from the mailbox: only the driver PnP-bound to the
    /// `SwDeviceCreate`'d device can answer that I/O. The mailbox's `driver_pid` stands in only
    /// on the `devgen` path, and only with [`TRUST_MAILBOX_ENV`] set: LocalService can write it.
    fn resolve(&mut self) {
        const NO_DEVNODE: &str = "this pad has no SwDeviceCreate devnode to ask";
        let devnode = self.devnode.clone();
        let mut proof_error = None;
        let answer = match &devnode {
            Some((instance_id, transport)) => {
                match channel_proof::query(instance_id, *transport, self.pad_index) {
                    Ok(pid) => PidAnswer::Proved(pid),
                    Err(e) => {
                        proof_error = Some(e);
                        PidAnswer::NoProof
                    }
                }
            }
            None if std::env::var_os(TRUST_MAILBOX_ENV).is_none() => PidAnswer::Refused,
            None => {
                PidAnswer::Mailbox(self.boot_load(core::mem::offset_of!(PadBootstrap, driver_pid)))
            }
        };
        match self.delivery.step(Observation::Pid(answer)) {
            Action::Deliver { pid, unverified } => {
                if unverified {
                    tracing::warn!(
                        mailbox = %self.boot_name,
                        reason = NO_DEVNODE,
                        "delivering this pad channel on the UNVERIFIED mailbox pid — a local \
                         service that wins the startup race can redirect this pad's input section \
                         (forged gamepad input + a read of pad state). Documented residual; the \
                         virtual MOUSE and the XUSB pad are both proved."
                    );
                }
                self.deliver(pid);
            }
            Action::NoProof { failures, warn } => {
                let devnode = devnode.map(|(id, _)| id).unwrap_or_default();
                let error = proof_error.map(|e| format!("{e:#}")).unwrap_or_default();
                // Debug while the driver may still be starting; one warn once it is clearly not.
                if warn {
                    tracing::warn!(
                        mailbox = %self.boot_name,
                        devnode = %devnode,
                        error = %error,
                        "the pad's devnode has not answered a channel proof — the host will NOT \
                         hand the DATA section to a pid it cannot verify, so this pad stays \
                         unattached (an old driver? reinstall: run the Punktfunk \
                         installer again)"
                    );
                } else {
                    tracing::debug!(
                        mailbox = %self.boot_name,
                        attempt = failures,
                        error = %error,
                        "no channel proof yet"
                    );
                }
            }
            Action::WarnRefused => tracing::warn!(
                mailbox = %self.boot_name,
                reason = NO_DEVNODE,
                "no channel proof for this pad's driver — refusing the DATA section, since \
                 any local service can write the mailbox pid. Set {TRUST_MAILBOX_ENV}=1 to \
                 accept the old, unverified handshake on a driver bring-up box"
            ),
            _ => {}
        }
    }

    /// Hand the DATA section to `pid` and tell the delivery how it went.
    fn deliver(&mut self, pid: u32) {
        match self.deliver_to(pid) {
            Ok((seq, process)) => {
                self.target = Some(Delivered { process });
                self.delivery.step(Observation::Delivered(pid));
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
                self.delivery.step(Observation::DeliveryFailed);
            }
        }
    }

    /// Duplicate the DATA section into a verified WUDFHost `pid`, then publish handle
    /// value + owning pid, bumping `handle_seq` last. An unconsumed duplicate dies with
    /// the target (nothing to reap after the duplication).
    ///
    /// Returns `(handle_seq, process)` — caller retains the handle so [`Self::pump`]
    /// can tell a UMDF host restart from a different claimant without trusting a pid.
    /// The `SYNCHRONIZE` right that makes that probe work comes with the shared open.
    fn deliver_to(&self, pid: u32) -> Result<(u32, OwnedHandle)> {
        let process = pf_win_display::open_wudfhost(pid, "gamepad-channel")?;
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
        let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);
        // Handle value + owning pid before the seq (Release): a driver that sees the new seq
        // (Acquire) sees a complete delivery.
        let mailbox = self.boot.view();
        mailbox.store_u64(
            core::mem::offset_of!(PadBootstrap, data_handle),
            value,
            Ordering::Relaxed,
        );
        mailbox.store_u32(
            core::mem::offset_of!(PadBootstrap, handle_pid),
            pid,
            Ordering::Relaxed,
        );
        fence(Ordering::Release);
        mailbox.store_u32(
            core::mem::offset_of!(PadBootstrap, handle_seq),
            seq,
            Ordering::Release,
        );
        Ok((seq, process))
    }

    /// Pump until the section is delivered or `timeout` passes. Closes the DualShock 4 identity
    /// race: the driver reads `device_type` from DATA when hidclass asks for descriptors.
    pub(super) fn deliver_eager(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            self.pump();
            let delivered = matches!(self.delivery.phase, Phase::Delivered { .. });
            if delivered || Instant::now() >= deadline {
                if !delivered {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::FromRawHandle;
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
        let delivered = Delivered { process };
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
