//! Shared PyroWave access-unit framing for both host encoders.
//!
//! Turns pyrowave's packetized bitstream into either one dense packet or a
//! datagram-aligned windowed AU. Each `chunk`-sized window opens with a 4-byte
//! prefix (`u16` used + `u16` kind): `WIN_PACKED` (whole packets share a window)
//! or a `FRAG` chain (one oversized atomic packet). A lost shard zeroes its
//! window (`used = 0`); the receiver skips it and drops any interrupted
//! fragment chain. Padding after `used` is zeroed.
//!
//! No GPU/FFI. Tests below pin the walk. Clients parse this exact layout, so
//! both backends emit it from here. See `design/pyrowave-codec-plan.md`.

pub const WINDOW_PREFIX: usize = 4;
const WIN_PACKED: u16 = 0;
const WIN_FRAG_FIRST: u16 = 1;
const WIN_FRAG_CONT: u16 = 2;
const WIN_FRAG_LAST: u16 = 3;

/// Shard payload minus the window prefix, so a whole codec packet plus prefix
/// fits one shard. Dense mode uses `dense_cap` (one packet per AU).
pub fn packet_boundary(wire_chunk: Option<usize>, dense_cap: usize) -> usize {
    wire_chunk.map(|c| c - WINDOW_PREFIX).unwrap_or(dense_cap)
}

/// Stamp `ycbcr_range = LIMITED`, `chroma_siting = LEFT` (and, when `bt2020_pq`,
/// BT.2020/PQ/matrix) on the frame's 8-byte `BitstreamSequenceHeader`.
///
/// Pyrowave's C API zero-fills VUI, so it signals FULL and CENTER; both host CSCs
/// emit BT.709 LIMITED (black = Y′16) with left-sited chroma. `seq_offset` is the
/// SOF packet's start. Colour bits live in the LE second word's top byte
/// (`seq_offset + 7`): primaries bit 27 (`0x08`), transfer bit 28 (`0x10`),
/// transform bit 29 (`0x20`), range bit 30 (`0x40`), siting bit 31 (`0x80`).
pub fn stamp_color_bits(bitstream: &mut [u8], seq_offset: usize, bt2020_pq: bool) {
    if let Some(b) = bitstream.get_mut(seq_offset + 7) {
        *b |= 0x40 | 0x80;
        if bt2020_pq {
            *b |= 0x08 | 0x10 | 0x20;
        }
    }
}

/// 3-bit wire sequence counter from a pyrowave block header.
///
/// Layout is `{ u16 ballot; u16 payload_words:12, sequence:3, extended:1; u32 }`
/// (`pyrowave_common.hpp`, `sizeof == 8`). The counter is bits 12..14 of the
/// LE half-word at `packet_offset + 2`.
///
/// The decoder restarts a frame only when this value changes
/// (`diff = (hdr.sequence - last_seq) & 0x7`), so a repeat is more blocks of
/// the same frame. Linux-only caller (alternating encoder handles); Windows
/// builds `-D warnings`, so `dead_code` is allowed off-Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn wire_sequence(bitstream: &[u8], packet_offset: usize) -> Option<u8> {
    let lo = *bitstream.get(packet_offset + 2)?;
    let hi = *bitstream.get(packet_offset + 3)?;
    Some(((u16::from_le_bytes([lo, hi]) >> 12) & 0x7) as u8)
}

/// 32×32-block count for a mode, matching upstream `WaveletBuffers::init_block_meta`.
/// The vendored RDO packs the block index in 16 bits (`RDOperation.block_offset_saving`);
/// a count above `u16::MAX` wraps inside the rate controller, so the host rejects
/// those modes (~8K 4:4:4).
pub fn block_count_32x32(width: u32, height: u32, chroma444: bool) -> u32 {
    const LEVELS: u32 = 5;
    let align = |v: u32| ((v + 31) & !31).max(128);
    let (aw, ah) = (align(width), align(height));
    let mut count = 0u32;
    for level in (0..LEVELS).rev() {
        let lw = (aw / 2) >> level;
        let lh = (ah / 2) >> level;
        let blocks_x8 = lw.div_ceil(8);
        let blocks_y8 = lh.div_ceil(8);
        let per_band = blocks_x8.div_ceil(4) * blocks_y8.div_ceil(4);
        let bands = if level == LEVELS - 1 { 4 } else { 3 };
        for component in 0..3u32 {
            if level == 0 && component != 0 && !chroma444 {
                continue;
            }
            count += per_band * bands;
        }
    }
    count
}

