//! Session lifecycle and the two hot-path state machines.
//!
//! - **Host** ([`Session::submit_frame`]): encoded access unit → FEC + packetize →
//!   seal under the exporter's media keys → transport send.
//! - **Client** ([`Session::poll_frame`]): transport recv → open → reorder + FEC recover +
//!   reassemble → whole access unit.
//!
//! Input rides the QUIC connection, not this plane.

use crate::config::{Config, Role};
use crate::crypto::{MediaKeys, SessionCrypto};
use crate::error::{PunktfunkError, Result};
use crate::fec::{coder_for, ErasureCoder};
use crate::packet::{
    encode_v2, PacketHeader, Packetizer, Reassembler, ReassemblerLimits, StreamedAu, V2Stamp,
    MAX_DATAGRAM_BYTES,
};
use crate::stats::{Stats, StatsCounters};
use crate::transport::Transport;

/// One contiguous piece of an access unit under [`Session::set_deliver_frame_parts`].
/// Handed up while the rest is still on the wire so a `PARTIAL_FRAME` decoder can start
/// ahead of the last packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePart {
    /// Byte offset of `data` in the AU. The reassembler emits parts in order, but a
    /// memory-pressure or jump-to-live drop can skip entries: treat a mismatch (or a
    /// non-`first` part with no AU open) as the AU lost — flush the decoder, wait for `first`.
    pub offset: u32,
    /// First part of this AU. A `first` for a new `frame_index` while an AU is still open
    /// means that AU died (aged out or cleared) — flush the decoder; no abort part is sent.
    pub first: bool,
    /// Final part: the whole AU is in ([`Frame::complete`] is set on this part only).
    pub last: bool,
}

/// A reassembled, FEC-recovered access unit, ready to hand to the platform decoder.
pub struct Frame {
    pub data: Vec<u8>,
    pub frame_index: u32,
    pub pts_ns: u64,
    pub flags: u32,
    /// Config generation the host stamped (`punktfunk/2`): the `StreamConfig` this frame
    /// decodes under. Always 0 on `punktfunk/1`.
    pub epoch: u8,
    /// `false` when the frame aged out of the loss window with shards missing and the
    /// session opted in ([`Session::set_deliver_partial_frames`]). Only chunk-aligned AUs
    /// ([`crate::packet::USER_FLAG_CHUNK_ALIGNED`]); missing ranges are zero-filled in place.
    pub complete: bool,
    /// `Some` = one piece of an AU under slice-progressive delivery ([`FramePart`]); `data`
    /// is only that piece. `None` = a whole AU, or an aged-out chunk-aligned partial
    /// (`complete` distinguishes).
    pub part: Option<FramePart>,
    /// Unix-epoch ns (CLOCK_REALTIME, same basis as `pts_ns` and the skew handshake) when
    /// this AU finished reassembly. Stamped by [`Session::poll_frame`]; the reassembler
    /// leaves 0 (it has no clock). Do not stamp at the pre-decode pull — that folds queue
    /// wait into apparent network latency.
    pub received_ns: u64,
}

/// What a session takes beyond [`Config`] ([`Session::new`]). `Default` is unsealed with capture
/// times from 0, for a loopback.
#[derive(Clone, Debug, Default)]
pub struct MediaV2 {
    /// The host instant, Unix ns, that capture time 0 stands for. Both ends learn it in the
    /// handshake; the wire carries microseconds after it.
    pub clock_origin_ns: u64,
    /// Sealing keys from the connection's exporter. `None` on a carrier that already
    /// encrypts (WebTransport).
    pub keys: Option<MediaKeys>,
    /// Host: the session clock video pts leave in. Its origin is `clock_origin_ns`. `None`
    /// on a receiver, and on a sender whose pts are already wire time.
    pub clock: Option<std::sync::Arc<crate::quic::v2::clock::SessionClock>>,
}

/// One end of a stream. Built for a single [`Role`]; the other role's methods return
/// [`PunktfunkError::InvalidArg`].
///
/// Receive-side anti-replay: each opened datagram's AEAD-authenticated sequence is
/// filtered by [`ReplayWindow`]. Reordering inside the window is accepted; a captured
/// sealed datagram is not. Video also dedups per-frame in the reassembler.
pub struct Session {
    config: Config,
    /// The header fields every packet carries; `seq` is filled per packet.
    stamp: V2Stamp,
    /// Host: the session clock video pts leave in. `None` when pts are already wire time.
    clock: Option<std::sync::Arc<crate::quic::v2::clock::SessionClock>>,
    coder: Box<dyn ErasureCoder>,
    /// `Arc` so the second seal lane can share the cipher; uncontended otherwise.
    crypto: Option<std::sync::Arc<SessionCrypto>>,
    /// Receive-side anti-replay over the peer's authenticated sequence. `Some` exactly when
    /// `crypto` is — the plaintext probe path has no sequence to filter on.
    replay: Option<ReplayWindow>,
    /// Client: the newest `punktfunk/2` epoch a delivered frame carried ([`Session::set_epoch`]).
    newest_epoch: Option<u8>,
    transport: Box<dyn Transport>,
    packetizer: Packetizer,
    reassembler: Reassembler,
    stats: StatsCounters,
    /// Monotonic wire sequence, also the AES-GCM nonce counter.
    next_seq: u64,
    /// Client recv ring, reused across [`poll_frame`](Self::poll_frame). Filled by one
    /// `recvmmsg`, consumed across calls (`recv_idx`..`recv_count`). Allocated on first
    /// client poll so host sessions do not carry it.
    recv_scratch: Vec<Vec<u8>>,
    recv_lens: Vec<usize>,
    recv_count: usize,
    recv_idx: usize,
    /// Host send pool. `seal_frame` seals in place here; the caller sends then returns the
    /// buffers via [`reclaim_wires`](Self::reclaim_wires). After warmup each keeps its capacity.
    wire_pool: Vec<Vec<u8>>,
    /// Receive-path stage timing (`PUNKTFUNK_PERF`); [`take_pump_perf`](Self::take_pump_perf)
    /// reads and resets. `None` when off — the hot path then pays one branch per stage.
    perf: Option<PumpPerf>,
    /// Send-path stage timing (`PUNKTFUNK_PERF`); [`take_seal_perf`](Self::take_seal_perf)
    /// reads and resets. Same arming and branch-cost contract as `perf`.
    seal_perf: Option<SealPerf>,
    /// Second seal lane, spawned by the first frame that crosses [`TWO_LANE_MIN_PACKETS`].
    /// Host only — client sessions never seal frames.
    seal_lane: Option<SealLane>,
    /// Two-lane sealing enabled (default). `PUNKTFUNK_SEAL_LANES=1` forces single-lane.
    seal_two_lane: bool,
    /// Reused Vecs for the lane hand-off. The worker's half round-trips here, so
    /// steady-state two-lane frames move `n/2` headers with no allocation.
    lane_scratch: Vec<Vec<u8>>,
}

/// Stamp [`Frame::received_ns`] as the frame leaves [`Session::poll_frame`]. Completed
/// frames return as the last shard lands, so this is reassembly completion. CLOCK_REALTIME
/// to match `pts_ns` and the skew handshake — not monotonic; the math is cross-machine.
fn stamp_received(mut f: Frame) -> Frame {
    f.received_ns = crate::quic::wall_clock_ns();
    f
}

/// Write one packet's plaintext at its final wire offset: `header ‖ body`, the packet number
/// inside the header. A sealed wire ends in TAG_LEN zeros for the tag.
fn stage_wire(
    wire: &mut Vec<u8>,
    stamp: &V2Stamp,
    clock: Option<&crate::quic::v2::clock::SessionClock>,
    seq: u64,
    sealed: bool,
    hdr: &PacketHeader,
    body: &[u8],
) {
    wire.clear();
    let pts_ns = clock.map_or(hdr.pts_ns, |c| c.video_to_wire(hdr.pts_ns));
    let hdr = PacketHeader { pts_ns, ..*hdr };
    wire.extend_from_slice(&encode_v2(&hdr, &V2Stamp { seq, ..*stamp }));
    wire.extend_from_slice(body);
    if sealed {
        wire.resize(wire.len() + crate::crypto::TAG_LEN, 0);
    }
}

mod perf;
mod replay;
mod seal;

pub use perf::{PumpPerf, SealPerf};

use perf::TimedCoder;
use replay::ReplayWindow;
use seal::{
    hand_chunk, seal_wire_slice, SealJob, SealLane, SEAL_CHUNK_SHARDS, TWO_LANE_MIN_PACKETS,
};
pub use seal::{SealSink, SendFn};

/// Datagrams per client `recvmmsg` (the reused ring). 128 keeps the syscall rate
/// ≤ ~3.4k/s at ~430k pkt/s (~4.8 Gbps) and drains the kernel buffer deeper per pump;
/// cost is `RECV_BATCH × RECV_BUF` (~256 KB, client sessions only).
const RECV_BATCH: usize = 128;

