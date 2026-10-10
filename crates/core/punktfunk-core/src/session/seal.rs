//! Persistent worker that AES-GCM-seals the back half of a large frame while the
//! send thread seals the front. [`Session`](super::Session) owns the split;
//! this module owns the lane and [`seal_wire_slice`]. Byte-identical to a
//! sequential pass; pinned by `zero_copy_seal_matches_one_by_one_path`.

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
    for (i, wire) in wires.iter_mut().enumerate() {
        let seq = seq_base.wrapping_add(i as u64);
        let (aad, rest) = wire.split_at_mut(crate::packet::V2_CLEAR_LEN);
        c.seal_media(seq, aad, rest)?;
    }
    Ok(())
}