/// Deflates the per-frame rate budget by measured AU/bitstream inflation.
///
/// Window packing pads most windows and adds 4-byte prefixes plus FRAG tails,
/// so the wire is larger than the codec bitstream. The pin is a link budget,
/// so this EMA-scales the target handed to pyrowave's rate control. Sealed-
/// datagram framing and FEC parity are not compensated: H.26x sessions carry
/// those on top of the configured bitrate too.
pub struct WireBudget {
    /// EMA of AU/bitstream bytes, ×1024 fixed point.
    scale_x1024: u32,
}

impl WireBudget {
    /// Startup prior ×1024 ≈ 1.25. EMA converges in ~a second of frames.
    const PRIOR_X1024: u32 = 1280;
    /// EMA weight 1/8: per-frame wobble damps; a mode change re-converges in ~16 frames.
    const EMA_SHIFT: u32 = 3;
    /// Never inflate the budget (×1.0 floor); never deflate below half (tiny bitrates window coarsely).
    const MIN_X1024: u32 = 1024;
    const MAX_X1024: u32 = 2048;

    pub fn new() -> WireBudget {
        WireBudget {
            scale_x1024: Self::PRIOR_X1024,
        }
    }
}

impl Default for WireBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl WireBudget {
    pub fn observe(&mut self, bitstream_len: usize, au_len: usize) {
        if bitstream_len == 0 {
            return;
        }
        let sample = ((au_len as u64 * 1024) / bitstream_len as u64)
            .clamp(Self::MIN_X1024 as u64, Self::MAX_X1024 as u64) as u32;
        let ema = self.scale_x1024 as i64;
        self.scale_x1024 = (ema + ((sample as i64 - ema) >> Self::EMA_SHIFT)) as u32;
    }

    /// Codec budget that makes the wire hit `budget` bytes/frame under the measured inflation.
    pub fn deflate(&self, budget: usize) -> usize {
        let scale = self.scale_x1024.clamp(Self::MIN_X1024, Self::MAX_X1024) as u64;
        ((budget as u64 * 1024) / scale) as usize
    }
}

/// Per-frame rate budget (hard CBR): `bitrate / (8 * fps)`, never below 64 KiB. Both local
/// encoders size their bitstream from [`Self::bytes`] and hand [`Self::rate_control`] to
/// pyrowave's rate control.
pub struct FrameBudget {
    fps: u32,
    /// Bytes per frame the link allows.
    pub bytes: usize,
    /// Windowing inflation → rate-budget deflation, so the pin holds on the wire.
    wire: WireBudget,
}

impl FrameBudget {
    const FLOOR: usize = 64 * 1024;

    pub fn new(bitrate_bps: u64, fps: u32) -> FrameBudget {
        FrameBudget {
            fps,
            bytes: Self::bytes_for(bitrate_bps, fps),
            wire: WireBudget::new(),
        }
    }

    fn bytes_for(bitrate_bps: u64, fps: u32) -> usize {
        ((bitrate_bps / (8 * u64::from(fps.max(1)))) as usize).max(Self::FLOOR)
    }

    /// Retarget in place: free, since every frame is intra and nothing waits on the old rate.
    pub fn retarget(&mut self, bitrate_bps: u64) {
        self.bytes = Self::bytes_for(bitrate_bps, self.fps);
        tracing::debug!(
            mbps = bitrate_bps / 1_000_000,
            budget_kib = self.bytes / 1024,
            "pyrowave: per-frame rate budget retargeted in place"
        );
    }

    /// The target for pyrowave's rate control: [`Self::bytes`], deflated by the measured
    /// windowing inflation when `chunked`, so the pin is the wire rather than the bitstream.
    pub fn rate_control(&self, chunked: bool) -> usize {
        if chunked {
            self.wire.deflate(self.bytes).max(Self::FLOOR)
        } else {
            self.bytes
        }
    }

    /// Feed one windowed AU's inflation: `packets` became `au_len` wire bytes.
    pub fn observe(&mut self, packets: &[(usize, usize)], au_len: usize) {
        let raw: usize = packets.iter().map(|&(_, s)| s).sum();
        self.wire.observe(raw, au_len);
    }
}

/// Where [`build_au`] writes: a growing `Vec`, or a fixed slice for the worker's mapped
/// return buffer. Each write reports whether it fit.
trait AuSink {
    fn len(&self) -> usize;
    fn put(&mut self, bytes: &[u8]) -> bool;
    /// Zero-fill up to `to`.
    fn pad_to(&mut self, to: usize) -> bool;
    fn patch(&mut self, at: usize, bytes: &[u8]);
}

impl AuSink for Vec<u8> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn put(&mut self, bytes: &[u8]) -> bool {
        self.extend_from_slice(bytes);
        true
    }
    fn pad_to(&mut self, to: usize) -> bool {
        self.resize(to.max(Vec::len(self)), 0);
        true
    }
    fn patch(&mut self, at: usize, bytes: &[u8]) {
        self[at..at + bytes.len()].copy_from_slice(bytes);
    }
}

