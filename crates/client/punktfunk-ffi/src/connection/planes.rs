//! The pulled planes: video access units, audio (raw Opus or decoded PCM) and pad
//! audio, and why the session ended once a pull returns `Closed`.

#[cfg(feature = "quic")]
use crate::*;

// Handshake-resolved audio format, read fresh each call; the host never changes it live.
#[cfg(feature = "quic")]
use punktfunk_core::audio::plane::PlaneFormat as AudioFormat;

/// In-core decode for either audio plane ([`punktfunk_core::audio::plane::PlaneDecoder`]), plus the
/// fixed buffer and drought bookkeeping the C surface needs on top.
#[cfg(feature = "quic")]
#[derive(Default)]
pub(super) struct AudioPcmState {
    /// Built on the first packet.
    decoder: Option<punktfunk_core::audio::plane::PlaneDecoder>,
    /// Interleaved f32. Sized once; growth would dangle the pointer handed to the embedder.
    pcm: Vec<f32>,
    /// Seq-gap tracker. Without it a lost packet is a hard click in the playout ring.
    gaps: punktfunk_core::audio::AudioGapTracker,
    /// Last real decode's per-channel samples, the Opus PLC unit. 0 = nothing to size from.
    frame_samples: usize,
    /// PLC frames already given during a drought. Subtract so a later gap is not covered twice.
    drought_frames: u32,
}

#[cfg(feature = "quic")]
impl AudioPcmState {
    /// Size `pcm` once, for one longest frame plus a full concealment run at this session's
    /// frame. The embedder holds a pointer into it until the next PCM call; growth would
    /// dangle it, so the decoder clamps into it instead.
    fn ensure_buffer(&mut self, fmt: AudioFormat) {
        if !self.pcm.is_empty() {
            return;
        }
        let run = punktfunk_core::audio::max_conceal_packets(fmt.frame_us) as usize;
        self.pcm = vec![0f32; (1 + run) * fmt.max_frame_samples().max(1)];
        // Cap the tracker at this same run. A larger cap would silently truncate frames.
        self.gaps.set_frame_us(fmt.frame_us);
    }

    /// Decode one packet into `pcm`. Missing seqs are concealed first, then the
    /// real frame, one interleaved buffer. Empty `data` is DTX: account the slot,
    /// flush owed concealment, never decode. `Ok(0)` = nothing to hand out.
    fn decode_packet(
        &mut self,
        data: &[u8],
        seq: u32,
        fmt: AudioFormat,
    ) -> Result<usize, PunktfunkStatus> {
        if self.decoder.is_none() {
            let dec = punktfunk_core::audio::plane::PlaneDecoder::new(&fmt)
                .map_err(|_| PunktfunkStatus::Unsupported)?;
            self.ensure_buffer(fmt);
            self.decoder = Some(dec);
        }
        let dec = self.decoder.as_mut().unwrap();
        let ch = fmt.channels as usize;

        // Conceal the seq gap first (50 ms cap), less drought frames already in the ring.
        let missing = self
            .gaps
            .missing_before(seq)
            .saturating_sub(std::mem::take(&mut self.drought_frames));
        let mut filled = 0usize;
        for _ in 0..missing {
            match dec.conceal(self.frame_samples, &mut self.pcm[filled..]) {
                Ok(n) if n > 0 => filled += n * ch,
                _ => break,
            }
        }

        if data.is_empty() {
            return Ok(filled);
        }
        match dec.decode(data, &mut self.pcm[filled..]) {
            Ok(n) => {
                if n > 0 {
                    self.frame_samples = n;
                }
                Ok(filled + n * ch)
            }
            // Undecodable: keep concealment already earned. This slot is a ring gap.
            Err(_) if filled > 0 => Ok(filled),
            Err(_) => Err(PunktfunkStatus::BadPacket),
        }
    }

    /// One drought concealment frame, no packet. `Ok(0)` before the first decode.
    fn conceal(&mut self, fmt: AudioFormat) -> Result<usize, PunktfunkStatus> {
        let Some(dec) = self.decoder.as_mut() else {
            return Ok(0);
        };
        match dec.conceal(self.frame_samples, &mut self.pcm) {
            Ok(n) if n > 0 => {
                self.drought_frames = self.drought_frames.saturating_add(1);
                Ok(n * fmt.channels as usize)
            }
            // Nothing to build from, or libopus declined: write nothing (same as a timeout).
            _ => Ok(0),
        }
    }
}

