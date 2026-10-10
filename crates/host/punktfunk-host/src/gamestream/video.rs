//! GameStream video packetizer: one encoded access unit → UDP datagrams a stock
//! Moonlight client decodes (and recovers under loss). Each datagram is
//! `RTP(12 BE) + reserved[4] + NV_VIDEO_PACKET(16 LE) + payload`. The AU is
//! prefixed with an 8-byte `video_short_frame_header_t` and striped into ≤4 FEC
//! blocks of ≤255 shards. Spec: `design/research/gamestream-protocol-research.json`.
//!
//! Reed–Solomon (`Gf8Coder`) runs over the **whole `blocksize` shard**. Moonlight
//! recovers `packetSize + 16` bytes from the datagram start and rejects a shard
//! whose reconstructed `flags` is invalid, so NV fields RS must reproduce
//! (`streamPacketIndex`, `frameIndex`, `flags`, `multiFec*`) are written **before**
//! encoding; RTP/seq/timestamp/`fecInfo` are stamped **after**. `pct = 0` is data-only.
//!
//! With `SS_ENC_VIDEO`, **FEC first, then AES-128-GCM per shard**. The client
//! decrypts received shards and runs RS over those plaintexts; parity over
//! ciphertext recovers nothing. Wire: `[iv 12][frameNumber u32 LE][tag 16] ||
//! ciphertext(blocksize)`. Spent datagrams return through [`VideoPacketizer::recycle`].

use punktfunk_core::fec::{ErasureCoder, Gf8Coder};

/// Moonlight keys on the RTP extension bit (0x10) plus version 2.
const RTP_HEADER_BYTE: u8 = 0x80 | 0x10;
/// `ENC_VIDEO_HEADER`: `[iv 12][frameNumber u32 LE][tag 16]`. Outside the FEC blocksize
/// (`prefix || ciphertext(blocksize)`); 16-aligned so the shard behind it stays aligned.
/// The client subtracted this from negotiated `packetSize`, so the datagram still fits the MTU.
const ENC_PREFIX: usize = 32;
const FLAG_PIC: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;
const MULTI_FEC_FLAGS: u8 = 0x10;
const MAX_DATA_SHARDS_PER_BLOCK: usize = 255;
const MAX_FEC_BLOCKS: usize = 4;
/// RTP(12) + reserved(4) + NV_VIDEO_PACKET(16).
const SHARD_HEADER: usize = 32;
const FRAME_HEADER: usize = 8;
/// ~6 MiB of 1.4 KiB datagrams: one 4K IDR in flight without allocating; idle sessions
/// do not sit on tens of MiB. Returned buffers past this drop.
const POOL_MAX: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    Idr,
    P,
}

pub struct VideoPacketizer {
    /// ANNOUNCE `x-nv-video[0].packetSize`.
    packet_size: usize,
    payload_per_shard: usize,
    /// Requested percent; the wire carries `(100·m)/k` so Moonlight derives the same `m`.
    fec_percentage: usize,
    /// Client `fec.minRequiredFecPackets`. Small frames would otherwise get `⌈k·pct/100⌉ == 1`.
    min_fec: usize,
    frame_index: u32,
    seq: u32,
    /// `(k, m)` Cauchy-matrix cache lives across frames; block shape only moves with frame size.
    coder: Gf8Coder,
    /// `/launch` rikey when `SS_ENC_VIDEO` is on; `None` is the plaintext wire.
    enc_key: Option<[u8; 16]>,
    pool: Vec<Vec<u8>>,
    /// Latched by the oversize-frame drop so a 120 fps stream logs it once, not per frame.
    oversize_logged: bool,
    /// One block's data shards while parity encode borrows them; shell allocation is one-time.
    data_scratch: Vec<Vec<u8>>,
    parity_scratch: Vec<Vec<u8>>,
}

/// Process-global `SS_ENC_VIDEO` nonce counter; never reset. Nonce is
/// `counter_le[8] || 0,0,0 || 'V'`. A session-scoped counter would reuse (key, nonce)
/// on a keyless `/resume` that starts a fresh packetizer on the same rikey.
static IV_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_iv() -> [u8; 12] {
    let n = IV_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut iv = [0u8; 12];
    iv[..8].copy_from_slice(&n.to_le_bytes());
    iv[11] = b'V';
    iv
}

