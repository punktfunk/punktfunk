//! A seat host's pad audio: the slot's ring that `pf_seat_audio.dll` fills from audiodg.
//!
//! A seat session lists no local audio endpoint, so the seat listener's enumerator gives it one
//! per pad slot (`pf_driver_proto::seat_audio`). Creating a [`SeatPadSource`] makes the slot's
//! endpoint appear in this session; dropping it takes the endpoint away.

use crate::AudioCapturer;
use anyhow::{bail, Context, Result};
use pf_driver_proto::seat_audio::{self as proto, RingHeader, FRAME_BYTES, RATE};
use pf_paths_win::SecurityAttributes;
use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
    WIN32_ERROR,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSWinStationName, WTS_CURRENT_SERVER_HANDLE,
    WTS_CURRENT_SESSION,
};
use windows::Win32::System::Threading::CreateEventW;

/// The ring: SYSTEM, and audiodg (LocalService) writing.
const RING_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GRGW;;;LS)";
/// The ownership event: anyone signed in may wait on it, which is all a presence check does.
const OWNER_SDDL: &str = "D:P(A;;GA;;;SY)(A;;0x00100000;;;AU)";
/// Frames per chunk handed to the pad framer: 20 ms.
const CHUNK_FRAMES: u64 = 960;
/// An unbounded read reports idle after this long without frames.
const IDLE: Duration = Duration::from_secs(5);

/// Whether this session came in through the seat listener, whose enumerator serves pad
/// endpoints. A seat on plain `RDP-Tcp` has Remote Audio only.
pub fn available() -> bool {
    let mut name = PWSTR::null();
    let mut bytes = 0;
    // SAFETY: queries this session's WinStation name into a buffer WTS allocates.
    let queried = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            WTS_CURRENT_SESSION,
            WTSWinStationName,
            &mut name,
            &mut bytes,
        )
    };
    if queried.is_err() || name.is_null() {
        return false;
    }
    // SAFETY: WTS returned a nul-terminated string, freed right after.
    let station = unsafe { name.to_string() }.unwrap_or_default();
    // SAFETY: the buffer came from WTSQuerySessionInformationW.
    unsafe { WTSFreeMemory(name.0.cast()) };
    station
        .to_ascii_lowercase()
        .starts_with(&proto::LISTENER.to_ascii_lowercase())
}

/// This seat's claim on one pad slot: the ownership event and the ring section.
pub struct SeatPadSource {
    owner: Option<Arc<OwnedHandle>>,
    mapping: HANDLE,
    view: NonNull<u8>,
}

/// Each slot's ownership event while a source of this process holds it. A streamer restarted on
/// its slot opens before the last one has wound down, and shares that one's event.
static OWNERS: Mutex<[Weak<OwnedHandle>; proto::SLOTS as usize]> =
    Mutex::new([const { Weak::new() }; proto::SLOTS as usize]);

/// The slot's ownership event, shared with a live source of this process, else made here. One
/// that exists without such a source is someone else's, so the slot is refused rather than
/// adopted.
fn claim(session: u32, slot: u8) -> Result<Arc<OwnedHandle>> {
    let mut owners = OWNERS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(owner) = owners[usize::from(slot)].upgrade() {
        return Ok(owner);
    }
    let owner_sa =
        SecurityAttributes::new(OWNER_SDDL).context("build a pad ring security descriptor")?;
    let name = proto::owner_name(session, slot);
    let wide = HSTRING::from(name.as_str());
    // SAFETY: clears the thread's error slot, so ERROR_ALREADY_EXISTS below is the create's.
    unsafe { SetLastError(WIN32_ERROR(0)) };
    // SAFETY: a named manual-reset event that only marks presence, scoped by `owner_sa`.
    let event = unsafe { CreateEventW(Some(owner_sa.as_ptr()), true, false, &wide) }
        .with_context(|| format!("create {name}"))?;
    // SAFETY: read before any other call can touch the thread's error slot.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // SAFETY: the create returned a handle this process alone owns from here.
    let event = Arc::new(unsafe { OwnedHandle::from_raw_handle(event.0) });
    if existed {
        bail!("{name} already exists: another process holds this pad slot");
    }
    owners[usize::from(slot)] = Arc::downgrade(&event);
    Ok(event)
}

// SAFETY: the handles are process-wide; the view is shared memory read through atomics and
// raw pointers only.
unsafe impl Send for SeatPadSource {}
// SAFETY: as above.
unsafe impl Sync for SeatPadSource {}

