//! The remote encoder's proxy: the AU section the far side's encode thread publishes into,
//! delivered with `SET_ENCODE`, and the [`Encoder`] the stream loop drives — every control
//! call forwarded as `ENCODE_CTL`. The driver's WUDFHost and a capture worker take the same
//! requests, each over its own [`Transport`].
//!
//! The section is sealed and planted through [`Transport::handles`], adopt-on-success. The
//! reader over it is [`super::au_reader`]; the chunks and access units it assembles are
//! [`super::au_assembly`].

use std::mem::offset_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use pf_driver_proto::encode::{self, au, EncodeCtlRequest, SetEncodeRequest};
use pf_driver_proto::section::SectionView;
use pf_encode_core::{AuChunk, EncodedFrame, Encoder, EncoderCaps};
use pf_frame::CapturedFrame;

use super::au_assembly::{AuAssembly, AuDump, Clocks};
use super::au_reader::{AuReader, AuView};
use super::transport::{AuMemory, Planted, Transport};
use crate::{DriverEncodeParams, EncodeCtlSender};

/// `SET_ENCODE` answered with no encoder open: the driver's failure domain, the backend's raw
/// code, its stage tag, and the list it walked. No fallback follows; the session ends here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverEncodeOpenError {
    pub backends: [u32; 4],
    pub status: u32,
    pub error: i32,
    pub name: String,
}

impl std::fmt::Display for DriverEncodeOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tried: Vec<&str> = self
            .backends
            .iter()
            .take_while(|&&b| b != 0)
            .map(|&b| backend_name(b))
            .collect();
        write!(
            f,
            "driver encoder open failed: none of {tried:?} opened — status {}, error {:#010x}, \
             at '{}'",
            self.status, self.error as u32, self.name
        )
    }
}

impl std::error::Error for DriverEncodeOpenError {}

impl DriverEncodeOpenError {
    /// Windows assigned the monitor no swap-chain within the driver's wait: it committed the
    /// path inactive, as it does while the console display is off. A driver reload changes nothing.
    pub fn is_no_swap_chain(&self) -> bool {
        self.status == encode::SET_ENCODE_NO_DEVICE && self.name == "noswap"
    }
}

fn backend_name(b: u32) -> &'static str {
    pf_driver_proto::encode::backend::name(b).unwrap_or("?")
}

/// A NUL-padded tag as text: the far side's failure stage names.
pub(crate) fn nul_tag(name: &[u8]) -> String {
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    String::from_utf8_lossy(&name[..end]).into_owned()
}

/// `pf_frame::HdrMeta` as the 28 bytes `SET_ENCODE` and `SET_HDR_META` carry: its `repr(C)`
/// image, all zero for `None`.
fn hdr_meta_bytes(meta: Option<pf_frame::HdrMeta>) -> [u8; 28] {
    meta.map_or([0; 28], bytemuck::cast)
}

/// `frames` is the header's `frames_input`: only a driver that stamps it scaled what it was
/// asked to. A v9 driver and a capture worker encode the surface at its own size.
/// Confirmations as a ctl's `(arg0, arg1)`: the newest wire index, then the sixteen before it
/// shifted over a present bit. `None` is `(0, 0)`.
fn acked_args(acked: Option<pf_encode_core::Acked>) -> (u32, u32) {
    acked.map_or((0, 0), |a| (a.last as u32, u32::from(a.mask) << 1 | 1))
}

fn caps_from_wire(w: &encode::EncoderCapsWire, frames: bool) -> EncoderCaps {
    EncoderCaps {
        supports_rfi: w.supports_rfi != 0,
        chroma_444: w.chroma_444 != 0,
        intra_refresh: w.intra_refresh != 0,
        intra_refresh_recovery: w.intra_refresh_recovery != 0,
        intra_refresh_period: w.intra_refresh_period,
        blends_cursor: w.blends_cursor != 0,
        downscales_input: frames,
        crops_input: frames,
    }
}

