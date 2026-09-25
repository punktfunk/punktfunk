//! Client reassembly: buffer incoming shards, FEC-recover lost ones, emit access units.
//!
//! [`Reassembler::push`] is the hot path and is kept as one function (disjoint field
//! borrows). A malformed header or exhausted in-flight budget drops the packet; it
//! never aborts the session. Geometry is per-frame: the first packet pins
//! `shard_bytes`, later packets must match.
//!
//! Two index spaces (`video`, `probe`) so a speed-test burst cannot move the video
//! loss window. Incomplete frames age out after [`LOSS_WINDOW_NS`] of capture time
//! (or [`HARD_LOSS_WINDOW`] indices). Opt-in paths emit chunk-aligned partials and
//! slice-progressive prefixes.
//!
//! In-flight memory is capped at [`IN_FLIGHT_BUF_FACTOR`] × `max_frame_bytes`.
//! Pinning: `design/shard-payload-reneg.md`. Late-shard netting:
//! [`crate::stats::Stats::fec_late_shards`].

use super::*;
use crate::config::Config;
use crate::error::Result;
use crate::fec::ErasureCoder;
use crate::session::{Frame, FramePart};
use crate::stats::StatsCounters;
use std::collections::HashMap;
use zerocopy::FromBytes;

/// Incomplete-frame fuse: capture time behind the newest pts. Time, not index count —
/// 4 frames is 66 ms at 60 fps and 33 ms at 120 fps, inside Wi-Fi retry/reorder.
/// 120 ms sits above radio jitter; a real loss still trips ~2× faster than a
/// 16-frame window at 60 fps. A false trip requests a recovery IDR.
pub(super) const LOSS_WINDOW_NS: u64 = 120_000_000;

/// Index cap beside [`LOSS_WINDOW_NS`]. A hostile pts would otherwise grow the
/// window without bound. 120 ms at 120 fps ≈ 14 indices; 64 covers ~500 fps.
const HARD_LOSS_WINDOW: u32 = 64;

/// Fuse for opted-in chunk-aligned partials. The full 120 ms would inject
/// independently-decodable ancient frames into a live stream. 30 ms ≈ 2 periods at 60 fps.
const PARTIAL_WINDOW_NS: u64 = 30_000_000;

/// How far `completed` remembers emitted/abandoned indices, so a straggler cannot
/// resurrect them. Must be ≥ [`HARD_LOSS_WINDOW`]: shards can arrive after the verdict.
const REORDER_WINDOW: u32 = 64;

/// Presence map and held recovery shards. Data bytes live in [`FrameBuf::buf`] at
/// their final AU offset — this struct never copies them.
struct BlockState {
    data_shards: usize,
    recovery_shards: usize,
    /// Data-shard origin in the frame buffer. Uniform: `block_index × max_data_shards`.
    /// Slice-stream: sentinel `frame_bytes / shard_bytes`; final `total_data − K`.
    base_shard: usize,
    /// Present data-shard slots; also the FEC reconstruct input map.
    have_data: Vec<bool>,
    data_received: usize,
    recovery: Vec<Option<Vec<u8>>>,
    recovery_received: usize,
    /// Reconstructed or unrecoverable. Later shards for this block are ignored.
    done: bool,
    /// `true` iff reconstruct consumed parity (`missing > 0`). Only then a late data
    /// shard was counted in `fec_recovered_shards` and must net into `fec_late_shards`.
    reconstructed: bool,
}

struct FrameBuf {
    /// Pinned by the first packet; later packets must match. Per-frame so a mid-session
    /// `shard_payload` change cannot splice two geometries into one buffer.
    /// See `design/shard-payload-reneg.md`.
    shard_bytes: usize,
    /// Exact AU size. `0` = opened by a streamed-AU sentinel; cannot complete until
    /// the final block pins the totals.
    frame_bytes: usize,
    /// `0` = unpinned streamed frame. A legacy-opened frame always has ≥ 1.
    block_count: usize,
    pts_ns: u64,
    user_flags: u32,
    /// Leading blocks already handed up as slice-progressive prefix parts.
    next_part_block: u16,
    /// AU shard offset of the next prefix part (`next_part_block`'s start).
    delivered_shards: usize,
    /// Whole data region, zeroed. Shards copy to their final offset; FEC writes only
    /// the holes. On completion this Vec is [`Frame::data`] (truncated to `frame_bytes`).
    buf: Vec<u8>,
    blocks: HashMap<u16, BlockState>,
    /// Blocks reconstructed into `buf`. A failed block never counts; the frame ages out.
    blocks_ok: usize,
}

/// Per-session header bounds, applied before any allocation. Derived from [`Config`].
///
/// Geometry is per-frame, not per-session (`design/shard-payload-reneg.md`): the first
/// packet pins `shard_bytes` in `[min_shard_bytes, max_shard_bytes]`; later packets
/// must match; the block ceiling derives from the pin (a smaller shard needs more
/// blocks). Ordered control-stream changes cannot splice unordered datagrams.
#[derive(Clone, Copy, Debug)]
pub struct ReassemblerLimits {
    /// Floor of the per-frame pin. Production is [`crate::config::MIN_SHARD_PAYLOAD`];
    /// a hand-configured session may start below it.
    pub min_shard_bytes: usize,
    /// Ceiling of the pin, advertised in `Hello::max_shard_payload`. Recv buffers are
    /// sized for a sealed datagram of this shard size.
    pub max_shard_bytes: usize,
    pub max_data_shards: usize,
    pub max_total_shards: usize,
    pub max_frame_bytes: usize,
}

