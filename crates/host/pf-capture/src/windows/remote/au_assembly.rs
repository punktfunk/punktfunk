//! The wire chunks and whole access units the driver-encode proxy hands the stream loop,
//! assembled from the slots [`AuReader`] takes. One slot is one [`AuChunk`]; a whole AU
//! concatenates its chunks until LAST, and a FIRST mid-assembly drops the prefix. Each chunk
//! records the progress clocks the supervisor reads and goes to the AU dump when one is open.
//!
//! Pure over the reader, a [`Clocks`] and a wait, so the same code drains the driver's section
//! on Windows and a `Vec` in the tests. The section, its event and the control calls are the
//! proxy's (`encoder.rs`).

use super::au_reader::{AuReader, Taken};
use anyhow::{bail, Context, Result};
use pf_driver_proto::encode::au;
use pf_encode_core::{AuChunk, EncodedFrame};
use pf_frame::health::DriverSplit;
use std::time::{Duration, Instant};

/// The clocks a remote source runs on: its monotonic now, wall time for the wire's `pts_ns`,
/// and the QPC domain the driver's slot stamps are in. One value per source, so a test runs
/// the whole session on a virtual clock.
#[derive(Clone, Copy)]
pub(crate) struct Clocks {
    /// The source's, its supervisor's and the proxy's telemetry's now.
    pub now: fn() -> Instant,
    /// Now, in epoch nanoseconds.
    pub now_ns: fn() -> u64,
    /// Age of a QPC stamp in µs; 0 when the stamp is ahead.
    pub qpc_age_us: fn(u64) -> u64,
    /// A span of QPC ticks in µs.
    pub qpc_ticks_us: fn(u64) -> u64,
}

impl Clocks {
    /// A driver `qpc_pts` as the wire's epoch-nanosecond `pts_ns`: now minus the stamp's age.
    /// `0` (no stamp) reads as now.
    fn pts_ns(&self, qpc: u64) -> u64 {
        if qpc == 0 {
            return (self.now_ns)();
        }
        (self.now_ns)().saturating_sub((self.qpc_age_us)(qpc).saturating_mul(1000))
    }

    /// The driver's stamps on one slot as a split. `None` from a driver that wrote none (a zero
    /// submit) or stamps out of order. A present stamp of `0`, or one past the submit (a head
    /// that names the vblank the frame is for), leaves the pool wait unmeasured rather than
    /// negative.
    fn driver_split(&self, t: &Taken) -> Option<DriverSplit> {
        if t.qpc_submit == 0 || t.qpc_published < t.qpc_submit {
            return None;
        }
        let span = |ticks: u64| Duration::from_micros((self.qpc_ticks_us)(ticks));
        Some(DriverSplit {
            pool: (t.qpc_pts != 0 && t.qpc_submit >= t.qpc_pts)
                .then(|| span(t.qpc_submit - t.qpc_pts)),
            encode: span(t.qpc_published - t.qpc_submit),
            ipc: Duration::from_micros((self.qpc_age_us)(t.qpc_published)),
        })
    }
}

/// Access units to disk while `PUNKTFUNK_IDD_DIAG` is on, in the framing the S5 encode probe
/// wrote — `[u32 len][u64 pts_ns][u8 keyframe][bytes]` per chunk — so a field failure replays
/// through the same corpus tooling. A write error drops the dump; the stream never fails for it.
pub(crate) struct AuDump(std::io::BufWriter<std::fs::File>);

