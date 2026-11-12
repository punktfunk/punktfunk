//! The endpoint object audiodg renders a pad slot into. It is timer-driven: the engine pulls
//! one period at a time, and the frames go into the slot's ring for the seat host.
//!
//! Real-time methods (`GetCurrentPadding`, `GetOutputDataPointer`, `ReleaseOutputDataPointer`)
//! only touch atomics, the scratch buffer and the mapped ring: no allocation, lock or wait.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use pf_driver_proto::seat_audio::{self as proto, RingHeader, FRAME_BYTES, RATE, RING_FRAMES};
use std::sync::OnceLock;
use windows::core::{implement, Result, HSTRING};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_INITIALIZED, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_UNEXPECTED,
    HANDLE,
};
use windows::Win32::Media::Audio::Apo::{APO_CONNECTION_PROPERTY, BUFFER_SILENT, BUFFER_VALID};
use windows::Win32::Media::Audio::WAVEFORMATEX;
use windows::Win32::System::Com::CoTaskMemAlloc;
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualAlloc, VirtualFree, VirtualLock,
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MEM_COMMIT, MEM_RELEASE,
    MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::RemoteDesktop::{
    IAudioDeviceEndpoint, IAudioDeviceEndpoint_Impl, IAudioEndpoint, IAudioEndpointControl,
    IAudioEndpointControl_Impl, IAudioEndpointRT, IAudioEndpointRT_Impl, IAudioEndpoint_Impl,
    IAudioOutputEndpointRT, IAudioOutputEndpointRT_Impl, AE_CURRENT_POSITION, POSITION_CONTINUOUS,
};

/// The engine's default period when it passes none: 10 ms in 100 ns units.
const DEFAULT_PERIOD: i64 = 100_000;
/// One second bounds the locked scratch buffer.
const MAX_LATENCY: i64 = 10_000_000;

#[implement(
    IAudioEndpoint,
    IAudioDeviceEndpoint,
    IAudioEndpointRT,
    IAudioOutputEndpointRT,
    IAudioEndpointControl
)]
pub(crate) struct Endpoint {
    session: u32,
    slot: u8,
    scratch: OnceLock<Scratch>,
    ring: OnceLock<Ring>,
    /// Frames handed out by `GetOutputDataPointer` and not yet released.
    pending: AtomicU32,
    /// Frames released since `Reset`.
    written: AtomicU64,
    started: AtomicBool,
    /// The clock the frames play against, as if it began when the written frames did.
    start: AtomicI64,
    frequency: i64,
}

impl Endpoint {
    pub(crate) fn new(session: u32, slot: u8) -> Self {
        let mut frequency = 0;
        // SAFETY: plain query; never fails on Windows XP and later.
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        Self {
            session,
            slot,
            scratch: OnceLock::new(),
            ring: OnceLock::new(),
            pending: AtomicU32::new(0),
            written: AtomicU64::new(0),
            started: AtomicBool::new(false),
            start: AtomicI64::new(0),
            frequency: frequency.max(1),
        }
    }

    /// Map the slot's ring if its host published it. Without one the endpoint still runs and
    /// discards, so a stream that outlives its pad keeps its clock.
    fn open_ring(&self) {
        if self.ring.get().is_none() {
            if let Some(ring) = Ring::open(self.session, self.slot) {
                let _ = self.ring.set(ring);
            }
        }
    }

    fn now() -> i64 {
        let mut now = 0;
        // SAFETY: plain query.
        let _ = unsafe { QueryPerformanceCounter(&mut now) };
        now
    }

    /// Starts the clock at `now`. The frames written before a `Stop` have played by now, so it
    /// resumes where they end.
    fn begin_clock(&self, now: i64) {
        let written = self.written.load(Ordering::Acquire);
        let begun = now - ticks_for(written, self.frequency);
        self.start.store(begun, Ordering::Release);
        self.started.store(true, Ordering::Release);
    }

    /// The frames played by `now` of the `written` ones: all of them while the clock is stopped.
    fn played_at(&self, written: u64, now: i64) -> u64 {
        if !self.started.load(Ordering::Acquire) {
            return written;
        }
        played_frames(
            written,
            now - self.start.load(Ordering::Acquire),
            self.frequency,
        )
    }
}

