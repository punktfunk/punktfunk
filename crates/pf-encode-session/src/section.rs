//! The host-created AU section as its writer sees it, and the [`EncodeSession`] one `SET_ENCODE`
//! installs on a frame source.
//!
//! [`AuSection`] writes through a [`SectionView`] the caller mapped, and owns the ready event
//! from then on. Every field past the host-stamped layout is written through atomic views over
//! the mapping — the encode thread is the only writer, the host reads under the `latest`
//! token's generation check.

use std::collections::VecDeque;
use std::mem::offset_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use pf_driver_proto::encode::au::{self, AuHeader, AuSlot};
use pf_driver_proto::encode::{self as wire, EncodeCtlRequest, FrameToken, SetEncodeRequest};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::SetEvent;

use crate::{Fail, lock};

/// A read/write mapping of the section, bounds- and alignment-checked by whoever mapped it:
/// the driver's audited view, the capture worker's own.
pub trait SectionView: Send + Sync {
    /// The 4-aligned word at `off`, as the atomic both sides treat it as.
    fn atomic_u32(&self, off: usize) -> &AtomicU32;
    /// [`Self::atomic_u32`] for an 8-aligned u64.
    fn atomic_u64(&self, off: usize) -> &AtomicU64;
    /// Copy `dst.len()` bytes out from `off`.
    fn read_bytes(&self, off: usize, dst: &mut [u8]);
    /// One `memcpy` of `src` into `off..`: the heap, whose single writer publishes after it.
    fn copy_from_slice(&self, off: usize, src: &[u8]);
}

/// The mapped section and the ready event, owned. `Send` and `Sync` come from its fields.
pub struct AuSection {
    view: Box<dyn SectionView>,
    event: OwnedHandle,
    /// Host-stamped heap bounds, validated once at adopt time.
    heap_offset: u32,
    heap_bytes: u32,
}

impl AuSection {
    /// Take `view`, a mapping of `section_bytes`, and adopt `event`. `Err` means NOTHING was
    /// adopted: `view` unmaps as it drops and `event` is left for the host to reap.
    ///
    /// # Safety
    /// `event` must be a live event handle this process owns and nothing else closes.
    pub unsafe fn adopt(
        view: Box<dyn SectionView>,
        section_bytes: u32,
        event: RawHandle,
    ) -> Result<Self, Fail> {
        // The header read below needs the whole fixed layout to be there.
        if (section_bytes as usize) < au::HEAP_OFFSET {
            tracing::info!("encode: AU section of {section_bytes} B is shorter than its layout");
            return Err((-9, "section"));
        }
        let mut raw = [0u8; au::AU_HEADER_SIZE];
        view.read_bytes(0, &mut raw);
        let header: AuHeader = bytemuck::pod_read_unaligned(&raw);
        let fits =
            au::au_readable(&header) && au::section_bytes(header.heap_bytes) <= section_bytes;
        if !fits {
            tracing::info!("encode: AU section unreadable: {header:?} in {section_bytes} B");
            return Err((-9, "section"));
        }
        // SAFETY: the caller's contract — `event` is ours alone to close from here on.
        let event = unsafe { OwnedHandle::from_raw_handle(event) };
        Ok(Self {
            view,
            event,
            heap_offset: header.heap_offset,
            heap_bytes: header.heap_bytes,
        })
    }

    /// `(offset, bytes)` of the heap inside the section.
    pub fn heap(&self) -> (u32, u32) {
        (self.heap_offset, self.heap_bytes)
    }

    fn u32_at(&self, off: usize) -> &AtomicU32 {
        self.view.atomic_u32(off)
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        self.view.atomic_u64(off)
    }

    /// Slot `i`'s state word.
    pub fn slot_state(&self, i: usize) -> &AtomicU32 {
        self.u32_at(au::slot_offset(i) + offset_of!(AuSlot, state))
    }