/// Stamp a fresh section's header, magic last with Release, so the far side sees it whole.
///
/// `generation` is the far side's word: its encode session stores its own over whatever is
/// here, and the reader checks that. The host does not seed it.
pub(crate) fn stamp_header(view: SectionView<'_>, heap_bytes: u32, wire_seq_base: u32) {
    let stamp = |field: usize, value: u32| view.atomic_u32(field).store(value, Ordering::Relaxed);
    stamp(offset_of!(au::AuHeader, version), au::AU_VERSION);
    stamp(
        offset_of!(au::AuHeader, heap_offset),
        au::HEAP_OFFSET as u32,
    );
    stamp(offset_of!(au::AuHeader, heap_bytes), heap_bytes);
    stamp(
        offset_of!(au::AuHeader, slot_table_offset),
        au::SLOT_TABLE_OFFSET as u32,
    );
    stamp(offset_of!(au::AuHeader, slot_count), au::AU_SLOTS);
    stamp(offset_of!(au::AuHeader, wire_seq_base), wire_seq_base);
    view.atomic_u32(offset_of!(au::AuHeader, magic))
        .store(au::AU_MAGIC, Ordering::Release);
}

/// The access-unit dump while `PUNKTFUNK_IDD_DIAG` is on: one file per open.
#[cfg(target_os = "windows")]
fn au_dump(target_id: u32) -> Option<AuDump> {
    crate::knobs::Knobs::process()
        .idd_diag
        .as_deref()
        .and_then(|dir| AuDump::create(dir, target_id))
}

#[cfg(not(target_os = "windows"))]
fn au_dump(_target_id: u32) -> Option<AuDump> {
    None
}

/// NVIDIA's clocks, held up while the proxy lives. Opt-in: without it the card sits in its
/// idle clocks under a stream.
#[cfg(target_os = "windows")]
fn clock_hold(backend_opened: u32) -> Option<super::clock_boost::ClockBoost> {
    (backend_opened == encode::backend::NVENC)
        .then(pf_gpu::selected_gpu)
        .flatten()
        .filter(|_| crate::knobs::Knobs::from_env().nvenc_clock_boost)
        .and_then(|gpu| super::clock_boost::ClockBoost::hold(gpu.info.luid()))
}