impl IAudioEndpoint_Impl for Endpoint_Impl {
    fn GetFrameFormat(&self) -> Result<*mut WAVEFORMATEX> {
        let format = &proto::FORMAT_PCM16;
        // SAFETY: a fresh allocation the caller frees with CoTaskMemFree.
        let out = unsafe { CoTaskMemAlloc(format.len()) }.cast::<u8>();
        if out.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        // SAFETY: `out` holds `format.len()` bytes.
        unsafe { core::ptr::copy_nonoverlapping(format.as_ptr(), out, format.len()) };
        Ok(out.cast())
    }

    fn GetFramesPerPacket(&self) -> Result<u32> {
        Ok(1)
    }

    fn GetLatency(&self) -> Result<i64> {
        Ok(self.scratch.get().map_or(0, |scratch| scratch.latency))
    }

    fn SetStreamFlags(&self, _flags: u32) -> Result<()> {
        Ok(())
    }

    fn SetEventHandle(&self, _event: HANDLE) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
}

impl IAudioDeviceEndpoint_Impl for Endpoint_Impl {
    /// The engine passes a 10 ms period and coefficient 0; zero means "the default" for both.
    fn SetBuffer(&self, period: i64, coefficient: u32) -> Result<()> {
        if self.scratch.get().is_some() {
            return Err(ERROR_ALREADY_INITIALIZED.to_hresult().into());
        }
        let period = if period <= 0 { DEFAULT_PERIOD } else { period };
        let latency = period
            .checked_mul(i64::from(coefficient.max(1)))
            .filter(|&latency| latency <= MAX_LATENCY)
            .ok_or(E_INVALIDARG)?;
        let frames = u32::try_from((latency * i64::from(RATE) + 9_999_999) / 10_000_000)
            .map_err(|_| E_INVALIDARG)?;
        let scratch = Scratch::new(frames, latency).ok_or(E_OUTOFMEMORY)?;
        self.scratch.set(scratch).map_err(|_| E_UNEXPECTED)?;
        self.open_ring();
        Ok(())
    }

    fn GetRTCaps(&self) -> Result<windows::core::BOOL> {
        Ok(false.into())
    }

    fn GetEventDrivenCapable(&self) -> Result<windows::core::BOOL> {
        Ok(false.into())
    }

    fn WriteExclusiveModeParametersToSharedMemory(
        &self,
        _process: usize,
        _period: i64,
        _duration: i64,
        _coefficient: u32,
        size: *mut u32,
        memory: *mut usize,
    ) -> Result<()> {
        if !size.is_null() {
            // SAFETY: the caller's out parameter, checked non-null.
            unsafe { size.write(0) };
        }
        if !memory.is_null() {
            // SAFETY: the caller's out parameter, checked non-null.
            unsafe { memory.write(0) };
        }
        Err(E_NOTIMPL.into())
    }
}

impl IAudioEndpointRT_Impl for Endpoint_Impl {
    /// Played frames follow the clock and never pass the frames written. The engine pulls
    /// nothing while the position is invalid.
    fn GetCurrentPadding(&self, padding: *mut i64, position: *mut AE_CURRENT_POSITION) {
        let written = self.written.load(Ordering::Acquire);
        let now = Endpoint::now();
        let played = self.played_at(written, now);
        if !padding.is_null() {
            let queued = (written - played).saturating_mul(10_000_000) / u64::from(RATE);
            // SAFETY: the engine's out parameter, checked non-null.
            unsafe { padding.write(i64::try_from(queued).unwrap_or(i64::MAX)) };
        }
        if !position.is_null() {
            let qpc = i128::from(now) * 10_000_000 / i128::from(self.frequency);
            // SAFETY: the engine's out parameter, checked non-null.
            unsafe {
                position.write(AE_CURRENT_POSITION {
                    u64DevicePosition: played,
                    u64StreamPosition: played,
                    u64PaddingFrames: 0,
                    hnsQPCPosition: i64::try_from(qpc).unwrap_or(i64::MAX),
                    f32FramesPerSecond: RATE as f32,
                    Flag: POSITION_CONTINUOUS,
                });
            }
        }
    }

    fn ProcessingComplete(&self) {}

    fn SetPinInactive(&self) -> Result<()> {
        Ok(())
    }

    fn SetPinActive(&self) -> Result<()> {
        Ok(())
    }
}

impl IAudioOutputEndpointRT_Impl for Endpoint_Impl {
    fn GetOutputDataPointer(&self, frames: u32, _position: *const AE_CURRENT_POSITION) -> usize {
        let Some(scratch) = self.scratch.get() else {
            return 0;
        };
        if frames > scratch.frames
            || self
                .pending
                .compare_exchange(0, frames, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return 0;
        }
        scratch.base.as_ptr() as usize
    }