impl VideoPacketizer {
    pub fn new(packet_size: usize, fec_percentage: u8, min_fec: u8) -> Self {
        VideoPacketizer {
            packet_size,
            // `pps` divides in `packetize`; 0 panics. Tiny `packet_size` (≤16) underflows or
            // yields pps==0. `stream_config` already rejects that; `.max(1)` cannot panic.
            payload_per_shard: (packet_size + 16).saturating_sub(SHARD_HEADER).max(1),
            fec_percentage: fec_percentage as usize,
            min_fec: min_fec as usize,
            frame_index: 0,
            seq: 0,
            coder: Gf8Coder::default(),
            enc_key: None,
            pool: Vec::new(),
            oversize_logged: false,
            data_scratch: Vec::new(),
            parity_scratch: Vec::new(),
        }
    }

    /// Call once before the first frame (session rikey).
    pub fn set_encryption_key(&mut self, key: [u8; 16]) {
        self.enc_key = Some(key);
    }

    /// Block geometry is recomputed every `packetize_into`; the client reads `m` from
    /// per-packet `fecInfo` — neither side caches a session percent.
    pub fn set_fec_percent(&mut self, pct: u8) {
        self.fec_percentage = pct as usize;
    }

    /// The paced sender calls this after each frame (`spawn_sender` / `spawn_packetizer`).
    pub fn recycle(&mut self, spent: &mut Vec<Vec<u8>>) {
        let room = POOL_MAX.saturating_sub(self.pool.len());
        self.pool.extend(spent.drain(..).take(room));
    }

    fn take_buf(&mut self, blocksize: usize) -> Vec<u8> {
        let mut b = self.pool.pop().unwrap_or_default();
        b.clear();
        b.resize(blocksize, 0); // recycled buffers may hold stale bytes; resize from empty zero-fills
        b
    }