/// A slice with a write cursor.
struct SliceSink<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl AuSink for SliceSink<'_> {
    fn len(&self) -> usize {
        self.len
    }
    fn put(&mut self, bytes: &[u8]) -> bool {
        let end = self.len + bytes.len();
        if end > self.buf.len() {
            return false;
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        true
    }
    fn pad_to(&mut self, to: usize) -> bool {
        let to = to.max(self.len);
        if to > self.buf.len() {
            return false;
        }
        self.buf[self.len..to].fill(0);
        self.len = to;
        true
    }
    fn patch(&mut self, at: usize, bytes: &[u8]) {
        self.buf[at..at + bytes.len()].copy_from_slice(bytes);
    }
}

/// Frame `packets` (offset, size into `bitstream`) into the wire AU.
/// `None` copies the single dense packet; `Some(chunk)` emits whole `chunk`-sized windows.
pub fn build_au(
    packets: &[(usize, usize)],
    bitstream: &[u8],
    wire_chunk: Option<usize>,
) -> Vec<u8> {
    let Some(chunk) = wire_chunk else {
        let (off, size) = packets[0];
        return bitstream[off..off + size].to_vec();
    };
    let mut au: Vec<u8> = Vec::with_capacity((packets.len() + 1) * chunk);
    let fit = build_windows(packets, bitstream, chunk, &mut au);
    debug_assert!(fit, "a Vec sink never runs out");
    au
}

/// [`build_au`] into `out`: the AU's length, or `None` when `out` is shorter than
/// [`au_bound`] says it may need to be.
pub fn build_au_into(
    packets: &[(usize, usize)],
    bitstream: &[u8],
    wire_chunk: Option<usize>,
    out: &mut [u8],
) -> Option<usize> {
    let Some(chunk) = wire_chunk else {
        let (off, size) = packets[0];
        let dst = out.get_mut(..size)?;
        dst.copy_from_slice(&bitstream[off..off + size]);
        return Some(size);
    };
    let mut sink = SliceSink { buf: out, len: 0 };
    build_windows(packets, bitstream, chunk, &mut sink).then_some(sink.len)
}

/// Bytes [`build_au`] can need for `packets`: every packet opens at most one window, an
/// oversized one a window per fragment.
pub fn au_bound(packets: &[(usize, usize)], wire_chunk: Option<usize>) -> usize {
    let Some(chunk) = wire_chunk else {
        return packets.first().map_or(0, |&(_, s)| s);
    };
    let payload_max = chunk.saturating_sub(WINDOW_PREFIX).max(1);
    packets
        .iter()
        .map(|&(_, s)| s.div_ceil(payload_max).max(1))
        .sum::<usize>()
        * chunk
}

/// The strict inverse of [`build_au`]'s windowed layout: the codec packet stream, or what makes
/// `au` something `build_au` never emits. Clients parse leniently instead (a zeroed window is a
/// lost shard); this is the encoder-side check that every window is well formed.
#[cfg(any(test, feature = "test-support"))]
pub fn unwindow(au: &[u8], chunk: usize) -> anyhow::Result<Vec<u8>> {
    use anyhow::{bail, ensure};
    ensure!(
        chunk > WINDOW_PREFIX && au.len() % chunk == 0,
        "AU of {} B is not whole {chunk}-byte windows",
        au.len()
    );
    let mut out = Vec::new();
    let mut frag: Option<Vec<u8>> = None;
    for (i, win) in au.chunks(chunk).enumerate() {
        let used = u16::from_le_bytes([win[0], win[1]]) as usize;
        let kind = u16::from_le_bytes([win[2], win[3]]);
        ensure!(
            WINDOW_PREFIX + used <= chunk,
            "window {i} overruns: used {used}"
        );
        let body = &win[WINDOW_PREFIX..WINDOW_PREFIX + used];
        ensure!(
            win[WINDOW_PREFIX + used..].iter().all(|&b| b == 0),
            "window {i} has non-zero padding after used"
        );
        match (kind, frag.take()) {
            (WIN_PACKED, None) => out.extend_from_slice(body),
            (WIN_FRAG_FIRST, None) => frag = Some(body.to_vec()),
            (WIN_FRAG_CONT, Some(mut f)) => {
                f.extend_from_slice(body);
                frag = Some(f);
            }
            (WIN_FRAG_LAST, Some(f)) => {
                out.extend_from_slice(&f);
                out.extend_from_slice(body);
            }
            (k, _) => bail!("window {i}: kind {k} out of place"),
        }
    }
    ensure!(frag.is_none(), "AU ends inside a fragment chain");
    Ok(out)
}

