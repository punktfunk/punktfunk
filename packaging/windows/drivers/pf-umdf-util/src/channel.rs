//! The sealed pad channel, driver side (`design/gamepad-channel-sealing.md`): poll the named
//! bootstrap mailbox by index, publish our pid (iff the host's proto version matches), adopt the
//! host-delivered DATA-section handle, and validate the mapped section's magic and `pad_index`
//! before use. One implementation shared by `pf-xusb`, `pf-gamepad`, `pf-mouse` and `pf-touchpad`,
//! parameterized by [`ChannelConfig`].
//!
//! This module **forbids `unsafe`**: the entire state machine is safe Rust over
//! [`section`](crate::section)'s checked accessors — the memory-safety surface of the sealed
//! channel lives in that module alone.

#![forbid(unsafe_code)]

use crate::section::{MappedView, ViewCell, close_handle_value};
use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use pf_driver_proto::gamepad::{BOOT_MAGIC, GAMEPAD_PROTO_VERSION, PadBootstrap};

// PadBootstrap field offsets (the mailbox handshake; pinned by pf_driver_proto's asserts).
const BOOT_OFF_MAGIC: usize = offset_of!(PadBootstrap, magic);
const BOOT_OFF_HOST_PROTO: usize = offset_of!(PadBootstrap, host_proto);
const BOOT_OFF_DRIVER_PID: usize = offset_of!(PadBootstrap, driver_pid);
const BOOT_OFF_DRIVER_PROTO: usize = offset_of!(PadBootstrap, driver_proto);
const BOOT_OFF_DATA_HANDLE: usize = offset_of!(PadBootstrap, data_handle);
const BOOT_OFF_HANDLE_PID: usize = offset_of!(PadBootstrap, handle_pid);
const BOOT_OFF_HANDLE_SEQ: usize = offset_of!(PadBootstrap, handle_seq);
const BOOT_SIZE: usize = core::mem::size_of::<PadBootstrap>();

/// What varies between the drivers.
pub struct ChannelConfig {
    /// Log-line prefix (`"pf-xusb"` / `"pf-gamepad"`).
    pub tag: &'static str,
    /// Mailbox name prefix, completed with the pad index (`"Global\\pfxusb-boot-"` / `"Global\\pfds-boot-"`).
    pub boot_name_prefix: &'static str,
    /// The DATA section's magic (`XUSB_MAGIC` / `PAD_MAGIC`).
    pub data_magic: u32,
    /// The DATA section's size (`size_of::<XusbShm>()` / `size_of::<PadShm>()`).
    pub data_size: usize,
    /// Fallback map length when the full `data_size` map is refused — the legacy section size of a
    /// layout that grew by tail extension (`PAD_SHM_LEGACY_SIZE` for the pad channel). Sections are
    /// pagefile-backed and page-granular, so the full-size map is expected to succeed against
    /// either host generation; this exists so a refused map can never fail the pad closed. Set
    /// equal to `data_size` for layouts that never grew. A caller gates tail-extension features on
    /// `MappedView::mapped_len()` (plus the layout's own capability field), never on assumption.
    pub min_data_size: usize,
    /// `offset_of!(…Shm, pad_index)` in the DATA section.
    pub pad_index_off: usize,
    /// The driver's logger (each driver tees to its own debug file).
    pub log: fn(&str),
}

/// Per-pad channel state (a `static` in each driver — per-pad because
/// `UmdfHostProcessSharing=ProcessSharingDisabled` gives each pad its own WUDFHost).
pub struct ChannelClient {
    /// The pad index from the devnode Location (which mailbox to poll + the `pad_index` the
    /// delivered DATA section must carry).
    index: AtomicU32,
    /// The adopted DATA view; leaked-on-publish (see [`ViewCell`]) so a re-delivery can never
    /// unmap a view a concurrent callback still reads through.
    data: ViewCell,
    /// The last `handle_seq` consumed (CAS-guarded so concurrent pumps adopt a delivery exactly
    /// once). Reset to 0 when the mailbox disappears, so a NEW host session's delivery is always
    /// fresh even if its (per-host-process) seq counter collides with the previous session's.
    consumed_seq: AtomicU32,
    logged_proto_mismatch: AtomicBool,
    logged_pid: AtomicBool,
}

impl Default for ChannelClient {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`ChannelClient::adopt`] made of a delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adopt {
    Adopted,
    /// `MapViewOfFile` refused: transient, retried on the next pump.
    Unmapped,
    /// Wrong magic or pad index: the host's mistake, not retried.
    Rejected,
}

impl ChannelClient {
    pub const fn new() -> ChannelClient {
        ChannelClient {
            index: AtomicU32::new(0),
            data: ViewCell::new(),
            consumed_seq: AtomicU32::new(0),
            logged_proto_mismatch: AtomicBool::new(false),
            logged_pid: AtomicBool::new(false),
        }
    }

    /// Set the pad index (from the devnode Location, in `EvtDeviceAdd`).
    pub fn set_index(&self, idx: u32) {
        self.index.store(idx, Ordering::Relaxed);
    }

    pub fn index(&self) -> u32 {
        self.index.load(Ordering::Relaxed)
    }