    /// Write slot `i`'s record, state last with Release, so a reader that Acquire-loads
    /// `PUBLISHED` sees the fields that belong to these bytes.
    pub fn publish_slot(&self, i: usize, slot: &AuSlot) {
        let base = au::slot_offset(i);
        self.u32_at(base + offset_of!(AuSlot, offset))
            .store(slot.offset, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, len))
            .store(slot.len, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, wire_seq))
            .store(slot.wire_seq, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, source_seq))
            .store(slot.source_seq, Ordering::Relaxed);
        self.u64_at(base + offset_of!(AuSlot, qpc_pts))
            .store(slot.qpc_pts, Ordering::Relaxed);
        self.u32_at(base + offset_of!(AuSlot, flags))
            .store(slot.flags, Ordering::Relaxed);
        self.u64_at(base + offset_of!(AuSlot, qpc_submit))
            .store(slot.qpc_submit, Ordering::Relaxed);
        self.u64_at(base + offset_of!(AuSlot, qpc_published))
            .store(slot.qpc_published, Ordering::Relaxed);
        self.slot_state(i).store(au::PUBLISHED, Ordering::Release);
    }

    /// Copy `bytes` into the heap at section offset `offset`. `false` — nothing written — for a
    /// range outside the heap, which no reservation the ring allocator hands out can be.
    #[must_use]
    pub fn write_heap(&self, offset: u32, bytes: &[u8]) -> bool {
        let start = offset as usize;
        let end = start + bytes.len();
        let heap_end = self.heap_offset as usize + self.heap_bytes as usize;
        if start < self.heap_offset as usize || end > heap_end {
            return false;
        }
        // The encode thread is the only writer; the host reads only slots it Acquire-loaded.
        self.view.copy_from_slice(start, bytes);
        true
    }

    /// Store the publish token (Release, after the slot) and wake the host.
    pub fn publish_latest(&self, token: FrameToken) {
        self.u64_at(offset_of!(AuHeader, latest))
            .store(token.pack(), Ordering::Release);
        // SAFETY: `event` is the live host-created ready event this section owns.
        unsafe {
            let _ = SetEvent(HANDLE(self.event.as_raw_handle()));
        }
    }

    pub fn store_u32(&self, off: usize, v: u32) {
        self.u32_at(off).store(v, Ordering::Relaxed);
    }

    pub fn store_u64(&self, off: usize, v: u64) {
        self.u64_at(off).store(v, Ordering::Relaxed);
    }

    pub fn add_u32(&self, off: usize, n: u32) -> u32 {
        self.u32_at(off).fetch_add(n, Ordering::Relaxed) + n
    }

    pub fn add_u64(&self, off: usize, n: u64) -> u64 {
        self.u64_at(off).fetch_add(n, Ordering::Relaxed) + n
    }
}

/// One `ENCODE_CTL` op for the encode thread, drained between frames. One-shot: the host
/// sends the next only after this one returned, so the queue never holds more than a few.
#[derive(Clone, Copy, Debug)]
pub enum Ctl {
    RequestKeyframe,
    /// Wire indexes `first..=last`.
    InvalidateRefFrames(u32, u32),
    DistrustReferences,
    /// kbps.
    ReconfigureBitrate(u32),
    /// `pf_frame::HdrMeta` as its 28 bytes.
    SetHdrMeta([u8; 28]),
    Flush,
    /// The newest wire index the client confirmed; `None` restores the chain.
    SetReferenceFloor(Option<u32>),
}

impl Ctl {
    /// The queued op `req` asks for. `None` for `reset` and `close`, which act on the session
    /// from the control thread, and for an op this build does not know.
    pub fn queued(req: &EncodeCtlRequest) -> Option<Self> {
        Some(match req.op {
            wire::ENCODE_CTL_REQUEST_KEYFRAME => Self::RequestKeyframe,
            wire::ENCODE_CTL_INVALIDATE_REF_FRAMES => Self::InvalidateRefFrames(req.arg0, req.arg1),
            wire::ENCODE_CTL_DISTRUST_REFERENCES => Self::DistrustReferences,
            wire::ENCODE_CTL_RECONFIGURE_BITRATE => Self::ReconfigureBitrate(req.arg0),
            wire::ENCODE_CTL_SET_HDR_META => Self::SetHdrMeta(req.payload),
            wire::ENCODE_CTL_FLUSH => Self::Flush,
            wire::ENCODE_CTL_SET_REFERENCE_FLOOR => {
                Self::SetReferenceFloor((req.arg1 != 0).then_some(req.arg0))
            }
            _ => return None,
        })
    }
}