impl ReassemblerLimits {
    pub fn from_config(c: &Config) -> Self {
        let max_data = c.fec.max_data_per_block as usize;
        // Ceiling from the 90% live FEC range, not the session-start percent. The
        // sender ramps `fec_percent` without renegotiating; a stale snapshot then
        // fails `total > max_total_shards` and wedges large frames at 100% loss.
        let max_total =
            (max_data + (max_data * 90).div_ceil(100)).min(c.fec.scheme.max_total_shards());
        ReassemblerLimits {
            // Never reject the session's own `shard_payload`. Only the client takes
            // that from `Welcome`, and `Config::validate` already refuses a sub-floor.
            min_shard_bytes: crate::config::MIN_SHARD_PAYLOAD.min(c.shard_payload),
            max_shard_bytes: crate::config::max_shard_payload(),
            max_data_shards: max_data,
            max_total_shards: max_total,
            max_frame_bytes: c.max_frame_bytes,
        }
    }
}

/// One index space: in-flight frames, recently-emitted memory, loss-window anchor.
/// [`Reassembler`] keeps two — video and probe — because they use separate counters
/// ([`VIDEO_CAP_PROBE_SEQ`]): a probe burst must not move the video loss window.
/// [`VIDEO_CAP_PROBE_SEQ`]: crate::quic::VIDEO_CAP_PROBE_SEQ
#[derive(Default)]
struct ReassemblyWindow {
    frames: HashMap<u32, FrameBuf>,
    /// Terminated frame indices (emitted or abandoned) so a straggler cannot resurrect
    /// them. Values are parity-restored shard indexes (`block × max_data_shards + shard`);
    /// a later arrival nets `fec_recovered_shards` into `fec_late_shards`. Removal keeps
    /// duplicates from counting twice. Pruned with `frames` to [`REORDER_WINDOW`].
    completed: HashMap<u32, Vec<u32>>,
    /// Loss-window anchor `(frame_index, capture pts)`. Incomplete frames die once they
    /// sit [`LOSS_WINDOW_NS`] behind this pts or [`HARD_LOSS_WINDOW`] indices.
    newest_frame: Option<(u32, u64)>,
}

/// Cap on in-flight `FrameBuf::buf` bytes (both index spaces): factor × `max_frame_bytes`.
/// Buffers are allocated whole at the first shard; without this, [`HARD_LOSS_WINDOW`]
/// max-sized frames opened by one header each could commit gigabytes.
pub(super) const IN_FLIGHT_BUF_FACTOR: usize = 4;

/// Recovery-shard pool cap. Several max-recovery blocks; ~720 KB at a 1408-byte shard.
/// Jumbo sessions keep larger entries but need ~6× fewer buffers per block.
const RECOVERY_POOL_MAX: usize = 512;

/// First-shard delay samples held between drains ([`Reassembler::take_shard_delays`]).
/// One per video frame and the pump drains every iteration, so this bounds only a
/// consumer that stopped reading: at the cap a frame costs one comparison and no
/// clock read.
const SHARD_DELAY_SAMPLES: usize = 256;

/// Bytes a [`BlockState`] charges the in-flight budget. Vectors size from header
/// fields, so a slice-streamed frame can mint thousands of blocks while `buf` stays
/// near zero — they must meter like the buffer. `pub(super)` so budget tests read
/// the cost model instead of a baked-in frame count.
pub(super) fn block_state_bytes(data_shards: usize, recovery_shards: usize) -> usize {
    std::mem::size_of::<BlockState>()
        + data_shards // have_data: Vec<bool>
        + recovery_shards * std::mem::size_of::<Option<Vec<u8>>>() // recovery slot table
}

/// Call before any `buf` truncate so release nets the increments made at
/// allocation and block insert.
fn frame_cost(f: &FrameBuf) -> usize {
    f.buf.len()
        + f.blocks
            .values()
            .map(|b| block_state_bytes(b.data_shards, b.recovery_shards))
            .sum::<usize>()
}

/// Return held parity buffers to the pool and credit their bytes. Every take-out of
/// `BlockState::recovery` must go through here or the in-flight budget drifts.
fn reclaim_parity(
    block: &mut BlockState,
    recovery_pool: &mut Vec<Vec<u8>>,
    in_flight_bytes: &mut usize,
) {
    for slot in block.recovery.iter_mut() {
        if let Some(rb) = slot.take() {
            *in_flight_bytes -= rb.len();
            if recovery_pool.len() < RECOVERY_POOL_MAX {
                recovery_pool.push(rb);
            }
        }
    }
}

pub struct Reassembler {
    limits: ReassemblerLimits,
    /// Opt-in: emit aged-out [`USER_FLAG_CHUNK_ALIGNED`] frames instead of dropping
    /// them. Still counted in `frames_dropped` — a partial is lost data.
    deliver_partial: bool,
    /// Newest parked partial (newest-wins; partials are lossy).
    pending_partial: Option<Frame>,
    /// Opt-in: emit each newly-contiguous AU prefix as a [`Frame`] with `part = Some`
    /// while the rest is still in flight. Any multi-block frame tiles via `base_shard`.
    deliver_parts: bool,
    /// Video index space. Aged-out frames count as `frames_dropped` (recovery trigger).
    video: ReassemblyWindow,
    /// Probe filler ([`FLAG_PROBE`]), including old hosts that still use video indexes.
    /// Aged-out probes are not `frames_dropped` — that would fire video recovery.
    probe: ReassemblyWindow,
    /// Pooled recovery-shard buffers. Data shards land in the frame buffer; these do not.
    recovery_pool: Vec<Vec<u8>>,
    /// In-flight `buf` bytes plus per-block [`block_state_bytes`], both windows.
    in_flight_bytes: usize,
    /// `first shard arrival − pts_ns`, ns, one per video frame that opened, oldest
    /// first. Raw: the reader adds the clock offset. A frame parity never recovers
    /// is timed here and nowhere else, which is what keeps the delay signal alive
    /// while the queue is deepest.
    shard_delay_ns: Vec<i64>,
}

