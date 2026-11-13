//! Persistent workers beside the send thread. The seal lane AES-GCM-seals the back half of
//! a large frame, or each chunk of a pipelined one, while the send thread seals the front;
//! the parity lane computes a pipelined frame's parity while its data streams. This module
//! owns both lanes, the split ([`seal_staged`]) and [`seal_wire_slice`]. Byte-identical to
//! a sequential pass; pinned by `zero_copy_seal_matches_one_by_one_path` and
//! `chunk_pipeline_matches_the_whole_frame_seal`.

use super::perf::TimedCoder;
use crate::crypto::SessionCrypto;
use crate::error::Result;
use crate::fec::ErasureCoder;
use crate::packet::Geometry;

/// Rendezvous is ~µs; AES-GCM is ~1 µs/packet. At 256 packets the halved span
/// (≥ ~125 µs) dwarfs the hand-off. ≈300 KB of wire, ≥150 Mbps at 60 fps.
pub(super) const TWO_LANE_MIN_PACKETS: usize = 256;

/// Round-trips through the channels so the buffers return to the pool.
pub(super) struct SealJob {
    pub(super) bufs: Vec<Vec<u8>>,
    pub(super) seq_base: u64,
    pub(super) timed: bool,
    pub(super) ns: u64,
    pub(super) result: Result<()>,
}

/// A persistent worker behind a bound-1 rendezvous, not a per-frame spawn. Drop closes
/// the channel and the worker exits.
pub(super) struct JobLane<J> {
    pub(super) to_worker: std::sync::mpsc::SyncSender<J>,
    pub(super) from_worker: std::sync::mpsc::Receiver<J>,
}

/// Seals a chunk, or the back half of a frame.
pub(super) type SealLane = JobLane<SealJob>;

/// Computes a pipelined frame's parity ([`parity_beside`]).
pub(super) type ParityLane = JobLane<ParityJob>;

static LANE_BOOST: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Install the call every seal and parity lane makes first, to raise its own priority: the
/// send thread waits on them. First caller wins; later calls are ignored.
pub fn set_lane_boost(boost: fn()) {
    let _ = LANE_BOOST.set(boost);
}

impl<J: Send + 'static> JobLane<J> {
    /// Thread `name` runs the [`set_lane_boost`] hook, then `work` on each job, handing it
    /// back, until the lane drops. `None` if the thread can't start.
    fn start(name: &str, mut work: impl FnMut(&mut J) + Send + 'static) -> Option<Self> {
        let (to_worker, jobs) = std::sync::mpsc::sync_channel::<J>(1);
        let (done_tx, from_worker) = std::sync::mpsc::sync_channel::<J>(1);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if let Some(boost) = LANE_BOOST.get() {
                    boost();
                }
                while let Ok(mut job) = jobs.recv() {
                    work(&mut job);
                    if done_tx.send(job).is_err() {
                        break; // session gone mid-frame — nothing left to work for
                    }
                }
            })
            .ok()?;
        Some(JobLane {
            to_worker,
            from_worker,
        })
    }
}

impl SealLane {
    pub(super) fn spawn(crypto: std::sync::Arc<SessionCrypto>) -> Option<SealLane> {
        JobLane::start("punktfunk-seal2", move |job: &mut SealJob| {
            let t0 = job.timed.then(std::time::Instant::now);
            job.result = seal_wire_slice(&crypto, &mut job.bufs, job.seq_base);
            if let Some(t0) = t0 {
                job.ns = t0.elapsed().as_nanos() as u64;
            }
        })
    }
}

/// One frame's parity on the [`ParityLane`]. Round-trips so `recovery` returns to the
/// packetizer's pool.
pub(super) struct ParityJob {
    geo: Geometry,
    /// The caller's frame, valid only while [`parity_beside`] has the job out.
    frame: &'static [u8],
    recovery: Vec<Vec<Vec<u8>>>,
    timed: bool,
    ns: u64,
    result: Result<()>,
}