/// A source's poke for the proxy it opened last: the flag that proxy's
/// [`Encoder::ready_aus`] takes, and the signal of its AU event.
#[derive(Default)]
pub(crate) struct Poke {
    flag: Arc<AtomicBool>,
    signal: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Poke {
    /// Raise the flag, then wake the proxy's wait.
    pub(crate) fn fire(&self) {
        self.flag.store(true, Ordering::Relaxed);
        let signal = self.signal.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(signal) = signal.as_ref() {
            signal();
        }
    }
}

/// Open the far side's encoder for `params` and hand back the stream loop's [`Encoder`]: plant
/// a section in its process, stamp the header, send `SET_ENCODE`. Structured failure:
/// [`DriverEncodeOpenError`] when the far side walked the list and none opened, else the
/// delivery error. There is no fallback. An open that succeeds takes `poke` over.
///
/// The framing rides the request's v10 tail, after the surface size a far side that reads no
/// framing opens at; caps say whether it framed ([`caps_from_wire`]).
pub(crate) fn open(
    t: &dyn Transport,
    params: &DriverEncodeParams,
    clocks: Clocks,
    poke: &Poke,
) -> Result<Box<dyn Encoder>> {
    let target_id = t.endpoint().target_id;
    let heap = au::heap_bytes_for(params.bitrate_kbps, params.fps);
    let Planted {
        memory,
        section: section_v,
        event: event_v,
    } = t.handles().plant(au::section_bytes(heap) as usize)?;
    let signal = memory.signal();
    // SAFETY: the reader built over this view lives in the proxy beside `memory`, declared
    // ahead of it, so it drops first; every earlier return drops `memory` after the view's
    // last use.
    let mem = unsafe { memory.share() };
    stamp_header(mem, heap, params.wire_seq_base);
    let req = SetEncodeRequest {
        target_id,
        _pad: 0,
        section: section_v,
        event: event_v,
        section_bytes: mem.len() as u32,
        codec: params.codec,
        chroma: params.chroma,
        bit_depth: params.bit_depth,
        width: params.width,
        height: params.height,
        fps: params.fps,
        bitrate_kbps: params.bitrate_kbps,
        hdr: params.hdr as u32,
        hdr_meta: hdr_meta_bytes(params.hdr_meta),
        wire_chunk_bytes: params.wire_chunk_bytes,
        wire_seq_base: params.wire_seq_base,
        backends: params.backends,
        flags: if params.sdr_fp16 {
            encode::SET_ENCODE_FLAG_SDR_FP16
        } else {
            0
        } | if params.full_range {
            0
        } else {
            encode::SET_ENCODE_FLAG_STUDIO_RANGE
        },
        slices: params.slices,
        // The host process carries `host.env`, so its environment is the knob source; the
        // driver reads these instead of WUDFHost's own (machine) environment.
        knobs: pf_encode_core::knobs::dev_override::from_env(),
        encode_width: params.encode_width,
        encode_height: params.encode_height,
        src_rect: params.src_rect,
    };
    let reply = match t.set_encode(&req) {
        Ok(r) => r,
        Err(e) => {
            // A failed IOCTL adopted nothing: the duplicates are ours to reap. One that
            // completed with a short reply did adopt them, and closing would double-close.
            if e.downcast_ref::<encode::ReplyTooShort>().is_none() {
                t.handles().close_remote(section_v);
                t.handles().close_remote(event_v);
            }
            return Err(e.context("deliver the AU section to the driver (SET_ENCODE)"));
        }
    };
    if reply.status != 0 {
        return Err(anyhow::Error::new(DriverEncodeOpenError {
            backends: params.backends,
            status: reply.status,
            error: reply.error,
            name: nul_tag(&reply.name),
        }));
    }
    let ctl = t.encode_ctl();
    let view = AuView::new(mem);
    let header = view.header();
    if !au::au_readable(&header) {
        // SET_ENCODE already succeeded, so the driver holds an open encode session — and no
        // `EncoderProxy` exists yet to close it on drop. Send the same CLOSE that `Drop` does,
        // or the session stays open on a section nobody will ever drain.
        let _ = ctl(&EncodeCtlRequest {
            target_id,
            op: encode::ENCODE_CTL_CLOSE,
            arg0: header.generation,
            arg1: 0,
            payload: [0; 28],
        });
        bail!("AU section header failed its layout gate after SET_ENCODE: {header:?}");
    }
    let caps = caps_from_wire(&reply.caps, header.frames_input != 0);
    let crop = match params.src_rect {
        [0, 0, 0, 0] => [0, 0, params.width, params.height],
        rect => rect,
    };
    tracing::info!(
        target_id = target_id,
        backend = backend_name(reply.backend_opened),
        size = ?(params.width, params.height),
        ?crop,
        encode = ?(params.encode_width, params.encode_height),
        ?caps,
        applied_kbps = reply.applied_bitrate_kbps,
        sdr_fp16 = params.sdr_fp16,
        full_range = params.full_range,
        heap_bytes = heap,
        generation = header.generation,
        wire_seq_base = params.wire_seq_base,
        "driver encode: encoder open, AU section adopted"
    );
    *poke.signal.lock().unwrap_or_else(|e| e.into_inner()) = Some(signal);
    Ok(Box::new(EncoderProxy {
        poked: poke.flag.clone(),
        au: AuAssembly::new(
            AuReader::new(view, params.wire_seq_base),
            clocks,
            au_dump(target_id),
        ),
        section: memory,
        target_id,
        ctl,
        caps,
        crop,
        applied_bps: u64::from(reply.applied_bitrate_kbps) * 1000,
        retarget_after: 0,
        hdr_meta: params.hdr_meta,
        hdr_meta_retry: None,
        reference_floor: None,
        floor_refused: false,
        confirmed: None,
        confirmed_refused: false,
        wire_chunk: params.wire_chunk_bytes as usize,
        wire_chunk_warned: false,
        opened_at: (clocks.now)(),
        backend: backend_name(reply.backend_opened),
        #[cfg(target_os = "windows")]
        _clock_boost: clock_hold(reply.backend_opened),
    }))
}

/// How long a refused HDR metadata update waits before it is sent again: the loop calls
/// every tick, and each call is a synchronous IOCTL to a driver that just said no.
const HDR_META_RETRY: Duration = Duration::from_secs(1);

/// The stream loop's [`Encoder`] for a driver-encoded session. Nothing is submitted: the
/// driver encodes what DWM composes, and [`Encoder::ready_aus`] tells the loop how many
/// access units it owes wire indexes for. Every control call is one synchronous
/// `ENCODE_CTL`; `set_hdr_meta` only on a change, since the loop repeats it every tick, and
/// after a refusal no sooner than [`HDR_META_RETRY`].
pub struct EncoderProxy {
    au: AuAssembly<'static>,
    section: Box<dyn AuMemory>,
    /// The source's [`Poke`] flag: set, it ends a [`Encoder::ready_aus`] wait with nothing owed.
    poked: Arc<AtomicBool>,
    target_id: u32,
    ctl: EncodeCtlSender,
    caps: EncoderCaps,
    /// The rectangle of each surface `SET_ENCODE` asked for.
    crop: [u32; 4],
    /// The rate this side last asked for. It stands until the driver has published past
    /// [`Self::retarget_after`], because its stamp until then describes the rate before the ask.
    applied_bps: u64,
    /// `published_total` when the last bitrate ctl was queued.
    retarget_after: u64,
    hdr_meta: Option<pf_frame::HdrMeta>,
    /// When a refused HDR metadata update may be sent again.
    hdr_meta_retry: Option<Instant>,
    /// The reference floor the driver holds; sent only when it changes.
    reference_floor: Option<pf_encode_core::Acked>,
    /// The driver refused the floor op once: it predates it, and the chain stays.
    floor_refused: bool,
    /// The confirmations the driver holds; sent only when they change.
    confirmed: Option<pf_encode_core::Acked>,
    /// The driver refused the confirmations op once: it predates it and keeps no anchor.
    confirmed_refused: bool,
    /// Decided at `SET_ENCODE`; a later `set_wire_chunking` that differs is logged once.
    wire_chunk: usize,
    wire_chunk_warned: bool,
    /// Stands in for `last_au_qpc` until the first publish, so a never-producing encoder is
    /// silent from a known instant rather than invisible. Also this proxy's identity.
    opened_at: Instant,
    /// The backend the driver opened, for the status surface.
    backend: &'static str,
    /// NVENC only: the GPU's clocks stay up until this proxy drops.
    #[cfg(target_os = "windows")]
    _clock_boost: Option<super::clock_boost::ClockBoost>,
}

impl Drop for EncoderProxy {
    /// Stop the driver's session: without this a lingering display keeps a hardware encoder
    /// busy on a section nobody drains. Scoped to this proxy's generation, so a predecessor
    /// dropped after its replacement opened never stops the live one.
    fn drop(&mut self) {
        let generation = self.snapshot().generation;
        if let Err(e) = self.ctl(encode::ENCODE_CTL_CLOSE, generation, 0, [0; 28]) {
            tracing::debug!(
                target_id = self.target_id,
                error = %format!("{e:#}"),
                "driver encode: close not taken (older driver, or the WUDFHost is gone)"
            );
        }
    }
}

impl EncoderProxy {
    fn ctl(&self, op: u32, arg0: u32, arg1: u32, payload: [u8; 28]) -> Result<()> {
        (self.ctl)(&EncodeCtlRequest {
            target_id: self.target_id,
            op,
            arg0,
            arg1,
            payload,
        })
    }

