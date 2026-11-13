//! Opening a session's encoder: the checks every backend shares, the per-OS ladder in
//! `resolve`, which opens the backend a [`Resolved`] names, and the live-session record every
//! opened encoder carries.

use crate::env_knobs::Knobs;
use crate::{
    validate_dimensions, Acked, AuChunk, ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps,
    Resolved,
};
use anyhow::Result;
use pf_frame::{CapturedFrame, PixelFormat};

/// Open the encoder `resolved` names, for `params`. `params.cuda` is GPU frames from the NVIDIA
/// zero-copy path, derived by the caller from the first captured frame; otherwise packed RGB/BGR
/// CPU frames. The crate's env knobs are read here, once per open.
pub fn open_video(resolved: &Resolved, params: OpenParams) -> Result<Box<dyn Encoder>> {
    let knobs = Knobs::from_env();
    let (inner, backend) = open_video_backend(
        resolved,
        OpenParams {
            bitrate_bps: params.bitrate_bps.max(MIN_BITRATE_BPS),
            ..params
        },
        &knobs,
    )?;
    // Open-time fallback (Vulkan→VAAPI) and gamescope (no embedded cursor) still
    // reach here. `open_video` cannot re-plan capture, so a warning is all it does.
    if params.cursor_blend && !inner.caps().blends_cursor {
        tracing::warn!(
            backend,
            "session negotiated a composited cursor but this encode backend does not blend \
             CapturedFrame::cursor — the pointer will be MISSING from the stream unless the \
             capturer composites it"
        );
    }
    Ok(track_session(inner, backend))
}

/// Tie `inner` to a `pf_gpu` live-session record labelled `backend` — the label of the
/// branch that opened, not re-derived (Vulkan Video falls back to VAAPI, and a dispatch mirror
/// would report the wrong one). GPU identity is [`pf_gpu::selected_gpu`]; dropping the returned
/// encoder ends the record. Public for encoders opened outside [`open_video`] (the Windows
/// driver proxy).
pub fn track_session(inner: Box<dyn Encoder>, backend: &'static str) -> Box<dyn Encoder> {
    let gpu = if backend == "software" {
        pf_gpu::ActiveGpu {
            id: String::new(),
            name: "CPU (openh264)".into(),
            vendor_id: 0,
            backend,
        }
    } else {
        match pf_gpu::selected_gpu() {
            Some(sel) => pf_gpu::ActiveGpu {
                id: sel.info.id,
                name: sel.info.name,
                vendor_id: sel.info.vendor_id,
                backend,
            },
            None => pf_gpu::ActiveGpu {
                id: String::new(),
                name: "GPU".into(),
                vendor_id: 0,
                backend,
            },
        }
    };
    Box::new(TrackedEncoder {
        inner,
        _session: pf_gpu::session_begin(gpu),
    })
}

/// Ties the `pf_gpu` live-session record to the encoder's lifetime; pure delegation
/// otherwise.
struct TrackedEncoder {
    inner: Box<dyn Encoder>,
    _session: pf_gpu::ActiveSession,
}