impl ParityLane {
    /// Shares the session's coder: one frame at a time uses its work buffers.
    pub(super) fn spawn(coder: std::sync::Arc<dyn ErasureCoder>) -> Option<ParityLane> {
        JobLane::start("punktfunk-fec", move |job: &mut ParityJob| {
            let ns = std::sync::atomic::AtomicU64::new(0);
            let timed = TimedCoder {
                inner: coder.as_ref(),
                ns: &ns,
            };
            let c: &dyn ErasureCoder = if job.timed { &timed } else { coder.as_ref() };
            job.result = crate::packet::parity(&job.geo, job.frame, c, &mut job.recovery);
            job.ns = ns.into_inner();
        })
    }
}

/// Runs `data_pass` here while the lane computes `frame`'s parity into `recovery`, and
/// returns its result beside the lane's `(ns, result)`. `None`: the worker is gone, and
/// `recovery` with it. The nanoseconds are the coder's, counted only when `timed`.
#[allow(unsafe_code)]
pub(super) fn parity_beside<R>(
    lane: &ParityLane,
    geo: Geometry,
    frame: &[u8],
    recovery: &mut Vec<Vec<Vec<u8>>>,
    timed: bool,
    data_pass: impl FnOnce() -> R,
) -> (R, Option<(u64, Result<()>)>) {
    // SAFETY: the worker reads `frame` only while it holds the job, and the job is back
    // before this returns or unwinds: `catch_unwind` holds a panic in `data_pass` until the
    // `recv`, which returns once the worker has handed the job back or has exited.
    let frame = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(frame) };
    let job = ParityJob {
        geo,
        frame,
        recovery: std::mem::take(recovery),
        timed,
        ns: 0,
        result: Ok(()),
    };
    if let Err(std::sync::mpsc::SendError(job)) = lane.to_worker.send(job) {
        *recovery = job.recovery;
        return (data_pass(), None);
    }
    let data = std::panic::catch_unwind(std::panic::AssertUnwindSafe(data_pass));
    let back = lane.from_worker.recv();
    let data = data.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    let Ok(job) = back else {
        return (data, None);
    };
    *recovery = job.recovery;
    (data, Some((job.ns, job.result)))
}

/// Data shards per pipeline step: ~180 KB of wire, 0.15 ms at 10 GbE and ~0.12 ms of
/// AES-GCM, so the lane and the wire stay busy and the hand-off stays small beside them.
pub(super) const SEAL_CHUNK_SHARDS: usize = 128;

/// The session's counted send: how many of the packets the kernel took.
pub type SendFn<'a> = dyn FnMut(&[&[u8]]) -> Result<usize> + 'a;

/// Where [`Session::seal_frame_chunks_at`](super::Session::seal_frame_chunks_at) hands each
/// sealed chunk, with the send it must use.
pub type SealSink<'a> = dyn FnMut(&[Vec<u8>], &mut SendFn<'_>) -> Result<()> + 'a;

/// One step of the chunk pipeline: `wires[chunk_start..used]` goes to the lane, and
/// the chunk before it comes back sealed for `sink`. The new job goes out first so
/// the lane works while `sink` sends. `in_flight` tracks the job left at the lane;
/// the caller collects it when the frame ends.
#[allow(clippy::too_many_arguments)]
pub(super) fn hand_chunk(
    lane: &SealLane,
    wires: &mut Vec<Vec<u8>>,
    used: &mut usize,
    chunk_start: usize,
    seq_base: u64,
    timed: bool,
    scratch: &mut Vec<Vec<u8>>,
    in_flight: &mut bool,
    done: &mut Vec<Vec<u8>>,
    seal_ns: &mut u64,
    sink: &mut SealSink<'_>,
    send: &mut SendFn<'_>,
) -> Result<()> {
    let mut bufs = std::mem::take(scratch);
    bufs.extend(wires.drain(chunk_start..*used));
    *used = chunk_start;
    let job = SealJob {
        bufs,
        seq_base,
        timed,
        ns: 0,
        result: Ok(()),
    };
    if lane.to_worker.send(job).is_err() {
        return Err(crate::error::PunktfunkError::Unsupported("seal lane died"));
    }
    if std::mem::replace(in_flight, true) {
        let mut prev = lane
            .from_worker
            .recv()
            .map_err(|_| crate::error::PunktfunkError::Unsupported("seal lane died"))?;
        *seal_ns += prev.ns;
        let r = prev.result.and_then(|()| sink(&prev.bufs, send));
        done.append(&mut prev.bufs);
        *scratch = prev.bufs;
        r?;
    }
    Ok(())
}