impl Reassembler {
    pub fn new(limits: ReassemblerLimits) -> Self {
        Reassembler {
            limits,
            deliver_partial: false,
            pending_partial: None,
            deliver_parts: false,
            video: ReassemblyWindow::default(),
            probe: ReassemblyWindow::default(),
            recovery_pool: Vec::new(),
            in_flight_bytes: 0,
            shard_delay_ns: Vec::with_capacity(SHARD_DELAY_SAMPLES),
        }
    }

    /// The first-shard delays since the last call, oldest first. Raw
    /// `arrival − pts_ns`: the caller applies the clock offset, as it does for a
    /// completed AU.
    pub fn take_shard_delays(&mut self) -> std::vec::Drain<'_, i64> {
        self.shard_delay_ns.drain(..)
    }

    pub fn set_deliver_partial(&mut self, on: bool) {
        self.deliver_partial = on;
        if !on {
            self.pending_partial = None;
        }
    }

    pub fn set_deliver_parts(&mut self, on: bool) {
        self.deliver_parts = on;
    }

    pub fn take_partial(&mut self) -> Option<Frame> {
        self.pending_partial.take()
    }

    /// Ingest one already-decrypted packet. The AU when its last block completes, else `None`.
    pub fn push(
        &mut self,
        pkt: &[u8],
        coder: &dyn ErasureCoder,
        stats: &StatsCounters,
    ) -> Result<Option<Frame>> {
        // Malformed or non-video: drop, never fatal — must not abort `poll_frame`.
        // A reconstruct failure drops the block; the stream recovers at the next keyframe.
        if pkt.len() < HEADER_LEN {
            StatsCounters::add(&stats.packets_dropped, 1);
            return Ok(None);
        }
        let hdr = match PacketHeader::read_from_bytes(&pkt[..HEADER_LEN]) {
            Ok(h) => h,
            Err(_) => {
                StatsCounters::add(&stats.packets_dropped, 1);
                return Ok(None);
            }
        };

        // Split so the window, pool, and in-flight budget can be touched while a
        // frame entry is mutably borrowed.
        let Reassembler {
            limits,
            deliver_partial,
            pending_partial,
            deliver_parts,
            video,
            probe,
            recovery_pool,
            in_flight_bytes,
            shard_delay_ns,
        } = self;
        let deliver_parts = *deliver_parts;
        let lim = *limits;
        let shard_bytes = hdr.shard_bytes as usize;
        let data_shards = hdr.data_shards as usize;
        let recovery_shards = hdr.recovery_shards as usize;
        let total = data_shards + recovery_shards;
        let shard_index = hdr.shard_index as usize;
        let block_count = hdr.block_count as usize;
        let frame_bytes = hdr.frame_bytes as usize;

        // Bound every attacker-controlled header field before allocating on it.
        // `shard_bytes` is a range, not equality: geometry is per-frame; the pin
        // below rejects a mid-frame change. Even size matches `Config::validate`.
        let drop = |stats: &StatsCounters| {
            StatsCounters::add(&stats.packets_dropped, 1);
        };
        if hdr.magic != PUNKTFUNK_MAGIC
            || shard_bytes < lim.min_shard_bytes
            || shard_bytes > lim.max_shard_bytes
            || shard_bytes % 2 != 0
            || pkt.len() < HEADER_LEN + shard_bytes
            || data_shards == 0
            || data_shards > lim.max_data_shards
            || total == 0
            || total > lim.max_total_shards
            || shard_index >= total
            || frame_bytes > lim.max_frame_bytes
        {
            drop(stats);
            return Ok(None);
        }
        // Streamed-AU sentinel: `block_count == 0` (legacy never emits 0) is a
        // non-final block of an AU whose total is still unknown. Bound by negotiated
        // limits: full-K, not the last allowed block. Exact geometry waits for the pin.
        let sentinel = block_count == 0;
        // Variable-size, base-addressed blocks: uniform rules below do not apply.
        // Flagged on every packet because reorder can deliver the final block first.
        let slice_stream = hdr.user_flags & crate::packet::USER_FLAG_SLICE_STREAM != 0;
        let block_idx = hdr.block_index as usize;
        // Per-packet shard-size ceiling for block caps and buffer extent. Allocation
        // uses only what this packet proves (`need_shards`); the in-flight budget
        // bounds the rest — one datagram must not commit `max_frame_bytes`.
        let total_data_max = lim.max_frame_bytes.div_ceil(shard_bytes).max(1);
        // Per-frame FEC-block ceiling at this shard size. A session-level cap from
        // the negotiated size would reject legitimate post-shrink frames.
        let max_blocks = total_data_max.div_ceil(lim.max_data_shards).max(1);
        // Slice pipeline: every non-final block is at least
        // `min(MIN_STREAM_BLOCK_SHARDS, max_data_per_block)` data shards, so a
        // max-size frame bounds the count (+ final + rounding). Matches the sender.
        let slice_block_cap = total_data_max
            / super::packetize::MIN_STREAM_BLOCK_SHARDS.min(lim.max_data_shards.max(1))
            + 2;
        let total_data = frame_bytes.div_ceil(shard_bytes).max(1);
        if sentinel && slice_stream {
            // Slice sentinel: `frame_bytes` is the block's base byte offset.
            // Shard-aligned; the block's range must fit the negotiated frame budget.
            if frame_bytes % shard_bytes != 0
                || frame_bytes + data_shards * shard_bytes > lim.max_frame_bytes
                || block_idx + 1 >= slice_block_cap
            {
                drop(stats);
                return Ok(None);
            }
        } else if sentinel {
            if frame_bytes != 0 || data_shards != lim.max_data_shards || block_idx + 1 >= max_blocks
            {
                drop(stats);
                return Ok(None);
            }
        } else {
            let block_cap = if slice_stream {
                slice_block_cap
            } else {
                max_blocks
            };
            if block_count > block_cap || block_idx >= block_count {
                drop(stats);
                return Ok(None);
            }
            if slice_stream {
                // Only the final block is non-sentinel. It must be last, K must fit
                // the block size, and its shards must sit in the frame
                // (`base = total_data − data_shards`).
                if block_idx + 1 != block_count
                    || data_shards > lim.max_data_shards
                    || data_shards > total_data
                {
                    drop(stats);
                    return Ok(None);
                }
            } else {
                // Uniform sender: consecutive full-K blocks, last smaller, exact
                // `frame_bytes` on every non-sentinel. Offset
                // `(block × max_data_per_block + shard) × shard_bytes` is then
                // computable on arrival. A mismatched header is dropped, not placed.
                let expect_blocks = total_data.div_ceil(lim.max_data_shards).max(1);
                let expect_data_shards = if block_idx + 1 == expect_blocks {
                    total_data - (expect_blocks - 1) * lim.max_data_shards
                } else {
                    lim.max_data_shards
                };
                if block_count != expect_blocks || data_shards != expect_data_shards {
                    drop(stats);
                    return Ok(None);
                }
            }
        }
        let body = &pkt[HEADER_LEN..HEADER_LEN + shard_bytes];

        // Probe filler reassembles in its own window so its indexes never move the
        // video loss window or count as `frames_dropped`.
        let is_probe = hdr.user_flags & (FLAG_PROBE as u32) != 0;
        if is_probe {
            // Probe receive accounting, stamped at the routing decision so video in
            // flight around the burst cannot contaminate it. Bytes = whole plaintext
            // packet. First packet since the pump zeroed the slot claims first-arrival.
            let now_ns = crate::stats::now_monotonic_ns();
            StatsCounters::add(&stats.probe_packets_received, 1);
            StatsCounters::add(&stats.probe_bytes_received, pkt.len() as u64);
            let _ = stats.probe_first_arrival_ns.compare_exchange(
                0,
                now_ns,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            );
            stats
                .probe_last_arrival_ns
                .store(now_ns, std::sync::atomic::Ordering::Relaxed);
        } else if hdr.shard_index < hdr.data_shards {
            // DATA payload only. ABR compares delivered throughput to an encoder
            // target, so parity, headers, and probe filler stay out of the numerator.
            StatsCounters::add(&stats.media_bytes_received, shard_bytes as u64);
        }
        let win = if is_probe { probe } else { video };
        win.advance_window(
            hdr.frame_index,
            hdr.pts_ns,
            stats,
            !is_probe,
            recovery_pool,
            in_flight_bytes,
            lim.max_data_shards,
            (*deliver_partial && !is_probe).then_some(pending_partial),
        );

        // Terminated (emitted or abandoned): drop. Recovery shards of an early-complete
        // frame land here; a late original nets `fec_late_shards` below.
        if let Some(reconstructed) = win.completed.get_mut(&hdr.frame_index) {
            // A data shard parity already restored was late, not lost. Count it so
            // loss windows net `recovered − late`; reordering must not look like loss.
            // Remove the match so wire duplicates count nothing. No probe/video split.
            if shard_index < data_shards {
                let fw = block_idx as u32 * lim.max_data_shards as u32 + shard_index as u32;
                if let Some(pos) = reconstructed.iter().position(|&s| s == fw) {
                    reconstructed.swap_remove(pos);
                    StatsCounters::add(&stats.fec_late_shards, 1);
                }
            }
            drop(stats);
            return Ok(None);
        }
        if win.is_stale(hdr.frame_index, hdr.pts_ns) {
            drop(stats);
            return Ok(None);
        }

        // Extent this packet proves the buffer needs. A sentinel has no total but
        // pins its own block (slice: wire base; legacy: full-K position). Never
        // `total_data_max` (8–64 MiB): every slice AU would open at the ceiling and
        // exhaust the in-flight budget after ~3 concurrent frames.
        let need_shards = if sentinel && slice_stream {
            frame_bytes / shard_bytes + data_shards
        } else if sentinel {
            // Full-K uniform (firewall-enforced); the block index alone gives the end.
            (block_idx + 1).saturating_mul(lim.max_data_shards)
        } else {
            total_data
        }
        .min(total_data_max);
        let buf_len = need_shards * shard_bytes;
        let frame = match win.frames.entry(hdr.frame_index) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                if *in_flight_bytes + buf_len > IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes {
                    // Several max-size frames already in flight. Dropping this packet
                    // is milder than the loss window killing the frame moments later.
                    drop(stats);
                    return Ok(None);
                }
                *in_flight_bytes += buf_len;
                // This packet is the frame's first: timed before the frame's own
                // pacing spread, and before FEC knows whether it will ever complete.
                if !is_probe && shard_delay_ns.len() < SHARD_DELAY_SAMPLES {
                    shard_delay_ns.push(crate::stats::now_realtime_ns() as i64 - hdr.pts_ns as i64);
                }
                e.insert(FrameBuf {
                    shard_bytes,
                    // Slice sentinel `frame_bytes` is the block base, not AU size.
                    // Stay 0 until the final block pins the totals.
                    frame_bytes: if sentinel { 0 } else { frame_bytes },
                    block_count,
                    pts_ns: hdr.pts_ns,
                    user_flags: hdr.user_flags,
                    next_part_block: 0,
                    delivered_shards: 0,
                    buf: vec![0; buf_len],
                    blocks: HashMap::new(),
                    blocks_ok: 0,
                })
            }
        };
        // First packet pinned shard size; a later in-bounds but different size would
        // compute different offsets into one buffer. Also keeps a mid-session
        // `shard_payload` change from landing a straggler in the wrong geometry.
        if frame.shard_bytes != shard_bytes {
            drop(stats);
            return Ok(None);
        }
        // Mixed slice/uniform would firewall under one rule and place under the other.
        // Placement stays memory-safe without this; this is the tighter drop.
        if (frame.user_flags ^ hdr.user_flags) & crate::packet::USER_FLAG_SLICE_STREAM != 0 {
            drop(stats);
            return Ok(None);
        }
        if sentinel {
            // No totals to cross-check. Unpinned: match by construction. Once pinned
            // (either order — reorder is normal), a sentinel must still be non-final.
            // Full-K was already enforced; offsets sit in range by `idx + 1 < count`.
            if frame.block_count != 0 {
                if block_idx + 1 >= frame.block_count {
                    drop(stats);
                    return Ok(None);
                }
                if slice_stream {
                    // The pinning packet created the final block. A later sentinel
                    // must sit strictly below its base or it overwrites landed shards.
                    let final_k = frame
                        .blocks
                        .get(&((frame.block_count - 1) as u16))
                        .map(|b| b.data_shards)
                        .unwrap_or(0);
                    let pinned_total = frame.frame_bytes.div_ceil(shard_bytes).max(1);
                    if frame_bytes / shard_bytes + data_shards > pinned_total - final_k {
                        drop(stats);
                        return Ok(None);
                    }
                }
            }
        } else if frame.block_count == 0 {
            // Final-block totals arrived: retro-validate every sentinel-created block
            // against the geometry these totals derive, then pin. Totals that put an
            // already-received block out of range or not full-K drop the whole frame —
            // landed offsets cannot be trusted, so delivering would splice the decoder.
            let lied = if slice_stream {
                // No uniform K to demand. Every sentinel must sit strictly below the
                // final base (`total_data − data_shards`; subtraction is firewall-safe)
                // and be a non-final index. Sentinel overlap is not policed here —
                // placement stays in-bounds; the tiling check refuses to deliver gaps.
                let final_base = total_data - data_shards;
                frame.blocks.iter().any(|(&bi, b)| {
                    let bi = bi as usize;
                    bi + 1 >= block_count || b.base_shard + b.data_shards > final_base
                })
            } else {
                let expect_blocks = total_data.div_ceil(lim.max_data_shards).max(1);
                let final_k = total_data - (expect_blocks - 1) * lim.max_data_shards;
                frame.blocks.iter().any(|(&bi, b)| {
                    let bi = bi as usize;
                    bi >= expect_blocks
                        || (bi + 1 < expect_blocks && b.data_shards != lim.max_data_shards)
                        || (bi + 1 == expect_blocks && b.data_shards != final_k)
                })
            };
            if lied {
                let mut f = win
                    .frames
                    .remove(&hdr.frame_index)
                    .expect("frame entry exists");
                *in_flight_bytes -= frame_cost(&f);
                // Remember the index (late-shard memory, like an aged-out frame) so
                // stragglers cannot resurrect it. Count the drop: recovery-keyframe
                // is the right outcome for a frame destroyed by a bad header.
                win.completed.insert(
                    hdr.frame_index,
                    reconstructed_shards(&f.blocks, lim.max_data_shards),
                );
                for block in f.blocks.values_mut() {
                    reclaim_parity(block, recovery_pool, in_flight_bytes);
                }
                if !is_probe {
                    StatsCounters::add(&stats.frames_dropped, 1);
                }
                drop(stats);
                return Ok(None);
            }
            frame.frame_bytes = frame_bytes;
            frame.block_count = block_count;
        } else if frame.block_count != block_count || frame.frame_bytes != frame_bytes {
            drop(stats);
            return Ok(None);
        }
        // Grow to this packet's proven extent. A streamed frame opens on whichever
        // block arrived first; the buffer never shrinks (completion truncates).
        // Re-check the budget: growth commits memory too.
        if buf_len > frame.buf.len() {
            let delta = buf_len - frame.buf.len();
            if *in_flight_bytes + delta > IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes {
                drop(stats);
                return Ok(None);
            }
            *in_flight_bytes += delta;
            frame.buf.resize(buf_len, 0);
        }
        let FrameBuf {
            buf,
            blocks,
            blocks_ok,
            block_count: frame_block_count,
            pts_ns: frame_pts_ns,
            user_flags: frame_user_flags,
            next_part_block,
            delivered_shards,
            ..
        } = frame;
        let (frame_pts_ns, frame_user_flags) = (*frame_pts_ns, *frame_user_flags);

        // First packet sizes the block. `data_shards` is already pinned; `recovery_shards`
        // varies per frame (adaptive FEC) — later packets must match the first.
        let base_shard = if slice_stream {
            if sentinel {
                frame_bytes / shard_bytes // sentinel wire base (firewall: shard-aligned)
            } else {
                total_data - data_shards // final block sits at the end of the frame
            }
        } else {
            block_idx * lim.max_data_shards
        };
        let block = match blocks.entry(hdr.block_index) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                // Block state is sized from header shard counts — same in-flight budget
                // as the frame buffer, or a slice-streamed frame mints unmetered state.
                let cost = block_state_bytes(data_shards, recovery_shards);
                if *in_flight_bytes + cost > IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes {
                    drop(stats);
                    return Ok(None);
                }
                *in_flight_bytes += cost;
                e.insert(BlockState {
                    data_shards,
                    recovery_shards,
                    base_shard,
                    have_data: vec![false; data_shards],
                    data_received: 0,
                    recovery: vec![None; recovery_shards],
                    recovery_received: 0,
                    done: false,
                    reconstructed: false,
                })
            }
        };
        if block.recovery_shards != recovery_shards {
            drop(stats);
            return Ok(None);
        }
        // Slice-stream base is per-block wire input, like `recovery_shards`. Two
        // packets of "one" block must not place shards at different offsets.
        if block.base_shard != base_shard {
            drop(stats);
            return Ok(None);
        }
        // Geometry above already agrees on K; `have_data` and recovery-slot math
        // still assume it. An explicit check keeps a later firewall refactor from
        // turning that into an OOB panic.
        if block.data_shards != data_shards {
            drop(stats);
            return Ok(None);
        }
        if block.done {
            // Late original after reconstruct (`!have_data`): net out of
            // `fec_recovered_shards`. Covers multi-block frames still in flight.
            // `have_data` = duplicate; a failed reconstruct never counted missing.
            if block.reconstructed
                && shard_index < block.data_shards
                && !block.have_data[shard_index]
            {
                block.have_data[shard_index] = true; // arrived — dedup a later re-dup
                StatsCounters::add(&stats.fec_late_shards, 1);
            }
            return Ok(None);
        }

        if shard_index < data_shards {
            // Lands at its final AU offset — the only copy past decrypt.
            if !block.have_data[shard_index] {
                let off = (block.base_shard + shard_index) * shard_bytes;
                // Firewall + pin keep accepted bases in range; same refactor
                // insurance as the `data_shards` check above.
                if off + shard_bytes > buf.len() {
                    drop(stats);
                    return Ok(None);
                }
                buf[off..off + shard_bytes].copy_from_slice(body);
                block.have_data[shard_index] = true;
                block.data_received += 1;
            }
        } else {
            let slot = shard_index - data_shards;
            if block.recovery[slot].is_none() {
                // Parity is a heap buffer sized from the wire. Meter it or a
                // max-recovery frame exceeds `4 × max_frame_bytes`. Soft refuse:
                // the block cannot reconstruct and the frame ages out.
                if *in_flight_bytes + body.len() > IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes {
                    drop(stats);
                    return Ok(None);
                }
                let mut rb = recovery_pool.pop().unwrap_or_default();
                rb.clear();
                rb.extend_from_slice(body);
                *in_flight_bytes += rb.len();
                block.recovery[slot] = Some(rb);
                block.recovery_received += 1;
            }
        }

        if block.data_received + block.recovery_received >= block.data_shards {
            let missing = block.data_shards - block.data_received;
            let outcome = if missing == 0 {
                Ok(()) // originals already in place
            } else {
                let base = block.base_shard * shard_bytes;
                let region = &mut buf[base..base + block.data_shards * shard_bytes];
                let mut slots: Vec<&mut [u8]> = region.chunks_mut(shard_bytes).collect();
                let parity: Vec<(usize, &[u8])> = block
                    .recovery
                    .iter()
                    .enumerate()
                    .filter_map(|(j, s)| s.as_deref().map(|b| (j, b)))
                    .collect();
                coder.reconstruct_into(block.recovery_shards, &mut slots, &block.have_data, &parity)
            };
            // Parity is spent either way — reclaim for the next block.
            reclaim_parity(block, recovery_pool, in_flight_bytes);
            block.done = true;
            match outcome {
                Ok(()) => {
                    // In-order, `missing` is true loss. Under reorder the early
                    // trigger also "recovers" shards still in flight; their arrival
                    // counts `fec_late_shards` so estimators can net the two.
                    block.reconstructed = missing > 0;
                    StatsCounters::add(&stats.fec_recovered_shards, missing as u64);
                    *blocks_ok += 1;
                }
                Err(_) => {
                    // Corrupt shards that passed the header checks: discard the
                    // block (never `blocks_ok`) and keep the session. Recover at
                    // the next keyframe.
                    StatsCounters::add(&stats.packets_dropped, 1);
                    return Ok(None);
                }
            }
        }

        // Use the FRAME's pinned block count, not this header. A streamed frame
        // can complete on a reordered sentinel (`block_count == 0` in the header)
        // after the final block pinned; it cannot complete before (`0 != blocks_ok`).
        let block_count = *frame_block_count;
        if block_count != 0 && *blocks_ok == block_count {
            let mut done = win.frames.remove(&hdr.frame_index).unwrap();
            win.completed.insert(
                hdr.frame_index,
                reconstructed_shards(&done.blocks, lim.max_data_shards),
            );
            *in_flight_bytes -= frame_cost(&done); // before the truncate below
                                                   // Slice-stream: bases were in range but may not TILE. A gap or overlap
                                                   // would stamp `complete` with wrong bytes and no loss counter would move.
                                                   // Refuse: index is already in `completed`, so count the drop.
            if done.user_flags & crate::packet::USER_FLAG_SLICE_STREAM != 0 {
                let total_data = done.frame_bytes.div_ceil(done.shard_bytes).max(1);
                let mut next = 0usize;
                let tiled = (0..block_count).all(|bi| match done.blocks.get(&(bi as u16)) {
                    Some(b) if b.base_shard == next => {
                        next += b.data_shards;
                        true
                    }
                    _ => false,
                }) && next == total_data;
                if !tiled {
                    if !is_probe {
                        StatsCounters::add(&stats.frames_dropped, 1);
                    }
                    drop(stats);
                    return Ok(None);
                }
            }
            done.buf.truncate(done.frame_bytes); // drop trailing-shard zero padding
                                                 // Slice-progressive consumers already hold the prefix — hand up only
                                                 // the suffix (`last`), or the whole AU if nothing was delivered early.
                                                 // Probe filler stays whole: the speed test accounts AUs, not slices.
            let (data, part) = if deliver_parts && !is_probe {
                let lo = (done.delivered_shards * shard_bytes).min(done.frame_bytes);
                let part = FramePart {
                    offset: lo as u32,
                    first: lo == 0,
                    last: true,
                };
                if lo == 0 {
                    (done.buf, Some(part))
                } else {
                    (done.buf[lo..].to_vec(), Some(part))
                }
            } else {
                (done.buf, None)
            };
            return Ok(Some(Frame {
                data,
                frame_index: hdr.frame_index,
                pts_ns: done.pts_ns,
                flags: done.user_flags,
                complete: true,
                part,
                received_ns: 0, // stamped by Session::poll_frame at the session boundary
            }));
        }
        // If this packet completed a block that extends the contiguous prefix
        // (possibly unlocking out-of-order finished blocks), emit ONE part.
        // Only successful completes count; stop short of the final block — its
        // zero-padded tail is trimmed only at completion above.
        if deliver_parts && !is_probe {
            let start = *delivered_shards;
            while let Some(b) = blocks.get(&*next_part_block) {
                if !(b.done && (b.reconstructed || b.data_received == b.data_shards)) {
                    break;
                }
                if block_count != 0 && (*next_part_block as usize) + 1 >= block_count {
                    break;
                }
                // A prefix is only a prefix if this block starts where the last ended.
                // An in-bounds but non-contiguous base must not extend it.
                if b.base_shard != *delivered_shards {
                    break;
                }
                *delivered_shards = b.base_shard + b.data_shards;
                *next_part_block += 1;
            }
            if *delivered_shards > start {
                let (lo, hi) = (start * shard_bytes, *delivered_shards * shard_bytes);
                return Ok(Some(Frame {
                    data: buf[lo..hi].to_vec(),
                    frame_index: hdr.frame_index,
                    pts_ns: frame_pts_ns,
                    flags: frame_user_flags,
                    complete: false,
                    part: Some(FramePart {
                        offset: lo as u32,
                        first: start == 0,
                        last: false,
                    }),
                    received_ns: 0, // stamped by Session::poll_frame at the session boundary
                }));
            }
        }
        Ok(None)
    }

    /// Drop all in-flight state in both index spaces. After
    /// [`Session::flush_backlog`](crate::session::Session::flush_backlog) the remaining
    /// shards are gone and `newest_frame` points into the discarded past.
    pub fn reset(&mut self) {
        self.video = ReassemblyWindow::default();
        self.probe = ReassemblyWindow::default();
        // Dropped buffers return to the allocator, not the pool — flush is rare.
        self.in_flight_bytes = 0;
        // A parked partial is from the discarded past too; leaving it would hand it
        // up as the first frame after jump-to-live.
        self.pending_partial = None;
        // Samples of the backlog that was just thrown away describe a queue the
        // session no longer has.
        self.shard_delay_ns.clear();
    }

    /// Test-only in-flight byte commitment. Mixed-geometry tests assert it returns
    /// to zero once every frame has terminated — drift here wedges the budget.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight_bytes
    }

    /// `(missing, recovery)` for an in-flight video frame: shards it still needs past
    /// what its parity can rebuild, and the parity shards it carries. A block with no
    /// shard in yet counts all its data as missing. `None` when no shard of the frame
    /// is in flight, or a streamed frame has not pinned its size.
    pub fn missing_beyond_parity(&self, frame_index: u32) -> Option<(u32, u32)> {
        let f = self.video.frames.get(&frame_index)?;
        if f.frame_bytes == 0 {
            return None;
        }
        let total_data = f.frame_bytes.div_ceil(f.shard_bytes).max(1);
        let (mut seen, mut missing, mut recovery) = (0, 0, 0);
        for b in f.blocks.values() {
            seen += b.data_shards;
            recovery += b.recovery_shards;
            if !b.done {
                missing += b
                    .data_shards
                    .saturating_sub(b.data_received + b.recovery_received);
            }
        }
        missing += total_data.saturating_sub(seen);
        Some((missing as u32, recovery as u32))
    }
}