impl Session {
    /// [`crate::packet::encode_v2`] headers both ways, sealed under `media.keys` when it has
    /// them.
    pub fn new(config: Config, media: MediaV2, transport: Box<dyn Transport>) -> Result<Session> {
        config.validate()?;
        let coder = coder_for(config.fec.scheme);
        let crypto = media
            .keys
            .as_ref()
            .map(|keys| std::sync::Arc::new(SessionCrypto::media(keys, config.role)));
        let replay = crypto.is_some().then(ReplayWindow::new);
        let packetizer = Packetizer::new(&config);
        let reassembler = Reassembler::new(
            ReassemblerLimits::from_config(&config),
            media.clock_origin_ns,
        );
        Ok(Session {
            stamp: V2Stamp {
                seq: 0,
                epoch: 0,
                clock_origin_ns: media.clock_origin_ns,
                max_data_per_block: config.fec.max_data_per_block,
            },
            clock: media.clock,
            coder,
            crypto,
            replay,
            newest_epoch: None,
            transport,
            packetizer,
            reassembler,
            stats: StatsCounters::default(),
            next_seq: 0,
            recv_scratch: Vec::new(),
            recv_lens: Vec::new(),
            recv_count: 0,
            recv_idx: 0,
            wire_pool: Vec::new(),
            // Read once at construct; set `PUNKTFUNK_PERF` before connecting.
            perf: std::env::var("PUNKTFUNK_PERF")
                .is_ok_and(|v| v != "0")
                .then(PumpPerf::default),
            seal_perf: std::env::var("PUNKTFUNK_PERF")
                .is_ok_and(|v| v != "0")
                .then(SealPerf::default),
            seal_lane: None,
            // Default two-lane; `PUNKTFUNK_SEAL_LANES=1` is single-lane. Byte-identical;
            // only who seals changes.
            seal_two_lane: std::env::var("PUNKTFUNK_SEAL_LANES")
                .map(|v| v != "1")
                .unwrap_or(true),
            lane_scratch: Vec::new(),
            config,
        })
    }

    /// Host: stamp `epoch` on every packet from the next frame on, after the `StreamConfig`
    /// that announces it.
    pub fn set_epoch(&mut self, epoch: u8) {
        self.stamp.epoch = epoch;
    }

    /// Drain receive-path stage timings since the last call (window semantics: the pump
    /// reads once per report interval). `None` when `PUNKTFUNK_PERF` is off.
    pub fn take_pump_perf(&mut self) -> Option<PumpPerf> {
        self.perf.as_mut().map(std::mem::take)
    }

    /// Drain send-path stage timings since the last call (window semantics: the host send
    /// loop reads once per perf window). `None` when `PUNKTFUNK_PERF` is off.
    pub fn take_seal_perf(&mut self) -> Option<SealPerf> {
        self.seal_perf.as_mut().map(std::mem::take)
    }

    /// Start or stop timing the send path. `PUNKTFUNK_PERF` starts it at construct; the host's
    /// recorder starts it for as long as a capture runs.
    pub fn set_seal_perf(&mut self, on: bool) {
        if on != self.seal_perf.is_some() {
            self.seal_perf = on.then(SealPerf::default);
        }
    }

    /// Fold externally-timed socket time into [`SealPerf::sock_ns`]. The paced video path
    /// times its own `send_sealed` chunks behind a `&self` borrow the session cannot
    /// self-time. No-op when perf is off.
    pub fn note_sock_ns(&mut self, ns: u64) {
        if let Some(p) = self.seal_perf.as_mut() {
            p.sock_ns += ns;
        }
    }

    pub fn role(&self) -> Role {
        self.config.role
    }

    pub fn stats(&self) -> Stats {
        self.stats.snapshot()
    }

    /// Zero probe-scoped arrival stamps ([`Stats::probe_first_arrival_ns`]) so the next
    /// burst's first packet claims the slot. Call before the burst can hit the host
    /// (`ProbeRequest` still queued locally) or the reset races a probe packet. Cumulative
    /// probe counters stay: per-burst deltas come from base snapshots.
    pub fn reset_probe_arrivals(&self) {
        let l = std::sync::atomic::Ordering::Relaxed;
        self.stats.probe_first_arrival_ns.store(0, l);
        self.stats.probe_last_arrival_ns.store(0, l);
        self.stats.probe_prev_arrival_ns.store(0, l);
        self.stats.probe_last_key.store(0, l);
        self.stats.probe_reorders.store(0, l);
        for b in &self.stats.probe_gap_buckets {
            b.store(0, l);
        }
    }

    // -- Host path --------------------------------------------------------

    /// Host: FEC-protect, packetize, and seal one access unit without sending. Counts the
    /// frame as submitted; transmit via [`send_sealed`](Self::send_sealed), whole or paced
    /// so the NIC does not drop a line-rate burst. Nonce advances per packet in order —
    /// seal once, send intact. Holding the `Vec`s keeps the buffers alive for the batch.
    pub fn seal_frame(
        &mut self,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
    ) -> Result<Vec<Vec<u8>>> {
        self.seal_frame_inner(data, pts_ns, user_flags, None)
    }

    /// [`seal_frame`](Self::seal_frame) with the caller's `frame_index` instead of the
    /// packetizer counter. The encode loop owns video numbering so encoder invalidation
    /// stays 1:1 with the wire across rebuilds ([`Packetizer::packetize_each`]). One
    /// numbering style per index space.
    pub fn seal_frame_at(
        &mut self,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
    ) -> Result<Vec<Vec<u8>>> {
        self.seal_frame_inner(data, pts_ns, user_flags, Some(frame_index))
    }

    fn seal_frame_inner(
        &mut self,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: Option<u32>,
    ) -> Result<Vec<Vec<u8>>> {
        self.seal_run(true, |p, coder, emit| {
            p.packetize_each(data, pts_ns, user_flags, frame_index, coder, emit)
        })
    }

    /// Bytes one AU of `frame_len` puts on the wire at the current geometry.
    pub fn frame_wire_len(&self, frame_len: usize) -> usize {
        let header = match self.crypto {
            Some(_) => crate::packet::V2_HEADER_LEN + crate::crypto::TAG_LEN,
            None => crate::packet::V2_HEADER_LEN,
        };
        self.packetizer.geometry(frame_len).wire_packets()
            * (self.packetizer.shard_payload() + header)
    }

