//! Persistent worker that AES-GCM-seals the back half of a large frame while the
//! send thread seals the front. This module owns the lane, the split
//! ([`seal_staged`]) and [`seal_wire_slice`]. Byte-identical to a sequential
//! pass; pinned by `zero_copy_seal_matches_one_by_one_path`.

use crate::crypto::SessionCrypto;
use crate::error::Result;

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

/// Bound-1 rendezvous, not a per-frame spawn. Drop closes the channel and the
/// worker exits.
pub(super) struct SealLane {
    pub(super) to_worker: std::sync::mpsc::SyncSender<SealJob>,
    pub(super) from_worker: std::sync::mpsc::Receiver<SealJob>,
}

impl SealLane {
    pub(super) fn spawn(crypto: std::sync::Arc<SessionCrypto>) -> Option<SealLane> {
        let (to_worker, jobs) = std::sync::mpsc::sync_channel::<SealJob>(1);
        let (done_tx, from_worker) = std::sync::mpsc::sync_channel::<SealJob>(1);
        std::thread::Builder::new()
            .name("punktfunk-seal2".into())
            .spawn(move || {
                while let Ok(mut job) = jobs.recv() {
                    let t0 = job.timed.then(std::time::Instant::now);
                    job.result = seal_wire_slice(&crypto, &mut job.bufs, job.seq_base);
                    if let Some(t0) = t0 {
                        job.ns = t0.elapsed().as_nanos() as u64;
                    }
                    if done_tx.send(job).is_err() {
                        break; // session gone mid-frame — nothing left to seal for
                    }
                }
            })
            .ok()?;
        Some(SealLane {
            to_worker,
            from_worker,
        })
    }
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