/// Data shards of a terminating frame that exist only because parity restored them
/// (`reconstructed` blocks' still-absent originals), as frame-wide indexes
/// (`block × max_data_shards + shard`) for [`ReassemblyWindow::completed`]. Empty
/// for a clean frame.
fn reconstructed_shards(blocks: &HashMap<u16, BlockState>, max_data_shards: usize) -> Vec<u32> {
    let mut v = Vec::new();
    for (&bi, b) in blocks {
        if b.reconstructed {
            for (i, have) in b.have_data.iter().enumerate() {
                if !have {
                    v.push(bi as u32 * max_data_shards as u32 + i as u32);
                }
            }
        }
    }
    v
}

impl ReassemblyWindow {
    /// Track the newest frame. Declare incomplete frames outside [`LOSS_WINDOW_NS`]
    /// or [`HARD_LOSS_WINDOW`] lost (video: `frames_dropped`, which requests a
    /// recovery keyframe). Prune `completed` to [`REORDER_WINDOW`].
    #[allow(clippy::too_many_arguments)]
    fn advance_window(
        &mut self,
        frame_index: u32,
        pts_ns: u64,
        stats: &StatsCounters,
        count_drops: bool,
        recovery_pool: &mut Vec<Vec<u8>>,
        in_flight_bytes: &mut usize,
        max_data_shards: usize,
        // `Some(sink)` = deliver aged-out CHUNK_ALIGNED frames instead of only dropping them.
        mut partial_sink: Option<&mut Option<Frame>>,
    ) {
        let (newest, newest_pts) = match self.newest_frame {
            // Newer iff within the forward half of the index space.
            Some((n, p)) if frame_index.wrapping_sub(n) > u32::MAX / 2 => (n, p),
            _ => (frame_index, pts_ns),
        };
        self.newest_frame = Some((newest, newest_pts));

        let before = self.frames.len();
        let completed = &mut self.completed;
        let partial_on = partial_sink.is_some();
        self.frames.retain(|&idx, f| {
            // Chunk-aligned partials use [`PARTIAL_WINDOW_NS`]; everything else the full window.
            let window_ns = if partial_on && f.user_flags & USER_FLAG_CHUNK_ALIGNED != 0 {
                PARTIAL_WINDOW_NS
            } else {
                LOSS_WINDOW_NS
            };
            let keep = newest.wrapping_sub(idx) <= HARD_LOSS_WINDOW
                && newest_pts.saturating_sub(f.pts_ns) <= window_ns;
            if !keep {
                // Remember the index so a straggler cannot resurrect the frame
                // (which would re-allocate and double-count the drop). Restored
                // shards join late-shard memory exactly like an emitted frame.
                completed.insert(idx, reconstructed_shards(&f.blocks, max_data_shards));
                *in_flight_bytes -= frame_cost(f);
                // Chunk-aligned: the buffer is already the consumer shape (received
                // at final offsets, zeros in holes). Newest-wins. Still counted dropped.
                if let Some(sink) = partial_sink.as_deref_mut() {
                    // `frame_bytes > 0` also excludes an unpinned streamed frame
                    // (total still the 0 sentinel): truncate-to-0 would emit empty.
                    if f.user_flags & USER_FLAG_CHUNK_ALIGNED != 0 && f.frame_bytes > 0 {
                        let mut buf = std::mem::take(&mut f.buf);
                        buf.truncate(f.frame_bytes);
                        let newer = sink
                            .as_ref()
                            .is_none_or(|p| idx.wrapping_sub(p.frame_index) <= u32::MAX / 2);
                        if newer {
                            *sink = Some(Frame {
                                data: buf,
                                frame_index: idx,
                                pts_ns: f.pts_ns,
                                flags: f.user_flags,
                                complete: false,
                                part: None,
                                received_ns: 0, // stamped by Session::poll_frame at the session boundary
                            });
                        }
                    }
                }
                for block in f.blocks.values_mut() {
                    reclaim_parity(block, recovery_pool, in_flight_bytes);
                }
            }
            keep
        });
        let pruned = before - self.frames.len();
        if pruned > 0 && count_drops {
            StatsCounters::add(&stats.frames_dropped, pruned as u64);
        }
        self.completed
            .retain(|&idx, _| newest.wrapping_sub(idx) <= REORDER_WINDOW);
    }