    /// Test wrapper around [`packetize_into`](Self::packetize_into).
    #[cfg(test)]
    pub fn packetize(
        &mut self,
        au: &[u8],
        frame_type: FrameType,
        timestamp_90k: u32,
        frame_index: Option<u32>,
    ) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.packetize_into(&mut out, au, frame_type, timestamp_90k, frame_index, 0);
        out
    }

    /// `frame_index`: `Some(i)` is the stream loop's index so encoder RFI stays 1:1 with the
    /// wire across encoder rebuilds (`Encoder::submit_indexed`); `None` uses the internal counter.
    /// `processing_100us` is capture→packetize latency in 1/10 ms (Sunshine overlay field); `0` = unmeasured.
    pub fn packetize_into(
        &mut self,
        out: &mut Vec<Vec<u8>>,
        au: &[u8],
        frame_type: FrameType,
        timestamp_90k: u32,
        frame_index: Option<u32>,
        processing_100us: u16,
    ) {
        let frame_index = frame_index.unwrap_or_else(|| {
            let i = self.frame_index;
            self.frame_index = i.wrapping_add(1);
            i
        });
        let pps = self.payload_per_shard;
        let blocksize = SHARD_HEADER + pps;
        let pct = self.fec_percentage;
        // Encrypted datagrams reserve `ENC_PREFIX` in front of the shard; `off = 0` is plaintext.
        let off = if self.enc_key.is_some() {
            ENC_PREFIX
        } else {
            0
        };

        let mut header = short_frame_header(frame_type, processing_100us);
        let total_len = FRAME_HEADER + au.len();
        let last_payload_len = match total_len % pps {
            0 => pps,
            r => r,
        };
        header[4..6].copy_from_slice(&(last_payload_len as u16).to_le_bytes());

        let total_data = total_len.div_ceil(pps).max(1);
        // Cap k so k + ⌈k·pct/100⌉ ≤ 255 (GF(2⁸)): k ≤ 255·100/(100+pct).
        let max_data = if pct > 0 {
            (255 * 100) / (100 + pct)
        } else {
            MAX_DATA_SHARDS_PER_BLOCK
        };
        let n_blocks = total_data.div_ceil(max_data).clamp(1, MAX_FEC_BLOCKS);
        let per_block = total_data.div_ceil(n_blocks);
        // The clamp caps the block COUNT, not the shards per block, so k can still exceed 255:
        // `255 - k` then underflows and `fecInfo`'s 10-bit k truncates. A large IDR at the
        // ANNOUNCE packetSize floor reaches it. Overshooting `max_data` alone is fine — it
        // only costs parity, since `m` is already `min(255 - k)`.
        if per_block > MAX_DATA_SHARDS_PER_BLOCK {
            if !self.oversize_logged {
                self.oversize_logged = true;
                tracing::warn!(
                    total_data,
                    per_block,
                    pps,
                    packet_size = self.packet_size,
                    "frame needs more FEC shards than the wire format can address — dropping it \
                     (raise the client's packetSize)"
                );
            }
            return;
        }

        out.reserve(total_data + total_data * pct / 100 + n_blocks);
        for b in 0..n_blocks {
            let first = b * per_block;
            let last = ((b + 1) * per_block).min(total_data);
            if first >= last {
                break;
            }
            let k = last - first;
            let block_seq_base = self.seq;
            let multi_fec_blocks = ((b as u8) << 4) | (((n_blocks - 1) as u8) << 6);

            // NV fields RS must reproduce (`streamPacketIndex`, `frameIndex`, `flags`,
            // `multiFec*`) go in now; RTP + `fecInfo` stay zero until after encode.
            debug_assert!(self.data_scratch.is_empty());
            for i in 0..k {
                let global = first + i;
                let seq = block_seq_base + i as u32;
                let mut buf = self.take_buf(off + blocksize);
                let shard = &mut buf[off..];
                let mut flags = FLAG_PIC;
                if global == 0 {
                    flags |= FLAG_SOF;
                }
                if global == total_data - 1 {
                    flags |= FLAG_EOF;
                }
                shard[16..20].copy_from_slice(&(seq << 8).to_le_bytes()); // streamPacketIndex
                shard[20..24].copy_from_slice(&frame_index.to_le_bytes()); // frameIndex
                shard[24] = flags;
                shard[26] = MULTI_FEC_FLAGS;
                shard[27] = multi_fec_blocks;
                // Payload [ps, pe): 8-byte header then AU. Only shard 0 can straddle (FRAME_HEADER < pps).
                let ps = global * pps;
                let pe = (ps + pps).min(total_len);
                let mut w = SHARD_HEADER;
                if ps < FRAME_HEADER {
                    let h_end = pe.min(FRAME_HEADER);
                    shard[w..w + (h_end - ps)].copy_from_slice(&header[ps..h_end]);
                    w += h_end - ps;
                }
                if pe > FRAME_HEADER {
                    let a_start = ps.max(FRAME_HEADER) - FRAME_HEADER;
                    let a_end = pe - FRAME_HEADER;
                    shard[w..w + (a_end - a_start)].copy_from_slice(&au[a_start..a_end]);
                }
                self.data_scratch.push(buf);
            }

            // `encode_into` overwrites every parity row, so recycled bytes never survive.
            // Wire percent is `(100·m)/k` so the client derives the same `m`.
            let m = if pct > 0 {
                (k * pct).div_ceil(100).max(self.min_fec).min(255 - k)
            } else {
                0
            };
            let wire_pct = if m > 0 { (100 * m) / k } else { 0 };
            if m > 0 {
                while self.parity_scratch.len() < m {
                    let b = self.pool.pop().unwrap_or_default();
                    self.parity_scratch.push(b);
                }
                // Parity over plaintext (`[off..]`). The client decrypts then RS-recovers;
                // parity over ciphertext would recover nothing.
                let refs: Vec<&[u8]> = self.data_scratch.iter().map(|s| &s[off..]).collect();
                if self
                    .coder
                    .encode_into(&refs, m, &mut self.parity_scratch)
                    .is_err()
                {
                    // Impossible shard shape: send the block data-only rather than panic mid-stream.
                    self.parity_scratch.clear();
                }
            }

            // Stamp RTP + `fecInfo` only. Leave `flags`/`streamPacketIndex` so a recovered
            // shard's RS-reconstructed NV header stays valid.
            self.seq = block_seq_base + k as u32;
            let key = self.enc_key;
            for (i, mut buf) in self.data_scratch.drain(..).enumerate() {
                let seq = block_seq_base + i as u32;
                finalize(
                    &mut buf[off..],
                    seq,
                    timestamp_90k,
                    frame_index,
                    multi_fec_blocks,
                    fec_info(k, i, wire_pct),
                );
                // Seal failure: drop, never send. A 0-byte push would go on the wire; FEC covers the gap.
                if seal_shard(&mut buf, key, frame_index) {
                    out.push(buf);
                }
            }
            // Take scratch so the loop can use `self.pool`; restore after so the Vec allocation lives.
            let mut parity = std::mem::take(&mut self.parity_scratch);
            for (j, mut par) in parity.drain(..).enumerate() {
                let seq = self.seq;
                self.seq = self.seq.wrapping_add(1);
                finalize(
                    &mut par,
                    seq,
                    timestamp_90k,
                    frame_index,
                    multi_fec_blocks,
                    fec_info(k, k + j, wire_pct),
                );
                // `encode_into` sizes parity to the shard; an encrypted session copies it
                // into a prefixed buffer and returns the scratch to the pool.
                let mut buf = if off == 0 {
                    par
                } else {
                    let mut b = self.take_buf(off + blocksize);
                    b[off..].copy_from_slice(&par);
                    self.pool.push(par);
                    b
                };
                if seal_shard(&mut buf, key, frame_index) {
                    out.push(buf);
                }
            }
            self.parity_scratch = parity;
        }
    }
}

