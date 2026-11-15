//! [`Drive`] on one thread: a scripted pool, the fake encoder and a section in plain memory.
//!
//! The pool also plays the host. It hands out the script's frames whatever the credit, queues
//! a control op between two of them, reads each published slot back when the loop releases
//! its frame, and signals stop once the script has run out and nothing is owed. Nothing here
//! waits on a clock, so a run is the same every time.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::offset_of;
use std::os::windows::io::RawHandle;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytemuck::Zeroable;
use pf_driver_proto::encode::SetEncodeRequest;
use pf_driver_proto::encode::au::{self, AuHeader, AuSlot};
use pf_encode_core::fake::FakeEncoder;
use pf_encode_core::rfi::{MidWave, RecoveryPolicy};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::core::PCWSTR;

use super::{Drive, FrameSource, MAX_SUBMIT_FAILURES, Rates, Slot};
use crate::section::{AuSection, Ctl, EncodeSession, SectionView};
use crate::{Fail, lock};

/// The section as plain memory. Every header and slot field is one aligned word of one width,
/// so the two atomic views never name the same bytes.
struct Mem {
    header: [u8; au::AU_HEADER_SIZE],
    words32: Vec<AtomicU32>,
    words64: Vec<AtomicU64>,
    heap: Mutex<Vec<u8>>,
}

impl Mem {
    fn new() -> Arc<Self> {
        let stamped = AuHeader {
            magic: au::AU_MAGIC,
            version: au::AU_VERSION,
            heap_offset: au::HEAP_OFFSET as u32,
            heap_bytes: au::HEAP_MIN_BYTES,
            slot_table_offset: au::SLOT_TABLE_OFFSET as u32,
            slot_count: au::AU_SLOTS,
            ..AuHeader::zeroed()
        };
        let mut header = [0u8; au::AU_HEADER_SIZE];
        header.copy_from_slice(bytemuck::bytes_of(&stamped));
        Arc::new(Self {
            header,
            words32: (0..au::HEAP_OFFSET / 4)
                .map(|_| AtomicU32::new(0))
                .collect(),
            words64: (0..au::HEAP_OFFSET / 8)
                .map(|_| AtomicU64::new(0))
                .collect(),
            heap: Mutex::new(vec![0; au::HEAP_MIN_BYTES as usize]),
        })
    }

    fn word(&self, off: usize) -> u32 {
        self.words32[off / 4].load(Ordering::Acquire)
    }

    fn slot_word(&self, slot: usize, field: usize) -> u32 {
        self.word(au::slot_offset(slot) + field)
    }

    fn published(&self) -> usize {
        (0..au::AU_SLOTS as usize)
            .filter(|&i| self.slot_word(i, offset_of!(AuSlot, state)) == au::PUBLISHED)
            .count()
    }
}

struct MemView(Arc<Mem>);

impl SectionView for MemView {
    fn atomic_u32(&self, off: usize) -> &AtomicU32 {
        &self.0.words32[off / 4]
    }

    fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        &self.0.words64[off / 8]
    }

    fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        dst.copy_from_slice(&self.0.header[off..off + dst.len()]);
    }

    fn copy_from_slice(&self, off: usize, src: &[u8]) {
        let at = off - au::HEAP_OFFSET;
        lock(&self.0.heap)[at..at + src.len()].copy_from_slice(src);
    }
}

/// An event handle as a plain word: `HANDLE` is not `Sync`, and the pool has to be.
type Event = usize;

fn event(manual_reset: bool) -> Event {
    // SAFETY: an unnamed event with default security; the call reads no memory of ours.
    let made = unsafe { CreateEventW(None, manual_reset, false, PCWSTR::null()) };
    made.expect("create event").0 as Event
}

fn raw(event: Event) -> RawHandle {
    event as *mut c_void
}

fn signal(event: Event) {
    // SAFETY: an event this test made and has not closed.
    unsafe { SetEvent(HANDLE(raw(event))) }.expect("signal event");
}

fn close(event: Event) {
    // SAFETY: an event this test made, closed once, after the loop waiting on it returned.
    unsafe { CloseHandle(HANDLE(raw(event))) }.expect("close event");
}

fn session(mem: &Arc<Mem>, wire_seq_base: u32) -> EncodeSession {
    let request = SetEncodeRequest {
        wire_seq_base,
        ..SetEncodeRequest::zeroed()
    };
    let bytes = au::section_bytes(au::HEAP_MIN_BYTES);
    let ready = raw(event(false));
    // SAFETY: `ready` is a live event made above and owned by nothing else: the section
    // closes it.
    let section = unsafe { AuSection::adopt(Box::new(MemView(mem.clone())), bytes, ready) };
    EncodeSession::new(request, section.expect("adopt section"), 1)
}

enum Step {
    Frame,
    /// Queued for the loop to drain before the next frame.
    Ctl(Ctl),
}

#[derive(Default)]
struct PoolState {
    script: VecDeque<Step>,
    taken: u64,
    owed: usize,
    /// Slots already read, where the host leaves them published.
    seen: u32,
    /// `(wire_seq, flags)` of each slot read, in publish order.
    aus: Vec<(u32, u32)>,
}