/// The windowed layout, for either sink. `false` = the sink ran out.
fn build_windows<S: AuSink>(
    packets: &[(usize, usize)],
    bitstream: &[u8],
    chunk: usize,
    au: &mut S,
) -> bool {
    let payload_max = chunk - WINDOW_PREFIX;
    let mut open: Option<(usize, usize)> = None;
    let close = |au: &mut S, open: &mut Option<(usize, usize)>| -> bool {
        match open.take() {
            Some((start, used)) => {
                au.patch(start, &(used as u16).to_le_bytes());
                au.patch(start + 2, &WIN_PACKED.to_le_bytes());
                au.pad_to(start + chunk)
            }
            None => true,
        }
    };
    for &(off, size) in packets {
        let bytes = &bitstream[off..off + size];
        if size <= payload_max {
            let fits = open.is_some_and(|(_, used)| used + size <= payload_max);
            if !fits {
                if !close(au, &mut open) {
                    return false;
                }
                let start = au.len();
                if !au.pad_to(start + WINDOW_PREFIX) {
                    return false;
                }
                open = Some((start, 0));
            }
            if !au.put(bytes) {
                return false;
            }
            if let Some((_, used)) = open.as_mut() {
                *used += size;
            }
        } else {
            // Oversized atomic packet: a FRAG chain of full windows, never packed.
            if !close(au, &mut open) {
                return false;
            }
            let mut o = 0usize;
            while o < size {
                let take = (size - o).min(payload_max);
                let kind = if o == 0 {
                    WIN_FRAG_FIRST
                } else if o + take == size {
                    WIN_FRAG_LAST
                } else {
                    WIN_FRAG_CONT
                };
                let start = au.len();
                if !au.pad_to(start + WINDOW_PREFIX) {
                    return false;
                }
                au.patch(start, &(take as u16).to_le_bytes());
                au.patch(start + 2, &kind.to_le_bytes());
                if !au.put(&bytes[o..o + take]) || !au.pad_to(start + chunk) {
                    return false;
                }
                o += take;
            }
        }
    }
    close(au, &mut open)
}

/// Per-chunk target (~3–4 chunks at 400 Mb/s 60 fps, ~833 KB).
///
/// The sealer flushes only past one FEC block (200 × 1408 = 281 600 B on
/// 1500-MTU IPv4); 256 KiB sits just under that. pf-encode is not told the
/// session's FEC geometry, so this is a fixed byte target. Smaller cuts also
/// grant a fresh `max(bytes/4, 128 KiB)` microburst per sealed batch — the
/// overrun the pacer exists to stop.
const STREAM_CHUNK_TARGET_BYTES: usize = 256 * 1024;
/// Clamp for `PUNKTFUNK_PYROWAVE_CHUNK_KIB` (see [`stream_chunk_step`]).
const STREAM_CHUNK_MIN_KIB: usize = 4;
const STREAM_CHUNK_MAX_KIB: usize = 8192;

/// Whether the knobs in force arm streamed-AU chunks. Default off.
///
/// An unpinned streamed frame (final block never arrived, `frame_bytes` still
/// 0) is excluded from partial delivery; the whole-AU path still hands the
/// consumer a blurred partial. PyroWave clients opt into partial delivery
/// unconditionally, so flipping the default is a live behaviour change.
/// `PUNKTFUNK_PYROWAVE_STREAMED_AU=1` arms it. Outer gates remain the client's
/// `VIDEO_CAP_STREAMED_AU` and the host's `PUNKTFUNK_STREAMED_AU`.
fn stream_armed() -> bool {
    crate::knobs::get().pyrowave_streamed_au == 1
}

/// Never below one window: a target of 0 would yield an empty chunk that spins.
fn chunk_step(window: usize, target: usize) -> usize {
    (target / window.max(1)).max(1) * window.max(1)
}

/// Streamed-AU chunk size for `wire_chunk`, or `None` to stay on the whole-AU
/// path (feature unarmed, or dense mode).
///
/// Dense AUs are one atomic pyrowave packet with no window framing, so a cut
/// is neither shard-aligned nor a parse boundary. Real sessions set
/// `plan.wire_chunk = Some(session.shard_payload())`.
/// `PUNKTFUNK_PYROWAVE_CHUNK_KIB` overrides the target (clamped to
/// [`STREAM_CHUNK_MIN_KIB`]..=[`STREAM_CHUNK_MAX_KIB`]); garbage uses the default.
/// Reads the knobs in force; [`AuStream`] latches the answer per encoder.
pub fn stream_chunk_step(wire_chunk: Option<usize>) -> Option<usize> {
    let window = wire_chunk.filter(|&w| w > 0)?;
    if !stream_armed() {
        return None;
    }
    let target = match crate::knobs::get().pyrowave_chunk_64kib {
        0 => STREAM_CHUNK_TARGET_BYTES,
        // 64 KiB steps; the floor rounds down to the encoder's own minimum.
        steps => (usize::from(steps) * 64 * 1024)
            .clamp(STREAM_CHUNK_MIN_KIB * 1024, STREAM_CHUNK_MAX_KIB * 1024),
    };
    Some(chunk_step(window, target))
}