/// Pull the next reassembled access unit, waiting up to `timeout_ms`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// On `Ok`, `*out` borrows until the next `next_au` on this handle (audio/rumble
/// planes do not invalidate it).
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one thread pulls
/// video; it may run concurrently with one audio and one rumble puller.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_au(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkFrame,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_frame(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(frame) => {
                let mut slot = lock_recover(&c.last);
                let f = slot.insert(frame);
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkFrame {
                        data: f.data.as_ptr(),
                        len: f.data.len(),
                        frame_index: f.frame_index,
                        pts_ns: f.pts_ns,
                        flags: f.flags,
                        received_ns: f.received_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// One audio packet. Opus on ordinary sessions, PCM on `0xD3`. `data` borrows
/// until the next `next_audio`. Plane is `host_caps & HOST_CAP_AUDIO_HIRES`.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkAudioPacket {
    pub data: *const u8,
    pub len: usize,
    pub seq: u32,
    pub pts_ns: u64,
}

/// Pull the next audio packet, waiting up to `timeout_ms`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// On `Ok`, `out->data` borrows until the next audio call (independent of video).
/// Drain from a dedicated thread — Opus every 5 ms, lossless every 1–5 ms; queue
/// holds 320 ms.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one audio puller;
/// it may run concurrently with the video/rumble pullers.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_audio(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPacket,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(pkt) => {
                let mut slot = lock_recover(&c.last_audio);
                let p = slot.insert(pkt);
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkAudioPacket {
                        data: p.data.as_ptr(),
                        len: p.data.len(),
                        seq: p.seq,
                        pts_ns: p.pts_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Mute this client's own speakers. Local only: the host keeps encoding and a session joined
/// to the same display keeps hearing the game. Audio keeps arriving and decoding — zero only
/// what you queue for the device — so unmute lands in step instead of re-syncing. Does not
/// clear `PUNKTFUNK_AUDIO_MUTE_HOST`.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_audio_muted(
    c: *mut PunktfunkConnection,
    muted: bool,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.set_audio_muted(muted);
        PunktfunkStatus::Ok
    })
}

/// Why this session is silent: `PUNKTFUNK_AUDIO_MUTE_LOCAL`, `PUNKTFUNK_AUDIO_MUTE_HOST`,
/// both, or `0`. Name the reason in the overlay from this — a local unmute leaves an
/// operator mute standing, and the player is owed the difference.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_mute(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.audio_mute())
}

/// Host-resolved audio channel count: `2` (stereo), `6` (5.1) or `8` (7.1).
/// `*out` is filled when non-NULL. Raw `0xC9` Opus is encoded for this layout
/// ([`punktfunk_core::audio::layout_for`]); or use [`punktfunk_connection_next_audio_pcm`].
/// Fixed until a reconfigure.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_channels(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.audio_channels)
}

/// Resolved sample rate. Open the device from this, not `PUNKTFUNK_AUDIO_SAMPLE_RATE_HZ`
/// (the Opus default). Accessor, not a field: `PunktfunkAudioPcm` has no size guard.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u32`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_sample_rate(
    c: *mut PunktfunkConnection,
    out: *mut u32,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.audio_sample_rate_hz)
}

/// Resolved sample depth (`16`, or `24` on lossless). Plane is
/// `host_caps & PUNKTFUNK_HOST_CAP_AUDIO_HIRES`, not this: 48 kHz/16-bit matches both.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_bits(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.audio_bits)
}