/// AES-128-GCM in place over `[ENC_PREFIX..]`, then `[iv 12][frameNumber u32 LE][tag 16]`.
/// **No AAD** — the prefix is unauthenticated, matching stock Moonlight. `key = None` is a no-op.
/// GCM failure is a mis-sized buffer: drop the shard rather than send plaintext under an
/// encrypted negotiation; the client discards a tag miss and FEC covers the gap.
fn seal_shard(buf: &mut [u8], key: Option<[u8; 16]>, frame_index: u32) -> bool {
    use aes_gcm::aead::consts::U12;
    use aes_gcm::aead::{AeadInOut, KeyInit};
    use aes_gcm::{aes::Aes128, AesGcm};

    let Some(key) = key else { return true };
    // `off` and `key` both come from `self.enc_key` in one call. A desync would encrypt
    // from offset 32 into the shard body and corrupt every packet silently.
    debug_assert!(
        buf.len() > ENC_PREFIX,
        "a sealed shard must carry the ENC_VIDEO_HEADER prefix"
    );
    let iv = next_iv();
    let Ok(cipher) = AesGcm::<Aes128, U12>::new_from_slice(&key) else {
        return false;
    };
    let tag = match cipher.encrypt_inout_detached(
        (&iv).into(),
        &[],
        aes_gcm::aead::inout::InOutBuf::from(&mut buf[ENC_PREFIX..]),
    ) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = ?e, "gamestream: video shard seal failed — dropping it");
            return false;
        }
    };
    buf[..12].copy_from_slice(&iv);
    buf[12..16].copy_from_slice(&frame_index.to_le_bytes());
    buf[16..ENC_PREFIX].copy_from_slice(&tag);
    true
}

/// Wire `fecInfo` LE: `dataShards<<22 | fecIndex<<12 | fecPercentage<<4`.
fn fec_info(k: usize, fec_index: usize, pct: usize) -> u32 {
    ((k as u32) << 22) | ((fec_index as u32) << 12) | ((pct as u32) << 4)
}

/// Do not touch NV `flags`/`streamPacketIndex`/`multiFecFlags` (RS-covered).
fn finalize(
    buf: &mut [u8],
    seq: u32,
    ts_90k: u32,
    frame_index: u32,
    multi_fec_blocks: u8,
    fec_info: u32,
) {
    buf[0] = RTP_HEADER_BYTE;
    buf[2..4].copy_from_slice(&(seq as u16).to_be_bytes()); // sequenceNumber
    buf[4..8].copy_from_slice(&ts_90k.to_be_bytes()); // 90 kHz
    buf[20..24].copy_from_slice(&frame_index.to_le_bytes()); // re-affirm for parity
    buf[27] = multi_fec_blocks; // re-affirm for parity
    buf[28..32].copy_from_slice(&fec_info.to_le_bytes()); // fecInfo
}

