//! Host side: split an access unit into FEC-protected shard packets.

use super::*;
use crate::config::Config;
use crate::error::{PunktfunkError, Result};
use crate::fec::ErasureCoder;

/// Splits an access unit into FEC-protected shard packets. Host-side only.
///
/// [`packetize_each`](Self::packetize_each) takes `Some(frame_index)` when the caller owns
/// numbering (encode-loop RFI must match the wire), or `None` to draw from the internal
/// counter. Probe filler uses [`alloc_probe_index`](Self::alloc_probe_index) so a burst
/// never consumes video indexes ([`crate::quic::VIDEO_CAP_PROBE_SEQ`]). Do not mix
/// `Some`/`None` in one index space.
pub struct Packetizer {
    next_frame_index: u32,
    next_probe_index: u32,
    shard_payload: usize,
    /// Negotiated frame-size cap. [`set_shard_payload`](Self::set_shard_payload) re-derives
    /// per-frame block ceilings from this and the live shard size.
    max_frame_bytes: usize,
    fec: crate::config::FecConfig,
    /// Zero-padded scratch for the last data shard (partial or empty-frame). Every other
    /// data shard is a `shard_payload` slice into the frame; only the last can be short.
    tail: Vec<u8>,
    /// Per-block parity pools for [`ErasureCoder::encode_into`]. Data-first wire order
    /// emits every block's data before any parity, so each block's recovery must live
    /// until the frame's second emission pass — one shared pool would overwrite.
    recovery: Vec<Vec<Vec<u8>>>,
    /// Peer's per-block `data + recovery` ceiling, frozen from the negotiated config
    /// ([`ReassemblerLimits::from_config`]). Adaptive FEC may raise `fec_percent` live;
    /// clamp parity to this or the far side drops the whole block and loss ratchets FEC up.
    max_total_shards: usize,
    /// Per-frame block ceiling for streamed AUs (size unknown until finish). Receiver
    /// re-derives from each packet's `shard_bytes`; keep this in step with the stamped size.
    max_blocks: usize,
    /// Streamed block-count ceiling in SLICE mode ([`USER_FLAG_SLICE_STREAM`]): variable-K,
    /// floored at `min(MIN_STREAM_BLOCK_SHARDS, max_data_per_block)` shards per block.
    slice_block_cap: usize,
}

/// In-progress streamed access unit. Encoder chunks enter via
/// [`Packetizer::push_streamed`]; blocks leave under sentinel headers (`block_count = 0`,
/// see [`PacketHeader`]) before the AU size is known. [`Packetizer::finish_streamed`] seals
/// the tail with the real totals. Requires [`crate::quic::VIDEO_CAP_STREAMED_AU`]; slice
/// cuts also need [`crate::quic::VIDEO_CAP_MULTI_SLICE`].
pub struct StreamedAu {
    frame_index: u32,
    pts_ns: u64,
    user_flags: u32,
    /// Unsealed remainder (sub-shard plus anything below the slice-flush floor). Flushes
    /// keep ≥ 1 shard back so [`Packetizer::finish_streamed`] always has a real tail to seal.
    pending: Vec<u8>,
    blocks_out: u16,
    total_bytes: u64,
    /// Whole shards already emitted — next sentinel base in shard units. Bases stay
    /// shard-aligned so the layout tiles; the receiver derives the final base the same way.
    emitted_shards: u64,
}

/// Slice-flush floor. Below this, per-block FEC is `ceil(k × pct/100) ≥ 1` regardless of
/// `k` (~22 KB at the standard shard payload). Smaller slices ride with the next one.
pub const MIN_STREAM_BLOCK_SHARDS: usize = 16;

impl StreamedAu {
    pub fn frame_index(&self) -> u32 {
        self.frame_index
    }
}

impl Packetizer {
    pub fn new(config: &Config) -> Self {
        let max_data = config.fec.max_data_per_block as usize;
        let mut p = Packetizer {
            next_frame_index: 0,
            next_probe_index: 0,
            shard_payload: config.shard_payload,
            max_frame_bytes: config.max_frame_bytes,
            fec: config.fec,
            tail: Vec::new(),
            recovery: Vec::new(),
            // Mirrors `ReassemblerLimits::from_config` — keep the two in step.
            max_total_shards: (max_data + config.fec.recovery_for(max_data))
                .min(config.fec.scheme.max_total_shards()),
            // Derived from the shard size below (single source of truth for the formulas).
            max_blocks: 0,
            slice_block_cap: 0,
        };
        p.set_shard_payload(config.shard_payload);
        p
    }