struct ScriptedPool<'a> {
    session: &'a EncodeSession,
    mem: Arc<Mem>,
    wake: Event,
    stop: Event,
    /// The host frees each slot it has read; `false` leaves them published.
    frees: bool,
    state: Mutex<PoolState>,
}

impl FrameSource for ScriptedPool<'_> {
    fn set_live(&self, _live: bool) {}

    fn take_within(&self, _budget: usize) -> (Option<Slot>, u64) {
        let mut st = lock(&self.state);
        match st.script.pop_front() {
            Some(Step::Frame) => {
                st.taken += 1;
                st.owed += 1;
                (Some((0, 0, st.taken)), 0)
            }
            Some(Step::Ctl(op)) => {
                self.session.push_ctl(op);
                signal(self.wake);
                (None, 0)
            }
            None => {
                if st.owed == 0 {
                    signal(self.stop);
                }
                (None, 0)
            }
        }
    }

    fn republish(&self) -> Option<Slot> {
        None
    }

    fn cursor_pending(&self) -> bool {
        false
    }

    fn cursor_republish(&self) -> Option<Slot> {
        None
    }

    fn cursor_changed(&self) {}

    fn frame(&self, _slot: usize, pts_ns: u64) -> Result<CapturedFrame, Fail> {
        Ok(CapturedFrame {
            width: 16,
            height: 16,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Pixelless,
            cursor: None,
            provenance: Default::default(),
        })
    }

    fn release(&self, _slot: usize) {
        let mut st = lock(&self.state);
        for i in 0..au::AU_SLOTS as usize {
            let fresh = st.seen & (1 << i) == 0;
            if fresh && self.session.section.slot_state(i).load(Ordering::Acquire) == au::PUBLISHED
            {
                let wire_seq = self.mem.slot_word(i, offset_of!(AuSlot, wire_seq));
                let flags = self.mem.slot_word(i, offset_of!(AuSlot, flags));
                st.aus.push((wire_seq, flags));
                if self.frees {
                    let state = self.session.section.slot_state(i);
                    state.store(au::FREE, Ordering::Release);
                } else {
                    st.seen |= 1 << i;
                }
            }
        }
        st.owed -= 1;
        if st.script.is_empty() && st.owed == 0 {
            signal(self.stop);
        }
    }

    fn event(&self) -> RawHandle {
        raw(self.wake)
    }

    fn has_full(&self) -> bool {
        !lock(&self.state).script.is_empty()
    }

    fn drop_one(&self) -> u64 {
        0
    }
}

/// Run `script` to its end, or until the loop leaves on its own. Returns what the host read
/// and how many frames the loop took.
fn run(
    mem: &Arc<Mem>,
    session: &EncodeSession,
    enc: FakeEncoder,
    script: Vec<Step>,
    frees: bool,
) -> (Vec<(u32, u32)>, u64) {
    drive(mem, session, enc, script, frees, true)
}

/// [`run`] on a thread that is `live`, or on one its successor detached.
fn drive(
    mem: &Arc<Mem>,
    session: &EncodeSession,
    enc: FakeEncoder,
    script: Vec<Step>,
    frees: bool,
    live: bool,
) -> (Vec<(u32, u32)>, u64) {
    let pool = ScriptedPool {
        session,
        mem: mem.clone(),
        wake: event(false),
        stop: event(true),
        frees,
        state: Mutex::new(PoolState {
            script: script.into(),
            ..PoolState::default()
        }),
    };
    let live = AtomicBool::new(live);
    let rates = Rates::off_panel(1000, 1_000_000);
    let stop = raw(pool.stop);
    Drive::new(Box::new(enc), &pool, session, stop, &live, rates, 0, None).run();
    close(pool.wake);
    close(pool.stop);
    let st = lock(&pool.state);
    (st.aus.clone(), st.taken)
}

/// A three-frame wave the driver owns, as NVENC's is.
const WAVE: RecoveryPolicy = RecoveryPolicy {
    cycle: Some(3),
    mid_wave: MidWave::Queue,
    abandons: false,
};
const WHOLE: u32 = au::AU_FIRST | au::AU_LAST;
const KEY: u32 = WHOLE | au::AU_KEYFRAME;
const POINT: u32 = WHOLE | au::AU_RECOVERY_POINT;
const CLOSE: u32 = POINT | au::AU_RECOVERY_CLOSE;

fn frames(n: usize) -> Vec<Step> {
    (0..n).map(|_| Step::Frame).collect()
}

/// A loss with no anchor starts a wave, and one reported while it runs spoils it: the first
/// close reaches the section unmarked, and the wave queued behind it carries both marks.
#[test]
fn a_loss_during_a_wave_is_published_as_a_queued_wave() {
    let mem = Mem::new();
    let session = session(&mem, 100);
    let mut script = frames(2);
    script.push(Step::Ctl(Ctl::InvalidateRefFrames(100, 101)));
    script.extend(frames(2));
    script.push(Step::Ctl(Ctl::InvalidateRefFrames(100, 103)));
    script.extend(frames(5));
    let (aus, taken) = run(&mem, &session, FakeEncoder::new(WAVE), script, true);
    assert_eq!(taken, 9);
    assert_eq!(
        aus,
        [
            (100, KEY),
            (101, WHOLE),
            (102, POINT),
            (103, WHOLE),
            (104, WHOLE),
            (105, POINT),
            (106, WHOLE),
            (107, CLOSE),
            (108, WHOLE),
        ]
    );
    assert_eq!(
        mem.word(offset_of!(AuHeader, encoder_state)),
        au::ENCODER_ENCODING
    );
}