/// Buffer `i` is packet `seq_base + i`, sealed in place: the header's clear prefix is the
/// associated data and the rest is sealed. Same layout and nonce order as the fused
/// single-lane path.
pub(super) fn seal_wire_slice(
    c: &SessionCrypto,
    wires: &mut [Vec<u8>],
    seq_base: u64,
) -> Result<()> {
    c.seal_wires(seq_base, wires)
}

/// Where the back half starts when a frame of `used` packets seals across the lane: `None`
/// below [`TWO_LANE_MIN_PACKETS`], or with two-lane sealing off.
pub(super) fn lane_split(two_lane: bool, used: usize) -> Option<usize> {
    (two_lane && used >= TWO_LANE_MIN_PACKETS).then_some(used / 2)
}

/// [`seal_wire_slice`] with its nanoseconds when `timed`, else 0.
fn seal_timed(c: &SessionCrypto, wires: &mut [Vec<u8>], seq_base: u64, timed: bool) -> Result<u64> {
    let t0 = timed.then(std::time::Instant::now);
    seal_wire_slice(c, wires, seq_base)?;
    Ok(t0.map_or(0, |t| t.elapsed().as_nanos() as u64))
}

/// Seals a staged frame in place, packet `i` as `seq_base + i`, and returns the nanoseconds
/// spent when `timed`. A frame that [`lane_split`] splits spawns the lane when `lane` is
/// empty; if no lane can take it the frame seals in one pass.
pub(super) fn seal_staged(
    c: &std::sync::Arc<SessionCrypto>,
    wires: &mut Vec<Vec<u8>>,
    seq_base: u64,
    two_lane: bool,
    lane: &mut Option<SealLane>,
    scratch: &mut Vec<Vec<u8>>,
    timed: bool,
) -> Result<u64> {
    if let Some(half) = lane_split(two_lane, wires.len()) {
        if lane.is_none() {
            *lane = SealLane::spawn(c.clone()); // None if spawn fails → single-lane
        }
        if let Some(ns) = seal_split(lane, c, wires, half, seq_base, scratch, timed)? {
            return Ok(ns);
        }
    }
    seal_timed(c, wires, seq_base, timed)
}

/// The lane seals `wires[half..]` while this thread seals the front; byte-identical to a
/// sequential pass. Takes the lane for the frame. A healthy round-trip puts it back, even
/// when a half fails to seal; a dead worker leaves `lane` empty so the next large frame
/// respawns it. `Ok(None)`: no lane, or its worker is gone before it took the job — `wires`
/// is whole and unsealed.
fn seal_split(
    lane: &mut Option<SealLane>,
    c: &SessionCrypto,
    wires: &mut Vec<Vec<u8>>,
    half: usize,
    seq_base: u64,
    scratch: &mut Vec<Vec<u8>>,
    timed: bool,
) -> Result<Option<u64>> {
    let Some(taken) = lane.take() else {
        return Ok(None);
    };
    let mut tail = std::mem::take(scratch);
    tail.extend(wires.drain(half..));
    let job = SealJob {
        bufs: tail,
        seq_base: seq_base.wrapping_add(half as u64),
        timed,
        ns: 0,
        result: Ok(()),
    };
    if let Err(std::sync::mpsc::SendError(job)) = taken.to_worker.send(job) {
        wires.extend(job.bufs);
        return Ok(None);
    }
    // Collect both halves before erroring so the lane is always drained and reusable.
    let front = seal_timed(c, wires, seq_base, timed);
    let Ok(mut done) = taken.from_worker.recv() else {
        // The worker died holding the back half: those packets are gone, so fail the
        // frame rather than return half an AU.
        front?;
        return Err(crate::error::PunktfunkError::Unsupported("seal lane died"));
    };
    *lane = Some(taken);
    wires.append(&mut done.bufs);
    *scratch = done.bufs;
    let front_ns = front?;
    done.result?;
    Ok(Some(front_ns + done.ns))
}