    fn ctl_logged(&self, what: &str, op: u32, arg0: u32, arg1: u32) -> bool {
        match self.ctl(op, arg0, arg1, [0; 28]) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    target_id = self.target_id,
                    error = %format!("{e:#}"),
                    "driver encode: {what} refused"
                );
                false
            }
        }
    }

    /// The header telemetry the driver keeps (`encoder_state`, `detached`, `last_au_qpc`,
    /// `drain_heartbeat_qpc`, `source_seq`, …); [`Encoder::telemetry`] is its supervisor cut.
    pub fn snapshot(&self) -> au::AuHeader {
        self.au.reader.view().header()
    }

    /// `(wire_seq, source_seq)` of the last chunk taken: AU progress against the header's
    /// `source_seq`, the "encoder wedged" signal (§2.5) the supervisor reads from Phase 4 on.
    #[allow(dead_code)]
    pub fn progress(&self) -> (u32, u32) {
        (self.au.last_wire_seq, self.au.last_source_seq)
    }
}

impl Encoder for EncoderProxy {
    fn submit(&mut self, _frame: &CapturedFrame) -> Result<()> {
        // Unreachable by the loop: `ready_aus` answers `Some`, so it owes records instead of
        // submitting. A caller that lands here is a bug worth one visible reset.
        bail!("driver encode: the driver encodes; nothing to submit")
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, _wire_index: u32) -> Result<()> {
        // The driver numbers its own access units; `reset` rebases them.
        self.submit(frame)
    }

    fn caps(&self) -> EncoderCaps {
        self.caps
    }