    /// The adopted DATA view regardless of mailbox liveness — for write paths where acting on a
    /// stale section is harmless (the pump owns the detach semantics).
    pub fn data(&self) -> Option<&'static MappedView> {
        self.data.get()
    }

    /// One tick of the sealed-channel state machine: publish our pid (+ proto version) in the
    /// mailbox, adopt a delivered DATA handle, and return the attached DATA view — `None` while
    /// unattached, on a host/driver version mismatch (fail closed), or when the mailbox is gone
    /// (host gone). The mailbox is re-opened by name on every call: the name existing doubles as
    /// host-liveness (the host closes it when the pad is torn down).
    pub fn pump(&self, cfg: &ChannelConfig) -> Option<&'static MappedView> {
        let name = format!("{}{}", cfg.boot_name_prefix, self.index());
        let boot = match MappedView::open_named(&name, BOOT_SIZE) {
            Some(b) => b,
            None => {
                // Mailbox gone → the host (or this pad) is gone. Forget the consumed seq so the
                // NEXT host session's first delivery always reads as fresh.
                self.consumed_seq.store(0, Ordering::Relaxed);
                return None;
            }
        };
        // Acquire pairs with the host's Release magic store, so a valid magic implies `host_proto`
        // is visible. A missing/garbled magic reads as "no usable mailbox" (same as absent).
        if boot.load_u32(BOOT_OFF_MAGIC, Ordering::Acquire) != BOOT_MAGIC {
            self.consumed_seq.store(0, Ordering::Relaxed);
            return None;
        }
        // Publish our proto version first (idempotent) — the host logs a mismatch even when we
        // refuse to publish a pid below.
        boot.store_u32(
            BOOT_OFF_DRIVER_PROTO,
            GAMEPAD_PROTO_VERSION,
            Ordering::Relaxed,
        );
        let host_proto = boot.load_u32(BOOT_OFF_HOST_PROTO, Ordering::Relaxed);
        if host_proto != GAMEPAD_PROTO_VERSION {
            if !self.logged_proto_mismatch.swap(true, Ordering::Relaxed) {
                (cfg.log)(&format!(
                    "[{}] host proto {host_proto} != driver proto {GAMEPAD_PROTO_VERSION} — \
                     refusing the handshake (update host + drivers together)",
                    cfg.tag
                ));
            }
            return None; // version mismatch — fail closed
        }
        let mypid = std::process::id();
        if boot.load_u32(BOOT_OFF_DRIVER_PID, Ordering::Relaxed) != mypid {
            boot.store_u32(BOOT_OFF_DRIVER_PID, mypid, Ordering::Release);
            if !self.logged_pid.swap(true, Ordering::Relaxed) {
                (cfg.log)(&format!("[{}] bootstrap: published pid {mypid}", cfg.tag));
            }
        }
        // A delivery addressed to us we haven't consumed? CAS so concurrent pumps (worker thread /
        // timer + IOCTL paths) adopt exactly once.
        let seq = boot.load_u32(BOOT_OFF_HANDLE_SEQ, Ordering::Acquire);
        let cur = self.consumed_seq.load(Ordering::Relaxed);
        if seq != 0
            && seq != cur
            && boot.load_u32(BOOT_OFF_HANDLE_PID, Ordering::Relaxed) == mypid
            && self
                .consumed_seq
                .compare_exchange(cur, seq, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            // A map that fails under quota pressure is transient: give the seq back so the next
            // pump retries. A section that fails validation is the host's mistake, kept consumed.
            if self.adopt(cfg, boot.load_u64(BOOT_OFF_DATA_HANDLE, Ordering::Relaxed))
                == Adopt::Unmapped
            {
                self.consumed_seq.store(cur, Ordering::SeqCst);
            }
        }
        self.data()
    }

    /// Map + validate a delivered DATA-section handle VALUE (untrusted until the mapped section
    /// carries our magic AND our pad index). On success we own the handle (adopt-on-success) and
    /// close it — the view keeps the section alive. On validation failure the handle is
    /// deliberately NOT closed: a tampered value could name an unrelated handle in our own table.
    fn adopt(&self, cfg: &ChannelConfig, value: u64) -> Adopt {
        let Some(view) = MappedView::from_handle_value(value, cfg.data_size)
            .or_else(|| MappedView::from_handle_value(value, cfg.min_data_size))
        else {
            if value != 0 {
                (cfg.log)(&format!(
                    "[{}] delivered DATA handle 0x{value:x} did not map — retrying next pump",
                    cfg.tag
                ));
            }
            return Adopt::Unmapped;
        };
        let magic = view.load_u32(0, Ordering::Relaxed);
        let idx = view.load_u32(cfg.pad_index_off, Ordering::Relaxed);
        let want = self.index();
        if magic != cfg.data_magic || idx != want {
            (cfg.log)(&format!(
                "[{}] delivered DATA section failed validation (magic 0x{magic:08x}, pad_index \
                 {idx}, want {want}) — ignoring",
                cfg.tag
            ));
            // `view` drops here → unmapped; the handle stays open (see above).
            return Adopt::Rejected;
        }
        // The value resolved to OUR pad's section, so it is the handle the host duplicated for us —
        // we own it; the (about-to-be-leaked) view keeps the section alive after the close.
        close_handle_value(value);
        self.data.set(view);
        (cfg.log)(&format!(
            "[{}] sealed pad channel mapped (index {want})",
            cfg.tag
        ));
        Adopt::Adopted
    }
}