impl Encoder for TrackedEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        self.inner.submit(frame)
    }
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.inner.submit_indexed(frame, wire_index)
    }
    fn caps(&self) -> EncoderCaps {
        self.inner.caps()
    }
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe()
    }
    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        self.inner.set_hdr_meta(meta)
    }
    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        self.inner.invalidate_ref_frames(first_frame, last_frame)
    }
    fn distrust_references(&mut self) {
        self.inner.distrust_references()
    }
    fn set_reference_floor(&mut self, acked: Option<Acked>) {
        self.inner.set_reference_floor(acked)
    }
    fn note_confirmed(&mut self, acked: Option<Acked>) {
        self.inner.note_confirmed(acked)
    }
    fn set_pipelined(&mut self, on: bool) -> bool {
        self.inner.set_pipelined(on)
    }
    fn set_au_wake(&mut self, wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        self.inner.set_au_wake(wake)
    }
    fn set_wire_chunking(&mut self, shard_payload: usize) {
        self.inner.set_wire_chunking(shard_payload)
    }
    fn set_send_spread_us(&mut self, us: u32) {
        self.inner.set_send_spread_us(us)
    }
    fn set_input_ring_depth(&mut self, depth: usize) {
        self.inner.set_input_ring_depth(depth)
    }
    fn set_input_crop(&mut self, rect: [u32; 4]) -> Result<()> {
        self.inner.set_input_crop(rect)
    }
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.inner.poll()
    }
    fn ready_event(&self) -> Option<isize> {
        self.inner.ready_event()
    }
    fn supports_chunked_poll(&self) -> bool {
        self.inner.supports_chunked_poll()
    }
    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        self.inner.poll_chunk()
    }
    fn ready_aus(&mut self, deadline: std::time::Instant) -> Option<usize> {
        self.inner.ready_aus(deadline)
    }
    fn telemetry(&self) -> Option<pf_frame::health::EncoderTelemetry> {
        self.inner.telemetry()
    }
    fn reset(&mut self) -> bool {
        self.inner.reset()
    }
    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        self.inner.reconfigure_bitrate(bps.max(MIN_BITRATE_BPS))
    }
    fn bitrate_retarget_is_synchronous(&self) -> bool {
        self.inner.bitrate_retarget_is_synchronous()
    }
    fn retarget_settled(&self) -> bool {
        self.inner.retarget_settled()
    }
    fn applied_bitrate_bps(&self) -> Option<u64> {
        self.inner.applied_bitrate_bps()
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

/// No backend clamps a bitrate of 0: an AMF rebuild fails its `TargetBitrate`
/// and NVENC's bisect lands at 10 Mbps. One floor here, at the trait boundary,
/// matching the host's own 500 kbps ABR floor.
pub const MIN_BITRATE_BPS: u64 = 500_000;

/// What a session asks [`open_video`] for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OpenParams {
    pub codec: Codec,
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub cuda: bool,
    pub bit_depth: u8,
    /// The session's colour: BT.2020 PQ, else BT.709 at either depth. Never read off the
    /// format — gamescope hands 10-bit SDR over as P010 or packed RGB too.
    pub hdr: bool,
    pub chroma: ChromaFormat,
    /// Backends whose fast path can't blend (Vulkan EFC) key off it.
    pub cursor_blend: bool,
    /// H.264/HEVC slices per frame the client takes. 1 = single-slice (some TVs wedge on
    /// multi-slice AUs). Only NVENC slices; `PUNKTFUNK_NVENC_SLICES` overrides.
    pub slices: u32,
    /// The range the session advertised: every conversion this encoder runs writes it and the
    /// bitstream signals it. `false` is studio.
    pub full_range: bool,
}

/// Open the platform encoder. The display label is the branch that opened
/// (`nvenc`/`vaapi`/`vulkan`/`amf`/`qsv`/`software`), including internal
/// fallbacks (Vulkan Video → VAAPI). Feeds the mgmt live-session record.
fn open_video_backend(
    resolved: &Resolved,
    p: OpenParams,
    knobs: &Knobs,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    let OpenParams {
        codec,
        width,
        height,
        fps,
        chroma,
        ..
    } = p;
    validate_dimensions(codec, width, height)?;
    // `fps` is `Rational(1, fps)` and `pts * 1e9 / fps`. 0 is a 1/0 rational
    // and a divide-by-zero; 1000 is a sanity ceiling.
    if fps == 0 || fps > 1000 {
        anyhow::bail!("invalid refresh/fps {fps}: must be 1..=1000 Hz");
    }
    // 4:4:4 is HEVC- and PyroWave-only. Degrade rather than emit a stream no
    // decoder expects.
    let chroma = if chroma.is_444() && codec != Codec::H265 && codec != Codec::PyroWave {
        tracing::warn!(
            ?codec,
            "4:4:4 requested for a non-HEVC codec — encoding 4:2:0"
        );
        ChromaFormat::Yuv420
    } else {
        chroma
    };
    crate::resolve::open(resolved, &OpenParams { chroma, ..p }, knobs)
}

#[cfg(test)]
mod tests {
    /// Every `Encoder` method must be forwarded by `TrackedEncoder`: the host loop only ever
    /// holds the wrapped box.
    #[test]
    fn tracked_encoder_forwards_every_trait_method() {
        crate::smoke_pattern::assert_writes_every_encoder_method(
            include_str!("open.rs"),
            "impl Encoder for TrackedEncoder {",
        );
    }
}