    fn request_keyframe(&mut self) {
        self.ctl_logged(
            "keyframe request",
            encode::ENCODE_CTL_REQUEST_KEYFRAME,
            0,
            0,
        );
    }

    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        let now = Instant::now();
        if meta == self.hdr_meta || self.hdr_meta_retry.is_some_and(|at| now < at) {
            return;
        }
        match self.ctl(encode::ENCODE_CTL_SET_HDR_META, 0, 0, hdr_meta_bytes(meta)) {
            Ok(()) => {
                self.hdr_meta = meta;
                self.hdr_meta_retry = None;
            }
            Err(e) => {
                self.hdr_meta_retry = Some(now + HDR_META_RETRY);
                tracing::warn!(
                    target_id = self.target_id,
                    error = %format!("{e:#}"),
                    "driver encode: HDR metadata update refused"
                );
            }
        }
    }

    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        self.caps.supports_rfi
            && self.ctl_logged(
                "reference invalidation",
                encode::ENCODE_CTL_INVALIDATE_REF_FRAMES,
                first_frame as u32,
                last_frame as u32,
            )
    }

    fn set_reference_floor(&mut self, acked: Option<pf_encode_core::Acked>) {
        if self.floor_refused || acked == self.reference_floor {
            return;
        }
        let (last, bits) = acked_args(acked);
        match self.ctl(encode::ENCODE_CTL_SET_REFERENCE_FLOOR, last, bits, [0; 28]) {
            Ok(()) => self.reference_floor = acked,
            Err(e) => {
                self.floor_refused = true;
                tracing::info!(
                    target_id = self.target_id,
                    error = %format!("{e:#}"),
                    "driver encode: this driver takes no reference floor — the chain stays"
                );
            }
        }
    }

    fn note_confirmed(&mut self, acked: Option<pf_encode_core::Acked>) {
        // Each send is a round trip into the driver host: one per four frames of progress.
        // The snapshot still holds the kept candidate inside its sixteen-frame window.
        let due = match (acked, self.confirmed) {
            (Some(a), Some(b)) => !(0..4).contains(&(a.last - b.last)),
            (a, b) => a.is_some() != b.is_some(),
        };
        if self.confirmed_refused || !due {
            return;
        }
        let (last, bits) = acked_args(acked);
        match self.ctl(encode::ENCODE_CTL_NOTE_CONFIRMED, last, bits, [0; 28]) {
            Ok(()) => self.confirmed = acked,
            Err(e) => {
                self.confirmed_refused = true;
                tracing::info!(
                    target_id = self.target_id,
                    error = %format!("{e:#}"),
                    "driver encode: this driver takes no confirmations — a late loss finds no kept anchor"
                );
            }
        }
    }

    fn distrust_references(&mut self) {
        self.ctl_logged(
            "reference distrust",
            encode::ENCODE_CTL_DISTRUST_REFERENCES,
            0,
            0,
        );
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.au.poll(&mut |ms| self.section.wait(ms))
    }

    fn set_pipelined(&mut self, _on: bool) -> bool {
        // The driver's encode thread retrieves: there is no host-side wait to pipeline.
        false
    }

    fn ready_event(&self) -> Option<isize> {
        // `ready_aus` waits on the section's own event.
        None
    }

    fn supports_chunked_poll(&self) -> bool {
        true
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        self.au.poll_chunk(&mut |ms| self.section.wait(ms))
    }

    /// The classifier's two clocks straight from the AU header: the drain worker's heartbeat and
    /// the encoder's publish stamp, both QPC ages turned into local instants. `opened_at` tells
    /// this proxy's counters from its predecessor's.
    fn telemetry(&self) -> Option<pf_frame::health::EncoderTelemetry> {
        let h = self.snapshot();
        let clocks = self.au.clocks;
        let stamp = |qpc: u64| {
            (qpc != 0)
                .then(|| Duration::from_micros((clocks.qpc_age_us)(qpc)))
                .and_then(|age| (clocks.now)().checked_sub(age))
        };
        Some(pf_frame::health::EncoderTelemetry {
            // Before the first access unit the encoder's open time stands in, so a
            // never-producing encoder is silent from a known instant.
            last_au: stamp(h.last_au_qpc).unwrap_or(self.opened_at),
            opened_at: self.opened_at,
            published_total: h.published_total,
            detached: h.detached,
            source_seq: h.source_seq,
            dropped_total: h.dropped_total,
            drain_heartbeat: stamp(h.drain_heartbeat_qpc),
            present_to_arrival: self.au.last_arrival,
            driver_split: self.au.last_split,
            au_repeat: self.au.au_repeat,
            state: h.encoder_state,
            backend: self.backend,
        })
    }

    /// Owed access units, waited for until `deadline`. A poke ends the wait with `Some(0)` and
    /// is taken, so the loop's next tick carries a recovery ask; one that finds AUs waiting
    /// stays for the wait after them.
    fn ready_aus(&mut self, deadline: Instant) -> Option<usize> {
        loop {
            let n = self.au.reader.ready_aus();
            if n > 0 || self.poked.swap(false, Ordering::Relaxed) {
                return Some(n);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Some(0);
            }
            self.section.wait(left.as_millis().clamp(1, 16) as u32);
        }
    }

    fn reset(&mut self) -> bool {
        let base = self.au.reset_base();
        if !self.ctl_logged("reset", encode::ENCODE_CTL_RESET, base, 0) {
            return false;
        }
        self.au.reader.rebase(base);
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let kbps = (bps / 1000).min(u64::from(u32::MAX)) as u32;
        if !self.ctl_logged(
            "bitrate reconfigure",
            encode::ENCODE_CTL_RECONFIGURE_BITRATE,
            kbps,
            0,
        ) {
            return false;
        }
        self.applied_bps = u64::from(kbps) * 1000;
        self.retarget_after = self.snapshot().published_total;
        true
    }

    fn bitrate_retarget_is_synchronous(&self) -> bool {
        false
    }

    /// The driver has published an access unit past the last ask, so its stamp is the answer.
    fn retarget_settled(&self) -> bool {
        self.snapshot().published_total > self.retarget_after
    }

    /// What the driver's backend is encoding at.
    ///
    /// The ctl is queued for the driver's encode thread and has no reply, so the ask stands
    /// until that thread has published an access unit past it — only then does the header
    /// stamp describe the new rate. `0` is a driver that predates the stamp, whose declines
    /// stay invisible as they always were.
    fn applied_bitrate_bps(&self) -> Option<u64> {
        let h = self.snapshot();
        if h.applied_bitrate_kbps == 0 || h.published_total <= self.retarget_after {
            return Some(self.applied_bps);
        }
        Some(u64::from(h.applied_bitrate_kbps) * 1000)
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        if shard_payload != self.wire_chunk && !self.wire_chunk_warned {
            self.wire_chunk_warned = true;
            tracing::warn!(
                target_id = self.target_id,
                opened = self.wire_chunk,
                asked = shard_payload,
                "driver encode: wire chunking is fixed at SET_ENCODE; a change needs a reopen"
            );
        }
    }

    fn set_send_spread_us(&mut self, _us: u32) {
        // Only the Linux split arbiter prices a send.
    }

    fn set_input_ring_depth(&mut self, _depth: usize) {
        // The driver owns its input ring.
    }

    /// The far side framed at `SET_ENCODE`, so this only checks that `rect` is what the open
    /// sent. Any other, or a far side that frames nothing, is refused.
    fn set_input_crop(&mut self, rect: [u32; 4]) -> Result<()> {
        if !self.caps.crops_input || rect != self.crop {
            bail!(
                "driver encode: frame {rect:?}: the encoder frames only {:?}, fixed at open",
                self.crop
            );
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.ctl(encode::ENCODE_CTL_FLUSH, 0, 0, [0; 28])
            .context("driver encode: flush")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::remote::fake::{self, Refusal};
    use crate::EncoderSite;

    /// The planted values are the far side's once `SET_ENCODE` completes: a failed IOCTL
    /// reaps both, a short reply (adopted) and a status refusal (adopted, then refused) none.
    #[test]
    fn a_planted_section_is_reaped_only_when_nothing_adopted_it() {
        let no_swap = Refusal::Status(encode::SET_ENCODE_NO_DEVICE);
        for (refusal, reaped) in [
            (Refusal::Ioctl, true),
            (Refusal::ShortReply, false),
            (no_swap, false),
        ] {
            let (t, far) = fake::transport(EncoderSite::Driver);
            far.lock().unwrap().refuse = Some(refusal);
            let params = fake::params(1920, 1080, false, 0);
            let err = open(&t, &params, fake::clock::CLOCKS, &Poke::default())
                .err()
                .expect("a refused open");
            let far = far.lock().unwrap();
            let req = far.set_encodes[0];
            let want = if reaped {
                vec![req.section, req.event]
            } else {
                vec![]
            };
            assert_eq!(far.closed, want, "{refusal:?}");
            if let Refusal::Status(_) = refusal {
                let e = err.downcast_ref::<DriverEncodeOpenError>().unwrap();
                assert!(e.is_no_swap_chain(), "{e}");
            }
        }
    }

    /// A framed open keeps the surface's size where a v9 driver reads it and sends the framing
    /// in the v10 tail. Only a far side that stamps `frames_input` reports that it frames, and
    /// it takes no crop but the one the open sent.
    #[test]
    fn only_a_framing_driver_reports_that_it_frames() {
        let crop = [0, 120, 3840, 1920];
        for frames in [false, true] {
            let (t, far) = fake::transport(EncoderSite::Driver);
            far.lock().unwrap().frames_input = frames;
            let params = DriverEncodeParams {
                encode_width: 1920,
                encode_height: 960,
                src_rect: crop,
                ..fake::params(3840, 2160, false, 0)
            };
            let mut enc =
                open(&t, &params, fake::clock::CLOCKS, &Poke::default()).expect("an encoder");
            let req = far.lock().unwrap().set_encodes[0];
            assert_eq!((req.width, req.height), (3840, 2160));
            assert_eq!((req.encode_width, req.encode_height), (1920, 960));
            assert_eq!(req.src_rect, crop);
            let caps = enc.caps();
            assert_eq!((caps.downscales_input, caps.crops_input), (frames, frames));
            assert_eq!(enc.set_input_crop(crop).is_ok(), frames);
            assert!(enc.set_input_crop([0, 0, 3840, 2160]).is_err());
        }
        // Unframed, the tail is zero: the request a v9 host sent.
        let (t, far) = fake::transport(EncoderSite::Driver);
        let params = fake::params(1920, 1080, false, 0);
        let _enc = open(&t, &params, fake::clock::CLOCKS, &Poke::default()).expect("an encoder");
        let req = far.lock().unwrap().set_encodes[0];
        assert_eq!((req.width, req.height), (1920, 1080));
        assert_eq!(
            (req.encode_width, req.encode_height, req.src_rect),
            (0, 0, [0; 4])
        );
    }

    /// Every `Encoder` method is written here: this is a proxy, so an inherited default
    /// would mean the driver never hears the call.
    #[test]
    fn the_proxy_writes_every_trait_method() {
        pf_encode_core::smoke_pattern::assert_writes_every_encoder_method(
            include_str!("encoder.rs"),
            "impl Encoder for EncoderProxy {",
        );
    }

    /// A poke ends the loop's access-unit wait at once with nothing owed, and is taken; with
    /// none the wait runs to its deadline. One that finds an AU waiting stays for the next wait.
    #[test]
    fn a_poke_ends_the_access_unit_wait() {
        let (t, far) = fake::transport(EncoderSite::Driver);
        let poke = Poke::default();
        let params = fake::params(1920, 1080, false, 0);
        let mut enc = open(&t, &params, fake::clock::CLOCKS, &poke).expect("an encoder");
        let quiet = Duration::from_millis(50);
        let t0 = Instant::now();
        assert_eq!(enc.ready_aus(t0 + quiet), Some(0));
        let unpoked = t0.elapsed();
        assert!(unpoked >= quiet, "{unpoked:?}");
        let t0 = Instant::now();
        let (woke, poked) = std::thread::scope(|s| {
            let fired = s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                let at = Instant::now();
                poke.fire();
                at
            });
            let woke = enc.ready_aus(t0 + Duration::from_secs(2));
            let back = Instant::now();
            (woke, back - fired.join().unwrap())
        });
        assert_eq!(woke, Some(0));
        assert!(poked < Duration::from_millis(500), "{poked:?}");
        let t0 = Instant::now();
        assert_eq!(enc.ready_aus(t0 + quiet), Some(0));
        assert!(t0.elapsed() >= quiet, "the poke was taken");
        far.lock().unwrap().frame(true);
        poke.fire();
        assert_eq!(enc.ready_aus(Instant::now()), Some(1), "the AU first");
        assert!(enc.poll_chunk().unwrap().is_some());
        let t0 = Instant::now();
        assert_eq!(enc.ready_aus(t0 + Duration::from_secs(2)), Some(0));
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "the poke ended the next wait"
        );
        println!("ready_aus: {unpoked:?} unpoked, to its deadline; {poked:?} from a poke");
    }
}