/// The thread a session's loop runs on, as the session stops it. Each side has its own: the
/// driver's worker, the capture worker's.
pub trait SessionThread: Send {
    /// Stop within the side's bound. A thread that will not return is detached and counted in
    /// the section's `detached` word.
    fn stop(self: Box<Self>, section: &AuSection);
}

/// One frame source's live encode: the request it was opened from, the section it publishes
/// into and the thread doing it. The source holds one `Arc`; the encode thread holds another
/// for as long as it runs, so a detached thread keeps the section mapped until it really exits.
pub struct EncodeSession {
    pub request: SetEncodeRequest,
    pub section: AuSection,
    /// Stamped into every publish token; bumped per `SET_ENCODE` by the source's owner.
    pub generation: u32,
    /// The first `wire_seq` the current thread stamps.
    pub wire_seq_base: AtomicU32,
    /// The source stopped fitting what the session opened for (a device epoch, a size): frames
    /// stop, the host's next `SET_ENCODE` rebuilds.
    pub stale: AtomicBool,
    /// The control mailbox ([`Ctl`]); the source's event wakes the thread to drain it.
    pub ctl: Mutex<VecDeque<Ctl>>,
    thread: Mutex<Option<Box<dyn SessionThread>>>,
}

impl EncodeSession {
    pub fn new(request: SetEncodeRequest, section: AuSection, generation: u32) -> Self {
        section.store_u32(offset_of!(AuHeader, generation), generation);
        section.store_u32(offset_of!(AuHeader, wire_seq_base), request.wire_seq_base);
        section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_CLOSED);
        Self {
            request,
            section,
            generation,
            wire_seq_base: AtomicU32::new(request.wire_seq_base),
            stale: AtomicBool::new(false),
            ctl: Mutex::new(VecDeque::new()),
            thread: Mutex::new(None),
        }
    }

    /// Queue one op for the thread; the caller wakes it.
    pub fn push_ctl(&self, op: Ctl) {
        lock(&self.ctl).push_back(op);
    }

    /// Everything queued, in order.
    pub fn take_ctl(&self) -> Vec<Ctl> {
        lock(&self.ctl).drain(..).collect()
    }

    /// Install the running thread; whatever it displaces is handed back to stop with no lock
    /// held.
    #[must_use]
    pub fn set_thread(&self, thread: Box<dyn SessionThread>) -> Option<Box<dyn SessionThread>> {
        lock(&self.thread).replace(thread)
    }

    /// Take the thread out; the caller stops it with no lock held.
    #[must_use]
    pub fn take_thread(&self) -> Option<Box<dyn SessionThread>> {
        lock(&self.thread).take()
    }

    /// The section half of a reset, with no thread running: the next thread stamps from
    /// `wire_seq_base`. The old thread's published slots are freed, because the new heap ring
    /// knows nothing of their bytes and would place its IDR over them while the host still
    /// reads them; a slot the host holds READING stays its own.
    pub fn rebase(&self, wire_seq_base: u32) {
        for i in 0..au::AU_SLOTS as usize {
            let _ = self.section.slot_state(i).compare_exchange(
                au::PUBLISHED,
                au::FREE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        self.wire_seq_base.store(wire_seq_base, Ordering::Release);
        self.stale.store(false, Ordering::Release);
        self.section
            .store_u32(offset_of!(AuHeader, wire_seq_base), wire_seq_base);
    }

    /// Stop the thread within its bound, detaching it if it will not.
    pub fn stop(&self) {
        if let Some(t) = self.take_thread() {
            t.stop(&self.section);
        }
    }
}