/// Resolved frame length in µs (ladder has sub-ms rungs). `0` = use
/// `PUNKTFUNK_AUDIO_FRAME_MS × 1000`. On 44.1 kHz this is a nominal length, not a
/// duration — size rings from it, advance clocks from samples / rate.
/// Not derivable from `next_audio_pcm`'s `frame_count` (that includes concealment).
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u16`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_frame_us(
    c: *mut PunktfunkConnection,
    out: *mut u16,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.audio_frame_us)
}

/// Why the session ended (`PUNKTFUNK_END_REASON_*`). Latches after `Closed`.
/// `LOCAL`/`GAME_EXITED`/`HOST_ENDED` are not failures. Unknown values = `NONE`.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_end_reason(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.end_reason() as u8)
}

/// One decoded audio frame from [`punktfunk_connection_next_audio_pcm`]: interleaved
/// f32 in wire order `FL FR FC LFE RL RR SL SR` (first `channels` of it). `samples`
/// points at `frame_count * channels` floats and borrows until the next PCM call.
/// Rate/depth are accessors, not fields: this type has no `struct_size`.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkAudioPcm {
    /// Interleaved f32 samples (wire channel order), `frame_count * channels` long.
    pub samples: *const f32,
    /// Samples per channel in this frame.
    pub frame_count: u32,
    /// Channel count (2/6/8) — the negotiated [`punktfunk_connection_audio_channels`].
    pub channels: u8,
    /// Source packet sequence number.
    pub seq: u32,
    /// Capture presentation timestamp (ns).
    pub pts_ns: u64,
}

/// Decode the next audio frame in-core to interleaved f32. Both planes share this
/// call; size the ring from [`punktfunk_connection_audio_sample_rate`]. Seq-gap
/// concealment is prepended in the same buffer. Quiet-wire droughts:
/// [`punktfunk_connection_audio_plc`]. Mutually exclusive with `next_audio`.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one audio puller.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_audio_pcm(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPcm,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let fmt = AudioFormat::of(&c.inner);
        let channels = fmt.channels;
        let pkt = match c
            .inner
            .next_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(pkt) => pkt,
            Err(e) => return e.status(),
        };
        let mut state = lock_recover(&c.audio_pcm);
        match state.decode_packet(&pkt.data, pkt.seq, fmt) {
            // Nothing to hand out: a DTX silence marker with no loss owed before it.
            Ok(0) => PunktfunkStatus::NoFrame,
            Ok(samples) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkAudioPcm {
                        samples: state.pcm.as_ptr(),
                        frame_count: (samples / channels.max(1) as usize) as u32,
                        channels,
                        seq: pkt.seq,
                        pts_ns: pkt.pts_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(status) => status,
        }
    })
}

/// One drought concealment frame with no packet (`design/host-source-stutter-fixes.md`).
/// Call on `NO_FRAME` when the ring is draining. Policy stays on the embedder:
/// bound in time, gated on a real underrun. `seq`/`pts_ns` are 0 — never feed A/V
/// sync. Same PCM slot as `next_audio_pcm`; drought frames are subtracted from
/// the next packet's gap so a covered loss is not concealed twice.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one audio puller.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_plc(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPcm,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let fmt = AudioFormat::of(&c.inner);
        let channels = fmt.channels;
        let mut state = lock_recover(&c.audio_pcm);
        match state.conceal(fmt) {
            Ok(0) => PunktfunkStatus::NoFrame,
            Ok(samples) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkAudioPcm {
                        samples: state.pcm.as_ptr(),
                        frame_count: (samples / channels.max(1) as usize) as u32,
                        channels,
                        seq: 0,
                        pts_ns: 0,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(status) => status,
        }
    })
}

/// Next 0xD1 pad-audio Opus frame, copied into `buf`. Return length, `0` =
/// nothing this poll, `-1` = ended. Fan out by pad/kind. One puller.
///
/// # Safety
/// `c` is a valid connection handle; the `out_*` pointers are writable (NULLs skipped);
/// `buf` is writable for `buf_len` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_pad_audio(
    c: *mut PunktfunkConnection,
    out_pad: *mut u8,
    out_kind: *mut u8,
    out_seq: *mut u32,
    out_pts_ns: *mut u64,
    buf: *mut u8,
    buf_len: usize,
    timeout_ms: u32,
) -> i32 {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return -1,
        };
        if buf.is_null() && buf_len != 0 {
            return -1;
        }
        match c
            .inner
            .next_pad_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Some(f) => {
                if f.opus.is_empty() || f.opus.len() > buf_len {
                    // Empty/oversized: skip like DTX; truncated Opus is undecodable anyway.
                    return 0;
                }
                // SAFETY: out-params are null or writable; the `buf` copy length was just bounded.
                unsafe {
                    put(out_pad, f.pad);
                    put(out_kind, f.kind);
                    put(out_seq, f.seq);
                    put(out_pts_ns, f.pts_ns);
                    std::ptr::copy_nonoverlapping(f.opus.as_ptr(), buf, f.opus.len());
                }
                f.opus.len() as i32
            }
            // `None` folds timeout and closed; the shutdown flag tells them apart so the
            // plane loop can exit instead of polling a dead session forever.
            None if c.inner.is_session_ended() => -1,
            None => 0,
        }
    }));
    r.unwrap_or(-1)
}

/// Declare pad `pad`'s 0xD1 render caps. Call at attach, before arrival; bits
/// fold into arrival flags 8/9. Latest-wins; unknown bits masked.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_pad_audio_caps(
    c: *mut PunktfunkConnection,
    pad: u8,
    audio_caps: u8,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.set_pad_audio_caps(pad, audio_caps);
        PunktfunkStatus::Ok
    })
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;

    /// Opus on `0xC9`, 48 kHz, 16-bit, stereo — what an embedder that does not call
    /// `punktfunk_connect_ex11` still gets.
    const OPUS_48K: AudioFormat = AudioFormat {
        codec: punktfunk_core::quic::AUDIO_CODEC_OPUS,
        rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
        bits: punktfunk_core::audio::pcm::BITS_16,
        channels: 2,
        frame_us: punktfunk_core::audio::FRAME_MS * 1000,
        layout: 0,
    };

    /// Lossless session at 48 kHz / 24-bit.
    const PCM_48K_24: AudioFormat = AudioFormat {
        codec: punktfunk_core::quic::AUDIO_CODEC_PCM,
        rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
        bits: punktfunk_core::audio::pcm::BITS_24,
        channels: 2,
        frame_us: punktfunk_core::audio::pcm::FRAME_US_LADDER[0],
        layout: 0,
    };

    /// Concealment run a 5 ms session owes: ten frames (50 ms cap).
    const CONCEAL_RUN: u32 =
        punktfunk_core::audio::max_conceal_packets(punktfunk_core::audio::FRAME_MS * 1000);

    /// One `0xD3` payload of `n` interleaved stereo samples at `bits`, from a
    /// deterministic ramp so any stride or sign-extension error is visible.
    fn pcm_frame(n: usize, bits: u8) -> (Vec<f32>, Vec<u8>) {
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            samples.push((i as f32 / n as f32) * 1.8 - 0.9);
        }
        let mut wire = Vec::new();
        punktfunk_core::audio::pcm::from_f32(&samples, bits, &mut wire);
        // Quantised once, so the expectation is what the wire carries rather than the
        // pre-quantisation floats.
        let mut expect = Vec::new();
        punktfunk_core::audio::pcm::to_f32(&wire, bits, &mut expect).expect("whole samples");
        (expect, wire)
    }

    /// PCM decode is bit-exact at the ABI boundary.
    #[test]
    fn the_pcm_plane_decodes_bit_exactly() {
        for bits in [
            punktfunk_core::audio::pcm::BITS_16,
            punktfunk_core::audio::pcm::BITS_24,
        ] {
            let fmt = AudioFormat { bits, ..PCM_48K_24 };
            // 5 ms at 48 kHz stereo — the longest rung of the ladder.
            let (expect, wire) = pcm_frame(240 * 2, bits);
            let mut state = AudioPcmState::default();
            let n = state.decode_packet(&wire, 0, fmt).expect("decodes");
            assert_eq!(n, expect.len(), "{bits}-bit: sample count");
            assert_eq!(
                &state.pcm[..n],
                &expect[..],
                "{bits}-bit PCM must reach the embedder unchanged"
            );

            // Stays exact packet after packet, at the buffer offsets a real session uses.
            let (expect2, wire2) = pcm_frame(240 * 2, bits);
            let n2 = state.decode_packet(&wire2, 1, fmt).expect("decodes");
            assert_eq!(&state.pcm[..n2], &expect2[..]);
        }
    }

    /// PCM gaps use `PcmConceal`, never libopus: the conceal frame repeats the last
    /// real one (`design/hi-res-audio.md`).
    #[test]
    fn a_missing_pcm_frame_is_concealed_without_libopus() {
        let bits = punktfunk_core::audio::pcm::BITS_24;
        let (expect, wire) = pcm_frame(240 * 2, bits);
        let mut state = AudioPcmState::default();
        assert_eq!(state.decode_packet(&wire, 0, PCM_48K_24), Ok(expect.len()));

        // Seq 2 is lost: one concealed frame lands in front of the real one,
        // same shape as Opus so nothing downstream branches on the plane.
        let (expect3, wire3) = pcm_frame(240 * 2, bits);
        let n = state.decode_packet(&wire3, 2, PCM_48K_24).expect("decodes");
        assert_eq!(
            n,
            2 * expect3.len(),
            "one concealed frame, then the real one"
        );
        // Concealed frame is the previous one faded: head matches last good sample
        // (raised cosine ~1.0), tail is silence. PLC-synthesized would match neither.
        assert!(
            (state.pcm[0] - expect[0]).abs() < 1e-3,
            "concealment must repeat the last good frame, got {} vs {}",
            state.pcm[0],
            expect[0]
        );
        let tail = state.pcm[expect.len() - 1];
        assert!(
            tail.abs() < 0.01,
            "the fade must land on silence, got {tail}"
        );
        // The real frame follows it, still bit-exact.
        assert_eq!(&state.pcm[expect3.len()..n], &expect3[..]);

        // Drought path takes the same route: PcmConceal, one frame, credited against
        // the next arrival's loss so the gap is never concealed twice.
        let before = state.drought_frames;
        assert_eq!(state.conceal(PCM_48K_24), Ok(expect.len()));
        assert_eq!(state.drought_frames, before + 1);
    }

    /// `pcm` must never reallocate: the embedder holds a pointer into it.
    #[test]
    fn the_pcm_buffer_is_never_reallocated() {
        for fmt in [OPUS_48K, PCM_48K_24] {
            let mut state = AudioPcmState::default();
            let (_, wire) = pcm_frame(240 * 2, fmt.bits);
            // Prime it. On the Opus arm the payload is undecodable: the buffer is
            // sized before the packet is looked at, which is the property under test.
            let _ = state.decode_packet(&wire, 0, fmt);
            let ptr = state.pcm.as_ptr();
            let len = state.pcm.len();
            assert!(len > 0, "sizing must have happened on the first packet");

            // A datagram far longer than any ladder rung — the case that would otherwise
            // grow the buffer.
            let (_, huge) = pcm_frame(200_000, fmt.bits);
            let _ = state.decode_packet(&huge, 1, fmt);
            // A long concealment run, at the far end of the buffer.
            for seq in 2..40 {
                let _ = state.decode_packet(&wire, seq * 7, fmt);
                let _ = state.conceal(fmt);
            }
            assert_eq!(state.pcm.as_ptr(), ptr, "the buffer moved");
            assert_eq!(state.pcm.len(), len, "the buffer was resized");
        }
    }

    /// A truncated `0xD3` payload (not a whole number of samples) must be refused
    /// rather than decoded as a shifted frame. Concealment already earned still goes out.
    #[test]
    fn a_torn_pcm_datagram_is_refused_not_shifted() {
        let mut state = AudioPcmState::default();
        let (_, wire) = pcm_frame(240 * 2, punktfunk_core::audio::pcm::BITS_24);
        assert_eq!(
            state.decode_packet(&wire[..wire.len() - 1], 0, PCM_48K_24),
            Err(PunktfunkStatus::BadPacket)
        );
        // With a gap owed, the concealment survives the bad packet instead of dying with it.
        assert_eq!(state.decode_packet(&wire, 1, PCM_48K_24), Ok(240 * 2));
        assert_eq!(
            state.decode_packet(&wire[..wire.len() - 1], 3, PCM_48K_24),
            Ok(240 * 2),
            "the gap before a torn packet is still concealed"
        );
        // Empty payload must not wipe the concealment source: PCM has no DTX, so this
        // is a torn datagram; clearing `prev` would leave the next loss with nothing to repeat.
        assert_eq!(state.decode_packet(&[], 4, PCM_48K_24), Ok(0));
        assert_eq!(
            state.decode_packet(&wire, 6, PCM_48K_24),
            Ok(2 * 240 * 2),
            "the frame before the empty one must still be there to conceal from"
        );
    }

    /// An Opus session stays on libopus at 48 kHz with the same buffer geometry;
    /// `PcmConceal` is never involved.
    #[test]
    fn an_opus_session_is_unaffected_by_the_lossless_plane() {
        let l = punktfunk_core::audio::LAYOUT_STEREO;
        let mut enc = opus::MSEncoder::new(
            48_000,
            l.streams,
            l.coupled,
            l.mapping,
            opus::Application::LowDelay,
        )
        .expect("MSEncoder");
        enc.set_vbr(false).unwrap();
        let mut frame = vec![0f32; 240 * 2];
        for (i, s) in frame.iter_mut().enumerate() {
            *s = 0.25 * (i as f32 * 0.05).sin();
        }
        let mut out = vec![0u8; 1500];
        let n = enc.encode_float(&frame, &mut out).unwrap();
        out.truncate(n);

        let mut state = AudioPcmState::default();
        assert_eq!(state.decode_packet(&out, 0, OPUS_48K), Ok(240 * 2));
        assert!(state.decoder.is_some(), "still a libopus decoder");
        // 120 ms of Opus plus a full concealment run.
        assert_eq!(state.pcm.len(), (1 + CONCEAL_RUN as usize) * 5760 * 2);
        // Gaps and droughts still go through libopus PLC.
        assert_eq!(state.decode_packet(&out, 2, OPUS_48K), Ok(2 * 240 * 2));
        assert_eq!(state.conceal(OPUS_48K), Ok(240 * 2));

        // Accessors report 48 kHz / 16-bit, matching `PUNKTFUNK_AUDIO_SAMPLE_RATE_HZ`.
        assert_eq!(OPUS_48K.rate_hz, punktfunk_core::audio::SAMPLE_RATE_HZ);
        assert!(!OPUS_48K.is_pcm());
    }

    /// Buffer is sized from the negotiated rate, not a hardcoded 48 kHz.
    #[test]
    fn the_buffer_follows_the_negotiated_rate() {
        let hi = AudioFormat {
            rate_hz: 96_000,
            ..PCM_48K_24
        };
        let mut state = AudioPcmState::default();
        // The longest ladder rung at 96 kHz is 5 ms = 480 samples/ch.
        let (expect, wire) = pcm_frame(480 * 2, hi.bits);
        assert_eq!(state.decode_packet(&wire, 0, hi), Ok(expect.len()));
        assert_eq!(&state.pcm[..expect.len()], &expect[..]);
        assert_eq!(
            state.pcm.len(),
            (1 + CONCEAL_RUN as usize) * 480 * 2,
            "sized from 96 kHz, not from the 48 kHz default"
        );
        // A full concealment run fits, which is the point of sizing it that way.
        let n = state
            .decode_packet(&wire, 1 + CONCEAL_RUN, hi)
            .expect("decodes");
        assert_eq!(n, (1 + CONCEAL_RUN as usize) * expect.len());
    }

    /// 50 ms cap at 2 ms/frame is 25 frames, not 10. Buffer must be sized for
    /// that run: growth would dangle the embedder pointer.
    #[test]
    fn a_short_frame_owes_more_concealment_and_the_buffer_was_sized_for_it() {
        let short = AudioFormat {
            rate_hz: 44_100,
            frame_us: 2_000,
            ..PCM_48K_24
        };
        let run = punktfunk_core::audio::max_conceal_packets(short.frame_us);
        assert_eq!(run, 25, "50 ms of 2 ms frames");

        // 2 ms at 44 100 Hz stereo: 88 samples per channel, not 88.2.
        let frame = punktfunk_core::audio::pcm::samples_per_frame(44_100, 2_000, 2);
        assert_eq!(frame, 176);
        let (expect, wire) = pcm_frame(frame, short.bits);

        let mut state = AudioPcmState::default();
        assert_eq!(state.decode_packet(&wire, 0, short), Ok(expect.len()));
        // Sized for the run at this frame, from the longest rung's frame size (5 ms = 220/ch).
        assert_eq!(state.pcm.len(), (1 + run as usize) * 220 * 2);
        let base = state.pcm.as_ptr();

        // A maximal gap: 25 concealed frames plus the real one, contiguous; the buffer
        // did not move under the embedder's pointer.
        let n = state.decode_packet(&wire, 1 + run, short).expect("decodes");
        assert_eq!(n, (1 + run as usize) * expect.len());
        assert!(n <= state.pcm.len(), "the run must fit what was allocated");
        assert!(
            std::ptr::eq(base, state.pcm.as_ptr()),
            "pcm was reallocated"
        );
    }

    /// In-core PCM decoder heals seq gaps with concealment: a lost packet's PLC
    /// lands in front of the arriving frame, DTX markers advance accounting without
    /// being decoded, and a gap is capped at 50 ms.
    #[test]
    fn audio_pcm_decode_conceals_seq_gaps() {
        const FRAME: usize = 240; // 5 ms @ 48 kHz, per channel
        let l = punktfunk_core::audio::LAYOUT_STEREO;
        let mut enc = opus::MSEncoder::new(
            48_000,
            l.streams,
            l.coupled,
            l.mapping,
            opus::Application::LowDelay,
        )
        .expect("MSEncoder");
        enc.set_vbr(false).unwrap();
        let mut packet = |tone: f32| {
            let mut frame = vec![0f32; FRAME * 2];
            for (i, s) in frame.iter_mut().enumerate() {
                *s = 0.25 * (i as f32 * tone).sin();
            }
            let mut out = vec![0u8; 1500];
            let n = enc.encode_float(&frame, &mut out).unwrap();
            out.truncate(n);
            out
        };

        let mut state = AudioPcmState::default();
        // In-order packets decode to exactly one frame each.
        assert_eq!(
            state.decode_packet(&packet(0.05), 0, OPUS_48K),
            Ok(FRAME * 2)
        );
        assert_eq!(
            state.decode_packet(&packet(0.05), 1, OPUS_48K),
            Ok(FRAME * 2)
        );
        // Seq 2 lost: one concealed frame precedes the real one, contiguously.
        assert_eq!(
            state.decode_packet(&packet(0.06), 3, OPUS_48K),
            Ok(2 * FRAME * 2)
        );
        // A duplicate conceals nothing.
        assert_eq!(
            state.decode_packet(&packet(0.06), 3, OPUS_48K),
            Ok(FRAME * 2)
        );
        // DTX marker, nothing lost before it: nothing to emit (the ABI maps 0 to NoFrame).
        assert_eq!(state.decode_packet(&[], 4, OPUS_48K), Ok(0));
        // A DTX marker after a loss still flushes the concealment owed (seq 5 lost).
        assert_eq!(state.decode_packet(&[], 6, OPUS_48K), Ok(FRAME * 2));
        // The DTX slot itself was accounted, not treated as a loss.
        assert_eq!(
            state.decode_packet(&packet(0.07), 7, OPUS_48K),
            Ok(FRAME * 2)
        );
        // A huge gap is capped at 50 ms of concealment — ten frames at this session's 5 ms.
        assert_eq!(
            state.decode_packet(&packet(0.07), 1000, OPUS_48K),
            Ok((CONCEAL_RUN as usize + 1) * FRAME * 2)
        );
    }

    /// Drought frames must be subtracted from the next gap so a covered loss is not concealed twice.
    #[test]
    fn drought_concealment_is_not_charged_again_by_the_loss_path() {
        const FRAME: usize = 240; // 5 ms @ 48 kHz, per channel
        let l = punktfunk_core::audio::LAYOUT_STEREO;
        let mut enc = opus::MSEncoder::new(
            48_000,
            l.streams,
            l.coupled,
            l.mapping,
            opus::Application::LowDelay,
        )
        .expect("MSEncoder");
        enc.set_vbr(false).unwrap();
        let mut packet = |tone: f32| {
            let mut frame = vec![0f32; FRAME * 2];
            for (i, s) in frame.iter_mut().enumerate() {
                *s = 0.25 * (i as f32 * tone).sin();
            }
            let mut out = vec![0u8; 1500];
            let n = enc.encode_float(&frame, &mut out).unwrap();
            out.truncate(n);
            out
        };

        let mut state = AudioPcmState::default();
        // Nothing has decoded: PLC has no state to extrapolate from, and the ABI reports NoFrame.
        assert_eq!(state.conceal(OPUS_48K), Ok(0));

        assert_eq!(
            state.decode_packet(&packet(0.05), 0, OPUS_48K),
            Ok(FRAME * 2)
        );
        // The wire goes quiet; the embedder covers four frames of it.
        for _ in 0..4 {
            assert_eq!(state.conceal(OPUS_48K), Ok(FRAME * 2));
        }
        // It comes back at seq 7 — six packets missing, four of them already in the ring.
        assert_eq!(
            state.decode_packet(&packet(0.06), 7, OPUS_48K),
            Ok(3 * FRAME * 2)
        );
        // The next drought starts from nothing owed, not from a stale credit.
        assert_eq!(
            state.decode_packet(&packet(0.06), 9, OPUS_48K),
            Ok(2 * FRAME * 2)
        );
    }
}