    fn ReleaseOutputDataPointer(&self, property: *const APO_CONNECTION_PROPERTY) {
        let pending = self.pending.swap(0, Ordering::AcqRel);
        let Some(scratch) = self.scratch.get() else {
            return;
        };
        if property.is_null() {
            return;
        }
        // SAFETY: the engine's connection property, checked non-null.
        let property = unsafe { *property };
        let frames = property.u32ValidFrameCount;
        if frames > pending || property.pBuffer != scratch.base.as_ptr() as usize {
            return;
        }
        let silent = match property.u32BufferFlags {
            BUFFER_VALID => false,
            BUFFER_SILENT => true,
            _ => return,
        };
        if let Some(ring) = self.ring.get() {
            ring.write(scratch.base, frames, silent);
        }
        self.written.fetch_add(u64::from(frames), Ordering::AcqRel);
    }

    fn PulseEndpoint(&self) {}
}

impl IAudioEndpointControl_Impl for Endpoint_Impl {
    fn Start(&self) -> Result<()> {
        if self.scratch.get().is_none() {
            return Err(E_UNEXPECTED.into());
        }
        self.open_ring();
        self.begin_clock(Endpoint::now());
        Ok(())
    }

    fn Reset(&self) -> Result<()> {
        self.written.store(0, Ordering::Release);
        self.start.store(Endpoint::now(), Ordering::Release);
        Ok(())
    }

    fn Stop(&self) -> Result<()> {
        self.started.store(false, Ordering::Release);
        Ok(())
    }
}

/// The frames `ticks` clock ticks have played, never more than the `written` ones.
fn played_frames(written: u64, ticks: i64, frequency: i64) -> u64 {
    let elapsed = i128::from(ticks.max(0)) * i128::from(RATE) / i128::from(frequency);
    written.min(elapsed.clamp(0, i128::from(u64::MAX)) as u64)
}