    /// Host: [`seal_frame_at`](Self::seal_frame_at) that hands `sink` the frame in wire
    /// order as it is sealed — [`SEAL_CHUNK_SHARDS`] data shards at a time, the parity
    /// last — so the first packet leaves after one chunk instead of the whole frame. The
    /// seal lane seals chunk k+1 while `sink` sends chunk k through the `send` it is
    /// given, and a third thread computes the parity meanwhile. Byte-identical to
    /// `seal_frame_at`. A small or unencrypted frame reaches `sink` whole. The buffers
    /// return to the pool when this returns.
    pub fn seal_frame_chunks_at(
        &mut self,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
        sink: &mut SealSink<'_>,
    ) -> Result<()> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "seal_frame called on a client session",
            ));
        }
        let geo = self.packetizer.geometry(data.len());
        let pipelined =
            self.seal_two_lane && self.crypto.is_some() && geo.total_data >= 2 * SEAL_CHUNK_SHARDS;
        if pipelined && self.seal_lane.is_none() {
            self.seal_lane = SealLane::spawn(self.crypto.clone().expect("checked above"));
        }
        if !pipelined || self.seal_lane.is_none() {
            let wires = self.seal_frame_inner(data, pts_ns, user_flags, Some(frame_index))?;
            let r = sink(&wires, &mut |p| self.send_sealed(p));
            self.reclaim_wires(wires);
            return r;
        }
        self.seal_chunks_pipelined(geo, data, pts_ns, user_flags, frame_index, sink)
    }

    /// The encrypted, multi-chunk half of [`seal_frame_chunks_at`](Self::seal_frame_chunks_at).
    /// `emit` writes plaintext into pooled wires; every [`SEAL_CHUNK_SHARDS`] data wires
    /// go to the lane and the chunk before them comes back sealed for `sink`. The parity
    /// is computed on a scoped thread while the data streams and is sealed here last.
    fn seal_chunks_pipelined(
        &mut self,
        geo: crate::packet::Geometry,
        data: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
        sink: &mut SealSink<'_>,
    ) -> Result<()> {
        geo.check()?;
        let perf_armed = self.seal_perf.is_some();
        let fec_ns = std::sync::atomic::AtomicU64::new(0);
        let (stamp, clock) = (self.stamp, self.clock.clone());
        let Session {
            packetizer,
            coder,
            crypto,
            next_seq,
            wire_pool,
            seal_lane,
            lane_scratch,
            transport,
            stats,
            seal_perf,
            ..
        } = self;
        let c = crypto.as_ref().expect("pipelined seal needs crypto");
        let lane = seal_lane.take().expect("pipelined seal needs the lane");
        let timed_coder;
        let coder_ref: &dyn ErasureCoder = if perf_armed {
            timed_coder = TimedCoder {
                inner: coder.as_ref(),
                ns: &fec_ns,
            };
            &timed_coder
        } else {
            coder.as_ref()
        };
        let scheme = coder_ref.scheme();
        let mut send = |p: &[&[u8]]| -> Result<usize> {
            let sent = transport.send_gso(p)?;
            if sent < p.len() {
                StatsCounters::add(&stats.packets_send_dropped, (p.len() - sent) as u64);
            }
            Ok(sent)
        };
        let mut wires = std::mem::take(wire_pool);
        let mut done: Vec<Vec<u8>> = Vec::with_capacity(wires.len());
        let mut scratch = std::mem::take(lane_scratch);
        let mut recovery = packetizer.take_recovery();
        let seq_first = *next_seq;
        let (mut used, mut chunk_start, mut chunk_seq) = (0usize, 0usize, seq_first);
        let mut in_flight = false;
        let (mut seal_ns, mut bytes) = (0u64, 0u64);
        let mut lane_ok = true;
        let mut emit = |hdr: &crate::packet::PacketHeader, body: &[u8]| -> Result<()> {
            let is_data = hdr.shard_index < hdr.data_shards;
            // The data/parity boundary closes a partial chunk; parity gathers as the tail.
            if !is_data && chunk_start < used {
                hand_chunk(
                    &lane,
                    &mut wires,
                    &mut used,
                    chunk_start,
                    chunk_seq,
                    perf_armed,
                    &mut scratch,
                    &mut in_flight,
                    &mut done,
                    &mut seal_ns,
                    sink,
                    &mut send,
                )?;
                chunk_start = used;
                chunk_seq = *next_seq;
            }
            if used == wires.len() {
                wires.push(Vec::new());
            }
            let wire = &mut wires[used];
            used += 1;
            stage_wire(wire, &stamp, clock.as_deref(), *next_seq, true, hdr, body);
            *next_seq = next_seq.wrapping_add(1);
            bytes += wire.len() as u64;
            if is_data && used - chunk_start >= SEAL_CHUNK_SHARDS {
                hand_chunk(
                    &lane,
                    &mut wires,
                    &mut used,
                    chunk_start,
                    chunk_seq,
                    perf_armed,
                    &mut scratch,
                    &mut in_flight,
                    &mut done,
                    &mut seal_ns,
                    sink,
                    &mut send,
                )?;
                chunk_start = used;
                chunk_seq = *next_seq;
            }
            Ok(())
        };
        // The data streams out while the parity is computed beside it.
        let mut result = std::thread::scope(|s| {
            let fec = s.spawn(|| crate::packet::parity(&geo, data, coder_ref, &mut recovery));
            let emitted = packetizer.emit_data(
                &geo,
                data,
                pts_ns,
                user_flags,
                frame_index,
                scheme,
                &mut emit,
            );
            let parity = fec
                .join()
                .unwrap_or(Err(PunktfunkError::Unsupported("parity thread panicked")));
            emitted.and(parity)
        });
        packetizer.put_recovery(recovery);
        if result.is_ok() {
            result =
                packetizer.emit_parity(&geo, pts_ns, user_flags, frame_index, scheme, &mut emit);
        }
        if result.is_ok() {
            // The tail seals here while the lane finishes the last chunk; wire order holds.
            let t0 = perf_armed.then(std::time::Instant::now);
            result = seal_wire_slice(c, &mut wires[chunk_start..used], chunk_seq);
            if let Some(t0) = t0 {
                seal_ns += t0.elapsed().as_nanos() as u64;
            }
        }
        if in_flight {
            match lane.from_worker.recv() {
                Ok(mut job) => {
                    seal_ns += job.ns;
                    if result.is_ok() {
                        result = job.result.and_then(|()| sink(&job.bufs, &mut send));
                    }
                    done.append(&mut job.bufs);
                    scratch = job.bufs;
                }
                Err(_) => {
                    lane_ok = false;
                    if result.is_ok() {
                        result = Err(PunktfunkError::Unsupported("seal lane died"));
                    }
                }
            }
        }
        if result.is_ok() {
            result = sink(&wires[chunk_start..used], &mut send);
        }
        let packets = next_seq.wrapping_sub(seq_first);
        done.append(&mut wires);
        *wire_pool = done;
        *lane_scratch = scratch;
        if lane_ok {
            *seal_lane = Some(lane);
        }
        if let Some(p) = seal_perf.as_mut() {
            p.fec_ns += fec_ns.load(std::sync::atomic::Ordering::Relaxed);
            p.seal_ns += seal_ns;
            p.frames += 1;
            p.packets += packets;
        }
        StatsCounters::add(&stats.frames_submitted, 1);
        StatsCounters::add(&stats.packets_sent, packets);
        StatsCounters::add(&stats.bytes_sent, bytes);
        result
    }

    /// Host: open a streamed AU ([`crate::quic::VIDEO_CAP_STREAMED_AU`]) — only toward a
    /// client that advertised it; anyone else uses [`seal_frame_at`](Self::seal_frame_at).
    /// Feed with [`seal_streamed_chunk`](Self::seal_streamed_chunk), close with
    /// [`seal_streamed_finish`](Self::seal_streamed_finish). The three batches are one
    /// frame; nonce order is emission order — send each batch before sealing the next.
    pub fn begin_streamed_frame_at(
        &mut self,
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
    ) -> Result<StreamedAu> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "seal_frame called on a client session",
            ));
        }
        Ok(self
            .packetizer
            .begin_streamed(pts_ns, user_flags, Some(frame_index)))
    }

    /// Feed one encoder chunk into a streamed AU ([`begin_streamed_frame_at`](Self::begin_streamed_frame_at)).
    /// `slice_end` flushes at an encoder slice; false keeps full-FEC-block granularity.
    /// An empty return is normal — the chunk is buffered until a block fills.
    pub fn seal_streamed_chunk(
        &mut self,
        au: &mut StreamedAu,
        chunk: &[u8],
        slice_end: bool,
    ) -> Result<Vec<Vec<u8>>> {
        self.seal_run(false, |p, coder, emit| {
            p.push_streamed(au, chunk, slice_end, coder, emit)
        })
    }

    /// Close a streamed AU: seal the last block with the real totals and `FLAG_EOF`,
    /// which retro-validates the frame at the receiver. Counts the frame as submitted.
    pub fn seal_streamed_finish(&mut self, au: StreamedAu) -> Result<Vec<Vec<u8>>> {
        self.seal_run(true, |p, coder, emit| p.finish_streamed(au, coder, emit))
    }

    /// Packetize → pooled-wire → seal for [`seal_frame`](Self::seal_frame) and the streamed
    /// sealers. `run` writes each packet's plaintext at its final wire offset; the seal
    /// pass then encrypts in place. `count_frame` is per-AU — a streamed AU counts once,
    /// at finish.
    fn seal_run(
        &mut self,
        count_frame: bool,
        run: impl FnOnce(
            &mut Packetizer,
            &dyn ErasureCoder,
            &mut dyn FnMut(&PacketHeader, &[u8]) -> Result<()>,
        ) -> Result<()>,
    ) -> Result<Vec<Vec<u8>>> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "seal_frame called on a client session",
            ));
        }
        // Disjoint field borrows: emit needs `crypto` / `next_seq` / the pool while
        // `packetizer` is `&mut`. Plaintext lands at the final wire offset (no per-packet Vec).
        let perf_armed = self.seal_perf.is_some();
        let fec_ns = std::sync::atomic::AtomicU64::new(0);
        let mut seal_ns = 0u64;
        let two_lane = self.seal_two_lane;
        let (stamp, clock) = (self.stamp, self.clock.clone());
        let Session {
            packetizer,
            coder,
            crypto,
            next_seq,
            wire_pool,
            seal_lane,
            lane_scratch,
            ..
        } = self;
        // TimedCoder shims FEC into SealPerf; the seal phase times itself.
        let timed_coder;
        let coder_ref: &dyn ErasureCoder = if perf_armed {
            timed_coder = TimedCoder {
                inner: coder.as_ref(),
                ns: &fec_ns,
            };
            &timed_coder
        } else {
            coder.as_ref()
        };
        let mut wires = std::mem::take(wire_pool);
        let mut used = 0usize;
        // Packetize through `stage_wire`. Nonce advances in emission order; sealing is a
        // later pass so it can split across lanes.
        let seq_base = *next_seq;
        let encrypting = crypto.is_some();
        let result = {
            let wires = &mut wires;
            let used = &mut used;
            let mut emit = move |hdr: &PacketHeader, body: &[u8]| -> Result<()> {
                if *used == wires.len() {
                    wires.push(Vec::new());
                }
                let wire = &mut wires[*used];
                *used += 1;
                stage_wire(
                    wire,
                    &stamp,
                    clock.as_deref(),
                    *next_seq,
                    encrypting,
                    hdr,
                    body,
                );
                *next_seq = next_seq.wrapping_add(1);
                Ok(())
            };
            run(packetizer, coder_ref, &mut emit)
        };
        result?;
        // Drop unused pool tail before sealing so a two-lane split hands the worker
        // exactly the frame's back half.
        wires.truncate(used);
        // Seal. Large frames split: the worker seals the back half under `seq_base + i`
        // while this thread seals the front — byte-identical to a sequential pass.
        if let Some(c) = crypto {
            if two_lane && used >= TWO_LANE_MIN_PACKETS && seal_lane.is_none() {
                *seal_lane = SealLane::spawn(c.clone()); // None if spawn fails → single-lane
            }
            let mut split_done = false;
            if two_lane && used >= TWO_LANE_MIN_PACKETS {
                // Take the lane for this frame. A healthy round-trip puts it back; either
                // failure arm drops the corpse so the next large frame respawns, not a dead channel.
                if let Some(lane) = seal_lane.take() {
                    let half = used / 2;
                    let mut tail = std::mem::take(lane_scratch);
                    tail.extend(wires.drain(half..));
                    let job = SealJob {
                        bufs: tail,
                        seq_base: seq_base.wrapping_add(half as u64),
                        timed: perf_armed,
                        ns: 0,
                        result: Ok(()),
                    };
                    match lane.to_worker.send(job) {
                        Ok(()) => {
                            // Seal the front while the worker runs; collect both results
                            // before erroring so the lane is always drained and reusable.
                            let t0 = perf_armed.then(std::time::Instant::now);
                            let front = seal_wire_slice(c, &mut wires, seq_base);
                            if let Some(t0) = t0 {
                                seal_ns += t0.elapsed().as_nanos() as u64;
                            }
                            match lane.from_worker.recv() {
                                Ok(mut done) => {
                                    *seal_lane = Some(lane);
                                    seal_ns += done.ns;
                                    wires.append(&mut done.bufs);
                                    *lane_scratch = done.bufs;
                                    front?;
                                    done.result?;
                                    split_done = true;
                                }
                                Err(_) => {
                                    // Worker died holding the back half: those packets are
                                    // gone. Surface the error — do not return `Ok` with half an AU.
                                    front?;
                                    return Err(PunktfunkError::Unsupported("seal lane died"));
                                }
                            }
                        }
                        Err(std::sync::mpsc::SendError(job)) => {
                            // Worker gone but the channel returned the job: reclaim the back
                            // half so the single-lane pass below seals the whole frame.
                            wires.extend(job.bufs);
                        }
                    }
                }
            }
            if !split_done {
                let t0 = perf_armed.then(std::time::Instant::now);
                seal_wire_slice(c, &mut wires, seq_base)?;
                if let Some(t0) = t0 {
                    seal_ns += t0.elapsed().as_nanos() as u64;
                }
            }
        }
        if let Some(p) = self.seal_perf.as_mut() {
            p.fec_ns += fec_ns.load(std::sync::atomic::Ordering::Relaxed);
            p.seal_ns += seal_ns;
            p.frames += count_frame as u64;
            p.packets += used as u64;
        }
        if count_frame {
            StatsCounters::add(&self.stats.frames_submitted, 1);
        }
        let bytes: u64 = wires.iter().map(|w| w.len() as u64).sum();
        StatsCounters::add(&self.stats.packets_sent, wires.len() as u64);
        StatsCounters::add(&self.stats.bytes_sent, bytes);
        Ok(wires)
    }

    /// Return [`seal_frame`](Self::seal_frame) buffers to the reuse pool after send.
    /// Optional: dropping them only forfeits reuse.
    pub fn reclaim_wires(&mut self, wires: Vec<Vec<u8>>) {
        self.wire_pool = wires;
    }

    /// Host: GSO on this session's transport where the platform has it.
    pub fn set_gso(&self, on: bool) {
        self.transport.set_gso(on);
    }

    /// Host: send one chunk of already-sealed packets as one batch. Returns how many the
    /// kernel accepted; the rest are send-buffer drops. Whole frame, or per paced chunk.
    pub fn send_sealed(&self, packets: &[&[u8]]) -> Result<usize> {
        // GSO where the transport has it, else a batch — same short-count drop contract.
        let sent = self.transport.send_gso(packets)?;
        if sent < packets.len() {
            StatsCounters::add(
                &self.stats.packets_send_dropped,
                (packets.len() - sent) as u64,
            );
        }
        Ok(sent)
    }

    /// Host: seal and send one access unit in one batched send. [`seal_frame`](Self::seal_frame)
    /// plus [`send_sealed`](Self::send_sealed) for callers that do not pace (synthetic, probe).
    pub fn submit_frame(&mut self, data: &[u8], pts_ns: u64, user_flags: u32) -> Result<()> {
        let wires = self.seal_frame(data, pts_ns, user_flags)?;
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        let t0 = self.seal_perf.is_some().then(std::time::Instant::now);
        let r = self.send_sealed(&refs);
        drop(refs); // release `wires` before reclaim_wires
        if let Some(t0) = t0 {
            self.note_sock_ns(t0.elapsed().as_nanos() as u64);
        }
        self.reclaim_wires(wires);
        r.map(|_| ())
    }

    /// Host: seal and send one probe filler in the probe index space
    /// ([`crate::packet::FLAG_PROBE`]) so a burst never consumes video `frame_index`es.
    /// Only against a client that advertised [`crate::quic::VIDEO_CAP_PROBE_SEQ`]; an
    /// older single-window reassembler would drop probe indexes as stale video.
    ///
    /// Returns `(wire packets offered, packets the send buffer refused)`. A burst adds these
    /// up itself: video shares the send loop, so a [`Stats`] delta would count its shards too.
    pub fn submit_probe_frame(&mut self, data: &[u8], pts_ns: u64) -> Result<(u32, u32)> {
        let idx = self.packetizer.alloc_probe_index();
        let wires =
            self.seal_frame_inner(data, pts_ns, crate::packet::FLAG_PROBE as u32, Some(idx))?;
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        let offered = refs.len() as u32;
        let t0 = self.seal_perf.is_some().then(std::time::Instant::now);
        let r = self.send_sealed(&refs);
        drop(refs);
        if let Some(t0) = t0 {
            self.note_sock_ns(t0.elapsed().as_nanos() as u64);
        }
        self.reclaim_wires(wires);
        r.map(|accepted| (offered, offered.saturating_sub(accepted as u32)))
    }

    /// Host: live-adjust FEC recovery percent. Affects the next sealed AU; the receiver
    /// needs no notification (each header carries that block's data/recovery counts).
    pub fn set_fec_percent(&mut self, pct: u8) {
        self.packetizer.set_fec_percent(pct);
    }

    /// Host: live-swap shard payload between AUs (`design/shard-payload-reneg.md`). Never
    /// with a `StreamedAu` in flight ([`Packetizer::set_shard_payload`]). Bounds match
    /// `Config::validate`. Shrink may go immediately; grow must be client-acked and must
    /// not exceed `Hello::max_shard_payload`.
    pub fn set_shard_payload(&mut self, shard_payload: usize) -> Result<()> {
        if self.config.role != Role::Host {
            return Err(PunktfunkError::InvalidArg(
                "set_shard_payload called on a client session",
            ));
        }
        // Probe a copy so `Config::validate` cannot drift; its key/salt copies zeroize on drop.
        let mut probe = self.config.clone();
        probe.shard_payload = shard_payload;
        probe.validate()?;
        self.config.shard_payload = shard_payload;
        self.packetizer.set_shard_payload(shard_payload);
        Ok(())
    }

    pub fn fec_percent(&self) -> u8 {
        self.packetizer.fec_percent()
    }

    // -- Client path ------------------------------------------------------

    /// Client opt-in: deliver aged-out incomplete chunk-aligned frames as
    /// [`Frame`]`{ complete: false }` instead of dropping them. A lost datagram costs a
    /// few blocks of blur, not the frame. No effect on AUs that do not carry the flag.
    pub fn set_deliver_partial_frames(&mut self, on: bool) {
        self.reassembler.set_deliver_partial(on);
    }

    /// Client opt-in: deliver each AU's newly-contiguous prefix as [`Frame`]s with
    /// [`Frame::part`]` = Some` while the rest is still on the wire
    /// ([`crate::packet::USER_FLAG_SLICE_STREAM`]). Every video delivery then carries
    /// `part: Some`; a frame with no early parts is the degenerate `{offset: 0, first, last}`.
    ///
    /// Do not combine with an all-intra (PyroWave) stream: `FrameChannel::pop` counts
    /// queue entries as whole AUs, so parts make one AU K entries and `len > 1` drains
    /// mid-AU (newest suffix, prefixes dropped). PyroWave's sequence header lives in
    /// window 0 of every AU — every frame would arrive headerless.
    ///
    /// Distinct from streamed-AU wire ([`crate::quic::VIDEO_CAP_STREAMED_AU`]): a streamed
    /// AU still completes as one `Frame` here.
    pub fn set_deliver_frame_parts(&mut self, on: bool) {
        self.reassembler.set_deliver_parts(on);
    }

    /// Client: capture → first-shard arrival for every frame that opened since the
    /// last call, ns, oldest first. Raw `arrival − pts_ns`; the caller adds the clock
    /// offset. A frame parity never recovers still has a sample here, which a
    /// completed-AU delay reading loses exactly when the queue is deepest.
    pub fn take_shard_delays(&mut self) -> std::vec::Drain<'_, i64> {
        self.reassembler.take_shard_delays()
    }

    /// See [`Reassembler::take_short_tails`].
    pub fn take_short_tails(&mut self) -> std::vec::Drain<'_, u32> {
        self.reassembler.take_short_tails()
    }

    /// See [`Reassembler::missing_beyond_parity`].
    pub fn missing_beyond_parity(&self, frame_index: u32) -> Option<(u32, u32)> {
        self.reassembler.missing_beyond_parity(frame_index)
    }

    /// Negotiated wire shard payload (bytes of AU per datagram) — the window size for
    /// chunk-aligned AUs (`USER_FLAG_CHUNK_ALIGNED`).
    pub fn shard_payload(&self) -> usize {
        self.config.shard_payload
    }

    /// Client: whether a frame of `epoch` predates the newest seen, wrapping (1–128 behind).
    /// It was encoded before a mode switch, so its decoder is gone. `punktfunk/1` frames are
    /// all epoch 0.
    fn behind_epoch(&mut self, epoch: u8) -> bool {
        let newest = *self.newest_epoch.get_or_insert(epoch);
        let ahead = epoch.wrapping_sub(newest);
        if ahead >= 128 {
            return true;
        }
        self.newest_epoch = Some(epoch);
        false
    }

    /// Client: drain the transport until a whole access unit is recovered, or no more
    /// packets are pending ([`PunktfunkError::NoFrame`]).
    pub fn poll_frame(&mut self) -> Result<Frame> {
        if self.config.role != Role::Client {
            return Err(PunktfunkError::InvalidArg(
                "poll_frame called on a host session",
            ));
        }
        if self.recv_scratch.is_empty() {
            // Max datagram + 1: an oversized read fills the buffer and we drop it below.
            self.recv_scratch = (0..RECV_BATCH)
                .map(|_| vec![0u8; MAX_DATAGRAM_BYTES + 1])
                .collect();
            self.recv_lens = vec![0usize; RECV_BATCH];
        }
        loop {
            if self.recv_idx >= self.recv_count {
                let t0 = self.perf.is_some().then(std::time::Instant::now);
                self.recv_count = self
                    .transport
                    .recv_batch(&mut self.recv_scratch, &mut self.recv_lens)?;
                if let (Some(p), Some(t0)) = (self.perf.as_mut(), t0) {
                    p.recv_ns += t0.elapsed().as_nanos() as u64;
                    p.batches += 1;
                }
                self.recv_idx = 0;
                if self.recv_count == 0 {
                    // Idle wire: hand over an aged-out partial if one is waiting (it only gets staler).
                    if let Some(p) = self.reassembler.take_partial() {
                        if !self.behind_epoch(p.epoch) {
                            return Ok(stamp_received(p));
                        }
                        StatsCounters::add(&self.stats.frames_dropped, 1);
                    }
                    return Err(PunktfunkError::NoFrame);
                }
            }
            let i = self.recv_idx;
            self.recv_idx += 1;
            let len = self.recv_lens[i];
            // recvmmsg truncates and caps `msg_len` at the buffer size: drop rather than
            // hand up a truncated packet (same contract as scalar `recv`'s `n >= RECV_BUF`).
            if len > MAX_DATAGRAM_BYTES {
                continue;
            }
            // Open in place in the ring: plaintext at [8..8+n] behind the seq prefix; a
            // probe datagram is the packet. Field-precise borrows keep the `recv_scratch`
            // slice alive across replay/reassembly. Short / undecryptable `continue`s skip
            // decrypt accounting (exception path, not line rate).
            let t_dec = self.perf.is_some().then(std::time::Instant::now);
            let (pkt_range, seq) = match &self.crypto {
                Some(c) => {
                    use crate::packet::V2_CLEAR_LEN;
                    if len < crate::packet::V2_HEADER_LEN + crate::crypto::TAG_LEN {
                        continue;
                    }
                    let wire = &mut self.recv_scratch[i][..len];
                    let low = u32::from_le_bytes(wire[1..V2_CLEAR_LEN].try_into().unwrap());
                    let reference = self.replay.as_ref().map_or(0, |w| w.reference());
                    let seq = crate::packet::expand_seq(reference, low);
                    if !self.replay.as_ref().is_none_or(|w| w.plausible(seq)) {
                        StatsCounters::add(&self.stats.packets_dropped, 1);
                        continue;
                    }
                    let (aad, rest) = wire.split_at_mut(V2_CLEAR_LEN);
                    match c.open_media(seq, aad, rest) {
                        Ok(n) => (0..V2_CLEAR_LEN + n, Some(seq)),
                        Err(_) => continue,
                    }
                }
                None => (0..len, None),
            };
            if let (Some(p), Some(t)) = (self.perf.as_mut(), t_dec) {
                p.decrypt_ns += t.elapsed().as_nanos() as u64;
            }
            // Reject a datagram whose authenticated sequence was already seen. Video also
            // dedups per-frame downstream; filtering here is uniform and cheap.
            if let (Some(w), Some(seq)) = (self.replay.as_mut(), seq) {
                if !w.accept(seq) {
                    StatsCounters::add(&self.stats.packets_dropped, 1);
                    continue;
                }
            }
            let pkt = &self.recv_scratch[i][pkt_range];
            StatsCounters::add(&self.stats.packets_received, 1);
            StatsCounters::add(&self.stats.bytes_received, pkt.len() as u64);
            let t_push = self.perf.is_some().then(std::time::Instant::now);
            let pushed = self
                .reassembler
                .push(pkt, self.coder.as_ref(), &self.stats)?;
            if let (Some(p), Some(t)) = (self.perf.as_mut(), t_push) {
                p.reasm_ns += t.elapsed().as_nanos() as u64;
                // Datagrams that reached the reassembler (replay-rejected ones do not).
                p.packets += 1;
            }
            if let Some(frame) = pushed {
                // Prefix parts are not completions: only the delivery that closes the AU,
                // or parts would multiply the completion rate.
                if frame.complete {
                    StatsCounters::add(&self.stats.frames_completed, 1);
                }
                if self.behind_epoch(frame.epoch) {
                    StatsCounters::add(&self.stats.frames_dropped, 1);
                    continue;
                }
                return Ok(stamp_received(frame));
            }
            // A no-complete push may still have aged a partial out; deliver it before
            // draining further (its successors are already arriving).
            if let Some(p) = self.reassembler.take_partial() {
                if self.behind_epoch(p.epoch) {
                    StatsCounters::add(&self.stats.frames_dropped, 1);
                    continue;
                }
                return Ok(stamp_received(p));
            }
        }
    }

    /// Client: discard the pending receive backlog (current recv ring plus the kernel
    /// socket buffer) and reset the reassembler. Returns datagrams thrown away
    /// (`packets_dropped`). The receive path has no other skip-ahead: packets arrive in
    /// order, and consume-at-arrival-rate never shrinks a standing queue. 1024 batches
    /// (≈131k datagrams at the 128-deep ring) only cap a line-rate sender outrunning the loop.
    pub fn flush_backlog(&mut self) -> Result<u64> {
        if self.config.role != Role::Client {
            return Err(PunktfunkError::InvalidArg(
                "flush_backlog called on a host session",
            ));
        }
        // Undelivered tail of the current ring is backlog too.
        let mut flushed = self.recv_count.saturating_sub(self.recv_idx) as u64;
        self.recv_count = 0;
        self.recv_idx = 0;
        if !self.recv_scratch.is_empty() {
            for _ in 0..1024 {
                let n = self
                    .transport
                    .recv_batch(&mut self.recv_scratch, &mut self.recv_lens)?;
                if n == 0 {
                    break;
                }
                flushed += n as u64;
            }
        }
        self.reassembler.reset();
        StatsCounters::add(&self.stats.packets_dropped, flushed);
        Ok(flushed)
    }
}