/// 8-byte `video_short_frame_header_t` (LE) prefixed to the AU. Caller stamps
/// `lastPayloadLen` at offset 4. `processing_100us` is host latency in 1/10 ms (`0` = N/A).
fn short_frame_header(frame_type: FrameType, processing_100us: u16) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[0] = 0x01; // headerType
    h[1..3].copy_from_slice(&processing_100us.to_le_bytes());
    h[3] = match frame_type {
        FrameType::Idr => 2,
        FrameType::P => 1,
    };
    // lastPayloadLen at [4..6] is stamped by the caller; [6..8] unknown = 0
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_block_layout() {
        let mut pk = VideoPacketizer::new(1392, 0, 0);
        assert_eq!(pk.payload_per_shard, 1376);
        let au = vec![0xABu8; 4000];
        let pkts = pk.packetize(&au, FrameType::Idr, 90_000, None);
        assert_eq!(pkts.len(), 3);
        for p in &pkts {
            assert_eq!(p.len(), SHARD_HEADER + 1376);
            assert_eq!(p[0], 0x90);
        }
        let first = &pkts[0];
        assert_eq!(first[24] & FLAG_SOF, FLAG_SOF);
        assert_eq!(first[24] & FLAG_PIC, FLAG_PIC);
        let frame_index = u32::from_le_bytes(first[20..24].try_into().unwrap());
        assert_eq!(frame_index, 0);
        let fec_info = u32::from_le_bytes(first[28..32].try_into().unwrap());
        assert_eq!(fec_info >> 22, 3); // dataShards
        assert_eq!((fec_info >> 12) & 0x3ff, 0); // fecIndex
        let last = &pkts[2];
        assert_eq!(last[24] & FLAG_EOF, FLAG_EOF);
        let fec_info_last = u32::from_le_bytes(last[28..32].try_into().unwrap());
        assert_eq!((fec_info_last >> 12) & 0x3ff, 2);
        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(u16::from_be_bytes(p[2..4].try_into().unwrap()), i as u16);
        }
    }

    /// `255 - k` is a `usize` and `fecInfo` carries k in 10 bits, so a block over 255 data
    /// shards underflows and truncates. The ANNOUNCE floor (packetSize 64 ⇒ 48 B of payload)
    /// reaches it with a frame `MAX_FEC_BLOCKS · 255 · 48` bytes and up.
    #[test]
    fn a_frame_too_large_to_address_is_dropped_not_corrupted() {
        let mut pk = VideoPacketizer::new(64, 20, 2);
        let pps = pk.payload_per_shard;
        // One shard past what four full blocks can carry.
        let au = vec![0u8; (MAX_FEC_BLOCKS * MAX_DATA_SHARDS_PER_BLOCK + 1) * pps];
        assert!(pk.packetize(&au, FrameType::Idr, 0, None).is_empty());

        // Just inside the limit still ships, and every block stays addressable.
        let mut pk = VideoPacketizer::new(64, 20, 2);
        let au = vec![0u8; MAX_FEC_BLOCKS * MAX_DATA_SHARDS_PER_BLOCK * pps - FRAME_HEADER];
        let pkts = pk.packetize(&au, FrameType::Idr, 0, None);
        assert!(!pkts.is_empty());
        for p in &pkts {
            let k = u32::from_le_bytes(p[28..32].try_into().unwrap()) >> 22;
            assert!(
                k <= MAX_DATA_SHARDS_PER_BLOCK as u32,
                "k={k} truncates on the wire"
            );
        }
    }

    /// Overshooting `max_data` is not the same failure: k stays under 255, so the block is
    /// valid and only carries less FEC than requested. It must still be sent.
    #[test]
    fn a_block_over_the_fec_ratio_still_ships() {
        let mut pk = VideoPacketizer::new(1024, 20, 2);
        let pps = pk.payload_per_shard;
        // 900 shards over four blocks is 225 each: past max_data (212) but under 255.
        let au = vec![0u8; 900 * pps - FRAME_HEADER];
        let pkts = pk.packetize(&au, FrameType::Idr, 0, None);
        assert!(!pkts.is_empty(), "a sendable block must not be dropped");
    }

    #[test]
    fn degenerate_packet_size_does_not_panic() {
        // `stream_config` rejects a bad packetSize, but `new` must still not panic on `% pps` /
        // `div_ceil(pps)` / underflow — pps is clamped to ≥ 1.
        for ps in [0usize, 15, 16, 17, 32] {
            let mut pk = VideoPacketizer::new(ps, 20, 2);
            assert!(pk.payload_per_shard >= 1, "pps must never be 0 (ps={ps})");
            let _ = pk.packetize(&[0xCDu8; 200], FrameType::Idr, 0, None);
        }
    }

    #[test]
    fn multi_block_split() {
        let mut pk = VideoPacketizer::new(1392, 0, 0);
        let au = vec![0u8; 600_000];
        let pkts = pk.packetize(&au, FrameType::P, 0, None);
        let total = (8 + au.len()).div_ceil(1376);
        assert_eq!(pkts.len(), total);
        let n_blocks = total.div_ceil(255).clamp(1, 4);
        let last_block = ((pkts.last().unwrap()[27]) >> 6) & 0x3;
        assert_eq!(last_block as usize, n_blocks - 1);
    }

    #[test]
    fn emits_parity_shards() {
        let mut pk = VideoPacketizer::new(1392, 20, 0);
        let au = vec![0xABu8; 4000];
        let pkts = pk.packetize(&au, FrameType::Idr, 0, None);
        // k=3, m=⌈3·20/100⌉=1 → 4 packets; wire_pct = 100·1/3 = 33.
        assert_eq!(pkts.len(), 4);
        for p in &pkts {
            let fec_info = u32::from_le_bytes(p[28..32].try_into().unwrap());
            assert_eq!(fec_info >> 22, 3); // dataShards
            assert_eq!((fec_info >> 4) & 0xff, 33); // fecPercentage
        }
        let parity = &pkts[3];
        let fec_info = u32::from_le_bytes(parity[28..32].try_into().unwrap());
        assert_eq!((fec_info >> 12) & 0x3ff, 3);
        assert_eq!(pkts[0][24] & FLAG_SOF, FLAG_SOF);
        assert_eq!(pkts[2][24] & FLAG_EOF, FLAG_EOF);
        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(u16::from_be_bytes(p[2..4].try_into().unwrap()), i as u16);
        }
    }

    /// RS over the full datagram must restore NV `flags` (the byte Moonlight validates).
    #[test]
    fn parity_recovers_full_datagram_incl_flags() {
        let mut pk = VideoPacketizer::new(1392, 50, 0);
        let au = vec![0x5Au8; 4000];
        let pkts = pk.packetize(&au, FrameType::Idr, 0, None);
        let k = 3usize;
        let m = pkts.len() - k;
        assert!(m >= 1);
        let mut received: Vec<Option<Vec<u8>>> = pkts.iter().map(|p| Some(p.clone())).collect();
        received[1] = None;
        let recovered = Gf8Coder::default()
            .reconstruct(k, m, &mut received)
            .unwrap();
        assert_eq!(recovered[1][24], FLAG_PIC); // flags
        assert_eq!(recovered[1][SHARD_HEADER..], pkts[1][SHARD_HEADER..]);
    }

    /// Host-processing latency is LE u16 at payload offset 1 of shard 0 (short frame header).
    #[test]
    fn frame_processing_latency_is_stamped() {
        let mut pk = VideoPacketizer::new(1392, 0, 0);
        let mut out = Vec::new();
        pk.packetize_into(&mut out, &[0u8; 100], FrameType::P, 0, None, 47);
        let payload = &out[0][SHARD_HEADER..];
        assert_eq!(payload[0], 0x01); // headerType
        assert_eq!(u16::from_le_bytes(payload[1..3].try_into().unwrap()), 47);
        // [`packetize`] stamps 0 (unmeasured).
        let pkts = pk.packetize(&[0u8; 100], FrameType::P, 0, None);
        let payload = &pkts[0][SHARD_HEADER..];
        assert_eq!(u16::from_le_bytes(payload[1..3].try_into().unwrap()), 0);
    }

    // Test-side reassembler: group by FEC block, RS-recover full datagrams, concat payloads.
    // Locks the wire layout against packetizer changes without a live Moonlight client.

    /// `None` if a block lost more shards than it had parity.
    fn recover_au(pkts: &[Option<Vec<u8>>]) -> Option<Vec<u8>> {
        // Block index is byte 27 (`block<<4 | (count-1)<<6`); k and fecIndex live in `fecInfo` at 28..32.
        let any = pkts.iter().flatten().next()?;
        let n_blocks = (((any[27] >> 6) & 0x3) + 1) as usize;
        let blocksize = any.len();
        let mut blocks: Vec<(usize, Vec<Option<Vec<u8>>>)> = Vec::new();
        for _ in 0..n_blocks {
            blocks.push((0, Vec::new()));
        }
        for p in pkts.iter().flatten() {
            let b = ((p[27] >> 4) & 0x3) as usize;
            let fec = u32::from_le_bytes(p[28..32].try_into().unwrap());
            let k = (fec >> 22) as usize;
            let idx = ((fec >> 12) & 0x3ff) as usize;
            let entry = &mut blocks[b];
            entry.0 = k;
            if entry.1.len() <= idx {
                entry.1.resize(idx + 1, None);
            }
            entry.1[idx] = Some(p.clone());
        }
        let mut payload = Vec::new();
        for (k, mut shards) in blocks {
            if k == 0 {
                return None; // no shard named this block's k
            }
            let have_data = shards.iter().take(k).flatten().count();
            let data: Vec<Vec<u8>> = if have_data == k {
                shards
                    .into_iter()
                    .take(k)
                    .map(|s| s.expect("all data shards present"))
                    .collect()
            } else {
                let m_present = shards.iter().skip(k).flatten().count();
                if have_data + m_present < k {
                    return None;
                }
                // Client derives `m` from the wire percent; so does this harness.
                let wire_pct = ((u32::from_le_bytes(
                    shards.iter().flatten().next()?[28..32].try_into().unwrap(),
                ) >> 4)
                    & 0xff) as usize;
                let m = (k * wire_pct).div_ceil(100);
                shards.resize(k + m, None);
                let mut recv = shards;
                let rec = Gf8Coder::default().reconstruct(k, m, &mut recv).ok()?;
                rec.into_iter().take(k).collect()
            };
            for shard in &data {
                assert_eq!(shard.len(), blocksize, "all shards share the block size");
                payload.extend_from_slice(&shard[SHARD_HEADER..]);
            }
        }
        // True length from lastPayloadLen; then drop the 8-byte frame header.
        let last_payload_len = u16::from_le_bytes(payload[4..6].try_into().unwrap()) as usize;
        let pps = blocksize - SHARD_HEADER;
        let total_data = payload.len() / pps;
        let total_len = (total_data - 1) * pps + last_payload_len;
        payload.truncate(total_len);
        Some(payload[FRAME_HEADER..].to_vec())
    }

    /// Deterministic AU (no `rand` — CI-stable).
    fn synthetic_au(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn harness_recovers_within_parity_budget_across_shapes() {
        for (len, pct, min_fec) in [
            (100usize, 20u8, 0u8),
            (4_000, 20, 2),
            (60_000, 10, 1),
            (600_000, 20, 0), // >255 shards → multi-block
        ] {
            let mut pk = VideoPacketizer::new(1392, pct, min_fec);
            let au = synthetic_au(len, 0xC0FFEE);
            let pkts = pk.packetize(&au, FrameType::Idr, 1234, Some(7));
            let all: Vec<Option<Vec<u8>>> = pkts.iter().map(|p| Some(p.clone())).collect();
            assert_eq!(
                recover_au(&all).as_deref(),
                Some(&au[..]),
                "lossless reassembly (len={len})"
            );
            // One data shard per block; m ≥ 1 so this stays within budget.
            let mut lossy = all.clone();
            let mut seen_blocks = std::collections::HashSet::new();
            for (i, p) in pkts.iter().enumerate() {
                let b = (p[27] >> 4) & 0x3;
                let fec = u32::from_le_bytes(p[28..32].try_into().unwrap());
                let is_data = ((fec >> 12) & 0x3ff) < (fec >> 22);
                if is_data && seen_blocks.insert(b) {
                    lossy[i] = None;
                }
            }
            assert_eq!(
                recover_au(&lossy).as_deref(),
                Some(&au[..]),
                "one data loss per block recovers (len={len} pct={pct})"
            );
        }
    }

    #[test]
    fn harness_refuses_past_parity_budget() {
        let mut pk = VideoPacketizer::new(1392, 20, 0);
        let au = synthetic_au(40_000, 0xBEEF); // 30 data, m = 6
        let pkts = pk.packetize(&au, FrameType::P, 0, None);
        let mut lossy: Vec<Option<Vec<u8>>> = pkts.iter().map(|p| Some(p.clone())).collect();
        for slot in lossy.iter_mut().take(7) {
            *slot = None; // 7 > m
        }
        assert_eq!(recover_au(&lossy), None);
    }

    /// Client decrypt: GCM over `[ENC_PREFIX..]` with the prefix IV, no AAD, tag at `[16..32]`.
    /// `None` if the tag misses.
    fn client_open(key: &[u8; 16], dg: &[u8]) -> Option<Vec<u8>> {
        use aes_gcm::aead::consts::U12;
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{aes::Aes128, AesGcm};
        let iv: [u8; 12] = dg[..12].try_into().ok()?;
        // `decrypt` wants `ciphertext || tag`; the wire does not store them adjacently.
        let mut ct_tag = dg[ENC_PREFIX..].to_vec();
        ct_tag.extend_from_slice(&dg[16..ENC_PREFIX]);
        AesGcm::<Aes128, U12>::new_from_slice(key)
            .ok()?
            .decrypt(
                (&iv).into(),
                Payload {
                    msg: &ct_tag,
                    aad: &[],
                },
            )
            .ok()
    }

    #[test]
    fn encrypted_shards_decrypt_to_the_plaintext_wire() {
        let key = [0xA5u8; 16];
        let au = synthetic_au(9_000, 0x1234);
        let mut plain = VideoPacketizer::new(1392, 20, 2);
        let mut enc = VideoPacketizer::new(1392, 20, 2);
        enc.set_encryption_key(key);
        let want = plain.packetize(&au, FrameType::Idr, 7777, Some(3));
        let got = enc.packetize(&au, FrameType::Idr, 7777, Some(3));
        assert_eq!(got.len(), want.len(), "same shard count either way");

        let mut ivs = std::collections::HashSet::new();
        for (sealed, expect) in got.iter().zip(&want) {
            assert_eq!(
                sealed.len(),
                ENC_PREFIX + expect.len(),
                "prefix sits OUTSIDE the FEC blocksize"
            );
            assert_eq!(
                u32::from_le_bytes(sealed[12..16].try_into().unwrap()),
                3,
                "prefix frameNumber"
            );
            assert!(ivs.insert(sealed[..12].to_vec()), "an IV was REUSED");
            assert_eq!(sealed[11], b'V', "IV marker byte");
            let opened = client_open(&key, sealed).expect("tag authenticates");
            assert_eq!(&opened, expect, "decrypts to the plaintext wire image");
        }
        // Tamper must fail: this is a GCM seal, not obfuscation.
        let mut tampered = got[0].clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        assert!(client_open(&key, &tampered).is_none(), "tamper detected");
        assert!(client_open(&[0x00; 16], &got[0]).is_none(), "wrong key");
    }

    /// FEC-then-encrypt: decrypt received shards, then RS-recover a shard never received.
    /// Parity over ciphertext would recover nothing.
    #[test]
    fn encrypted_stream_still_recovers_a_lost_shard() {
        let key = [0x3Cu8; 16];
        let au = synthetic_au(4_000, 0xFEED);
        let mut pk = VideoPacketizer::new(1392, 50, 1);
        pk.set_encryption_key(key);
        let sealed = pk.packetize(&au, FrameType::Idr, 0, Some(0));
        let mut received: Vec<Option<Vec<u8>>> = sealed
            .iter()
            .map(|dg| client_open(&key, dg))
            .collect::<Option<Vec<_>>>()
            .expect("all tags authenticate")
            .into_iter()
            .map(Some)
            .collect();
        received[1] = None;
        assert_eq!(
            recover_au(&received).as_deref(),
            Some(&au[..]),
            "RS recovery over the DECRYPTED shards restores the AU"
        );
    }

    /// Recycled buffers (stale bytes included) must not leak into the wire image.
    #[test]
    fn recycled_buffers_produce_identical_wire_bytes() {
        let mut fresh = VideoPacketizer::new(1392, 20, 2);
        let mut pooled = VideoPacketizer::new(1392, 20, 2);
        // Garbage of mixed sizes: stale bytes must not survive `take_buf`.
        let mut junk: Vec<Vec<u8>> = (0..600).map(|i| vec![0xEEu8; 100 + (i % 1500)]).collect();
        pooled.recycle(&mut junk);
        assert!(junk.is_empty());
        for (n, len) in [(0u32, 4_000usize), (1, 100), (2, 60_000), (3, 9)] {
            let au = synthetic_au(len, n);
            let a = fresh.packetize(&au, FrameType::P, n * 3000, Some(n));
            let mut b = Vec::new();
            pooled.packetize_into(&mut b, &au, FrameType::P, n * 3000, Some(n), 0);
            assert_eq!(a, b, "frame {n}: pooled bytes must match fresh bytes");
            let mut spent = b;
            pooled.recycle(&mut spent);
        }
    }
}