    /// Live-swap the wire shard payload (see `design/shard-payload-reneg.md`). Next AU
    /// only — never with a [`StreamedAu`] in flight: its tiling is keyed on the open size.
    /// Block ceilings follow here; the receiver re-derives from each header's `shard_bytes`.
    /// Call via [`Session::set_shard_payload`](crate::session::Session::set_shard_payload).
    pub fn set_shard_payload(&mut self, shard_payload: usize) {
        let max_data = self.fec.max_data_per_block as usize;
        let total_data_max = self.max_frame_bytes.div_ceil(shard_payload.max(1)).max(1);
        self.shard_payload = shard_payload;
        self.max_blocks = total_data_max.div_ceil(max_data).max(1);
        // Non-final SLICE blocks carry ≥ `min(MIN_STREAM_BLOCK_SHARDS, K)` data shards,
        // so a max-size frame bounds the count. Mirrors the receiver's slice firewall.
        self.slice_block_cap = total_data_max / MIN_STREAM_BLOCK_SHARDS.min(max_data.max(1)) + 2;
    }

    pub fn shard_payload(&self) -> usize {
        self.shard_payload
    }

    /// Next probe-space frame index (speed-test filler). Separate from video numbering so a
    /// burst never advances it. Clients that advertise [`crate::quic::VIDEO_CAP_PROBE_SEQ`]
    /// route [`FLAG_PROBE`] shards into their own reassembly window.
    pub fn alloc_probe_index(&mut self) -> u32 {
        let i = self.next_probe_index;
        self.next_probe_index = i.wrapping_add(1);
        i
    }

    /// Live-adjust FEC recovery percent (next AU). Packets carry their own data/recovery
    /// counts, so the receiver needs no notice.
    pub fn set_fec_percent(&mut self, pct: u8) {
        self.fec.fec_percent = pct.min(90);
    }

    pub fn fec_percent(&self) -> u8 {
        self.fec.fec_percent
    }

    /// One AU's shard geometry: what the data pass, the parity pass and a parity thread
    /// agree on before any of them runs.
    pub fn geometry(&self, frame_len: usize) -> Geometry {
        let payload = self.shard_payload;
        let total_data = frame_len.div_ceil(payload).max(1);
        let max_block = self.fec.max_data_per_block as usize;
        Geometry {
            payload,
            total_data,
            max_block,
            block_count: total_data.div_ceil(max_block).max(1),
            frame_bytes: frame_len as u32,
            fec: self.fec,
            max_total_shards: self.max_total_shards,
        }
    }

    /// The parity pool, for a caller that fills it off-thread with [`parity`] and hands
    /// it back through [`put_recovery`](Self::put_recovery) before
    /// [`emit_parity`](Self::emit_parity).
    pub fn take_recovery(&mut self) -> Vec<Vec<Vec<u8>>> {
        std::mem::take(&mut self.recovery)
    }

    pub fn put_recovery(&mut self, recovery: Vec<Vec<Vec<u8>>>) {
        self.recovery = recovery;
    }

    /// Packetize one AU, yielding `(header, shard)` to `emit` in wire order — also the
    /// order the session nonce advances. No per-packet allocation: the caller can seal
    /// into a pooled buffer ([`Session::seal_frame`](crate::session::Session::seal_frame)).
    /// An `emit` error is fatal.
    ///
    /// Wire order is data-first: every block's data shards, then every block's parity.
    /// Lossless completion is the last data shard, not the parity tail. The receiver is
    /// order-agnostic (`data + recovery ≥ k`).
    ///
    /// `frame_index`: `Some(i)` is the caller's index (encode-loop RFI 1:1 with the
    /// client); `None` draws from the internal counter. Do not mix styles in one space.
    pub fn packetize_each(
        &mut self,
        frame: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: Option<u32>,
        coder: &dyn ErasureCoder,
        mut emit: impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let frame_index = frame_index.unwrap_or_else(|| {
            let i = self.next_frame_index;
            self.next_frame_index = i.wrapping_add(1);
            i
        });
        let geo = self.geometry(frame.len());
        geo.check()?;
        let mut recovery = std::mem::take(&mut self.recovery);
        let fec = parity(&geo, frame, coder, &mut recovery);
        self.recovery = recovery;
        fec?;
        self.emit_data(&geo, frame, pts_ns, user_flags, frame_index, &mut emit)?;
        self.emit_parity(&geo, pts_ns, user_flags, frame_index, &mut emit)
    }