impl AuDump {
    /// One file in `dir` per encoder open, named by pid, target and open count, so neither two
    /// sessions nor a re-open share one.
    pub(crate) fn create(dir: &std::path::Path, target_id: u32) -> Option<Self> {
        static OPENS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let open = OPENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!(
            "pfvd-au-{}-{target_id}-{open}.bin",
            std::process::id()
        ));
        match std::fs::File::create(&path) {
            Ok(f) => {
                tracing::info!(path = %path.display(), "driver encode: dumping access units");
                Some(Self(std::io::BufWriter::new(f)))
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "driver encode: no AU dump");
                None
            }
        }
    }

    /// Append one chunk; `false` means the caller drops the dump.
    fn write(&mut self, c: &AuChunk) -> bool {
        use std::io::Write;
        let r = self
            .0
            .write_all(&(c.data.len() as u32).to_le_bytes())
            .and_then(|()| self.0.write_all(&c.pts_ns.to_le_bytes()))
            .and_then(|()| self.0.write_all(&[u8::from(c.keyframe)]))
            .and_then(|()| self.0.write_all(&c.data));
        if let Err(e) = r {
            tracing::warn!(error = %e, "driver encode: AU dump write failed — dump disabled");
            return false;
        }
        true
    }
}

/// The proxy's read side: the reader, the progress clocks of the last chunk it took, and the
/// dump.
pub(crate) struct AuAssembly<'a> {
    pub(crate) reader: AuReader<'a>,
    pub(crate) clocks: Clocks,
    /// The last taken chunk's `wire_seq` / `source_seq`: AU progress, the supervisor's second
    /// ground-truth clock (`progress`).
    pub(crate) last_wire_seq: u32,
    pub(crate) last_source_seq: u32,
    /// The access unit being taken re-encodes its predecessor's source frame.
    pub(crate) au_repeat: bool,
    /// Age of the last taken chunk's `qpc_pts` when the host took it — present→arrival.
    pub(crate) last_arrival: Option<Duration>,
    /// The driver's own split of that span, when its slot carried the stamps.
    pub(crate) last_split: Option<DriverSplit>,
    /// Bitstream capture while `PUNKTFUNK_IDD_DIAG` is on; dropped on the first write error.
    dump: Option<AuDump>,
}

impl<'a> AuAssembly<'a> {
    /// A slice never takes this long; past it the access unit is truncated and the loop's
    /// stall path resets the encoder.
    const CHUNK_WAIT: Duration = Duration::from_millis(500);
    /// A whole access unit closes within this of its first chunk: four slice waits.
    const AU_WAIT: Duration = Duration::from_secs(2);
    /// An open access unit past this is dropped: no wire carries a frame over the heap's cap.
    const MAX_AU_BYTES: usize = au::HEAP_MAX_BYTES as usize;

    pub(crate) fn new(reader: AuReader<'a>, clocks: Clocks, dump: Option<AuDump>) -> Self {
        Self {
            reader,
            clocks,
            last_wire_seq: 0,
            last_source_seq: 0,
            au_repeat: false,
            last_arrival: None,
            last_split: None,
            dump,
        }
    }

    /// One taken slot as a wire chunk, recording the progress clocks on the way through:
    /// the sequence pair the supervisor reads as encoder progress, and how old the OS present
    /// stamp already is — the ground truth for "late, not missing". The diagnostic dump takes
    /// its copy here, so a chunked session writes the same bytes in the same order.
    fn chunk(&mut self, t: Taken) -> AuChunk {
        self.last_wire_seq = t.wire_seq;
        self.au_repeat = t.repeat;
        self.last_source_seq = t.source_seq;
        self.last_arrival =
            (t.qpc_pts != 0).then(|| Duration::from_micros((self.clocks.qpc_age_us)(t.qpc_pts)));
        self.last_split = self.clocks.driver_split(&t);
        let chunk = AuChunk {
            data: t.data,
            pts_ns: self.clocks.pts_ns(t.qpc_pts),
            keyframe: t.flags & au::AU_KEYFRAME != 0,
            recovery_anchor: t.flags & au::AU_RECOVERY_ANCHOR != 0,
            recovery_point: t.flags & au::AU_RECOVERY_POINT != 0,
            recovery_close: t.flags & au::AU_RECOVERY_CLOSE != 0,
            chunk_aligned: t.flags & au::AU_CHUNK_ALIGNED != 0,
            first: t.flags & au::AU_FIRST != 0,
            last: t.flags & au::AU_LAST != 0,
        };
        if self.dump.as_mut().is_some_and(|d| !d.write(&chunk)) {
            self.dump = None;
        }
        chunk
    }