#[cfg(test)]
mod wire_equivalence_tests {
    use super::*;
    use crate::config::{FecConfig, FecScheme};
    use crate::crypto::MediaSuite;
    use crate::packet::{HEADER_LEN, V2_CLEAR_LEN};
    use crate::transport::loopback_pair;
    use zerocopy::FromBytes;

    fn host_cfg(scheme: FecScheme, fec_percent: u8) -> Config {
        Config {
            role: Role::Host,
            fec: FecConfig {
                scheme,
                fec_percent,
                max_data_per_block: 8,
            },
            shard_payload: 64,
            max_frame_bytes: 8 * 1024 * 1024,
            loopback_drop_period: 0,
        }
    }

    /// Sealed under test keys when `sealed`, else as on a carrier that already encrypts.
    /// Capture times count from 0.
    fn media(sealed: bool) -> MediaV2 {
        MediaV2 {
            clock_origin_ns: 0,
            keys: sealed.then(|| media_keys(MediaSuite::Aes128Gcm)),
            clock: None,
        }
    }

    fn host_session(cfg: Config, sealed: bool) -> Session {
        let (h, _c) = loopback_pair(0, 0);
        Session::new(cfg, media(sealed), Box::new(h)).unwrap()
    }

    /// Reference wire path: the `packetize` wrapper, then each packet staged and sealed on its
    /// own. Shares session state with `seal_frame` and nothing else, so the equality pin is real.
    fn seal_via_wrapper(sess: &mut Session, frame: &[u8], pts_ns: u64, flags: u32) -> Vec<Vec<u8>> {
        let packets = sess
            .packetizer
            .packetize(frame, pts_ns, flags, sess.coder.as_ref())
            .unwrap();
        let mut wires = Vec::new();
        for pkt in &packets {
            let hdr = PacketHeader::read_from_bytes(&pkt[..HEADER_LEN]).unwrap();
            let seq = sess.next_seq;
            sess.next_seq += 1;
            let sealed = sess.crypto.is_some();
            let mut wire = Vec::new();
            stage_wire(
                &mut wire,
                &sess.stamp,
                None,
                seq,
                sealed,
                &hdr,
                &pkt[HEADER_LEN..],
            );
            if let Some(c) = &sess.crypto {
                let (aad, rest) = wire.split_at_mut(V2_CLEAR_LEN);
                c.seal_media(seq, aad, rest).unwrap();
            }
            wires.push(wire);
        }
        wires
    }