/// The clock ticks `frames` frames take, rounded up so they have played once the ticks pass.
fn ticks_for(frames: u64, frequency: i64) -> i64 {
    let ticks =
        (i128::from(frames) * i128::from(frequency) + i128::from(RATE) - 1) / i128::from(RATE);
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

/// The locked buffer the engine renders one period into.
struct Scratch {
    base: NonNull<u8>,
    frames: u32,
    latency: i64,
}

// SAFETY: the buffer is plain memory this object owns; the engine writes it only between
// `GetOutputDataPointer` and `ReleaseOutputDataPointer`, which `pending` serialises.
unsafe impl Send for Scratch {}
// SAFETY: as above.
unsafe impl Sync for Scratch {}

impl Scratch {
    fn new(frames: u32, latency: i64) -> Option<Self> {
        let bytes = usize::try_from(frames)
            .ok()?
            .checked_mul(FRAME_BYTES)?
            .max(FRAME_BYTES);
        // SAFETY: a fresh private allocation, freed in `drop`.
        let base = unsafe { VirtualAlloc(None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        let base = NonNull::new(base.cast::<u8>())?;
        // SAFETY: the range was just committed. Best effort: an unlocked page still works.
        let _ = unsafe { VirtualLock(base.as_ptr().cast(), bytes) };
        Some(Self {
            base,
            frames,
            latency,
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // SAFETY: `base` came from VirtualAlloc and is released once.
        let _ = unsafe { VirtualFree(self.base.as_ptr().cast(), 0, MEM_RELEASE) };
    }
}

/// The slot's ring section, mapped for writing.
struct Ring {
    view: NonNull<u8>,
    mapping: HANDLE,
}

// SAFETY: the view is shared memory; endpoints move `write_pos` only through
// `proto::publish`, and every access goes through raw pointers or atomics.
unsafe impl Send for Ring {}
// SAFETY: as above.
unsafe impl Sync for Ring {}

impl Ring {
    fn open(session: u32, slot: u8) -> Option<Self> {
        let name = HSTRING::from(proto::ring_name(session, slot));
        // SAFETY: opens a named section for read/write; the handle is owned below.
        let mapping =
            unsafe { OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, &name) }.ok()?;
        // SAFETY: maps the whole section the seat host sized to SECTION_BYTES.
        let view = unsafe {
            MapViewOfFile(
                mapping,
                FILE_MAP_READ | FILE_MAP_WRITE,
                0,
                0,
                proto::SECTION_BYTES,
            )
        };
        let Some(view) = NonNull::new(view.Value.cast::<u8>()) else {
            // SAFETY: the handle was opened above and is not used again.
            let _ = unsafe { CloseHandle(mapping) };
            return None;
        };
        let ring = Self { view, mapping };
        if !ring.header().belongs_to(session, slot) {
            return None;
        }
        // SAFETY: the mapped range is SECTION_BYTES long. Best effort, as for `Scratch`.
        let _ = unsafe { VirtualLock(view.as_ptr().cast(), proto::SECTION_BYTES) };
        Some(ring)
    }

    /// The header's fixed fields; the magic is read first, with Acquire, as it was written last.
    fn header(&self) -> RingHeader {
        let field = |offset: usize| {
            // SAFETY: header fields are aligned u32s inside the mapped view.
            unsafe { AtomicU32::from_ptr(self.view.as_ptr().add(offset).cast()) }
        };
        let magic = field(0).load(Ordering::Acquire);
        RingHeader {
            magic,
            version: field(4).load(Ordering::Relaxed),
            session: field(8).load(Ordering::Relaxed),
            slot: field(12).load(Ordering::Relaxed),
            ring_frames: field(16).load(Ordering::Relaxed),
            ..RingHeader::new(0, 0)
        }
    }

    fn write_pos(&self) -> &AtomicU64 {
        // SAFETY: `write_pos` is an aligned u64 inside the mapped view, shared as an atomic.
        unsafe { AtomicU64::from_ptr(self.view.as_ptr().add(proto::WRITE_POS_OFFSET).cast()) }
    }

    /// Append `frames` from `source`, or that many silent frames, and publish them. Each
    /// activation of a slot is an endpoint of its own on this ring, and [`proto::publish`] keeps
    /// two of them from moving the cursor back.
    fn write(&self, source: NonNull<u8>, frames: u32, silent: bool) {
        let cursor = self.write_pos();
        let from = cursor.load(Ordering::Relaxed);
        let mut pos = from;
        let mut left = u64::from(frames);
        let mut done = 0_usize;
        while left > 0 {
            let index = proto::ring_index(pos);
            let run = left.min(RING_FRAMES - index as u64) as usize;
            // SAFETY: `index + run` stays inside the ring; `source` holds `frames` frames.
            unsafe {
                let target = self
                    .view
                    .as_ptr()
                    .add(proto::RING_OFFSET + index * FRAME_BYTES);
                if silent {
                    core::ptr::write_bytes(target, 0, run * FRAME_BYTES);
                } else {
                    let from = source.as_ptr().add(done * FRAME_BYTES);
                    core::ptr::copy_nonoverlapping(from, target, run * FRAME_BYTES);
                }
            }
            pos += run as u64;
            left -= run as u64;
            done += run;
        }
        proto::publish(cursor, from, u64::from(frames));
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the view and handle were created in `open` and are released once.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.as_ptr().cast(),
            });
            let _ = CloseHandle(self.mapping);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The usual QueryPerformanceCounter rate.
    const FREQUENCY: i64 = 10_000_000;

    /// A `Start` after a `Stop` leaves nothing queued, however long the endpoint had rendered.
    #[test]
    fn a_restart_resumes_where_the_written_frames_end() {
        let endpoint = Endpoint::new(1, 0);
        let second = endpoint.frequency;
        let ten_minutes = 10 * 60 * u64::from(RATE);
        endpoint.begin_clock(1_000 * second);
        endpoint.written.store(ten_minutes, Ordering::Release);
        // The engine stops the endpoint and starts it again twenty minutes on, with no `Reset`.
        endpoint.started.store(false, Ordering::Release);
        let later = 2_200 * second;
        endpoint.begin_clock(later);
        assert_eq!(endpoint.played_at(ten_minutes, later), ten_minutes);

        // Ten milliseconds the engine writes next play out over the next ten milliseconds.
        let ahead = ten_minutes + u64::from(RATE) / 100;
        assert_eq!(endpoint.played_at(ahead, later), ten_minutes);
        assert_eq!(endpoint.played_at(ahead, later + second / 100), ahead);
    }

    #[test]
    fn played_frames_never_pass_the_written_ones() {
        assert_eq!(played_frames(100, FREQUENCY, FREQUENCY), 100);
        assert_eq!(played_frames(100, -5, FREQUENCY), 0);
        assert_eq!(played_frames(10, i64::MAX, FREQUENCY), 10);
    }
}