    /// Pass 1 of [`packetize_each`](Self::packetize_each): every block's data shards, in
    /// order. `frame_index` is the caller's, already resolved.
    pub fn emit_data(
        &mut self,
        geo: &Geometry,
        frame: &[u8],
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
        emit: &mut dyn FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let payload = geo.payload;
        let full_shards = frame.len() / payload;
        self.tail.clear();
        self.tail.resize(payload, 0);
        let rem = frame.len() % payload;
        if rem > 0 {
            self.tail[..rem].copy_from_slice(&frame[full_shards * payload..]);
        }
        for b in 0..geo.block_count {
            let first = b * geo.max_block;
            for shard_index in 0..geo.data_count(b) {
                let s = first + shard_index;
                let body: &[u8] = if s < full_shards {
                    &frame[s * payload..(s + 1) * payload]
                } else {
                    &self.tail
                };
                emit(
                    &geo.header(pts_ns, frame_index, user_flags, b, shard_index),
                    body,
                )?;
            }
        }
        Ok(())
    }

    /// Pass 2: every block's parity, the frame's tail on the wire. The pool must hold this
    /// frame's parity ([`parity`]).
    pub fn emit_parity(
        &mut self,
        geo: &Geometry,
        pts_ns: u64,
        user_flags: u32,
        frame_index: u32,
        emit: &mut dyn FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        for b in 0..geo.block_count {
            let k = geo.data_count(b);
            for r in 0..geo.recovery_count(k) {
                let hdr = geo.header(pts_ns, frame_index, user_flags, b, k + r);
                emit(&hdr, &self.recovery[b][r])?;
            }
        }
        Ok(())
    }

    /// Open a streamed AU (see [`StreamedAu`]). `frame_index` matches
    /// [`packetize_each`](Self::packetize_each): `Some(i)` caller-owned; `None` internal.
    pub fn begin_streamed(
        &mut self,
        pts_ns: u64,
        user_flags: u32,
        frame_index: Option<u32>,
    ) -> StreamedAu {
        let frame_index = frame_index.unwrap_or_else(|| {
            let i = self.next_frame_index;
            self.next_frame_index = i.wrapping_add(1);
            i
        });
        StreamedAu {
            frame_index,
            pts_ns,
            user_flags,
            pending: Vec::new(),
            blocks_out: 0,
            total_bytes: 0,
            emitted_shards: 0,
        }
    }