    /// `seal_frame`'s pooled-wire path must be byte-identical to the wrapper path
    /// (same plaintext, same nonce sequence) across schemes, FEC percents, crypto on/off,
    /// and the frame shapes below.
    #[test]
    fn zero_copy_seal_matches_wrapper_path() {
        for scheme in [FecScheme::Gf8, FecScheme::Gf16] {
            for fec_percent in [0u8, 50] {
                for encrypt in [true, false] {
                    let mut opt = host_session(host_cfg(scheme, fec_percent), encrypt);
                    let mut refr = host_session(host_cfg(scheme, fec_percent), encrypt);

                    // shard_payload 64 × max_data_per_block 8: >512 B spans FEC blocks.
                    let frames: Vec<Vec<u8>> = vec![
                        pattern(3000),  // multi-block + partial tail shard
                        pattern(1024),  // exact multiple (2 full blocks)
                        pattern(100),   // single block, partial tail
                        Vec::new(),     // empty frame → 1 zeroed shard
                        pattern(64),    // exactly one full shard
                        pattern(20000), // > TWO_LANE_MIN_PACKETS wire packets → two-lane seal
                    ];
                    for (i, frame) in frames.iter().enumerate() {
                        let got = opt.seal_frame(frame, 1000 * i as u64, i as u32).unwrap();
                        let want = seal_via_wrapper(&mut refr, frame, 1000 * i as u64, i as u32);
                        assert_eq!(
                            got, want,
                            "wire mismatch: scheme={scheme:?} fec={fec_percent}% encrypt={encrypt} frame#{i}"
                        );
                        // Return buffers so later frames exercise pooled reuse (bigger after
                        // smaller and vice versa).
                        opt.reclaim_wires(got);
                    }
                    // 20000 bytes (~469 packets at shard 64) crosses TWO_LANE_MIN_PACKETS:
                    // equality above must have held through the two-lane split, not a fallback.
                    if encrypt {
                        assert!(
                            opt.seal_lane.is_some(),
                            "two-lane seal lane should have spawned for the large frame"
                        );
                    }
                }
            }
        }
    }