    /// The next wire chunk. Non-blocking between access units; inside one, the next chunk is
    /// owed and `wait(ms)` is called until it lands or [`Self::CHUNK_WAIT`] runs out.
    pub(crate) fn poll_chunk(&mut self, wait: &mut dyn FnMut(u32)) -> Result<Option<AuChunk>> {
        let deadline = self
            .reader
            .mid_au()
            .then(|| Instant::now() + Self::CHUNK_WAIT);
        loop {
            if let Some(t) = self.reader.take_next()? {
                return Ok(Some(self.chunk(t)));
            }
            let Some(deadline) = deadline else {
                return Ok(None);
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "driver encode: no further chunk of wire frame {} within {:?}",
                    self.reader.next_wire_seq(),
                    Self::CHUNK_WAIT
                );
            }
            wait(left.as_millis().clamp(1, 16) as u32);
        }
    }

    /// The next whole access unit: chunks concatenate in order until LAST closes it. A FIRST
    /// mid-assembly means the driver dropped this AU's tail and moved on: the prefix is
    /// discarded and the next AU starts over, as the loop's `poll_chunked` does for the
    /// streamed wire. A unit still open [`Self::AU_WAIT`] after its first chunk, or past
    /// [`Self::MAX_AU_BYTES`], is an error, as a late chunk is: the stall path resets past it.
    pub(crate) fn poll(&mut self, wait: &mut dyn FnMut(u32)) -> Result<Option<EncodedFrame>> {
        let start = |c: AuChunk| EncodedFrame {
            data: c.data,
            pts_ns: c.pts_ns,
            keyframe: c.keyframe,
            recovery_anchor: c.recovery_anchor,
            recovery_point: c.recovery_point,
            recovery_close: c.recovery_close,
            chunk_aligned: c.chunk_aligned,
        };
        let Some(first) = self.poll_chunk(wait)? else {
            return Ok(None);
        };
        let deadline = (self.clocks.now)() + Self::AU_WAIT;
        let mut last = first.last;
        let mut frame = start(first);
        while !last {
            let c = self
                .poll_chunk(wait)?
                .context("driver encode: access unit ended without its LAST chunk")?;
            last = c.last;
            if c.first {
                frame = start(c);
            } else {
                frame.data.extend_from_slice(&c.data);
                frame.keyframe |= c.keyframe;
            }
            let late = (self.clocks.now)() >= deadline;
            if !last && (late || frame.data.len() > Self::MAX_AU_BYTES) {
                bail!(
                    "driver encode: wire frame {} still open at {} bytes",
                    self.reader.next_wire_seq(),
                    frame.data.len()
                );
            }
        }
        Ok(Some(frame))
    }

    /// The `wire_seq` a `RESET` restarts the domain at: where this reader stands, so the
    /// loop's `au_seq` keeps matching. A half-read AU already spent its index on the wire: the
    /// loop closes that frame at the next FIRST, so the domain restarts past it.
    pub(crate) fn reset_base(&self) -> u32 {
        self.reader
            .next_wire_seq()
            .wrapping_add(u32::from(self.reader.mid_au()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::remote::au_reader::producer::{section, Producer};
    use crate::windows::remote::au_reader::AuView;
    use pf_driver_proto::section::SectionView;

    const WHOLE: u32 = au::AU_FIRST | au::AU_LAST;
    /// The test QPC runs at 1 MHz, so a tick is a microsecond, and stands at `QPC_NOW`.
    const QPC_NOW: u64 = 1_000_000;
    const NOW_NS: u64 = 5_000_000_000;

    fn now_ns() -> u64 {
        NOW_NS
    }

    fn qpc_age_us(stamp: u64) -> u64 {
        QPC_NOW.saturating_sub(stamp)
    }

    fn qpc_ticks_us(ticks: u64) -> u64 {
        ticks
    }

    const CLOCKS: Clocks = Clocks {
        now: Instant::now,
        now_ns,
        qpc_age_us,
        qpc_ticks_us,
    };

    fn assembly(mem: SectionView<'_>, wire_seq_base: u32) -> AuAssembly<'_> {
        AuAssembly::new(AuReader::new(AuView::new(mem), wire_seq_base), CLOCKS, None)
    }

    fn no_wait(_: u32) {}

    #[test]
    fn a_whole_access_unit_is_one_frame_with_its_flags() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 3, 40);
        let mut asm = assembly(mem, 40);
        assert!(
            asm.poll(&mut no_wait).unwrap().is_none(),
            "nothing published"
        );
        let marks = au::AU_KEYFRAME
            | au::AU_RECOVERY_ANCHOR
            | au::AU_RECOVERY_POINT
            | au::AU_RECOVERY_CLOSE
            | au::AU_CHUNK_ALIGNED;
        drv.publish(b"idr", 40, 9, QPC_NOW - 2_000, WHOLE | marks);
        drv.publish(b"p", 41, 10, 0, WHOLE);
        let f = asm.poll(&mut no_wait).unwrap().unwrap();
        assert_eq!(f.data, b"idr");
        assert_eq!(f.pts_ns, NOW_NS - 2_000_000, "now minus the present's age");
        assert!(f.keyframe && f.recovery_anchor && f.recovery_point);
        assert!(f.recovery_close && f.chunk_aligned);
        let f = asm.poll(&mut no_wait).unwrap().unwrap();
        assert_eq!(
            (f.data.as_slice(), f.pts_ns),
            (&b"p"[..], NOW_NS),
            "no stamp reads as now"
        );
        assert!(!f.keyframe && !f.recovery_anchor && !f.recovery_point);
        assert!(!f.recovery_close && !f.chunk_aligned);
        assert!(asm.poll(&mut no_wait).unwrap().is_none());
    }

    /// Inside an access unit the next chunk is owed: the poll waits on the section until the
    /// driver publishes it, and a keyframe bit on any chunk marks the frame.
    #[test]
    fn a_chunked_access_unit_assembles_in_order_while_the_poll_waits() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 0);
        let mut asm = assembly(mem, 0);
        drv.publish(b"aa", 0, 1, 0, au::AU_FIRST);
        let mut owed = [(&b"bb"[..], 0), (b"cc", au::AU_LAST | au::AU_KEYFRAME)].into_iter();
        let mut waits = 0;
        let f = asm
            .poll(&mut |ms| {
                assert!((1..=16).contains(&ms));
                waits += 1;
                if let Some((data, flags)) = owed.next() {
                    drv.publish(data, 0, 1, 0, flags);
                }
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            (f.data.as_slice(), f.keyframe, waits),
            (&b"aabbcc"[..], true, 2)
        );
        assert!(!asm.reader.mid_au() && asm.reader.next_wire_seq() == 1);
        // The streamed wire takes the same shape chunk by chunk.
        drv.publish(b"x", 1, 2, 0, au::AU_FIRST);
        drv.publish(b"y", 1, 2, 0, au::AU_LAST);
        let mut got = Vec::new();
        while let Some(c) = asm.poll_chunk(&mut no_wait).unwrap() {
            got.push((c.data, c.first, c.last));
        }
        assert_eq!(
            got,
            [(b"x".to_vec(), true, false), (b"y".to_vec(), false, true)]
        );
    }

    /// The driver dropped an access unit's tail and moved on: the next FIRST restarts the
    /// frame instead of being glued onto the cut-off prefix (80e487b57).
    #[test]
    fn a_first_mid_assembly_drops_the_prefix() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 0);
        let mut asm = assembly(mem, 0);
        drv.publish(b"cut-off", 0, 1, 0, au::AU_FIRST);
        drv.publish(b"next", 1, 2, 0, WHOLE);
        let f = asm.poll(&mut no_wait).unwrap().unwrap();
        assert_eq!(f.data, b"next");
        assert_eq!((asm.reader.gaps, asm.last_wire_seq), (1, 1));
        assert!(asm.poll(&mut no_wait).unwrap().is_none());
    }

    /// An access unit on its predecessor's source frame is a repeat, read off the unit itself
    /// rather than the loop's tick (1eba72e57). The mark the telemetry reads holds through a
    /// chunked repeat's last chunk.
    #[test]
    fn a_repeat_is_flagged_by_its_source_frame() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 0);
        let mut asm = assembly(mem, 0);
        drv.publish(b"new", 0, 7, 0, WHOLE);
        drv.publish(b"re", 1, 7, 0, au::AU_FIRST);
        drv.publish(b"sent", 1, 7, 0, au::AU_LAST);
        drv.publish(b"next", 2, 8, 0, WHOLE);
        let mut got = Vec::new();
        while let Some(f) = asm.poll(&mut no_wait).unwrap() {
            got.push((f.data, asm.au_repeat, asm.last_source_seq));
        }
        let want = [
            (&b"new"[..], false, 7),
            (b"resent", true, 7),
            (b"next", false, 8),
        ];
        assert_eq!(got, want.map(|(d, r, s)| (d.to_vec(), r, s)));
    }

    /// Two chunks past the wrap: both sort after the open chunk's end, in heap order between
    /// themselves.
    #[test]
    fn chunks_that_wrap_the_heap_keep_their_place() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 0);
        let mut asm = assembly(mem, 0);
        let heap = asm.reader.view().header().heap_bytes as usize;
        drv.publish(&vec![0xEE; heap - 64 * 1024 - 4], 0, 0, 0, WHOLE);
        assert!(asm.poll(&mut no_wait).unwrap().is_some());
        let head = vec![0x11; 64 * 1024];
        drv.publish(&head, 1, 1, 0, au::AU_FIRST);
        let wrapped = drv.publish(b"wrapped!", 1, 1, 0, 0);
        drv.publish(b"tail", 1, 1, 0, au::AU_LAST);
        assert_eq!(drv.slot_offset(wrapped) as usize, au::HEAP_OFFSET);
        let f = asm.poll(&mut no_wait).unwrap().unwrap();
        assert_eq!(f.data, [head.as_slice(), b"wrapped!", b"tail"].concat());
        assert_eq!(asm.reader.freed_unread, 0);
    }

    /// A chunk over half the heap wraps to its start and ends past the open chunk's end, so
    /// heap order would take its successor first, close the AU on it and free the chunk
    /// unread. The publish stamp keeps the order.
    #[test]
    fn a_wrapped_chunk_over_half_the_heap_keeps_its_place() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 0);
        let mut asm = assembly(mem, 0);
        drv.publish(&vec![0xEE; 600 * 1024], 0, 0, 0, WHOLE);
        assert!(asm.poll(&mut no_wait).unwrap().is_some());
        let head = vec![0x11; 100 * 1024];
        let big = vec![0x22; 750 * 1024];
        let first = drv.publish(&head, 1, 1, 0, au::AU_FIRST);
        let head_end = drv.slot_offset(first) + head.len() as u32;
        let mut placed = None;
        let f = asm
            .poll(&mut |_| {
                if placed.is_none() {
                    placed = Some((
                        drv.publish(&big, 1, 1, 0, 0),
                        drv.publish(b"tail", 1, 1, 0, au::AU_LAST),
                    ));
                }
            })
            .unwrap()
            .unwrap();
        let (wrapped, tail) = placed.unwrap();
        assert_eq!(
            drv.slot_offset(wrapped) as usize,
            au::HEAP_OFFSET,
            "wrapped"
        );
        assert!(
            drv.slot_offset(tail) >= head_end,
            "the tail sits past the head"
        );
        let want = [head.as_slice(), &big, b"tail"].concat();
        assert_eq!(f.data.len(), want.len(), "a hole");
        assert!(f.data == want, "out of order");
        assert!(asm.poll(&mut no_wait).unwrap().is_none());
        assert_eq!(asm.reader.freed_unread, 0);
    }

    /// A `RESET` mid-AU restarts the domain past the half-read unit, which already spent its
    /// index on the wire; the superseded encoder's leftovers never reach it.
    #[test]
    fn a_reset_rebases_past_a_half_read_access_unit() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 2, 10);
        let mut asm = assembly(mem, 10);
        assert_eq!(asm.reset_base(), 10);
        drv.publish(b"a", 10, 1, 0, WHOLE);
        drv.publish(b"b-head", 11, 2, 0, au::AU_FIRST);
        assert_eq!(asm.poll(&mut no_wait).unwrap().unwrap().data, b"a");
        assert!(asm.poll_chunk(&mut no_wait).unwrap().unwrap().first);
        assert_eq!(asm.reset_base(), 12);
        asm.reader.rebase(asm.reset_base());
        let stale = drv.publish(b"b-tail", 11, 2, 0, au::AU_LAST);
        drv.publish(b"c", 12, 3, 0, WHOLE);
        assert_eq!(asm.poll(&mut no_wait).unwrap().unwrap().data, b"c");
        assert_eq!(
            (drv.slot_state(stale), asm.reader.freed_unread),
            (au::FREE, 1)
        );
        assert_eq!(asm.reset_base(), 13);
    }

    /// A writer that never closes its access unit can't hold the poll: a trickle inside each
    /// slice wait meets the unit's deadline, a flood the byte cap. The reset skips the unit.
    #[test]
    fn an_access_unit_that_never_closes_is_dropped() {
        use crate::windows::remote::fake::clock;
        let trickle = Duration::from_millis(490);
        for (piece, step, bound) in [(1, trickle, "time"), (512 * 1024, Duration::ZERO, "bytes")] {
            let mut buf = section();
            let mem = SectionView::over(&mut buf);
            let mut drv = Producer::init(mem, 1, 0);
            let reader = AuReader::new(AuView::new(mem), 0);
            let mut asm = AuAssembly::new(reader, clock::CLOCKS, None);
            let data = vec![7u8; piece];
            drv.publish(&data, 0, 1, 0, au::AU_FIRST);
            let mut chunks = 1;
            let Err(err) = asm.poll(&mut |_| {
                clock::advance(step);
                drv.publish(&data, 0, 1, 0, 0);
                chunks += 1;
            }) else {
                panic!("{bound}: the poll returned");
            };
            assert!(
                format!("{err:#}").contains("still open"),
                "{bound}: {err:#}"
            );
            assert_eq!(asm.reset_base(), 1, "{bound}: the reset skips the unit");
            if bound == "time" {
                // 5 × 490 ms passes the 2 s deadline.
                assert_eq!(chunks, 6, "the deadline holds against a trickle");
            }
        }
    }

    /// A superseded encoder's publish in `latest` hides the section: the owed chunk is left
    /// where it is and waited for until the live encoder publishes again.
    #[test]
    fn a_publish_from_a_superseded_generation_is_ignored() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 5, 0);
        let mut asm = assembly(mem, 0);
        drv.publish(b"head", 0, 1, 0, au::AU_FIRST);
        let (mut tail, mut held) = (None, false);
        let f = asm
            .poll(&mut |_| match tail {
                None => {
                    let i = drv.publish(b"tail", 0, 1, 0, au::AU_LAST);
                    drv.stamp_latest(4, i as u8);
                    tail = Some(i);
                }
                Some(i) => {
                    held = drv.slot_state(i) == au::PUBLISHED;
                    drv.stamp_latest(5, i as u8);
                }
            })
            .unwrap()
            .unwrap();
        assert!(held, "the owed chunk was taken under a superseded publish");
        assert_eq!(f.data, b"headtail");
    }

    /// `[u32 len][u64 pts_ns][u8 keyframe][bytes]` per chunk, in wire order, one file per open.
    #[test]
    fn the_au_dump_frames_every_chunk() {
        let dir = std::env::temp_dir().join(format!("pf-capture-au-dump-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 0);
        let reader = AuReader::new(AuView::new(mem), 0);
        let mut asm = AuAssembly::new(reader, CLOCKS, AuDump::create(&dir, 7));
        drv.publish(
            b"key",
            0,
            1,
            QPC_NOW - 1_000,
            au::AU_FIRST | au::AU_KEYFRAME,
        );
        drv.publish(b"rest", 0, 1, QPC_NOW - 1_000, au::AU_LAST);
        drv.publish(b"p", 1, 2, 0, WHOLE);
        while asm.poll(&mut no_wait).unwrap().is_some() {}
        drop(asm);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(&format!("pfvd-au-{}-7-", std::process::id())));
        let mut want = Vec::new();
        for (data, pts_ns, keyframe) in [
            (&b"key"[..], NOW_NS - 1_000_000, 1u8),
            (b"rest", NOW_NS - 1_000_000, 0),
            (b"p", NOW_NS, 0),
        ] {
            want.extend((data.len() as u32).to_le_bytes());
            want.extend(pts_ns.to_le_bytes());
            want.push(keyframe);
            want.extend(data);
        }
        assert_eq!(std::fs::read(&files[0]).unwrap(), want);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The supervisor's clocks off the last chunk: the sequence pair, the present's age at
    /// arrival, and the driver's own split of it.
    #[test]
    fn each_chunk_records_the_progress_clocks() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 5);
        let mut asm = assembly(mem, 5);
        let us = Duration::from_micros;
        // The fake driver submits 7 ticks and publishes 11 after the present.
        drv.publish(b"a", 5, 9, QPC_NOW - 3_000, WHOLE);
        asm.poll(&mut no_wait).unwrap().unwrap();
        assert_eq!((asm.last_wire_seq, asm.last_source_seq), (5, 9));
        assert_eq!(asm.last_arrival, Some(us(3_000)));
        let split = asm.last_split.unwrap();
        assert_eq!(
            (split.pool, split.encode, split.ipc),
            (Some(us(7)), us(4), us(3_000 - 11))
        );
        // No present stamp: the arrival and the pool wait go unmeasured.
        drv.publish(b"b", 6, 10, 0, WHOLE);
        asm.poll(&mut no_wait).unwrap().unwrap();
        assert_eq!((asm.last_wire_seq, asm.last_arrival), (6, None));
        let split = asm.last_split.unwrap();
        assert_eq!((split.pool, split.encode), (None, us(4)));
    }

    /// An access unit whose next chunk never lands fails the poll after the chunk wait, so the
    /// loop's stall path resets the encoder.
    #[test]
    fn a_truncated_access_unit_fails_after_the_chunk_wait() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut drv = Producer::init(mem, 1, 3);
        let mut asm = assembly(mem, 3);
        drv.publish(b"head", 3, 1, 0, au::AU_FIRST);
        let started = Instant::now();
        let mut sleep = |ms: u32| std::thread::sleep(Duration::from_millis(ms.into()));
        let Err(e) = asm.poll(&mut sleep) else {
            panic!("a truncated access unit assembled");
        };
        assert!(started.elapsed() >= AuAssembly::CHUNK_WAIT);
        assert!(format!("{e:#}").contains("no further chunk of wire frame 3"));
        assert!(asm.reader.mid_au());
    }
}