    /// Feed one encoder chunk. Completed blocks leave as sentinels ([`PacketHeader`]).
    /// `slice_end` is an Annex-B cut: only then may a partial block flush, and only whole shards
    /// (remainder stays pending so bases stay aligned). Tails wait for
    /// [`MIN_STREAM_BLOCK_SHARDS`]. The last block — real totals — is never emitted here.
    pub fn push_streamed(
        &mut self,
        au: &mut StreamedAu,
        chunk: &[u8],
        slice_end: bool,
        coder: &dyn ErasureCoder,
        mut emit: impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        au.total_bytes += chunk.len() as u64;
        au.pending.extend_from_slice(chunk);
        let payload = self.shard_payload;
        let block_bytes = self.fec.max_data_per_block as usize * payload;
        // [`USER_FLAG_SLICE_STREAM`] gates slice cuts: without it `slice_end` is inert and
        // every sentinel is full-K. Callers pass `slice_end` always and gate with the flag.
        let slice_wire = au.user_flags & USER_FLAG_SLICE_STREAM != 0;
        // One chunk can fill several blocks; keep cutting so the leftover never exceeds K.
        loop {
            let whole = au.pending.len() / payload;
            // Full-K flush is the hard ceiling; slice boundaries may flush earlier.
            let must_flush = au.pending.len() > block_bytes;
            let slice_flush = slice_wire && slice_end && whole >= MIN_STREAM_BLOCK_SHARDS;
            if !(must_flush || slice_flush) {
                return Ok(());
            }
            // This sentinel plus the yet-to-seal final block, vs the mode's ceiling and u16.
            let cap = if slice_wire {
                self.slice_block_cap
            } else {
                self.max_blocks
            };
            if au.blocks_out as usize + 2 > cap.min(u16::MAX as usize) {
                return Err(PunktfunkError::Unsupported(
                    "streamed AU exceeds the negotiated max_frame_bytes",
                ));
            }
            // Never empty `pending`. A zero-padded final shard would derive base
            // `total_data − 1`, overlapping the block just flushed; the receiver then
            // rejects the AU. Slice arm only: the full-K `must_flush` is strict `>`, so
            // remainder is never empty. One shard rides out in the final block anyway.
            let mut k = whole.min(self.fec.max_data_per_block as usize);
            if k > 1 && k == whole && au.pending.len() == whole * payload {
                k -= 1;
            }
            let (bi, pts, uf) = (au.blocks_out, au.pts_ns, au.user_flags);
            let fi = au.frame_index;
            let base_bytes = if slice_wire {
                au.emitted_shards
                    .checked_mul(payload as u64)
                    .and_then(|b| u32::try_from(b).ok())
                    .ok_or(PunktfunkError::Unsupported("streamed AU exceeds u32 bytes"))?
            } else {
                0 // full-K: `encode_v2` places it at `block × K`
            };
            let flush_len = k * payload;
            self.emit_streamed_block(
                fi,
                pts,
                uf,
                bi,
                &au.pending[..flush_len],
                base_bytes,
                0,
                coder,
                &mut emit,
            )?;
            au.pending.drain(..flush_len);
            au.emitted_shards += k as u64;
            au.blocks_out += 1;
        }
    }

    /// Seal the final block with the real `frame_bytes`/`block_count`, which the receiver
    /// retro-validates the frame against. An empty AU is one zero-padded shard
    /// (`block_count = 1`, never a sentinel).
    pub fn finish_streamed(
        &mut self,
        au: StreamedAu,
        coder: &dyn ErasureCoder,
        mut emit: impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let frame_bytes = u32::try_from(au.total_bytes)
            .map_err(|_| PunktfunkError::Unsupported("streamed AU exceeds u32 bytes"))?;
        let block_count = au.blocks_out + 1;
        self.emit_streamed_block(
            au.frame_index,
            au.pts_ns,
            au.user_flags,
            au.blocks_out,
            &au.pending,
            frame_bytes,
            block_count,
            coder,
            &mut emit,
        )
    }

    /// One streamed block (data then parity). Sentinels pass `block_count = 0` and the
    /// `frame_bytes` [`PacketHeader`] describes; the final block passes the real totals.
    #[allow(clippy::too_many_arguments)]
    fn emit_streamed_block(
        &mut self,
        frame_index: u32,
        pts_ns: u64,
        user_flags: u32,
        block_index: u16,
        bytes: &[u8],
        frame_bytes: u32,
        block_count: u16,
        coder: &dyn ErasureCoder,
        emit: &mut impl FnMut(&PacketHeader, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let payload = self.shard_payload;
        if payload > u16::MAX as usize {
            return Err(PunktfunkError::InvalidArg("shard_payload exceeds u16"));
        }
        // At least one (zero-padded) data shard even for an empty final block (empty AU).
        let k = bytes.len().div_ceil(payload).max(1);
        let m = self
            .fec
            .recovery_for(k)
            .min(self.max_total_shards.saturating_sub(k));
        if k + m > u16::MAX as usize {
            return Err(PunktfunkError::Unsupported("block shard count exceeds u16"));
        }
        let full_shards = bytes.len() / payload;
        self.tail.clear();
        self.tail.resize(payload, 0);
        let rem = bytes.len() % payload;
        if rem > 0 {
            self.tail[..rem].copy_from_slice(&bytes[full_shards * payload..]);
        }
        let tail = &self.tail;
        let shard_at = |s: usize| -> &[u8] {
            if s < full_shards {
                &bytes[s * payload..(s + 1) * payload]
            } else {
                tail.as_slice()
            }
        };
        let data_shards: Vec<&[u8]> = (0..k).map(shard_at).collect();
        if self.recovery.is_empty() {
            self.recovery.push(Vec::new());
        }
        coder.encode_into(&data_shards, m, &mut self.recovery[0])?;

        let hdr = |shard_index: usize| PacketHeader {
            pts_ns,
            frame_index,
            frame_bytes,
            user_flags,
            block_index,
            block_count,
            data_shards: k as u16,
            recovery_shards: m as u16,
            shard_index: shard_index as u16,
            shard_bytes: payload as u16,
        };
        for (shard_index, body) in data_shards.iter().enumerate() {
            emit(&hdr(shard_index), body)?;
        }
        for (r, body) in self.recovery[0][..m].iter().enumerate() {
            emit(&hdr(k + r), body)?;
        }
        Ok(())
    }
}

/// One AU's shard geometry. One arithmetic for every pass, so the data thread and the
/// parity thread cannot disagree about a block.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub payload: usize,
    pub total_data: usize,
    pub max_block: usize,
    pub block_count: usize,
    /// `frame.len()`, as the header carries it.
    pub frame_bytes: u32,
    fec: crate::config::FecConfig,
    max_total_shards: usize,
}