    /// The chunk pipeline puts the same bytes on the wire as the whole-frame seal, in
    /// the same order, and hands a large frame over one chunk at a time.
    #[test]
    fn chunk_pipeline_matches_the_whole_frame_seal() {
        for encrypt in [true, false] {
            for fec_percent in [0u8, 50] {
                let cfg = host_cfg(FecScheme::Gf16, fec_percent);
                let mut piped = host_session(cfg.clone(), encrypt);
                let mut whole = host_session(cfg, encrypt);
                // At shard 64: 47, 2, 313 and 1 data shards; 313 is three chunks of 128.
                let frames = [pattern(3000), pattern(100), pattern(20000), Vec::new()];
                for (i, frame) in frames.iter().enumerate() {
                    let pts = 1000 * i as u64;
                    let want = whole.seal_frame_at(frame, pts, i as u32, i as u32).unwrap();
                    let mut got: Vec<Vec<u8>> = Vec::new();
                    let mut calls = 0usize;
                    piped
                        .seal_frame_chunks_at(frame, pts, i as u32, i as u32, &mut |chunk, send| {
                            calls += 1;
                            let refs: Vec<&[u8]> = chunk.iter().map(|b| b.as_slice()).collect();
                            send(&refs)?;
                            got.extend(chunk.iter().cloned());
                            Ok(())
                        })
                        .unwrap();
                    assert_eq!(got, want, "encrypt={encrypt} fec={fec_percent} frame#{i}");
                    let shards = piped.packetizer.geometry(frame.len()).total_data;
                    if encrypt && shards >= 2 * SEAL_CHUNK_SHARDS {
                        assert!(
                            calls >= shards / SEAL_CHUNK_SHARDS,
                            "{calls} sink calls for {shards} data shards"
                        );
                    } else {
                        assert_eq!(calls, 1, "a whole frame reaches the sink once");
                    }
                    assert_eq!(
                        piped.frame_wire_len(frame.len()),
                        want.iter().map(Vec::len).sum::<usize>(),
                        "frame_wire_len frame#{i}"
                    );
                    whole.reclaim_wires(want);
                }
            }
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + 7) as u8).collect()
    }

    /// A dead seal lane must fall back to a single-lane seal of the whole frame, and the
    /// corpse must be dropped so the next large frame respawns a fresh lane.
    #[test]
    fn dead_seal_lane_falls_back_to_single_lane_whole_frame() {
        let mut opt = host_session(host_cfg(FecScheme::Gf16, 20), true);
        let mut refr = host_session(host_cfg(FecScheme::Gf16, 20), true);
        // Worker already gone: both far ends dropped, so `send` fails immediately and
        // hands the job (back half of the frame) back.
        let (to_worker, jobs) = std::sync::mpsc::sync_channel::<SealJob>(1);
        let (done_tx, from_worker) = std::sync::mpsc::sync_channel::<SealJob>(1);
        drop(jobs);
        drop(done_tx);
        opt.seal_lane = Some(SealLane {
            to_worker,
            from_worker,
        });
        let frame = pattern(20000); // > TWO_LANE_MIN_PACKETS wire packets → takes the split path
        let got = opt.seal_frame(&frame, 7, 0).unwrap();
        let want = seal_via_wrapper(&mut refr, &frame, 7, 0);
        assert_eq!(got, want, "fallback must seal the whole frame, not half");
        assert!(
            opt.seal_lane.is_none(),
            "the dead lane must be dropped, not retried forever"
        );
        opt.reclaim_wires(got);
        let got2 = opt.seal_frame(&frame, 8, 1).unwrap();
        let want2 = seal_via_wrapper(&mut refr, &frame, 8, 1);
        assert_eq!(got2, want2);
        assert!(
            opt.seal_lane.is_some(),
            "a fresh lane respawns on the next large frame"
        );
    }

    /// A chunk-aligned frame that loses shards past FEC is delivered once it ages out
    /// (`complete: false`, survivors at exact offsets, holes zero-filled). Unflagged AUs
    /// still drop, even with the opt-in on.
    #[test]
    fn partial_delivery_of_chunk_aligned_frames() {
        use crate::packet::USER_FLAG_CHUNK_ALIGNED;
        let mk = |role| Config {
            role,
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                fec_percent: 0, // no parity — any drop leaves a hole
                max_data_per_block: 64,
            },
            shard_payload: 1024,
            max_frame_bytes: 8 * 1024 * 1024,
            loopback_drop_period: 0,
        };
        let (h, c) = crate::transport::loopback_pair(3, 1);
        let mut host = Session::new(mk(Role::Host), media(false), Box::new(h)).unwrap();
        let mut client = Session::new(mk(Role::Client), media(false), Box::new(c)).unwrap();
        client.set_deliver_partial_frames(true);

        let frame = pattern(8 * 1024);
        host.submit_frame(&frame, 1_000, USER_FLAG_CHUNK_ALIGNED)
            .unwrap();
        // Age the incomplete frame off the hard index window: push enough newer complete
        // frames past it, and collect everything the client emits.
        let mut got_partial = None;
        let mut completes = 0;
        for i in 0..80u64 {
            let filler = pattern(1024);
            host.submit_frame(&filler, 2_000 + i, USER_FLAG_CHUNK_ALIGNED)
                .unwrap();
            loop {
                match client.poll_frame() {
                    Ok(f) if !f.complete => got_partial = Some(f),
                    Ok(_) => completes += 1,
                    Err(PunktfunkError::NoFrame) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
        }
        let p = got_partial.expect("the lossy frame must be delivered partial");
        assert_eq!(p.pts_ns, 1_000);
        assert_eq!(p.data.len(), frame.len());
        assert!(p.flags & USER_FLAG_CHUNK_ALIGNED != 0);
        let mut zero_windows = 0;
        for w in 0..8 {
            let win = &p.data[w * 1024..(w + 1) * 1024];
            if win.iter().all(|&b| b == 0) {
                zero_windows += 1;
            } else {
                assert_eq!(win, &frame[w * 1024..(w + 1) * 1024], "window {w} corrupt");
            }
        }
        // loopback_pair(3, _) drops every 3rd datagram, so several of the 8 shards are
        // gone — the exact count depends on phase; some zeroed, every survivor intact.
        assert!(
            (1..8).contains(&zero_windows),
            "dropped shards zero-filled (got {zero_windows})"
        );
        assert!(completes > 40, "surviving filler frames flow normally");

        // Control: without the chunk-aligned flag the same loss is a drop, opt-in or not.
        let (h2, c2) = crate::transport::loopback_pair(3, 1);
        let mut host2 = Session::new(mk(Role::Host), media(false), Box::new(h2)).unwrap();
        let mut client2 = Session::new(mk(Role::Client), media(false), Box::new(c2)).unwrap();
        client2.set_deliver_partial_frames(true);
        host2.submit_frame(&pattern(8 * 1024), 1_000, 0).unwrap();
        let mut saw_partial = false;
        for i in 0..80u64 {
            host2.submit_frame(&pattern(1024), 2_000 + i, 0).unwrap();
            loop {
                match client2.poll_frame() {
                    Ok(f) => saw_partial |= !f.complete,
                    Err(PunktfunkError::NoFrame) => break,
                    Err(e) => panic!("unexpected: {e}"),
                }
            }
        }
        assert!(
            !saw_partial,
            "unflagged AUs must never be delivered partial"
        );
    }

    /// Chunk-aligned sessions do not renegotiate mid-session (`design/shard-payload-reneg.md`):
    /// `Welcome::shard_payload` is fixed at handshake, host packetizes at it, the client
    /// parse window is [`Session::shard_payload`], and partial delivery zero-fills exact
    /// windows of it. Pin 1216 (typical VPN MTU budget) and 512 (floor): frames deliver,
    /// loss is whole windows, window math matches the session value.
    #[test]
    fn chunk_aligned_sessions_work_at_clamped_shard_sizes() {
        use crate::packet::USER_FLAG_CHUNK_ALIGNED;
        for shard in [1216usize, crate::config::MIN_SHARD_PAYLOAD] {
            let mk = |role| Config {
                role,
                fec: FecConfig {
                    scheme: FecScheme::Gf16,
                    fec_percent: 0, // no parity — any drop leaves a hole
                    max_data_per_block: 64,
                },
                shard_payload: shard,
                max_frame_bytes: 8 * 1024 * 1024,
                loopback_drop_period: 0,
            };
            let (h, c) = crate::transport::loopback_pair(3, 1);
            let mut host = Session::new(mk(Role::Host), media(true), Box::new(h)).unwrap();
            let mut client = Session::new(mk(Role::Client), media(true), Box::new(c)).unwrap();
            client.set_deliver_partial_frames(true);
            // Parse window every embedder walks is the clamped session value.
            assert_eq!(client.shard_payload(), shard);
            assert_eq!(host.shard_payload(), shard);

            let frame = pattern(8 * shard);
            host.submit_frame(&frame, 1_000, USER_FLAG_CHUNK_ALIGNED)
                .unwrap();
            let mut got_partial = None;
            let mut completes = 0;
            for i in 0..80u64 {
                host.submit_frame(&pattern(shard), 2_000 + i, USER_FLAG_CHUNK_ALIGNED)
                    .unwrap();
                loop {
                    match client.poll_frame() {
                        Ok(f) if !f.complete => got_partial = Some(f),
                        Ok(_) => completes += 1,
                        Err(PunktfunkError::NoFrame) => break,
                        Err(e) => panic!("shard {shard}: unexpected: {e}"),
                    }
                }
            }
            let p = got_partial.expect("the lossy frame must be delivered partial");
            assert_eq!(p.data.len(), frame.len(), "shard {shard}");
            // Loss lands on exact `shard`-sized windows: zeroed for dropped datagrams,
            // byte-identical survivors — nothing spliced across windows.
            let mut zero_windows = 0;
            for w in 0..8 {
                let win = &p.data[w * shard..(w + 1) * shard];
                if win.iter().all(|&b| b == 0) {
                    zero_windows += 1;
                } else {
                    assert_eq!(
                        win,
                        &frame[w * shard..(w + 1) * shard],
                        "shard {shard}: window {w} corrupt"
                    );
                }
            }
            assert!(
                (1..8).contains(&zero_windows),
                "shard {shard}: dropped shards zero-filled (got {zero_windows})"
            );
            assert!(
                completes > 40,
                "shard {shard}: surviving filler frames flow normally"
            );
        }
    }

    /// Mid-session shard swap over the sealed loopback (`design/shard-payload-reneg.md`):
    /// shrink, jumbo grow, revert through one crypto/replay stream. Assert delivered
    /// frames byte-identical — never the mere absence of errors.
    #[test]
    fn mid_session_shard_swap_delivers_frames_over_the_sealed_wire() {
        let mk = |role: Role| {
            let mut c = host_cfg(FecScheme::Gf16, 20);
            c.role = role;
            c.shard_payload = 1408;
            c.fec.max_data_per_block = 64;
            c
        };
        let (ht, ct) = loopback_pair(0, 0);
        let mut host = Session::new(mk(Role::Host), media(true), Box::new(ht)).unwrap();
        let mut client = Session::new(mk(Role::Client), media(true), Box::new(ct)).unwrap();

        let phases: [(usize, &[usize]); 4] = [
            (1408, &[3000, 3 * 1408]),    // negotiated default (incl. exact multiple)
            (512, &[2000, 5 * 512 + 17]), // shrink
            (8908, &[100_000]),           // grow to jumbo (9000-MTU)
            (1216, &[2 * 1216 + 9]),      // revert
        ];
        let mut pts = 0u64;
        let mut delivered = 0usize;
        for (shard, lens) in phases {
            host.set_shard_payload(shard).unwrap();
            assert_eq!(host.shard_payload(), shard);
            for &len in lens {
                pts += 1_000_000;
                let src = pattern(len);
                host.submit_frame(&src, pts, 0).unwrap();
                let f = client
                    .poll_frame()
                    .unwrap_or_else(|e| panic!("shard {shard}: frame must be DELIVERED ({e})"));
                assert_eq!(
                    f.data, src,
                    "shard {shard}: {len} B frame must be byte-identical"
                );
                assert!(f.complete);
                delivered += 1;
            }
        }
        assert_eq!(delivered, 6, "every submitted frame must be delivered");
        // Host-only setter: a client must refuse it, and an invalid size must not stick.
        assert!(client.set_shard_payload(1408).is_err());
        assert!(
            host.set_shard_payload(1407).is_err(),
            "odd must be rejected"
        );
        assert!(
            host.set_shard_payload(crate::config::max_shard_payload() + 2)
                .is_err(),
            "oversized must be rejected"
        );
        assert_eq!(host.shard_payload(), 1216, "failed swaps must not stick");
    }

    fn v2_pair(drop_period: u32) -> (Session, Session) {
        v2_pair_keyed(drop_period, None)
    }

    fn v2_pair_keyed(drop_period: u32, keys: Option<MediaKeys>) -> (Session, Session) {
        v2_pair_with(drop_period, keys, None)
    }

    /// `clock` is the host's alone; the client reads wire time against its origin.
    fn v2_pair_with(
        drop_period: u32,
        keys: Option<MediaKeys>,
        clock: Option<std::sync::Arc<crate::quic::v2::clock::SessionClock>>,
    ) -> (Session, Session) {
        let mk = |role: Role| {
            let mut c = host_cfg(FecScheme::Gf16, 25);
            c.role = role;
            c.shard_payload = 512;
            c
        };
        let media = MediaV2 {
            clock_origin_ns: clock
                .as_ref()
                .map_or(1_700_000_000_000_000_000, |c| c.origin_ns()),
            keys,
            clock: None,
        };
        let (ht, ct) = loopback_pair(drop_period, 0);
        let host_media = MediaV2 {
            clock,
            ..media.clone()
        };
        let host = Session::new(mk(Role::Host), host_media, Box::new(ht)).unwrap();
        let client = Session::new(mk(Role::Client), media, Box::new(ct)).unwrap();
        (host, client)
    }

    /// A frame from before the newest epoch is dropped, across the u8 wrap; equal and newer pass.
    #[test]
    fn a_frame_behind_the_newest_epoch_is_dropped() {
        let (_host, mut client) = v2_pair(0);
        assert!(
            !client.behind_epoch(250),
            "the first epoch seen sets the mark"
        );
        assert!(!client.behind_epoch(250));
        assert!(!client.behind_epoch(2), "2 is six past 250");
        assert!(client.behind_epoch(250), "an encode from before the switch");
        assert!(client.behind_epoch(1));
        assert!(!client.behind_epoch(2));
    }

    /// A host clock puts session time on the wire: each frame reaches the client at the value
    /// its `HostTiming` names, near the capture time it left with.
    #[test]
    fn v2_frames_carry_the_host_session_clock() {
        let clock = std::sync::Arc::new(crate::quic::v2::clock::SessionClock::new());
        let (mut host, mut client) = v2_pair_with(0, None, Some(clock.clone()));
        for len in [100, 8 * 512 + 1] {
            let pts = crate::quic::wall_clock_ns();
            host.submit_frame(&pattern(len), pts, 0).unwrap();
            let f = client.poll_frame().unwrap();
            assert_eq!(f.pts_ns, clock.video_to_wire(pts));
            assert!(f.pts_ns.abs_diff(pts) < 50_000_000, "{} vs {pts}", f.pts_ns);
        }
    }

    fn media_keys(suite: crate::crypto::MediaSuite) -> MediaKeys {
        MediaKeys::derive(&[0x42; 32], suite)
    }

    /// Whole frames of every shape cross the `punktfunk/2` wire byte-identical, with their
    /// capture time, flags and epoch, and probe filler stays out of the video index space.
    #[test]
    fn v2_frames_cross_the_wire_with_their_epoch() {
        let (mut host, mut client) = v2_pair(0);
        let origin = 1_700_000_000_000_000_000u64;
        for (i, len) in [
            0usize,
            1,
            511,
            512,
            513,
            8 * 512,
            8 * 512 + 1,
            100 * 512 + 3,
        ]
        .into_iter()
        .enumerate()
        {
            host.set_epoch(i as u8);
            let src = pattern(len);
            let pts = origin + (i as u64 + 1) * 16_666_000;
            host.submit_frame(&src, pts, crate::packet::USER_FLAG_RECOVERY_ANCHOR)
                .unwrap();
            let f = client.poll_frame().unwrap();
            assert_eq!((f.data.len(), f.epoch, f.pts_ns), (len, i as u8, pts));
            assert_eq!(f.data, src);
            assert!(f.flags & crate::packet::USER_FLAG_RECOVERY_ANCHOR != 0);
        }
        host.submit_probe_frame(&pattern(500), origin).unwrap();
        let probe = client.poll_frame().unwrap();
        assert!(probe.flags & crate::packet::FLAG_PROBE as u32 != 0);
    }

    /// Parity repairs a dropped shard on the v2 wire.
    #[test]
    fn v2_parity_repairs_loss() {
        // Every 17th packet: at most one loss in any 8 + 2 block, always within parity.
        let (mut host, mut client) = v2_pair(17);
        let mut got = 0;
        for i in 0..20u64 {
            let src = pattern(20 * 512 + i as usize);
            host.submit_frame(&src, 1_700_000_000_000_000_000 + i * 1_000_000, 0)
                .unwrap();
            if let Ok(f) = client.poll_frame() {
                assert_eq!(f.data, src);
                got += 1;
            }
        }
        assert_eq!(got, 20);
        assert!(client.stats().fec_recovered_shards > 0);
    }

    /// A streamed frame in slice mode reassembles on the v2 wire whichever block lands first.
    #[test]
    fn v2_streamed_slices_reassemble() {
        let (mut host, mut client) = v2_pair(0);
        let src = pattern(40 * 512 + 9);
        let mut au = host
            .begin_streamed_frame_at(5, crate::packet::USER_FLAG_SLICE_STREAM, 3)
            .unwrap();
        let mut wires = Vec::new();
        for chunk in src.chunks(17 * 512) {
            wires.extend(host.seal_streamed_chunk(&mut au, chunk, true).unwrap());
        }
        wires.extend(host.seal_streamed_finish(au).unwrap());
        wires.reverse();
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        host.send_sealed(&refs).unwrap();
        let f = client.poll_frame().unwrap();
        assert_eq!((f.frame_index, f.data.len()), (3, src.len()));
        assert_eq!(f.data, src);
    }

    /// Sealed v2 media crosses under both suites, and every packet is 16 bytes longer than the
    /// unsealed one: the tag, no sequence prefix.
    #[test]
    fn v2_sealed_frames_cross_under_both_suites() {
        use crate::crypto::MediaSuite;
        for suite in [MediaSuite::Aes128Gcm, MediaSuite::ChaCha20Poly1305] {
            let (mut host, mut client) = v2_pair_keyed(0, Some(media_keys(suite)));
            for i in 0..6usize {
                let src = pattern(i * 3000 + 1);
                host.submit_frame(&src, 1_700_000_000_000_000_000 + i as u64 * 1000, 0)
                    .unwrap();
                assert_eq!(
                    client.poll_frame().unwrap().data,
                    src,
                    "{suite:?} frame {i}"
                );
            }
            let wires = host.seal_frame(&pattern(100), 0, 0).unwrap();
            assert_eq!(
                wires[0].len(),
                crate::packet::V2_HEADER_LEN + 512 + crate::crypto::TAG_LEN
            );
        }
    }

    /// A tampered clear prefix fails the tag, a replayed packet is dropped, and a frame that
    /// straddles a key change opens on both sides of it.
    #[test]
    fn v2_sealed_wire_resists_tamper_and_replay_across_a_key_change() {
        let (mut host, mut client) =
            v2_pair_keyed(0, Some(media_keys(crate::crypto::MediaSuite::Aes128Gcm)));
        host.next_seq = crate::crypto::MEDIA_KEY_PACKETS - 3;
        let src = pattern(20 * 512);
        let wires = host.seal_frame(&src, 1_700_000_000_000_000_000, 0).unwrap();
        assert!(wires.len() > 6, "the frame must straddle the key change");
        let mut tampered = wires[0].clone();
        tampered[0] = crate::packet::V2_STREAM_PROBE;
        let refs: Vec<&[u8]> = std::iter::once(tampered.as_slice())
            .chain(wires.iter().map(|w| w.as_slice()))
            .chain(std::iter::once(wires[1].as_slice()))
            .collect();
        host.send_sealed(&refs).unwrap();
        let f = client.poll_frame().unwrap();
        assert_eq!(f.data, src);
        assert!(matches!(client.poll_frame(), Err(PunktfunkError::NoFrame)));
        assert_eq!(client.stats().packets_received as usize, wires.len());
        assert!(
            client.stats().packets_dropped >= 1,
            "the replayed packet is dropped"
        );
    }

    /// A packet number far past the newest is dropped before any key is derived for it.
    #[test]
    fn v2_implausible_packet_numbers_are_dropped_unopened() {
        let (mut host, mut client) =
            v2_pair_keyed(0, Some(media_keys(crate::crypto::MediaSuite::Aes128Gcm)));
        host.next_seq = 3 * crate::crypto::MEDIA_KEY_PACKETS;
        host.submit_frame(&pattern(700), 1_700_000_000_000_000_000, 0)
            .unwrap();
        assert!(matches!(client.poll_frame(), Err(PunktfunkError::NoFrame)));
        assert!(client.stats().packets_dropped >= 2);
        assert_eq!(client.stats().packets_received, 0);
    }
}