/// Hands a finished datagram-aligned AU out in window-aligned pieces for
/// [`crate::Encoder::poll_chunk`] / `VIDEO_CAP_STREAMED_AU`. Shared so the
/// cut cannot drift between backends.
///
/// `encode_frame` is synchronous: `submit` returns only once the whole AU
/// sits in `pending`. Chunks pipeline seal/send with itself, not encode with
/// send — unlike H.26x sub-frame slices. The reassembler also completes a
/// streamed AU as one `Frame`; prefix decode is a separate opt-in that
/// PyroWave's newest-wins channel cannot take.
///
/// A chunk is a whole number of `chunk`-sized windows. Each window has one
/// `kind` in its 4-byte prefix; a mid-window cut would split a unit clients
/// parse atomically. Whole windows are `shard_payload` multiples, so sealer
/// sentinel bases stay shard-aligned.
pub struct AuChunker {
    au: Vec<u8>,
    cursor: usize,
    /// Whole-window byte count ([`chunk_step`]).
    step: usize,
    pts_ns: u64,
    keyframe: bool,
    recovery_anchor: bool,
    chunk_aligned: bool,
    /// Set once anything has been emitted, so an empty AU still owes exactly one
    /// chunk rather than an infinite stream.
    emitted: bool,
}

impl AuChunker {
    pub fn new(frame: crate::EncodedFrame, step: usize) -> AuChunker {
        AuChunker {
            au: frame.data,
            cursor: 0,
            step: step.max(1),
            pts_ns: frame.pts_ns,
            keyframe: frame.keyframe,
            recovery_anchor: frame.recovery_anchor,
            chunk_aligned: frame.chunk_aligned,
            emitted: false,
        }
    }

    /// Pieces concatenate to [`crate::Encoder::poll`]; `first` opens the wire frame and `last` closes it.
    // Not an `Iterator`: the chunker is driven by the encoder's poll cadence, not by a loop.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<crate::AuChunk> {
        if self.cursor >= self.au.len() {
            // `build_au` always emits at least one window, but a chunked poll that
            // returned nothing would leak the host's open `StreamedAu`.
            if self.emitted {
                return None;
            }
            self.emitted = true;
            return Some(self.chunk(Vec::new(), true, true));
        }
        let first = self.cursor == 0;
        let end = (self.cursor + self.step).min(self.au.len());
        let data = self.au[self.cursor..end].to_vec();
        self.cursor = end;
        self.emitted = true;
        Some(self.chunk(data, first, end == self.au.len()))
    }

    /// AU metadata is authoritative on `first`; a copy on every chunk keeps a mid-AU log honest.
    fn chunk(&self, data: Vec<u8>, first: bool, last: bool) -> crate::AuChunk {
        crate::AuChunk {
            data,
            pts_ns: self.pts_ns,
            keyframe: self.keyframe,
            recovery_anchor: self.recovery_anchor,
            recovery_point: false,
            recovery_close: false,
            chunk_aligned: self.chunk_aligned,
            first,
            last,
        }
    }
}

/// The datagram-aligned boundary and the streamed-AU cursor over it. Every PyroWave encoder
/// (Windows, Linux, and the Linux worker proxy) holds one, so the cut cannot drift between them.
#[derive(Default)]
pub struct AuStream {
    /// Datagram-aligned packetize boundary. `None` = one dense packet per AU.
    pub wire_chunk: Option<usize>,
    /// The streamed cut, fixed at [`Self::set_chunking`]: `supports_chunked_poll` is asked per
    /// AU, and a knob read live would flip the wire shape between them.
    step: Option<usize>,
    /// AU being handed out in streamed chunks (`Some` between `first` and `last`).
    chunker: Option<AuChunker>,
}

impl AuStream {
    /// Take `shard_payload` as the boundary. Below one block header plus a payload word it
    /// means nothing: `false`, and nothing changes.
    pub fn set_chunking(&mut self, shard_payload: usize) -> bool {
        if shard_payload < 64 {
            return false;
        }
        self.wire_chunk = Some(shard_payload);
        self.step = stream_chunk_step(self.wire_chunk);
        true
    }

    /// [`crate::Encoder::supports_chunked_poll`].
    pub fn supports_chunked_poll(&self) -> bool {
        self.step.is_some()
    }