    /// Frame sits outside the loss window. Accepting a shard would only allocate a
    /// buffer that the next [`advance_window`](Self::advance_window) immediately drops.
    fn is_stale(&self, frame_index: u32, pts_ns: u64) -> bool {
        match self.newest_frame {
            Some((n, newest_pts)) => {
                let behind = n.wrapping_sub(frame_index);
                behind <= u32::MAX / 2
                    && (behind > HARD_LOSS_WINDOW
                        || newest_pts.saturating_sub(pts_ns) > LOSS_WINDOW_NS)
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod reset_tests {
    use super::*;

    /// A parked partial is part of the discarded past and must not survive
    /// [`Reassembler::reset`] as the first frame after jump-to-live.
    #[test]
    fn reset_drops_a_parked_partial() {
        let mut r = Reassembler::new(ReassemblerLimits {
            min_shard_bytes: 64,
            max_shard_bytes: 64,
            max_data_shards: 8,
            max_total_shards: 16,
            max_frame_bytes: 4096,
        });
        r.pending_partial = Some(Frame {
            data: vec![0u8; 64],
            frame_index: 7,
            pts_ns: 1,
            flags: 0,
            complete: false,
            part: None,
            received_ns: 0,
        });
        r.reset();
        assert!(
            r.take_partial().is_none(),
            "a pre-flush partial must not survive reset()"
        );
    }
}
