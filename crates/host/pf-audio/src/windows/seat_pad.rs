//! A seat host's pad audio: the slot's ring that `pf_seat_audio.dll` fills from audiodg.
//!
//! A seat session lists no local audio endpoint, so the seat listener's enumerator gives it one
//! per pad slot (`pf_driver_proto::seat_audio`). Creating a [`SeatPadSource`] makes the slot's
//! endpoint appear in this session; dropping it takes the endpoint away.

use crate::AudioCapturer;
use anyhow::{bail, Context, Result};
use pf_driver_proto::seat_audio::{self as proto, RingHeader, FRAME_BYTES, RATE};
use pf_paths_win::SecurityAttributes;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSWinStationName, WTS_CURRENT_SERVER_HANDLE,
    WTS_CURRENT_SESSION,
};

/// The ring: SYSTEM, and audiodg (LocalService) writing.
const RING_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GRGW;;;LS)";
/// The presence section: SYSTEM writes it, anyone signed in reads it, which is all a presence
/// check does.
const PRESENCE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GR;;;AU)";
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

/// This seat's claim on one pad slot: its presence and the ring section.
pub struct SeatPadSource {
    presence: Option<Arc<Presence>>,
    mapping: HANDLE,
    view: NonNull<u8>,
}

/// One slot's presence section, mapped for writing. It holds [`proto::PRESENT_MAGIC`] while one
/// lives, and the last one of the slot zeroes it.
struct Presence {
    slot: u8,
    mapping: HANDLE,
    magic: NonNull<u32>,
}

// SAFETY: the handle is process-wide; the view is shared memory written through an atomic only.
unsafe impl Send for Presence {}
// SAFETY: as above.
unsafe impl Sync for Presence {}

/// Each slot's presence while a source of this process holds it. A streamer restarted on its
/// slot opens before the last one has wound down, and shares that one's presence.
static PRESENT: Mutex<[Weak<Presence>; proto::SLOTS as usize]> =
    Mutex::new([const { Weak::new() }; proto::SLOTS as usize]);

fn held() -> MutexGuard<'static, [Weak<Presence>; proto::SLOTS as usize]> {
    PRESENT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The slot's presence, shared with a live source of this process, else published here. A
/// section that exists already is taken: only a privileged process can make one in `Global\`,
/// and a reader that holds an old one open keeps it zeroed, not present.
fn claim(session: u32, slot: u8) -> Result<Arc<Presence>> {
    let mut held = held();
    if let Some(presence) = held[usize::from(slot)].upgrade() {
        return Ok(presence);
    }
    let sa = SecurityAttributes::new(PRESENCE_SDDL)
        .context("build a pad presence security descriptor")?;
    let name = proto::presence_name(session, slot);
    let size = u32::try_from(proto::PRESENCE_BYTES).context("size the pad presence")?;
    // SAFETY: a pagefile-backed section, named and scoped by `sa`.
    let mapping = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            Some(sa.as_ptr()),
            PAGE_READWRITE,
            0,
            size,
            &HSTRING::from(name.as_str()),
        )
    }
    .with_context(|| format!("create {name}"))?;
    // SAFETY: maps the section's first PRESENCE_BYTES, which it holds whoever created it.
    let view = unsafe {
        MapViewOfFile(
            mapping,
            FILE_MAP_READ | FILE_MAP_WRITE,
            0,
            0,
            proto::PRESENCE_BYTES,
        )
    };
    let Some(magic) = NonNull::new(view.Value.cast::<u32>()) else {
        // SAFETY: the handle was created above and is not used again.
        let _ = unsafe { CloseHandle(mapping) };
        bail!("map {name}");
    };
    let presence = Arc::new(Presence {
        slot,
        mapping,
        magic,
    });
    presence.store(proto::PRESENT_MAGIC);
    held[usize::from(slot)] = Arc::downgrade(&presence);
    Ok(presence)
}

impl Presence {
    fn store(&self, magic: u32) {
        // SAFETY: the view is page-aligned and mapped for writing until `drop` unmaps it.
        unsafe { AtomicU32::from_ptr(self.magic.as_ptr()) }.store(magic, Ordering::Release);
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        // A newer claim on the slot has written the magic again; it stays.
        let held = held();
        if held[usize::from(self.slot)].upgrade().is_none() {
            self.store(0);
        }
        drop(held);
        // SAFETY: the view and the mapping were created in `claim` and are released once.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.magic.as_ptr().cast(),
            });
            let _ = CloseHandle(self.mapping);
        }
    }
}

// SAFETY: the handles are process-wide; the view is shared memory read through atomics and
// raw pointers only.
unsafe impl Send for SeatPadSource {}
// SAFETY: as above.
unsafe impl Sync for SeatPadSource {}

impl SeatPadSource {
    /// Claim `slot` for this session. The ring exists before the endpoint is announced
    /// ([`claim`]).
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
            presence: None,
            mapping,
            view,
        };
        source.publish(RingHeader::new(session, slot));
        source.presence = Some(claim(session, slot)?);
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
        drop(self.presence.take());
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

    /// A slot is present while a source of this process holds it, and a restarted streamer shares
    /// that. A reader holding the section open, as another session's user may, keeps it absent
    /// once the last source goes, and the next claim takes the section back.
    #[test]
    fn a_slot_is_present_only_while_a_source_holds_it() {
        use windows::Win32::System::Memory::OpenFileMappingW;
        // A session no seat has, on the last slot.
        let (session, slot) = (u32::MAX, proto::SLOTS - 1);
        let Ok(first) = claim(session, slot) else {
            eprintln!("skipped: a Global section needs SeCreateGlobalPrivilege");
            return;
        };
        let again = claim(session, slot).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        let name = HSTRING::from(proto::presence_name(session, slot));
        // SAFETY: opens the section claimed above for reading; closed below.
        let reader = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, &name) }.unwrap();
        // SAFETY: maps the section's first PRESENCE_BYTES read-only; unmapped below.
        let view = unsafe { MapViewOfFile(reader, FILE_MAP_READ, 0, 0, proto::PRESENCE_BYTES) };
        assert!(!view.Value.is_null());
        // SAFETY: the view is page-aligned and stays mapped until the end of the test.
        let magic = || unsafe { AtomicU32::from_ptr(view.Value.cast()) }.load(Ordering::Acquire);
        assert_eq!(magic(), proto::PRESENT_MAGIC);
        drop((first, again));
        assert_eq!(magic(), 0, "a reader's handle keeps nothing present");
        let back = claim(session, slot).unwrap();
        assert_eq!(magic(), proto::PRESENT_MAGIC);
        drop(back);
        assert_eq!(magic(), 0);
        // SAFETY: the view and the handle were opened above and are released once.
        unsafe {
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(reader);
        }
    }
}