    /// Each AU drains through one method: a whole-AU `poll` while a cut is open would emit the
    /// same bytes twice under one frame index.
    pub fn check_whole_poll(&self) -> anyhow::Result<()> {
        if self.chunker.is_some() {
            anyhow::bail!("pyrowave: poll() on an AU already being drained through poll_chunk");
        }
        Ok(())
    }

    /// Forfeit a half-handed-out AU, so the next `poll_chunk` cannot splice its tail onto a
    /// fresh one.
    pub fn reset(&mut self) {
        self.chunker = None;
    }

    /// The next piece of the AU already being cut. `None` once it is done: the caller then
    /// produces the next whole AU for [`Self::cut`]. The host keys begin/finish off
    /// `first`/`last` and cannot interleave two AUs.
    pub fn next_open(&mut self) -> Option<crate::AuChunk> {
        let chunk = self.chunker.as_mut()?.next();
        if chunk.is_none() {
            self.chunker = None;
        }
        chunk
    }

    /// Start handing out `f`: window-aligned pieces when streaming is armed, else whole.
    pub fn cut(&mut self, f: crate::EncodedFrame) -> Option<crate::AuChunk> {
        match self.step {
            Some(step) => self.chunker.insert(AuChunker::new(f, step)).next(),
            None => Some(crate::AuChunk::whole(f)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(au: &[u8], chunk: usize) -> Vec<u8> {
        unwindow(au, chunk).expect("a well-formed windowed AU")
    }

    #[test]
    fn dense_is_the_single_packet() {
        let bs = (0u8..=200).collect::<Vec<u8>>();
        let au = build_au(&[(10, 50)], &bs, None);
        assert_eq!(au, bs[10..60]);
    }

    #[test]
    fn packed_windows_pack_small_packets_and_reconstruct() {
        let bs: Vec<u8> = (0..255u32).map(|i| i as u8).collect();
        let packets = [(0, 20), (20, 20), (40, 100)];
        let chunk = 64; // payload_max = 60
        let au = build_au(&packets, &bs, Some(chunk));
        let flat = walk(&au, chunk);
        let mut expect = Vec::new();
        for &(o, s) in &packets {
            expect.extend_from_slice(&bs[o..o + s]);
        }
        assert_eq!(flat, expect);
    }

    #[test]
    fn oversized_packet_fragments_and_reassembles() {
        let bs: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let chunk = 64; // payload_max = 60
        let au = build_au(&[(0, 500)], &bs, Some(chunk));
        assert_eq!(walk(&au, chunk), bs[0..500]);
    }

    /// The slice target lays out the same AU as the `Vec`, inside [`au_bound`], and says so
    /// when it cannot.
    #[test]
    fn slice_target_matches_the_vec_inside_the_bound() {
        let bs: Vec<u8> = (0..2000u32).map(|i| (i * 7) as u8).collect();
        type Case = (&'static [(usize, usize)], Option<usize>);
        let cases: [Case; 4] = [
            (
                &[(0, 20), (20, 20), (40, 100), (140, 500), (640, 3)],
                Some(64),
            ),
            (&[(0, 500)], Some(64)),
            (&[(0, 1), (1, 1), (2, 1)], Some(1408)),
            (&[(10, 50)], None),
        ];
        for (packets, chunk) in cases {
            let want = build_au(packets, &bs, chunk);
            let bound = au_bound(packets, chunk);
            assert!(want.len() <= bound, "{} > bound {bound}", want.len());
            let mut out = vec![0xAAu8; bound];
            let n = build_au_into(packets, &bs, chunk, &mut out).expect("fits the bound");
            assert_eq!(&out[..n], &want[..]);
            let mut short = vec![0u8; want.len().saturating_sub(1)];
            assert_eq!(build_au_into(packets, &bs, chunk, &mut short), None);
        }
    }

    #[test]
    fn boundary_reserves_the_window_prefix() {
        assert_eq!(packet_boundary(Some(1408), 999_999), 1404);
        assert_eq!(packet_boundary(None, 777), 777);
    }

    #[test]
    fn block_count_matches_the_apple_layout_invariant() {
        // Same walk as Apple `WaveletLayout`.
        let manual = |w: u32, h: u32, c444: bool| {
            let align = |v: u32| ((v + 31) & !31).max(128);
            let (aw, ah) = (align(w), align(h));
            let mut n = 0u32;
            for level in (0..5u32).rev() {
                let per = (((aw / 2) >> level).div_ceil(8).div_ceil(4))
                    * (((ah / 2) >> level).div_ceil(8).div_ceil(4));
                let bands = if level == 4 { 4 } else { 3 };
                for c in 0..3 {
                    if level == 0 && c != 0 && !c444 {
                        continue;
                    }
                    n += per * bands;
                }
            }
            n
        };
        for (w, h) in [(256, 144), (1920, 1080), (3840, 2160), (7680, 4320)] {
            assert_eq!(block_count_32x32(w, h, false), manual(w, h, false));
            assert_eq!(block_count_32x32(w, h, true), manual(w, h, true));
        }
        // 4:4:4 fits at 4K; the 16-bit RDO index wraps around 8K 4:4:4.
        assert!(block_count_32x32(3840, 2160, true) <= u16::MAX as u32);
        assert!(block_count_32x32(7680, 4320, true) > u16::MAX as u32);
        assert!(block_count_32x32(7680, 4320, false) <= u16::MAX as u32);
        // 4:2:0 wraps too, later. `Codec::max_dimension()` allows 8192px/axis, so
        // `validate_dimensions` rejects against this count.
        assert_eq!(block_count_32x32(8192, 6144, false), 73728);
        assert_eq!(block_count_32x32(8192, 8192, false), 98304);
        assert!(block_count_32x32(8192, 6144, false) > u16::MAX as u32);
        assert!(block_count_32x32(8192, 8192, false) > u16::MAX as u32);
        assert!(block_count_32x32(7680, 4320, false) <= u16::MAX as u32);
    }

    #[test]
    fn wire_budget_converges_and_deflates() {
        let mut wb = WireBudget::new();
        assert_eq!(wb.deflate(1_024_000), 819_200);
        for _ in 0..64 {
            wb.observe(1000, 1300);
        }
        let b = wb.deflate(1_024_000);
        let expect = 1_024_000_u64 * 1000 / 1300;
        assert!(
            (b as i64 - expect as i64).unsigned_abs() < 8_000,
            "budget {b} should approach {expect}"
        );
        for _ in 0..64 {
            wb.observe(1000, 1000);
        }
        assert_eq!(wb.deflate(1_024_000), 1_024_000);
        for _ in 0..256 {
            wb.observe(10, 1000);
        }
        assert!(wb.deflate(1_024_000) >= 512_000);
        wb.observe(0, 1000);
    }

    #[test]
    fn stamp_color_bits_sets_range_and_hdr_bits() {
        let mut bs = vec![0u8; 16];
        stamp_color_bits(&mut bs, 0, false);
        // Range = bit 30 (`0x40`) and LEFT siting = bit 31 (`0x80`) of the LE second word.
        assert_eq!(bs[7], 0xc0);
        assert!(bs[..7].iter().all(|&b| b == 0));
        assert!(bs[8..].iter().all(|&b| b == 0));
        stamp_color_bits(&mut bs, 0, false);
        assert_eq!(bs[7], 0xc0);
        stamp_color_bits(&mut bs, 100, false);
        // HDR adds BT.2020 primaries (`0x08`) + PQ (`0x10`) + matrix (`0x20`).
        stamp_color_bits(&mut bs, 0, true);
        assert_eq!(bs[7], 0xf8);
    }

    fn frame(data: Vec<u8>) -> crate::EncodedFrame {
        crate::EncodedFrame {
            data,
            pts_ns: 1_234_567,
            keyframe: true,
            recovery_anchor: false,
            recovery_point: false,
            recovery_close: false,
            chunk_aligned: true,
        }
    }

    fn drain(mut c: AuChunker) -> (Vec<u8>, Vec<usize>, Vec<bool>, Vec<bool>) {
        let (mut bytes, mut lens, mut firsts, mut lasts) = (Vec::new(), Vec::new(), vec![], vec![]);
        while let Some(ch) = c.next() {
            lens.push(ch.data.len());
            firsts.push(ch.first);
            lasts.push(ch.last);
            bytes.extend_from_slice(&ch.data);
            assert_eq!(ch.pts_ns, 1_234_567, "AU metadata rides every chunk");
            assert!(ch.keyframe && ch.chunk_aligned && !ch.recovery_anchor);
        }
        (bytes, lens, firsts, lasts)
    }

    /// Chunks concatenate to the AU; every cut is a whole-window boundary so no
    /// window's single `kind` is split across two wire frames.
    #[test]
    fn stream_chunks_tile_the_au_on_window_boundaries() {
        let bs: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let packets = [(0, 20), (20, 300), (320, 55), (375, 900), (1275, 40)];
        let chunk = 64;
        let au = build_au(&packets, &bs, Some(chunk));
        assert!(au.len() / chunk > 4, "need several windows to cut between");
        let step = chunk_step(chunk, 3 * chunk);
        assert_eq!(step, 3 * chunk);
        let (bytes, lens, firsts, lasts) = drain(AuChunker::new(frame(au.clone()), step));
        assert_eq!(bytes, au, "chunks concatenate to exactly the AU");
        assert!(
            lens.iter().all(|l| l % chunk == 0),
            "every chunk is a whole number of windows: {lens:?}"
        );
        assert!(
            lens[..lens.len() - 1].iter().all(|&l| l == step),
            "only the tail chunk may be short: {lens:?}"
        );
        assert_eq!(
            firsts,
            (0..lens.len()).map(|i| i == 0).collect::<Vec<_>>(),
            "exactly one opening chunk"
        );
        assert_eq!(
            lasts,
            (0..lens.len())
                .map(|i| i + 1 == lens.len())
                .collect::<Vec<_>>(),
            "exactly one closing chunk"
        );
        let mut expect = Vec::new();
        for &(o, s) in &packets {
            expect.extend_from_slice(&bs[o..o + s]);
        }
        assert_eq!(walk(&bytes, chunk), expect);
    }

    /// Round down to whole windows, never to zero — a target below one window
    /// becomes one window per chunk, not an empty chunk that would spin.
    #[test]
    fn chunk_step_rounds_down_to_whole_windows() {
        // 262144 / 1408 = 186.2 → 186 windows (261 888 B), not the 262 144 asked for.
        assert_eq!(chunk_step(1408, 256 * 1024), 186 * 1408);
        assert_eq!(chunk_step(1408, 1408), 1408);
        assert_eq!(chunk_step(1408, 1407), 1408);
        assert_eq!(chunk_step(1408, 0), 1408);
        assert_eq!(chunk_step(0, 4096), 4096); // never divide by zero
    }

    /// One `first && last` piece — `handle_chunk` turns that into begin+finish
    /// on one message, byte-identical to the whole-AU path.
    #[test]
    fn single_chunk_au_opens_and_closes_itself() {
        let au = vec![7u8; 512];
        let (bytes, lens, firsts, lasts) = drain(AuChunker::new(frame(au.clone()), 4096));
        assert_eq!(bytes, au);
        assert_eq!(lens, vec![512]);
        assert_eq!(firsts, vec![true]);
        assert_eq!(lasts, vec![true]);
    }

    /// Empty AU still owes one self-closing chunk: returning nothing would leave
    /// the host's `StreamedAu` open (`begin` on `first`, `finish` on `last`).
    #[test]
    fn empty_au_still_emits_one_self_closing_chunk() {
        let mut c = AuChunker::new(frame(Vec::new()), 4096);
        let ch = c.next().expect("one chunk");
        assert!(ch.first && ch.last && ch.data.is_empty());
        assert!(c.next().is_none(), "and never a second one");
    }

    /// Dense AUs never stream: no window framing, so a cut is neither shard-aligned
    /// nor a parse boundary.
    #[test]
    fn dense_mode_never_streams() {
        assert!(stream_chunk_step(None).is_none());
        assert!(stream_chunk_step(Some(0)).is_none());
    }

    /// `unwindow` refuses what `build_au` never emits.
    #[test]
    fn unwindow_refuses_a_malformed_au() {
        let bs: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let chunk = 64;
        let au = build_au(&[(0, 500)], &bs, Some(chunk));
        assert_eq!(unwindow(&au, chunk).unwrap(), bs[..500]);
        assert!(
            unwindow(&au[..au.len() - chunk], chunk).is_err(),
            "open chain"
        );
        assert!(unwindow(&au[chunk..], chunk).is_err(), "CONT without FIRST");
        assert!(unwindow(&au[..au.len() - 1], chunk).is_err(), "ragged tail");
        let mut packed = build_au(&[(0, 20)], &bs, Some(chunk));
        packed[chunk - 1] = 1;
        assert!(unwindow(&packed, chunk).is_err(), "non-zero padding");
    }

    /// One floor on both platforms: 64 KiB a frame. A low rate at a low frame rate is honoured,
    /// not lifted to 1 Mb/s.
    #[test]
    fn frame_budget_floors_at_64_kib_a_frame() {
        assert_eq!(FrameBudget::new(0, 60).bytes, 64 * 1024);
        assert_eq!(FrameBudget::new(600_000, 1).bytes, 75_000);
        let mut b = FrameBudget::new(400_000_000, 60);
        assert_eq!(b.bytes, 833_333);
        assert_eq!(b.rate_control(false), 833_333);
        assert_eq!(
            b.rate_control(true),
            833_333 * 1024 / 1280,
            "startup inflation prior"
        );
        b.retarget(0);
        assert_eq!(b.rate_control(true), 64 * 1024);
    }

    /// A boundary below one block header is ignored, never a switch back to dense.
    #[test]
    fn chunking_below_the_floor_keeps_the_boundary() {
        let mut s = AuStream::default();
        assert!(!s.set_chunking(63));
        assert_eq!(s.wire_chunk, None);
        assert!(s.set_chunking(1408));
        assert!(!s.set_chunking(0));
        assert_eq!(s.wire_chunk, Some(1408));
    }
}