/// A detached thread back from its wedge owns nothing: it leaves at the loop top, and the op
/// queued meanwhile stays for the thread that replaced it.
#[test]
fn a_detached_thread_drains_no_mailbox() {
    let mem = Mem::new();
    let session = session(&mem, 100);
    session.push_ctl(Ctl::RequestKeyframe);
    let (aus, taken) = drive(
        &mem,
        &session,
        FakeEncoder::new(WAVE),
        frames(2),
        true,
        false,
    );
    assert!(aus.is_empty());
    assert_eq!(taken, 0);
    assert_eq!(
        session.take_ctl().len(),
        1,
        "the keyframe request is the successor's"
    );
}

/// The host's reset: the thread stops, the session rebases, a fresh encoder starts. The old
/// slots are freed and the next stream opens with an IDR on the new base.
#[test]
fn a_reset_frees_the_slots_and_restamps_from_the_new_base() {
    let mem = Mem::new();
    let session = session(&mem, 100);
    let (aus, _) = run(&mem, &session, FakeEncoder::new(WAVE), frames(3), false);
    assert_eq!(aus, [(100, KEY), (101, WHOLE), (102, WHOLE)]);
    assert_eq!(mem.published(), 3);

    session.rebase(500);
    assert_eq!(mem.published(), 0);
    assert_eq!(mem.word(offset_of!(AuHeader, wire_seq_base)), 500);
    let (aus, _) = run(&mem, &session, FakeEncoder::new(WAVE), frames(2), false);
    assert_eq!(aus, [(500, KEY), (501, WHOLE)]);
}

/// A slot the host is still copying out across a reset keeps its bytes: the next stream's
/// first access unit lands past them, not at the heap's start.
#[test]
fn a_reset_places_past_a_slot_the_host_still_reads() {
    let mem = Mem::new();
    let session = session(&mem, 100);
    run(&mem, &session, FakeEncoder::new(WAVE), frames(3), false);
    let held = session.section.slot(0);
    assert_eq!(held.offset, au::HEAP_OFFSET as u32);
    let taken = session.section.slot_state(0).compare_exchange(
        au::PUBLISHED,
        au::READING,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    assert_eq!(taken, Ok(au::PUBLISHED));

    session.rebase(500);
    let (aus, _) = run(&mem, &session, FakeEncoder::new(WAVE), frames(2), false);
    assert_eq!(aus, [(500, KEY), (501, WHOLE)]);
    let idr = (0..au::AU_SLOTS as usize)
        .map(|i| session.section.slot(i))
        .find(|s| s.state == au::PUBLISHED && s.wire_seq == 500)
        .expect("the IDR's slot");
    println!("held {}..+{}, IDR at {}", held.offset, held.len, idr.offset);
    assert!(idr.offset >= held.offset + held.len, "IDR over held bytes");
}

/// A terminal submit error ends the thread on the first submit. Any other error is retried on
/// each new frame, and the thread leaves after [`MAX_SUBMIT_FAILURES`] in a row. Both mark the
/// encoder wedged.
#[test]
fn a_terminal_submit_error_ends_the_thread_at_once() {
    for (terminal, submits) in [(true, 1), (false, u64::from(MAX_SUBMIT_FAILURES))] {
        let mem = Mem::new();
        let session = session(&mem, 100);
        let mut enc = FakeEncoder::new(WAVE);
        (enc.fail_submits, enc.fail_terminal) = (u32::MAX, terminal);
        let (aus, taken) = run(&mem, &session, enc, frames(20), true);
        assert_eq!(taken, submits, "terminal: {terminal}");
        assert!(aus.is_empty());
        assert_eq!(
            mem.word(offset_of!(AuHeader, encoder_state)),
            au::ENCODER_WEDGED
        );
    }
}

/// A panel at a multiple of the stream's rate hands the stream its fraction. A whole panel, or
/// one the stream does not divide, leaves the whole rate.
#[test]
fn a_stream_takes_its_fraction_from_the_panel() {
    for (fps, panel_mhz, fps_mhz) in [
        (60, 59_940, 59_940),
        (60, 119_880, 59_940),
        (120, 119_880, 119_880),
        (60, 60_000, 60_000),
        (60, 120_000, 60_000),
        (60, 143_856, 60_000),
        (120, 59_940, 120_000),
    ] {
        let rates = Rates::off_panel(fps, panel_mhz);
        assert_eq!(
            (rates.fps_mhz, rates.panel_mhz),
            (fps_mhz, panel_mhz),
            "{fps} {panel_mhz}"
        );
    }
}