impl Geometry {
    pub fn data_count(&self, b: usize) -> usize {
        ((b + 1) * self.max_block).min(self.total_data) - b * self.max_block
    }

    pub fn recovery_count(&self, k: usize) -> usize {
        self.fec
            .recovery_for(k)
            .min(self.max_total_shards.saturating_sub(k))
    }

    pub fn total_recovery(&self) -> usize {
        (0..self.block_count)
            .map(|b| self.recovery_count(self.data_count(b)))
            .sum()
    }

    /// Data shards plus parity.
    pub fn wire_packets(&self) -> usize {
        self.total_data + self.total_recovery()
    }

    /// The u16 wire fields hold this frame. `Config::validate` covers the negotiated
    /// max; this catches an oversize frame anyway.
    pub fn check(&self) -> Result<()> {
        if self.payload > u16::MAX as usize {
            return Err(PunktfunkError::InvalidArg("shard_payload exceeds u16"));
        }
        if self.block_count > u16::MAX as usize {
            return Err(PunktfunkError::Unsupported(
                "frame too large: block count exceeds u16",
            ));
        }
        for b in 0..self.block_count {
            let k = self.data_count(b);
            if k + self.recovery_count(k) > u16::MAX as usize {
                return Err(PunktfunkError::Unsupported("block shard count exceeds u16"));
            }
        }
        Ok(())
    }

    fn header(
        &self,
        pts_ns: u64,
        frame_index: u32,
        user_flags: u32,
        b: usize,
        shard_index: usize,
    ) -> PacketHeader {
        let k = self.data_count(b);
        PacketHeader {
            pts_ns,
            frame_index,
            frame_bytes: self.frame_bytes,
            user_flags,
            block_index: b as u16,
            block_count: self.block_count as u16,
            data_shards: k as u16,
            recovery_shards: self.recovery_count(k) as u16,
            shard_index: shard_index as u16,
            shard_bytes: self.payload as u16,
        }
    }
}

/// Every block's parity into `out[b]`, from the frame alone, so it can run beside the
/// data pass on another thread. `out` is the packetizer's pool
/// ([`Packetizer::take_recovery`]).
pub fn parity(
    geo: &Geometry,
    frame: &[u8],
    coder: &dyn ErasureCoder,
    out: &mut Vec<Vec<Vec<u8>>>,
) -> Result<()> {
    let payload = geo.payload;
    let full_shards = frame.len() / payload;
    let mut tail = vec![0u8; payload];
    let rem = frame.len() % payload;
    if rem > 0 {
        tail[..rem].copy_from_slice(&frame[full_shards * payload..]);
    }
    if out.len() < geo.block_count {
        out.resize_with(geo.block_count, Vec::new);
    }
    for (b, parity) in out.iter_mut().enumerate().take(geo.block_count) {
        let first = b * geo.max_block;
        let k = geo.data_count(b);
        let shards: Vec<&[u8]> = (first..first + k)
            .map(|s| {
                if s < full_shards {
                    &frame[s * payload..(s + 1) * payload]
                } else {
                    tail.as_slice()
                }
            })
            .collect();
        coder.encode_into(&shards, geo.recovery_count(k), parity)?;
    }
    Ok(())
}