impl SeatPadSource {
    /// Claim `slot` for this session. The ring exists before the endpoint is announced. A slot
    /// whose ownership event another process made is refused ([`claim`]).
    pub fn create(slot: u8) -> Result<Arc<Self>> {
        if slot >= proto::SLOTS {
            bail!("pad slot {slot} is past the seat audio table");
        }
        let session = pf_win_display::own_session_id().context("read this host's session")?;
        let ring_sa =
            SecurityAttributes::new(RING_SDDL).context("build a pad ring security descriptor")?;
        let name = HSTRING::from(proto::ring_name(session, slot));
        let size = u32::try_from(proto::SECTION_BYTES).context("size the pad ring")?;
        // An existing ring is taken as it is: audiodg holds a restarted host's ring open.
        // SAFETY: a pagefile-backed section, named and scoped by `ring_sa`.
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(ring_sa.as_ptr()),
                PAGE_READWRITE,
                0,
                size,
                &name,
            )
        }
        .with_context(|| format!("create {}", proto::ring_name(session, slot)))?;
        // SAFETY: maps the whole section just created.
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
            // SAFETY: the handle was created above and is not used again.
            let _ = unsafe { CloseHandle(mapping) };
            bail!("map {}", proto::ring_name(session, slot));
        };
        let mut source = Self {
            owner: None,
            mapping,
            view,
        };
        source.publish(RingHeader::new(session, slot));
        source.owner = Some(claim(session, slot)?);
        Ok(Arc::new(source))
    }

    /// A reader starting at the newest frame.
    pub fn capturer(self: &Arc<Self>) -> SeatPadCapturer {
        SeatPadCapturer {
            read: self.write_pos(),
            source: self.clone(),
        }
    }

    /// Header fields first, the magic last with Release.
    fn publish(&mut self, header: RingHeader) {
        let field = |offset: usize| {
            // SAFETY: header fields are aligned u32s inside the mapped view.
            unsafe { AtomicU32::from_ptr(self.view.as_ptr().add(offset).cast()) }
        };
        field(4).store(header.version, Ordering::Relaxed);
        field(8).store(header.session, Ordering::Relaxed);
        field(12).store(header.slot, Ordering::Relaxed);
        field(16).store(header.ring_frames, Ordering::Relaxed);
        field(0).store(header.magic, Ordering::Release);
    }

    fn write_pos(&self) -> u64 {
        // SAFETY: `write_pos` is an aligned u64 inside the mapped view, shared as an atomic.
        unsafe { AtomicU64::from_ptr(self.view.as_ptr().add(proto::WRITE_POS_OFFSET).cast()) }
            .load(Ordering::Acquire)
    }

    /// Frame `pos` as four floats, FL FR BL BR.
    fn frame(&self, pos: u64, out: &mut Vec<f32>) {
        let at = proto::RING_OFFSET + proto::ring_index(pos) * FRAME_BYTES;
        let mut bytes = [0_u8; FRAME_BYTES];
        // SAFETY: one frame inside the ring. The endpoint may be writing elsewhere; a lapped
        // read is caught by `readable` before the next chunk.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.view.as_ptr().add(at),
                bytes.as_mut_ptr(),
                FRAME_BYTES,
            );
        }
        out.extend(
            bytes
                .chunks_exact(2)
                .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32768.0),
        );
    }
}

impl Drop for SeatPadSource {
    fn drop(&mut self) {
        // The endpoint leaves before its ring.
        drop(self.owner.take());
        // SAFETY: the view and the mapping were created in `create` and are released once.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.as_ptr().cast(),
            });
            let _ = CloseHandle(self.mapping);
        }
    }
}

/// Reads the slot's ring as interleaved 4-channel float.
pub struct SeatPadCapturer {
    source: Arc<SeatPadSource>,
    read: u64,
}

impl AudioCapturer for SeatPadCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        self.next_chunk_within(IDLE)
    }

    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        let deadline = Instant::now() + budget;
        loop {
            let (from, count) = proto::readable(self.source.write_pos(), self.read);
            if count > 0 {
                let take = count.min(CHUNK_FRAMES);
                let mut chunk = Vec::with_capacity(take as usize * proto::CHANNELS);
                for pos in from..from + take {
                    self.source.frame(pos, &mut chunk);
                }
                self.read = from + take;
                return Ok(chunk);
            }
            self.read = from;
            if Instant::now() >= deadline {
                return Ok(Vec::new());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn channels(&self) -> u32 {
        proto::CHANNELS as u32
    }

    fn sample_rate(&self) -> u32 {
        RATE
    }

    fn drain(&mut self) {
        self.read = self.source.write_pos();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A restarted streamer shares its slot's event; one another process made is refused until
    /// it goes.
    #[test]
    fn a_slot_is_shared_in_process_and_refused_when_squatted() {
        // A session no seat has, on the last slot.
        let (session, slot) = (u32::MAX, proto::SLOTS - 1);
        let first = claim(session, slot).unwrap();
        let again = claim(session, slot).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        drop((first, again));
        let name = HSTRING::from(proto::owner_name(session, slot));
        // SAFETY: a named event this test owns and closes below.
        let squatter = unsafe { CreateEventW(None, true, false, &name) }.unwrap();
        assert!(claim(session, slot).is_err());
        // SAFETY: the handle was created above and is closed once.
        let _ = unsafe { CloseHandle(squatter) };
        assert!(claim(session, slot).is_ok());
    }
}
