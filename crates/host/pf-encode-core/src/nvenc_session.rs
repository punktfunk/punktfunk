//! One direct-NVENC session, shared by the Windows D3D11 and Linux CUDA backends: the caps
//! probe, the bitrate-ceiling open ladder, per-frame submit (pic params, forced IDR, recovery
//! anchor, intra refresh wave), RFI, the split arbiter, in-place reconfigure, and the three
//! retrieve paths (blocking, chunked, two-thread).
//!
//! Each backend keeps what its OS owns: the library load, the device it opens on, how an
//! input surface is filled and registered, the retrieve thread's wait, and teardown of its own
//! registrations. Built on [`super::nvenc_core`].
//!
//! Invariant: `encoder` is live whenever `pending` is non-empty or `inited` is set, and every
//! `pending` entry names one of `bitstreams` with an encode submitted. Only this module writes
//! the fields that carry it.

use super::nvenc_core::{
    apply_low_latency_config, build_init_params, cached_ceiling, cached_split_verdict, encode_cap,
    force_frame_mode, open_split_mode, plan_range_recovery, prefix_diverged, preset_guid,
    resolve_slices, resolve_split_subframe, resolve_subframe, seed_config, seed_pic_params,
    seed_preset_config, session_wave_cycle, slice_offsets_len, slice_ready, store_ceiling,
    store_split_verdict, subframe_env_forced, ArbAction, BitstreamLock, CeilingKey, EncodeApi,
    LowLatencyConfig, NvStatusExt, RangePlan, SplitArbiter, SplitKey,
};
use super::nvenc_status;
use super::{max_forced_split_mode, resolve_split_mode, AuChunk, Codec, EncodedFrame};
use crate::rfi::{
    plan_frame, FramePlan, FrameRef, MidWave, Pending, Recovery, RecoveryPolicy, Wave, WaveMark,
};
use anyhow::{anyhow, bail, Context, Result};
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Bitstream pool per session. Stays above [`async_inflight_cap`] so a bitstream is never
/// reused mid-encode, and at or above `PUNKTFUNK_ENCODE_DEPTH` (≤ 6) so GPU waits overlap.
pub const POOL: usize = 8;

const DISABLE: u32 = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;

/// `PUNKTFUNK_NVENC_ASYNC=1`: two-thread retrieve from session open.
pub fn async_retrieve_requested() -> bool {
    crate::knobs::get().nvenc_async == 1
}

/// `PUNKTFUNK_NVENC_ASYNC=0`: never escalate to pipelined retrieve (Linux `set_pipelined`).
pub fn async_retrieve_vetoed() -> bool {
    crate::knobs::get().nvenc_async == 2
}

/// Two-thread in-flight cap (`PUNKTFUNK_NVENC_ASYNC_DEPTH`, default 4, clamped `2..=POOL-1`).
/// Read once per process: it is the submit backpressure condition.
pub fn async_inflight_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        match crate::knobs::get().nvenc_async_depth {
            0 => 4,
            n => usize::from(n),
        }
        .clamp(2, POOL - 1)
    })
}

/// The input carries full chroma: packed RGB, which NVENC CSCs itself, or planar YUV444.
/// FREXT `chromaFormatIDC = 3` engages only on one of these.
pub const fn full_chroma_input(fmt: nv::NV_ENC_BUFFER_FORMAT) -> bool {
    use nv::NV_ENC_BUFFER_FORMAT as F;
    matches!(
        fmt,
        F::NV_ENC_BUFFER_FORMAT_ARGB
            | F::NV_ENC_BUFFER_FORMAT_ARGB10
            | F::NV_ENC_BUFFER_FORMAT_ABGR10
            | F::NV_ENC_BUFFER_FORMAT_YUV444
    )
}

/// 10-bit input samples: AV1 `inputPixelBitDepthMinus8` follows the surface.
const fn ten_bit_input(fmt: nv::NV_ENC_BUFFER_FORMAT) -> bool {
    use nv::NV_ENC_BUFFER_FORMAT as F;
    matches!(
        fmt,
        F::NV_ENC_BUFFER_FORMAT_ARGB10
            | F::NV_ENC_BUFFER_FORMAT_ABGR10
            | F::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
    )
}

/// The device a session opens on: `deviceType` and `device` of
/// `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS` (an `ID3D11Device` or a `CUcontext`).
#[derive(Clone, Copy)]
pub struct OpenTarget {
    pub device_type: nv::NV_ENC_DEVICE_TYPE,
    pub device: *mut c_void,
}

/// `nvEncOpenEncodeSessionEx` on `target`. A failed open may still hold a session slot, so any
/// handle it returned is destroyed before the status comes back.
///
/// # Safety
/// `target.device` is a live device of `target.device_type` for the call.
pub unsafe fn open_session(
    api: &EncodeApi,
    target: OpenTarget,
) -> std::result::Result<*mut c_void, nv::NVENCSTATUS> {
    let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
        version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        deviceType: target.device_type,
        device: target.device,
        apiVersion: nv::NVENCAPI_VERSION,
        ..Default::default()
    };
    let mut enc: *mut c_void = std::ptr::null_mut();
    // SAFETY: per the contract; `params` (version set) and `enc` are live locals.
    if let Err(e) = unsafe { (api.open_encode_session_ex)(&mut params, &mut enc) }.nv_ok() {
        if !enc.is_null() {
            // SAFETY: the handle the failed open returned; nothing else holds it.
            let _ = unsafe { (api.destroy_encoder)(enc) };
        }
        return Err(e);
    }
    // Kernel-mode handshake succeeded: a later `NV_ENC_ERR_INVALID_VERSION` is not driver skew.
    nvenc_status::note_session_opened();
    Ok(enc)
}

/// The blocking lock a retrieve thread runs: wait for the encode, copy the AU, unlock.
///
/// # Safety
/// `enc` is a live session and `bs` one of its bitstreams with an encode submitted; both
/// outlive the call.
pub unsafe fn lock_copy(
    api: &EncodeApi,
    enc: usize,
    bs: usize,
) -> std::result::Result<(Vec<u8>, bool), String> {
    // SAFETY: per the contract; the guard copies the bytes, then unlocks on drop.
    match unsafe { BitstreamLock::new(api, enc as *mut c_void, bs as *mut c_void, false, None) } {
        Ok(lock) => Ok((lock.bytes().to_vec(), lock.keyframe())),
        Err(e) => Err(format!(
            "lock_bitstream (retrieve thread): {e:?} — {}",
            nvenc_status::explain(e)
        )),
    }
}

/// One in-flight encode for the retrieve thread. Handles travel as `usize` (process-global
/// driver handles); the thread is joined before the session is destroyed. `event` is the
/// Windows completion event, `0` on Linux.
pub struct RetrieveJob {
    pub bs: usize,
    pub event: usize,
}

/// A finished retrieve (AU or error). `bs` lets the encode thread check FIFO pairing.
pub struct RetrieveDone {
    pub bs: usize,
    pub result: std::result::Result<(Vec<u8>, bool), String>,
}

/// A backend's retrieve loop: take jobs until the channel closes, send each result back.
pub type RetrieveLoop = fn(usize, mpsc::Receiver<RetrieveJob>, mpsc::Sender<RetrieveDone>);

/// Two-thread retrieve: job/done channels, the thread, and AUs backpressure absorbed that
/// `poll` hands out first.
struct AsyncRetrieve {
    work_tx: Option<mpsc::SyncSender<RetrieveJob>>,
    done_rx: mpsc::Receiver<RetrieveDone>,
    join: Option<std::thread::JoinHandle<()>>,
    ready: VecDeque<EncodedFrame>,
}

impl AsyncRetrieve {
    /// Close the job channel so the thread finishes queued jobs against the live session, join
    /// it, and drop completions nobody absorbed. With a `limit`, first wait that long for the
    /// thread to leave its loop: it owns the done sender, so `Disconnected` is that moment.
    /// `false` = still inside the driver; dropping the handle detaches the thread.
    fn stop(mut self, limit: Option<Duration>) -> bool {
        drop(self.work_tx.take());
        if let Some(limit) = limit {
            let deadline = Instant::now() + limit;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.done_rx.recv_timeout(left) {
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => return false,
                }
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        while self.done_rx.try_recv().is_ok() {}
        true
    }
}

/// Keeps a raw capture source alive until its AU is retrieved (`pf_frame::FrameHold`).
pub type SourceHold = std::sync::Arc<dyn std::any::Any + Send + Sync>;

/// One submitted picture.
pub struct PendingEncode {
    pub bs: nv::NV_ENC_OUTPUT_PTR,
    map: nv::NV_ENC_INPUT_PTR,
    pts_ns: u64,
    /// First frame after a successful RFI: the client lifts its freeze here.
    anchor: bool,
    /// IDR predicted at submit, for chunks that ship before `pictureType` is known.
    idr_hint: bool,
    mark: WaveMark,
    _hold: Option<SourceHold>,
}

/// doNotWait sample cadence. Slice completions land ~0.2–1 ms apart; 50 µs stays under one
/// slice time without hammering the driver.
const CHUNK_SAMPLE_INTERVAL: Duration = Duration::from_micros(50);

/// Chunked readback of the front in-flight AU. `Some` from the first emitted chunk until
/// `last`; [`NvSession::poll`] refuses while it exists (a whole-AU poll would re-emit the
/// shipped prefix).
pub struct ChunkState {
    emitted: usize,
    slices_out: u32,
    opened: bool,
    /// Every emitted byte, compared to the finishing lock's AU. A doNotWait
    /// `bitstreamSizeInBytes` can run ahead of flushed slice bytes and ship unwritten buffer;
    /// the wire stays self-consistent, so only this compare sees it.
    shadow: Vec<u8>,
}

impl ChunkState {
    fn new() -> Self {
        ChunkState {
            emitted: 0,
            slices_out: 0,
            opened: false,
            shadow: Vec::new(),
        }
    }
}

/// Caps the backends act on beyond what [`NvSession::query_caps`] stores.
pub struct NvCaps {
    pub ten_bit: bool,
    pub async_encode: bool,
}

/// One filled input for [`NvSession::encode`].
pub struct EncodeInput {
    /// The backend's registration of the surface holding this frame.
    pub reg: nv::NV_ENC_REGISTERED_PTR,
    /// `inputPitch`; 0 takes the registration's.
    pub pitch: u32,
    /// The slot's completion event on an async session, else null.
    pub event: *mut c_void,
    pub pts_ns: u64,
    pub hold: Option<SourceHold>,
}

/// Where [`NvSession::encode`] spent its time, for the `PUNKTFUNK_PERF` submit split.
pub struct EncodeTimes {
    pub map: Duration,
    pub pic: Duration,
}

/// What [`open_at_ceiling`] opened.
pub struct Opened<H> {
    pub handle: H,
    pub bps: u64,
    /// The split mode that opened; reconfigure and the ceiling key re-present it.
    pub split_mode: u32,
    /// A bitrate ceiling the search proved, to cache under `split_mode`.
    pub ceiling: Option<u64>,
}

/// NVENC rejects `initialize_encoder` above the codec level's bitrate. Open at `requested` (or
/// the `cached` ceiling below it), then isolate a split rejection by retrying split-disabled,
/// then bisect `[FLOOR, target]` to within 20 Mbps, then try the floor without split.
///
/// Only a param/caps rejection means "above the ceiling"; any other error ends the search, so
/// a transient failure never caches a bogus ceiling. A floor that opened only after dropping
/// split proves nothing about the bitrate, so it caches none.
pub fn open_at_ceiling<H>(
    requested: u64,
    split_mode: u32,
    cached: Option<u64>,
    mut open: impl FnMut(u64, u32) -> Result<H>,
    mut destroy: impl FnMut(H),
) -> Result<Opened<H>> {
    const FLOOR_BPS: u64 = 10_000_000;
    const CLAMP_TOL_BPS: u64 = 20_000_000;
    let mut target = requested;
    if let Some(ceiling) = cached.filter(|&c| requested > c) {
        tracing::info!(
            requested_mbps = requested / 1_000_000,
            ceiling_mbps = ceiling / 1_000_000,
            "NVENC: requested bitrate above the cached codec-level ceiling — opening at the \
             ceiling"
        );
        target = ceiling;
    }
    let mut probe = open(target, split_mode);
    // The cache is advisory: a stale entry must not wedge the open.
    if probe.is_err() && target < requested {
        target = requested;
        probe = open(requested, split_mode);
    }
    // AV1 can reject AUTO split as INVALID_PARAM, which reads like a bitrate cap and would
    // fail even at the floor.
    let mut used_split = split_mode;
    if probe.is_err() && split_mode != DISABLE {
        if let Ok(h) = open(target, DISABLE) {
            tracing::warn!("NVENC: split-encode rejected by codec/config — disabled");
            used_split = DISABLE;
            probe = Ok(h);
        }
    }
    match probe {
        Ok(handle) => {
            return Ok(Opened {
                handle,
                bps: target,
                split_mode: used_split,
                ceiling: None,
            })
        }
        Err(e) if !nvenc_status::is_param_rejection(&e) => return Err(e),
        Err(_) => {}
    }
    // `lo` is the highest known-good rate (the floor assumed), `hi` the lowest rejected.
    let (mut lo, mut hi) = (FLOOR_BPS, target);
    let mut best: Option<(H, u64)> = None;
    while hi > lo + CLAMP_TOL_BPS {
        let mid = lo + (hi - lo) / 2;
        match open(mid, used_split) {
            Ok(h) => {
                if let Some((old, _)) = best.replace((h, mid)) {
                    destroy(old);
                }
                lo = mid;
            }
            Err(e) if nvenc_status::is_param_rejection(&e) => hi = mid,
            Err(e) => {
                if let Some((old, _)) = best.take() {
                    destroy(old);
                }
                return Err(e);
            }
        }
    }
    let mut proven = true;
    let (handle, bps) = match best {
        Some(b) => b,
        None => match open(FLOOR_BPS, used_split) {
            Ok(h) => (h, FLOOR_BPS),
            Err(_) => {
                let h = open(FLOOR_BPS, DISABLE)
                    .context("NVENC initialize_encoder rejected even at the floor bitrate")?;
                used_split = DISABLE;
                proven = false;
                (h, FLOOR_BPS)
            }
        },
    };
    tracing::warn!(
        requested_mbps = requested / 1_000_000,
        clamped_mbps = bps / 1_000_000,
        "NVENC: requested bitrate above the GPU codec-level ceiling — clamped to the max accepted"
    );
    Ok(Opened {
        handle,
        bps,
        split_mode: used_split,
        ceiling: proven.then_some(bps),
    })
}

/// What one open session owns and a fresh open derives again. [`NvSession::close`] replaces it
/// whole, so a field added here cannot outlive its session. [`NvSession`] derefs to it.
pub struct Live {
    encoder: *mut c_void,
    /// Submits on this session; the next one's bitstream slot is `next % POOL`.
    next: usize,
    bitstreams: Vec<nv::NV_ENC_OUTPUT_PTR>,
    pending: VecDeque<PendingEncode>,
    inited: bool,
    /// Opened with `enableEncodeAsync` (Windows completion events). Linux is always sync.
    pub session_async: bool,
    async_rt: Option<AsyncRetrieve>,
    /// Chunked poll armed (`slices ≥ 2` ∧ sub-frame ∧ sync retrieve on this thread).
    pub subframe_chunks: bool,
    pub chunk: Option<ChunkState>,
    /// Armed by a successful RFI; the next submit tags that AU as the recovery anchor. Without
    /// the tag the client lifts only on an IDR, which session glue suppresses after RFI.
    pub pending_anchor: bool,
    /// Intra refresh wave in flight: a declined RFI's answer instead of the IDR. The start
    /// frame carries `forceIntraRefreshWithFrameCnt`; the driver sweeps from there.
    pub wave: Option<Wave>,
    /// Timestamps `[start, close)` of the latest wave. Nothing before the close is an RFI
    /// anchor: every earlier picture still in the DPB is damaged, the span's own part dirty.
    pub wave_span: Option<(i64, i64)>,
    /// A loss landed inside the wave in flight. The driver ignores a re-force mid-sweep, so the
    /// sweep runs on with damage behind it: its close carries no mark, and a fresh wave starts
    /// on the frame after it (`wave_queued`).
    pub wave_spoiled: bool,
    pub wave_queued: bool,
    /// Last invalidated ref range. Dedupes the client's resends of one loss.
    pub last_rfi_range: Option<(i64, i64)>,
    /// `distrust_references` latched: a resident reference may predict from a hole the
    /// client decoded against, so RFI declines until an IDR flushes the DPB.
    pub distrusted: bool,
    /// Wire index each long-term slot holds; an IDR or an invalidation empties it.
    ltr_slots: Vec<Option<i64>>,
    ltr_next: usize,
    /// The two long-term pictures kept for a late loss ([`crate::rfi::Kept`]).
    kept: crate::rfi::Kept,
    /// The long-term slot the next frame predicts from alone: the anchor of a loss the DPB no
    /// longer reaches ([`NvSession::force_kept`]).
    ltr_force: Option<usize>,
    /// Slots marked since the last IDR, one bit each. The driver holds a marked picture until
    /// the next IDR whatever this table says, and each one takes a short-term DPB place.
    ltr_held: u32,
}

impl Default for Live {
    /// No session: a null handle and nothing in flight.
    fn default() -> Self {
        Live {
            encoder: std::ptr::null_mut(),
            next: 0,
            bitstreams: Vec::new(),
            pending: VecDeque::new(),
            inited: false,
            session_async: false,
            async_rt: None,
            subframe_chunks: false,
            chunk: None,
            pending_anchor: false,
            wave: None,
            wave_span: None,
            wave_spoiled: false,
            wave_queued: false,
            last_rfi_range: None,
            distrusted: false,
            ltr_slots: Vec::new(),
            ltr_next: 0,
            kept: crate::rfi::Kept::default(),
            ltr_force: None,
            ltr_held: 0,
        }
    }
}

/// The settings and the logic both direct-NVENC backends run, around the [`Live`] session.
/// See the module docs for the invariant its private fields carry.
pub struct NvSession {
    api: &'static EncodeApi,
    live: Live,
    pub codec: Codec,
    pub codec_guid: nv::GUID,
    /// `PUNKTFUNK_NVENC_PRESET` as P1..P7, latched so open and reconfigure present one preset.
    pub preset: u8,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// [`session_wave_cycle`] as the session opened. The recovery policy reads it every frame,
    /// and the open and every reconfigure present the same `intraRefreshCnt`.
    cycle: u32,
    /// Post-clamp target: the open ladder and reconfigure both write what the session runs at.
    pub bitrate_bps: u64,
    pub buffer_fmt: nv::NV_ENC_BUFFER_FORMAT,
    /// Encoded bit depth (8 or 10).
    pub bit_depth: u8,
    /// Effective HEVC 4:4:4 (FREXT). Cleared when the GPU or the input cannot carry it.
    pub chroma_444: bool,
    /// `NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`.
    pub yuv444_supported: bool,
    /// Effective HDR (BT.2020 PQ 10-bit).
    pub hdr: bool,
    /// Source mastering metadata, emitted in-band on each HDR keyframe. `None` = VUI only.
    pub hdr_meta: Option<pf_frame::HdrMeta>,
    /// The session's range, which the VUI states; `false` is studio.
    pub full_range: bool,
    /// GPU identity keying the ceiling and split caches: the Windows render-adapter LUID, the
    /// Linux `CUcontext`. Advisory: a collision costs one failed open and a re-search.
    pub gpu: u64,
    /// Next `inputTimeStamp`. `submit_indexed` pins it to the wire index so RFI timestamps stay
    /// 1:1 across rebuilds.
    pub frame_idx: i64,
    pub force_kf: bool,
    /// From `query_caps`. Gates RFI instead of failing later as opaque `InvalidParam`.
    pub rfi_supported: bool,
    pub custom_vbv: bool,
    /// Split mode the live session opened with; reconfigure re-presents it.
    pub split_mode: u32,
    /// `NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK`: sub-frame is default-on only when advertised.
    pub subframe_cap: bool,
    /// Live sub-frame readback. Every `build_init_params` reads it, so open and reconfigure
    /// present identical init params.
    pub subframe_on: bool,
    /// `PUNKTFUNK_NVENC_SUBFRAME=1`, latched at open. Only log severity reads it.
    pub subframe_forced: bool,
    /// Sub-frame as opened. A return to non-forced split restores it; never turns it on.
    pub subframe_opened_with: bool,
    /// The finish-lock prefix check caught sub-frame bytes the finished AU disowns. Later opens
    /// on this encoder resolve sub-frame off; a fresh encoder retests.
    pub subframe_broken: bool,
    /// Slice count latched at open so reconfigure presents the same slicing.
    pub slices: u32,
    /// H.264/HEVC slices per frame the client takes, `1..=32`. 1 = single-slice: decoders that
    /// never asked can wedge on multi-slice AUs.
    pub client_slices: u32,
    /// `NV_ENC_CAPS_NUM_ENCODER_ENGINES` (`0` = unprobed). The driver accepts a split wider
    /// than the hardware and silently encodes narrower; this is the only honest width.
    pub encoder_engines: u32,
    /// Submit stamp for the split arbiter (sync depth-1 only).
    pub last_submit_at: Option<Instant>,
    /// Whole-AU paced-send time (µs) the host reported; `0` keeps the arbiter out of the
    /// sub-frame trade it cannot price.
    pub send_spread_us: u32,
    pub arbiter: Option<SplitArbiter>,
    /// `sliceOffsets` for the doNotWait sampler, sized at open ([`slice_offsets_len`]).
    slice_offsets: Vec<u32>,
    /// Long-term slots the session opened with (`0` = none: AV1, or no GPU support).
    pub ltr_frames: u32,
    /// The newest frame the client confirmed, while the host holds confirmed references.
    pub reference_floor: Option<crate::codec::Acked>,
    /// What the client confirmed it decoded clean, whatever the link.
    pub confirmed: Option<crate::codec::Acked>,
}

impl std::ops::Deref for NvSession {
    type Target = Live;
    fn deref(&self) -> &Live {
        &self.live
    }
}

impl std::ops::DerefMut for NvSession {
    fn deref_mut(&mut self) -> &mut Live {
        &mut self.live
    }
}

impl NvSession {
    /// A closed session. The backend sets the input format, depth, chroma and GPU identity.
    pub fn new(
        api: &'static EncodeApi,
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        client_slices: u32,
    ) -> Self {
        Self {
            api,
            live: Live::default(),
            codec,
            codec_guid: super::nvenc_core::codec_guid(codec),
            preset: crate::knobs::get().nvenc_preset.max(1),
            width,
            height,
            fps,
            cycle: session_wave_cycle(codec, height, fps),
            bitrate_bps,
            buffer_fmt: nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            bit_depth: 8,
            chroma_444: false,
            yuv444_supported: false,
            hdr: false,
            hdr_meta: None,
            full_range: false,
            gpu: 0,
            frame_idx: 0,
            force_kf: false,
            rfi_supported: false,
            custom_vbv: false,
            split_mode: DISABLE,
            subframe_cap: false,
            subframe_on: false,
            subframe_forced: false,
            subframe_opened_with: false,
            subframe_broken: false,
            slices: 1,
            // A zero caller is single-slice; 32 is the knob's top.
            client_slices: client_slices.clamp(1, 32),
            encoder_engines: 0,
            last_submit_at: None,
            send_spread_us: 0,
            arbiter: None,
            slice_offsets: Vec::new(),
            ltr_frames: 0,
            reference_floor: None,
            confirmed: None,
        }
    }

    /// The live session handle; null while closed.
    pub fn handle(&self) -> *mut c_void {
        self.encoder
    }

    /// Open, pooled and ready: [`Self::finish_open`] ran and no teardown since.
    pub fn inited(&self) -> bool {
        self.inited
    }

    pub fn bitstreams(&self) -> &[nv::NV_ENC_OUTPUT_PTR] {
        &self.bitstreams
    }

    /// In-flight encodes, oldest first.
    pub fn pending(&self) -> &VecDeque<PendingEncode> {
        &self.pending
    }

    /// The two-thread retrieve is running.
    pub fn retrieving(&self) -> bool {
        self.async_rt.is_some()
    }

    /// The bitstream slot the next submit fills; the backend fills its input slot to match.
    pub fn slot(&self) -> usize {
        self.next % POOL
    }

    /// The next submit opens the session: NVENC emits an IDR regardless of pic flags.
    pub fn opening(&self) -> bool {
        self.next == 0
    }

    /// Frames a forced intra refresh wave takes on this session ([`session_wave_cycle`]),
    /// latched when it opens.
    pub fn wave_cycle(&self) -> u32 {
        self.cycle
    }

    /// How this session recovers. The driver owns the sweep: it ignores a re-force mid-sweep,
    /// so a loss there queues a fresh wave, and nothing abandons the one in flight.
    fn policy(&self) -> RecoveryPolicy {
        RecoveryPolicy {
            cycle: Some(self.wave_cycle()).filter(|&c| c != 0),
            mid_wave: MidWave::Queue,
            abandons: false,
        }
    }

    /// The wave as [`plan_frame`] reads it. `wave_spoiled` outlives a spoiled close until the
    /// next start frame, which reads it for the span, so only a started wave is spoiled.
    fn recovery(&self) -> Recovery {
        Recovery {
            wave: self.wave,
            spoiled: self.wave_spoiled && self.wave.is_some_and(|w| w.index > 0),
            queued: self.wave_queued,
        }
    }

    /// A real loss with no clean anchor: a wave heals without one ([`Recovery::lose`]). Its
    /// close still lifts unless a lost frame sits inside its sweep. Nonsense, a range past
    /// the head and a session that cannot wave stay the caller's keyframe.
    fn start_wave(&mut self, first: i64, last: i64) -> bool {
        if first < 0 || first > last || first >= self.frame_idx {
            return false;
        }
        let span_start = self.wave_span.map(|(start, _)| start);
        let spoils = self
            .wave
            .is_some_and(|w| w.spoiled_by(span_start, self.frame_idx, last));
        let Some(next) = self.recovery().lose(&self.policy(), spoils) else {
            return false;
        };
        tracing::debug!(
            first,
            last,
            mid_wave = self.wave.is_some(),
            spoiled = next.spoiled,
            "nvenc RFI: no clean anchor — intra refresh wave"
        );
        (self.wave, self.wave_queued) = (next.wave, next.queued);
        self.wave_spoiled |= next.spoiled;
        true
    }

    /// Whether the picture at `ts` is one the driver must not anchor on: anything before the
    /// latest wave's close.
    fn wave_dirty(&self, ts: i64) -> bool {
        self.wave_span.is_some_and(|(_, close)| ts < close)
    }

    /// One `NV_ENC_CAPS` value for this codec on `enc`; 0 on error ([`encode_cap`]).
    ///
    /// # Safety
    /// `enc` is a live session.
    pub unsafe fn get_cap(&self, enc: *mut c_void, which: nv::NV_ENC_CAPS) -> i32 {
        // SAFETY: per the contract.
        unsafe { encode_cap(self.api, enc, self.codec_guid, which) }
    }

    /// Probe caps on a throwaway session before the open ladder: an over-range mode fails
    /// here with a reason, not later as an opaque `InvalidParam` the ladder reads as "bitrate
    /// too high". Stores the caps every backend uses; returns the rest.
    ///
    /// # Safety
    /// `target.device` is a live device of `target.device_type` for the call.
    pub unsafe fn query_caps(&mut self, target: OpenTarget) -> Result<NvCaps> {
        // SAFETY: per the contract.
        let enc = unsafe { open_session(self.api, target) }
            .map_err(|e| nvenc_status::call_err("open_encode_session_ex (caps probe)", e))?;
        let cap = |which| {
            // SAFETY: `enc` is the live probe session opened above, destroyed right after.
            unsafe { self.get_cap(enc, which) }
        };
        use nv::NV_ENC_CAPS as C;
        let wmax = cap(C::NV_ENC_CAPS_WIDTH_MAX);
        let hmax = cap(C::NV_ENC_CAPS_HEIGHT_MAX);
        let ten_bit = cap(C::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE) != 0;
        let yuv444 = cap(C::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE);
        let rfi = cap(C::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION);
        let custom_vbv = cap(C::NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE);
        let async_encode = cap(C::NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT) != 0;
        let subframe = cap(C::NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK);
        let dyn_slice = cap(C::NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE);
        // Probe the split ceiling, don't infer it from rejection: the driver accepts a split
        // wider than the hardware and silently encodes narrower (`max_forced_split_mode`).
        let engines = cap(C::NV_ENC_CAPS_NUM_ENCODER_ENGINES);
        let max_ltr = cap(C::NV_ENC_CAPS_NUM_MAX_LTR_FRAMES);
        // SAFETY: the probe session opened above; this is its only destroy.
        let _ = unsafe { (self.api.destroy_encoder)(enc) };

        if wmax > 0 && hmax > 0 && (self.width as i32 > wmax || self.height as i32 > hmax) {
            bail!(
                "this GPU's NVENC max encode size for {:?} is {wmax}x{hmax}; client requested \
                 {}x{} (lower the client resolution or use a codec/GPU that supports it)",
                self.codec,
                self.width,
                self.height
            );
        }
        self.yuv444_supported = yuv444 != 0;
        if self.chroma_444 && !self.yuv444_supported {
            tracing::warn!("NVENC: this GPU can't 4:4:4 encode — falling back to 4:2:0");
            self.chroma_444 = false;
        }
        self.rfi_supported = rfi != 0;
        self.custom_vbv = custom_vbv != 0;
        self.subframe_cap = subframe != 0;
        self.encoder_engines = engines.max(0) as u32;
        let ltr_codec = matches!(self.codec, Codec::H264 | Codec::H265);
        self.ltr_frames = if self.rfi_supported && ltr_codec {
            (max_ltr.max(0) as u32).min(super::nvenc_core::LTR_FRAMES)
        } else {
            0
        };
        tracing::info!(
            rfi = self.rfi_supported,
            ltr_frames = self.ltr_frames,
            custom_vbv = self.custom_vbv,
            yuv444 = self.yuv444_supported,
            async_encode,
            subframe_readback = self.subframe_cap,
            dynamic_slice = dyn_slice != 0,
            engines = self.encoder_engines,
            client_slices = self.client_slices,
            max = %format!("{wmax}x{hmax}"),
            ten_bit,
            "NVENC capabilities probed"
        );
        Ok(NvCaps {
            ten_bit,
            async_encode,
        })
    }

    /// Split mode, slice count and sub-frame for the next open, from the probed caps.
    /// The slice count is the client's, unless `PUNKTFUNK_NVENC_SLICES` overrides it.
    /// A measured split verdict beats the static rule ([`open_split_mode`]), except on a
    /// single-slice client: a verdict cached for a sliced session must not turn split on.
    /// `subframe_broken` beats the operator's force.
    pub fn resolve_shape(&mut self) -> u32 {
        let pixel_rate = self.width as u64 * self.height as u64 * self.fps.max(1) as u64;
        let static_mode = resolve_split_mode(
            self.codec,
            self.bit_depth,
            pixel_rate,
            self.encoder_engines,
            self.client_slices,
        );
        let split_mode = if self.client_slices <= 1 {
            static_mode
        } else {
            open_split_mode(static_mode, &self.split_key())
        };
        self.slices = resolve_slices(self.codec, self.client_slices);
        let subframe = resolve_subframe(self.slices, self.subframe_cap) && !self.subframe_broken;
        self.subframe_forced = subframe_env_forced();
        // Before the ladder, the ceiling key and the chunked-poll latch: a drop inside
        // `build_init_params` would leave `poll_chunk` busy-polling.
        let (split_mode, subframe) =
            resolve_split_subframe(self.codec, split_mode, subframe, self.subframe_forced);
        self.subframe_on = subframe;
        self.subframe_opened_with = subframe;
        split_mode
    }

    /// Session `NV_ENC_CONFIG` at `bitrate`: the session's preset under ULL tuning plus the
    /// low-latency contract. Open and reconfigure both author it, so a retarget moves only
    /// bitrate and derived VBV.
    ///
    /// # Safety
    /// `enc` is a live session.
    unsafe fn build_config(&self, enc: *mut c_void, bitrate: u64) -> Result<nv::NV_ENC_CONFIG> {
        let mut preset = nv::NV_ENC_PRESET_CONFIG {
            version: nv::NV_ENC_PRESET_CONFIG_VER,
            presetCfg: nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..seed_config()
            },
            ..seed_preset_config()
        };
        // SAFETY: per the contract; `preset` is a live, version-set local.
        unsafe {
            (self.api.get_encode_preset_config_ex)(
                enc,
                self.codec_guid,
                preset_guid(self.preset),
                nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset,
            )
        }
        .nv_ok()
        .map_err(|e| nvenc_status::call_err("get_encode_preset_config_ex", e))?;
        // SAFETY: `presetCfg` is a live, writable field of the local above.
        unsafe { force_frame_mode(&raw mut preset.presetCfg) };
        let mut cfg = preset.presetCfg;
        apply_low_latency_config(
            &mut cfg,
            LowLatencyConfig {
                codec: self.codec,
                bitrate,
                fps: self.fps,
                custom_vbv: self.custom_vbv,
                chroma_444: self.chroma_444,
                full_chroma_input: full_chroma_input(self.buffer_fmt),
                bit_depth: self.bit_depth,
                av1_input_depth_minus8: if ten_bit_input(self.buffer_fmt) { 2 } else { 0 },
                hdr: self.hdr,
                full_range: self.full_range,
                rfi_supported: self.rfi_supported,
                intra_refresh_cnt: self.wave_cycle(),
                slices: self.slices,
                ltr_frames: self.ltr_frames,
            },
        );
        Ok(cfg)
    }

    /// Identity in the process-lifetime bitrate-ceiling cache.
    pub fn ceiling_key(&self, split_mode: u32) -> CeilingKey {
        CeilingKey {
            gpu: self.gpu,
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bit_depth: self.bit_depth,
            chroma_444: self.chroma_444,
            split_mode,
        }
    }

    /// Identity in the split-verdict cache: the ceiling key minus the split being decided.
    pub fn split_key(&self) -> SplitKey {
        SplitKey {
            gpu: self.gpu,
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bit_depth: self.bit_depth,
            chroma_444: self.chroma_444,
        }
    }

    /// Open and initialize one session at `bitrate`/`split_mode`. Destroys the handle on
    /// failure: NVENC has no re-init after a failed `initialize_encoder`.
    ///
    /// # Safety
    /// `target.device` is a live device of `target.device_type` for the call.
    unsafe fn try_open_session(
        &self,
        target: OpenTarget,
        bitrate: u64,
        split_mode: u32,
    ) -> Result<*mut c_void> {
        // SAFETY: per the contract.
        let enc = unsafe { open_session(self.api, target) }
            .map_err(|e| nvenc_status::call_err("open_encode_session_ex", e))?;
        // SAFETY: `enc` is the live session opened above.
        let mut cfg = match unsafe { self.build_config(enc, bitrate) } {
            Ok(cfg) => cfg,
            Err(e) => {
                // SAFETY: the session opened above; nothing else holds it.
                let _ = unsafe { (self.api.destroy_encoder)(enc) };
                return Err(e);
            }
        };
        // `build_init_params` refuses sub-frame on an async session.
        let mut init = build_init_params(
            self.codec_guid,
            preset_guid(self.preset),
            self.width,
            self.height,
            self.fps,
            &mut cfg,
            split_mode,
            self.session_async,
            self.subframe_on,
        );
        // SAFETY: `enc` is live; `init` and the `cfg` it points at outlive the sync call.
        match unsafe { (self.api.initialize_encoder)(enc, &mut init) }.nv_ok() {
            Ok(()) => Ok(enc),
            Err(e) => {
                // SAFETY: the session opened above; nothing else holds it.
                let _ = unsafe { (self.api.destroy_encoder)(enc) };
                Err(nvenc_status::call_err("initialize_encoder", e))
            }
        }
    }

    /// Open the session through [`open_at_ceiling`] at `split_mode` ([`Self::resolve_shape`]).
    /// Publishes the handle, the rate and the split that opened, and caches a proven ceiling.
    ///
    /// # Safety
    /// The session is closed, and `target.device` is a live device of `target.device_type`
    /// for the call.
    pub unsafe fn open(&mut self, target: OpenTarget, split_mode: u32) -> Result<()> {
        self.cycle = session_wave_cycle(self.codec, self.height, self.fps);
        // Sized per session, never per frame: the doNotWait sampler hands it to the driver.
        self.slice_offsets
            .resize(slice_offsets_len(self.width, self.height), 0);
        let api = self.api;
        let try_open = |bps, split| {
            // SAFETY: per this fn's contract.
            unsafe { self.try_open_session(target, bps, split) }
        };
        let destroy = |enc| {
            // SAFETY: a session the ladder opened and let go; this is its only destroy.
            let _ = unsafe { (api.destroy_encoder)(enc) };
        };
        let cached = cached_ceiling(&self.ceiling_key(split_mode));
        let opened = open_at_ceiling(self.bitrate_bps, split_mode, cached, try_open, destroy)?;
        if let Some(ceiling) = opened.ceiling {
            store_ceiling(self.ceiling_key(opened.split_mode), ceiling);
        }
        self.encoder = opened.handle;
        self.bitrate_bps = opened.bps;
        self.split_mode = opened.split_mode;
        Ok(())
    }

    /// One output bitstream per pool slot on the session [`Self::open`] published.
    pub fn create_bitstreams(&mut self) -> Result<()> {
        for _ in 0..POOL {
            let mut cb = nv::NV_ENC_CREATE_BITSTREAM_BUFFER {
                version: nv::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                ..Default::default()
            };
            ensure_open(self.encoder)?;
            // SAFETY: `encoder` is live (checked); `cb` is a live, version-set local, and its
            // buffer is copied into `bitstreams` before `cb` drops.
            unsafe { (self.api.create_bitstream_buffer)(self.encoder, &mut cb) }
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("create_bitstream_buffer", e))?;
            self.bitstreams.push(cb.bitstreamBuffer);
        }
        Ok(())
    }

    /// Run `run` on thread `name` as this session's two-thread retrieve.
    pub fn start_retrieve(&mut self, name: &str, run: RetrieveLoop) -> Result<()> {
        ensure_open(self.encoder)?;
        let (work_tx, work_rx) = mpsc::sync_channel::<RetrieveJob>(POOL);
        let (done_tx, done_rx) = mpsc::channel::<RetrieveDone>();
        let enc = self.encoder as usize;
        let join = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || run(enc, work_rx, done_tx))
            .context("spawn NVENC retrieve thread")?;
        self.async_rt = Some(AsyncRetrieve {
            work_tx: Some(work_tx),
            done_rx,
            join: Some(join),
            ready: VecDeque::new(),
        });
        Ok(())
    }

    /// The session is complete: latch chunked poll and arm the split arbiter.
    pub fn finish_open(&mut self) {
        self.inited = !self.encoder.is_null();
        self.subframe_chunks = self.chunks_armable(self.subframe_on);
        if self.subframe_chunks {
            tracing::info!(
                slices = self.slices,
                "NVENC sub-frame chunked poll armed (poll_chunk emits slice-boundary AU chunks)"
            );
        }
        self.arm_split_arbiter();
    }

    /// Chunked poll needs `slices ≥ 2`, sub-frame, and the bitstream lock on this thread: an
    /// async session hands it to the retrieve thread or the caller's event.
    fn chunks_armable(&self, subframe: bool) -> bool {
        self.slices >= 2 && subframe && !self.session_async && self.async_rt.is_none()
    }

    /// Arm a live split experiment (`PUNKTFUNK_NVENC_SPLIT_ARBITRATE=1`). An operator pin
    /// (`PUNKTFUNK_SPLIT_ENCODE`) wins and a cached verdict is not re-run. Sync depth-1 only:
    /// a pipelined or event-driven session keeps frames in flight, so `last_submit_at` names
    /// a newer submit than the AU it is measured against. Needs ≥ 2 engines; never H.264.
    /// Forced HEVC split drops sub-frame, so that trade is priced as a handicap
    /// (`spread × (slices−1)/slices`) or not arbitrated at all.
    fn arm_split_arbiter(&mut self) {
        let knobs = crate::knobs::get();
        if knobs.nvenc_split_arbitrate != 1 {
            return;
        }
        if knobs.split_encode != 0
            || cached_split_verdict(&self.split_key()).is_some()
            || self.session_async
            || self.async_rt.is_some()
            || self.encoder_engines < 2
            || self.codec == Codec::H264
        {
            return;
        }
        let handicap_us = if self.subframe_on && self.codec != Codec::Av1 {
            if self.send_spread_us == 0 || self.slices < 2 {
                tracing::debug!(
                    "NVENC split arbitration skipped: split would cost sub-frame readback and no \
                     send-spread has been reported, so the trade cannot be priced"
                );
                return;
            }
            let slices = self.slices as u64;
            self.send_spread_us as u64 * (slices - 1) / slices
        } else {
            0
        };
        // Challenge with the widest forced split unless already there (then DISABLE). Do not
        // challenge AUTO with DISABLE: that parks the session on the slow arm.
        let widest = max_forced_split_mode(self.encoder_engines);
        let challenger = if self.split_mode == widest {
            DISABLE
        } else {
            widest
        };
        if challenger == self.split_mode {
            return;
        }
        tracing::info!(
            incumbent = self.split_mode,
            challenger,
            handicap_us,
            send_spread_us = self.send_spread_us,
            "NVENC split arbitration armed — measuring both arms on the live session (no IDR)"
        );
        self.arbiter = Some(SplitArbiter::with_handicap(
            self.split_mode,
            challenger,
            handicap_us,
        ));
    }

    /// Move the live session to `mode` without an IDR (`resetEncoder=0` at the current rate).
    /// A refusal restores every field, so the session's own idea of itself stays truthful.
    fn apply_split_mode(&mut self, mode: u32) -> bool {
        let (prev_mode, prev_sub, prev_chunks) =
            (self.split_mode, self.subframe_on, self.subframe_chunks);
        // HEVC cannot hold split and sub-frame. Restore only up to `subframe_opened_with`.
        let (mode, subframe) = resolve_split_subframe(
            self.codec,
            mode,
            self.subframe_opened_with,
            self.subframe_forced,
        );
        self.split_mode = mode;
        self.subframe_on = subframe;
        // `reconfigure_bitrate` does not recompute this latch; a stale true makes
        // `poll_chunk` busy-poll while `numSlices` never advances.
        self.subframe_chunks = self.chunks_armable(subframe);
        if self.reconfigure_bitrate(self.bitrate_bps) {
            true
        } else {
            tracing::warn!(
                from = prev_mode,
                to = mode,
                "NVENC split arbitration: driver refused the in-place split change — staying put"
            );
            self.split_mode = prev_mode;
            self.subframe_on = prev_sub;
            self.subframe_chunks = prev_chunks;
            false
        }
    }

    /// Feed the arbiter the submit→AU time of the AU just retrieved.
    fn feed_split_arbiter(&mut self) {
        let Some(encode_us) = self
            .last_submit_at
            .take()
            .map(|t| t.elapsed().as_micros() as u64)
        else {
            return;
        };
        let Some(arb) = self.arbiter.as_mut() else {
            return;
        };
        let action = arb.on_frame(encode_us);
        let done = arb.is_done();
        match action {
            Some(ArbAction::SwitchTo(mode)) => {
                if !self.apply_split_mode(mode) {
                    // The session will not move: abandon rather than compare one arm twice.
                    self.arbiter = None;
                    return;
                }
            }
            Some(ArbAction::Settled(mode)) => store_split_verdict(self.split_key(), mode),
            None => {}
        }
        if done {
            // Switching back to the incumbent settles on the mode now live.
            store_split_verdict(self.split_key(), self.split_mode);
            self.arbiter = None;
        }
    }

    /// Two-thread backpressure: block on the oldest completion until fewer than `cap` encodes
    /// are in flight.
    pub fn wait_below(&mut self, cap: usize) -> Result<()> {
        while self.pending.len() >= cap {
            let Some(rt) = self.async_rt.as_mut() else {
                return Ok(());
            };
            let done = rt
                .done_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| anyhow!("NVENC retrieve stalled (5s) — encoder wedged?"))?;
            self.absorb_done(done)?;
        }
        Ok(())
    }

    /// Map `input.reg`, author this frame's pic params (forced IDR, recovery anchor, wave
    /// start, HDR SEI on every IDR) and submit it into bitstream [`Self::slot`]. The frame's
    /// plan is spent only once the picture is queued ([`Self::spend`]): a failed encode
    /// leaves the session as it was, and the retry is the same frame.
    ///
    /// # Safety
    /// The session is open, `input.reg` is registered on it, and its surface holds this frame
    /// (filled, or ordered before the encode) until the AU is retrieved. `input.event` is the
    /// slot's registered completion event or null.
    pub unsafe fn encode(&mut self, input: EncodeInput) -> Result<EncodeTimes> {
        let opening = self.opening();
        let slot = self.slot();
        let mut mp = nv::NV_ENC_MAP_INPUT_RESOURCE {
            version: nv::NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: input.reg,
            ..Default::default()
        };
        let tm = Instant::now();
        // SAFETY: per the contract; `mp` is a live, version-set local. The mapping is unmapped
        // once: here on a failed encode, else by whoever retires the `pending` entry.
        unsafe { (self.api.map_input_resource)(self.encoder, &mut mp) }
            .nv_ok()
            .map_err(|e| nvenc_status::call_err("map_input_resource", e))?;
        let t_map = tm.elapsed();

        let ts = self.frame_idx;
        let pts = ts as u64;
        let forced = self.force_kf;
        let ltr = self.ltr_peek(ts);
        // The opening frame is an IDR the driver emits unasked: only a forced one is planned
        // as an IDR, which drops the anchor and flushes the wave, queue and all.
        let pending = Pending {
            idr: forced,
            anchor: self.pending_anchor,
            acked: !opening && ltr.is_some_and(|(_, force)| force.is_some()),
            loss: false,
        };
        let plan = plan_frame(self.recovery(), pending, &self.policy());
        let flags = if forced {
            nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                | nv::NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
        } else {
            0
        };
        // The first frame after invalidation, or one that references a confirmed picture.
        let anchor = matches!(plan.refs, FrameRef::Anchor | FrameRef::Acked);
        let (wave, mark) = (plan.wave, plan.mark);
        // P-only + infinite GOP: IDRs are forced, or the opening frame NVENC emits as one.
        // Chunked poll flags early chunks from this before the driver reports `pictureType`.
        let idr = forced || opening;
        // The long-term step sits out an IDR, which empties the slots, and a wave.
        let ltr = ltr.filter(|_| !idr && wave.is_none());
        let mut pic = nv::NV_ENC_PIC_PARAMS {
            version: nv::NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.width,
            inputHeight: self.height,
            inputPitch: input.pitch,
            inputBuffer: mp.mappedResource,
            bufferFmt: mp.mappedBufferFmt,
            outputBitstream: self.bitstreams[slot],
            pictureStruct: nv::NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            inputTimeStamp: pts,
            encodePicFlags: flags,
            completionEvent: input.event,
            ..seed_pic_params()
        };

        // In-band HDR10 SEI on every IDR: ST.2086 mastering + CEA-861.3 CLL. AV1 carries them
        // as metadata OBUs instead. The scratch outlives `encode_picture`.
        let mastering_sei = self
            .hdr_meta
            .map(|m| pf_frame::hdr::hevc_mastering_display_sei(&m));
        let cll_sei = self
            .hdr_meta
            .map(|m| pf_frame::hdr::hevc_content_light_level_sei(&m));
        let mut sei: Vec<nv::NV_ENC_SEI_PAYLOAD> = Vec::new();
        if idr && self.hdr {
            if let Some(p) = mastering_sei.as_ref() {
                sei.push(nv::NV_ENC_SEI_PAYLOAD {
                    payloadSize: p.len() as u32,
                    payloadType: pf_frame::hdr::SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME,
                    payload: p.as_ptr().cast_mut(),
                });
            }
            if let Some(p) = cll_sei.as_ref() {
                sei.push(nv::NV_ENC_SEI_PAYLOAD {
                    payloadSize: p.len() as u32,
                    payloadType: pf_frame::hdr::SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO,
                    payload: p.as_ptr().cast_mut(),
                });
            }
        }

        // The wave's start frame: the driver sweeps the picture over `cycle` frames from here,
        // one union arm per codec. Zero on every other frame. Union-arm writes are safe.
        let force_cnt = wave.filter(|w| w.index == 0).map_or(0, |w| w.cycle);
        match self.codec {
            Codec::H265 => {
                pic.codecPicParams
                    .hevcPicParams
                    .forceIntraRefreshWithFrameCnt = force_cnt;
            }
            Codec::H264 => {
                pic.codecPicParams
                    .h264PicParams
                    .forceIntraRefreshWithFrameCnt = force_cnt;
            }
            Codec::Av1 => {
                pic.codecPicParams
                    .av1PicParams
                    .forceIntraRefreshWithFrameCnt = force_cnt;
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
        if let Some((mark, force)) = ltr {
            let bitmap = force.map_or(0, |(slot, _)| 1u32 << slot);
            let (mark, mark_idx) = mark.map_or((0, 0), |m| (1, m as u32));
            match self.codec {
                // SAFETY: H.264 session: `h264PicParams` is the active arm.
                Codec::H264 => unsafe {
                    let p = &mut pic.codecPicParams.h264PicParams;
                    p.set_ltrMarkFrame(mark);
                    p.ltrMarkFrameIdx = mark_idx;
                    p.set_ltrUseFrames(u32::from(bitmap != 0));
                    p.ltrUseFrameBitmap = bitmap;
                },
                // SAFETY: HEVC session: `hevcPicParams` is the active arm.
                Codec::H265 => unsafe {
                    let p = &mut pic.codecPicParams.hevcPicParams;
                    p.set_ltrMarkFrame(mark);
                    p.ltrMarkFrameIdx = mark_idx;
                    p.set_ltrUseFrames(u32::from(bitmap != 0));
                    p.ltrUseFrameBitmap = bitmap;
                },
                Codec::Av1 | Codec::PyroWave => {}
            }
        }
        if !sei.is_empty() {
            match self.codec {
                Codec::H265 => {
                    pic.codecPicParams.hevcPicParams.seiPayloadArray = sei.as_mut_ptr();
                    pic.codecPicParams.hevcPicParams.seiPayloadArrayCnt = sei.len() as u32;
                }
                Codec::H264 => {
                    pic.codecPicParams.h264PicParams.seiPayloadArray = sei.as_mut_ptr();
                    pic.codecPicParams.h264PicParams.seiPayloadArrayCnt = sei.len() as u32;
                }
                Codec::Av1 => {}
                Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
            }
        }
        let tp = Instant::now();
        // SAFETY: the session is open; `pic` names the mapping above and a pool bitstream, and
        // the SEI scratch it points at lives until this sync call returns.
        if let Err(e) = unsafe { (self.api.encode_picture)(self.encoder, &mut pic) }.nv_ok() {
            // Nothing owns the mapping yet; left mapped, the slot's next map fails too.
            // SAFETY: the mapping made above on this session, unmapped once.
            let _ = unsafe { (self.api.unmap_input_resource)(self.encoder, mp.mappedResource) };
            return Err(nvenc_status::call_err("encode_picture", e));
        }
        let t_pic = tp.elapsed();
        self.spend(ts, idr, plan, ltr);
        self.next += 1;
        let bs = self.bitstreams[slot];
        self.pending.push_back(PendingEncode {
            bs,
            map: mp.mappedResource,
            pts_ns: input.pts_ns,
            anchor,
            idr_hint: idr,
            mark,
            _hold: input.hold,
        });
        // The arbiter's cost clock; meaningful only on the sync depth-1 path it arms on.
        self.last_submit_at = Some(Instant::now());
        // The channel holds POOL jobs and in-flight stays below it, so this send never blocks.
        // A dead thread would strand this AU: the caller rebuilds.
        if let Some(rt) = &self.async_rt {
            let job = RetrieveJob {
                bs: bs as usize,
                event: input.event as usize,
            };
            if rt.work_tx.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
                bail!("NVENC retrieve thread gone — rebuilding the session");
            }
        }
        Ok(EncodeTimes {
            map: t_map,
            pic: t_pic,
        })
    }

    /// Frame `ts` is queued: spend what its plan took. A forced IDR flushes the DPB, so every
    /// later reference is clean again and nothing before it stays in a wave's span.
    fn spend(&mut self, ts: i64, idr: bool, plan: FramePlan, ltr: Option<LtrStep>) {
        self.frame_idx = ts + 1;
        self.pending_anchor = false;
        if std::mem::take(&mut self.force_kf) {
            self.distrusted = false;
            self.wave_span = self.wave_span.map(|(s, close)| (s, close.min(ts)));
        }
        if let Some(w) = plan.wave.filter(|w| w.index == 0) {
            // A wave after a spoiled one keeps the spoiled pictures dirty too.
            let start = match self.wave_span {
                Some((s, _)) if self.wave_spoiled => s,
                _ => ts,
            };
            self.wave_span = Some((start, ts + i64::from(w.cycle) - 1));
        }
        (self.wave, self.wave_queued) = (plan.next.wave, plan.next.queued);
        let spoiled_close = plan.dirty && plan.wave.is_some_and(Wave::closes);
        self.wave_spoiled = plan.next.spoiled || spoiled_close;
        self.ltr_mark(ts, idr, ltr);
    }

    /// The long-term step frame `ts` would take: the slot it marks and the one it forces.
    /// Under confirmed references, the newest confirmed slot ([`crate::rfi::ltr_acked_step`]);
    /// otherwise the kept candidate's mark ([`crate::rfi::Kept`]) and a late loss's anchor.
    /// `None` without long-term slots.
    fn ltr_peek(&mut self, ts: i64) -> Option<LtrStep> {
        let n = self.ltr_frames as usize;
        if self.ltr_slots.len() != n {
            self.ltr_slots = vec![None; n];
            self.ltr_next = 0;
        }
        if n == 0 {
            return None;
        }
        let slots = &self.live.ltr_slots;
        let force = self.live.ltr_force.and_then(|f| slots[f].map(|w| (f, w)));
        if let Some(acked) = self.reference_floor {
            // A kept anchor outranks the newest confirmed slot, which reaches back less far.
            let (mark, confirmed) = crate::rfi::ltr_acked_step(slots, &acked, ts, self.ltr_next);
            let mark = mark.filter(|&m| force.is_none_or(|(f, _)| f != m));
            return Some((mark, force.or(confirmed)));
        }
        let mut kept = self.live.kept;
        let keep = kept.step(ts, true, self.confirmed.as_ref(), |w| {
            slots.contains(&Some(w))
        });
        let free =
            |s: usize| slots[s].is_none_or(|w| !kept.holds(w)) && force.is_none_or(|(f, _)| f != s);
        let mark = keep
            .then(|| {
                (0..n)
                    .map(|k| (self.live.ltr_next + k) % n)
                    .find(|&s| free(s))
            })
            .flatten();
        Some((mark, force))
    }

    /// Record frame `ts` in the slot table: an IDR empties it, a step's mark fills its slot,
    /// and the kept pair follows what the slots hold.
    fn ltr_mark(&mut self, ts: i64, idr: bool, step: Option<LtrStep>) {
        let live = &mut self.live;
        if idr {
            live.ltr_slots.fill(None);
            live.ltr_held = 0;
            live.ltr_next = 0;
        }
        live.ltr_force = None;
        let marked = matches!(step, Some((Some(_), _)));
        if let Some((Some(m), _)) = step {
            live.ltr_slots[m] = Some(ts);
            live.ltr_held |= 1 << m;
            live.ltr_next = (m + 1) % live.ltr_slots.len();
        }
        if self.reference_floor.is_none() {
            let slots = &live.ltr_slots;
            live.kept.step(ts, marked, self.confirmed.as_ref(), |w| {
                slots.contains(&Some(w))
            });
        }
    }

    /// Empty every long-term slot a loss from `first` reached. Outside confirmed references
    /// each later picture chains from the lost one; under them the floor's own acks decide.
    fn taint_ltr_from(&mut self, first: i64) {
        if self.reference_floor.is_none() {
            for w in &mut self.live.ltr_slots {
                if w.is_some_and(|w| w >= first) {
                    *w = None;
                }
            }
        }
    }

    /// A loss the short-term window no longer reaches: the newest long-term picture before it
    /// becomes the next frame's only reference, which the client holds however late the report
    /// came. Under confirmed references only a confirmed one qualifies. `None` when none
    /// predates the loss, or `first` is not yet coded.
    fn force_kept(&mut self, first: i64) -> Option<bool> {
        if first < 0 || first >= self.frame_idx {
            return None;
        }
        let floor = self.reference_floor;
        let clean = |w: i64| w < first && floor.is_none_or(|a| a.contains(w));
        let (slot, wire) = self
            .ltr_slots
            .iter()
            .enumerate()
            .filter_map(|(s, w)| w.filter(|&w| clean(w)).map(|w| (s, w)))
            .max_by_key(|&(_, w)| w)?;
        self.live.ltr_force = Some(slot);
        self.pending_anchor = true;
        tracing::debug!(
            first,
            slot,
            wire,
            "nvenc RFI: the DPB is past the loss — anchoring on a kept long-term picture"
        );
        Some(true)
    }

    /// Unmap one retired input.
    ///
    /// # Safety
    /// `map` is null or a mapping on the live session, not yet unmapped.
    unsafe fn unmap(&self, map: nv::NV_ENC_INPUT_PTR) {
        if !map.is_null() {
            // SAFETY: per the contract.
            let _ = unsafe { (self.api.unmap_input_resource)(self.encoder, map) };
        }
    }

    /// AV1 keyframes carry the HDR volume as metadata OBUs after the sequence header; NVENC
    /// writes none itself.
    fn av1_hdr_obus(&self, mut data: Vec<u8>, keyframe: bool) -> Vec<u8> {
        if let Some(m) = self
            .hdr_meta
            .filter(|_| keyframe && self.hdr && self.codec == Codec::Av1)
        {
            let obus = pf_frame::hdr::av1_hdr_metadata_obus(&m);
            pf_frame::hdr::av1_insert_before_frame(&mut data, &obus);
        }
        data
    }

    /// Fold one retrieve-thread completion in on the encode thread: pop the oldest `pending`,
    /// check the pairing, unmap, queue the AU. A retrieve error surfaces after the unmap so the
    /// rebuild starts from clean state.
    fn absorb_done(&mut self, done: RetrieveDone) -> Result<()> {
        let Some(p) = self.pending.pop_front() else {
            bail!("NVENC retrieve: completion with no in-flight frame (pairing bug)");
        };
        if p.bs as usize != done.bs {
            bail!("NVENC retrieve: completion out of order (pairing bug)");
        }
        // SAFETY: `p.map` is the mapping `encode` recorded for this completed encode on the live
        // session (`async_rt` exists only while it is open); unmapped once, here.
        unsafe { self.unmap(p.map) };
        let (data, keyframe) = done.result.map_err(|e| anyhow!("{e}"))?;
        let data = self.av1_hdr_obus(data, keyframe);
        self.async_rt
            .as_mut()
            .expect("absorb_done is only reachable with a retrieve thread")
            .ready
            .push_back(EncodedFrame {
                data,
                pts_ns: p.pts_ns,
                keyframe,
                recovery_anchor: p.anchor,
                recovery_point: p.mark.point(),
                recovery_close: p.mark.close(),
                chunk_aligned: false,
            });
        Ok(())
    }

    /// The oldest AU. Two-thread: a non-blocking drain, `None` while in flight. Otherwise a
    /// blocking lock on the oldest encode.
    pub fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // A partially-chunked AU must finish through `poll_chunk`; a whole-AU poll would
        // double-emit.
        if self.chunk.is_some() {
            bail!("NVENC poll() called mid-chunked-AU — drain it via poll_chunk (caller bug)");
        }
        if self.async_rt.is_some() {
            while let Some(done) = self
                .async_rt
                .as_mut()
                .and_then(|rt| rt.done_rx.try_recv().ok())
            {
                self.absorb_done(done)?;
            }
            return Ok(self.async_rt.as_mut().and_then(|rt| rt.ready.pop_front()));
        }
        let Some(p) = self.pending.pop_front() else {
            return Ok(None);
        };
        // SAFETY: a `pending` entry implies the live session and a submitted encode on `p.bs`.
        // The blocking lock copies, then unlocks.
        let lock = match unsafe { BitstreamLock::new(self.api, self.encoder, p.bs, false, None) } {
            Ok(lock) => lock,
            Err(e) => {
                // SAFETY: the mapping for this retired encode, unmapped once. As in
                // `absorb_done`, the error surfaces after it so the rebuild starts clean.
                unsafe { self.unmap(p.map) };
                return Err(nvenc_status::call_err("lock_bitstream", e));
            }
        };
        let data = lock.bytes().to_vec();
        let keyframe = lock.keyframe();
        let unlocked = lock.unlock();
        // SAFETY: the mapping for this retired encode, unmapped once.
        unsafe { self.unmap(p.map) };
        unlocked.map_err(|e| nvenc_status::call_err("unlock_bitstream", e))?;
        // Sync depth-1: the lock blocked until the ASIC finished.
        self.feed_split_arbiter();
        let data = self.av1_hdr_obus(data, keyframe);
        Ok(Some(EncodedFrame {
            data,
            pts_ns: p.pts_ns,
            keyframe,
            recovery_anchor: p.anchor,
            recovery_point: p.mark.point(),
            recovery_close: p.mark.close(),
            chunk_aligned: false,
        }))
    }

    /// Chunked poll is live now. Dynamic: pipelined escalation and teardown both drop it.
    pub fn supports_chunked_poll(&self) -> bool {
        self.subframe_chunks && self.async_rt.is_none()
    }

    /// Slice-boundary chunks of the oldest AU: sample it with doNotWait locks for ~2 frame
    /// intervals, then finish through one blocking lock. A driver that hands a complete AU out
    /// once (R620) finishes on the sample that saw it complete. A finish that disowns the
    /// sampled bytes latches sub-frame off. A session that cannot chunk hands out one whole-AU
    /// chunk.
    pub fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        if !self.supports_chunked_poll() && self.chunk.is_none() {
            return Ok(self.poll()?.map(AuChunk::whole));
        }
        let Some(front) = self.pending.front() else {
            return Ok(None);
        };
        let (bs, pts_ns, anchor, idr_hint, mark) = (
            front.bs,
            front.pts_ns,
            front.anchor,
            front.idr_hint,
            front.mark,
        );
        // A driver that never publishes intermediate slices costs this budget, then the lock.
        let budget = Duration::from_micros(2_000_000 / self.fps.max(1) as u64);
        let t0 = Instant::now();
        // The most bytes this call's samples showed. An earlier call's ended in a chunk, which
        // `emitted` covers.
        let mut seen = 0;
        // The sample that showed every slice. R620 hands a complete AU out once, to that sample.
        let mut complete: Option<Vec<u8>> = None;
        loop {
            let emitted = self.chunk.as_ref().map_or(0, |c| c.emitted);
            let slices_out = self.chunk.as_ref().map_or(0, |c| c.slices_out);
            // SAFETY: `bs` is the front `pending` bitstream on the live session. `open` sized
            // `slice_offsets` for this frame.
            let lock = unsafe {
                BitstreamLock::new(
                    self.api,
                    self.encoder,
                    bs,
                    true,
                    Some(&mut self.slice_offsets),
                )
            };
            // LOCK_BUSY = not ready. The finishing blocking lock owns real failures.
            if let Ok(lock) = lock {
                let n = lock.info().numSlices;
                let bytes = lock.bytes().len();
                seen = seen.max(bytes);
                if n >= self.slices {
                    // Every slice readable: finish through the blocking lock (`numSlices`
                    // alone is not trusted across driver branches), with this read in hand.
                    complete = Some(lock.bytes().to_vec());
                    drop(lock);
                    break;
                }
                if bytes > emitted && slice_ready(n, slices_out, &lock.bytes()[emitted..]) {
                    // New slice(s): cut `[emitted..bytes)`, contiguous Annex-B on a NAL boundary.
                    let data = lock.bytes()[emitted..].to_vec();
                    lock.unlock()
                        .map_err(|e| nvenc_status::call_err("unlock_bitstream (chunk)", e))?;
                    let cs = self.chunk.get_or_insert_with(ChunkState::new);
                    cs.shadow.extend_from_slice(&data);
                    let first = !cs.opened;
                    cs.opened = true;
                    cs.emitted = bytes;
                    cs.slices_out = n;
                    return Ok(Some(AuChunk {
                        data,
                        pts_ns,
                        keyframe: idr_hint,
                        recovery_anchor: anchor,
                        recovery_point: mark.point(),
                        recovery_close: mark.close(),
                        chunk_aligned: false,
                        first,
                        last: false,
                    }));
                }
            }
            if t0.elapsed() > budget {
                break;
            }
            std::thread::sleep(CHUNK_SAMPLE_INTERVAL);
        }

        // The AU tail must not ride a +1 tick (depth-1 pump contract).
        let p = self.pending.pop_front().expect("front() checked above");
        // This AU ends here however the finish goes: a stale cursor would cut the next one.
        let cs = self.chunk.take().unwrap_or_else(ChunkState::new);
        // SAFETY: as in `poll`: the popped encode on the live session. Every read of the locked
        // bytes happens before its unlock.
        let lock = match unsafe { BitstreamLock::new(self.api, self.encoder, p.bs, false, None) } {
            Ok(lock) => lock,
            Err(e) => {
                // SAFETY: the mapping for this retired encode, unmapped once, error or not.
                unsafe { self.unmap(p.map) };
                return Err(nvenc_status::call_err("lock_bitstream (chunk finish)", e));
            }
        };
        // An empty finish after a sample saw the AU complete: that sample took it.
        let full = match (lock.bytes(), complete.as_deref()) {
            ([], Some(sampled)) => sampled,
            (full, _) => full,
        };
        let total = full.len();
        // The finished AU must start with the emitted bytes and must not be empty after a sample
        // saw bytes, or the wire carries undetectable corruption. Latch sub-frame off and bail
        // into stall recovery (rebuild without sub-frame, IDR).
        if prefix_diverged(&cs.shadow, cs.emitted, seen, full) {
            drop(lock);
            // SAFETY: the mapping for this retired encode, unmapped once.
            unsafe { self.unmap(p.map) };
            self.subframe_broken = true;
            tracing::warn!(
                emitted = cs.emitted,
                seen,
                total,
                "NVENC sub-frame readback diverged from the finished AU — this driver's early \
                 slice publishes cannot be trusted; disarming sub-frame for every later session \
                 open and rebuilding the encoder"
            );
            bail!(
                "NVENC chunked poll: sub-frame readback diverged from the finished AU ({} bytes \
                 emitted, {} total) — sub-frame disarmed, rebuild required",
                cs.emitted,
                total
            );
        }
        let data = full[cs.emitted..].to_vec();
        let keyframe = lock.keyframe();
        let unlocked = lock.unlock();
        // SAFETY: the mapping for this retired encode, unmapped once.
        unsafe { self.unmap(p.map) };
        unlocked.map_err(|e| nvenc_status::call_err("unlock_bitstream (chunk finish)", e))?;
        if cs.opened && keyframe != p.idr_hint {
            // P-only + infinite GOP never diverges; if it did, earlier chunks had the wrong flag.
            tracing::warn!(
                predicted = p.idr_hint,
                actual = keyframe,
                "NVENC chunked poll: picture type diverged from the submit-time prediction"
            );
        }
        // A sub-frame session finishes here: without this feed its incumbent is invisible and
        // the experiment never concludes.
        self.feed_split_arbiter();
        Ok(Some(AuChunk {
            data,
            pts_ns: p.pts_ns,
            keyframe,
            recovery_anchor: p.anchor,
            recovery_point: p.mark.point(),
            recovery_close: p.mark.close(),
            chunk_aligned: false,
            first: !cs.opened,
            last: true,
        }))
    }

    /// Range RFI ([`plan_range_recovery`]) with the wave as the no-anchor answer. `false` = the
    /// caller forces an IDR: no session, no GPU support, or distrusted references.
    pub fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        if self.encoder.is_null() || !self.rfi_supported || self.distrusted {
            return false;
        }
        self.taint_ltr_from(first);
        // A sweep in flight answers every ask until it closes. The driver neither honours an
        // invalidation mid-sweep nor keeps sweeping after one, and an anchor tagged there
        // lifts the client onto damage that only the next IDR clears.
        if self.wave.is_some() || super::nvenc_core::wave_always() {
            return self.start_wave(first, last);
        }
        let short_term = super::nvenc_core::RFI_DPB.saturating_sub(self.ltr_held.count_ones());
        match plan_range_recovery(first, last, self.frame_idx, self.last_rfi_range, short_term) {
            // Already invalidated. Re-arm the anchor: the previous recovery AU may itself have
            // been lost, and the next frame is equally clean.
            RangePlan::Covered => {
                self.pending_anchor = true;
                true
            }
            RangePlan::Decline => self
                .force_kept(first)
                .unwrap_or_else(|| self.start_wave(first, last)),
            RangePlan::Invalidate { first, last } => {
                // The driver would anchor on `first - 1`; a part-dirty wave picture there would
                // lift the client onto damage, so a kept picture or a wave answers instead.
                if self.wave_dirty(first - 1) {
                    return self
                        .force_kept(first)
                        .unwrap_or_else(|| self.start_wave(first, last));
                }
                // `inputTimeStamp` is the wire index, so the lost range maps 1:1 onto NVENC
                // timestamps across rebuilds.
                for ts in first..=last {
                    // SAFETY: the live session (checked non-null) on the encode thread; each
                    // `ts` lies in `[oldest_in_dpb, frame_idx - 1]`, a frame still in the DPB.
                    let st = unsafe { (self.api.invalidate_ref_frames)(self.encoder, ts as u64) };
                    if st.nv_ok().is_err() {
                        return false;
                    }
                }
                self.last_rfi_range = Some((first, last));
                self.pending_anchor = true;
                for slot in &mut self.ltr_slots {
                    if slot.is_some_and(|w| (first..=last).contains(&w)) {
                        *slot = None;
                    }
                }
                true
            }
        }
    }

    /// Retarget the live session in place: no reset, no IDR, same init params but the rate.
    /// Clamps to the cached codec-level ceiling first, so an overshoot does not bounce into a
    /// rebuild. `false` = the caller rebuilds, which owns the clamp search.
    pub fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        if !self.inited {
            // No live session yet: the lazy open runs at the new rate.
            self.bitrate_bps = bps;
            return true;
        }
        let bps = match cached_ceiling(&self.ceiling_key(self.split_mode)) {
            Some(ceiling) => bps.min(ceiling),
            None => bps,
        };
        // SAFETY: `inited` ⟹ the live session, on the encode thread between submit and poll.
        let mut cfg = match unsafe { self.build_config(self.encoder, bps) } {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"),
                    "NVENC reconfigure: config re-author failed — falling back to a rebuild");
                return false;
            }
        };
        let mut params = nv::NV_ENC_RECONFIGURE_PARAMS {
            version: nv::NV_ENC_RECONFIGURE_PARAMS_VER,
            // The session's recorded state, never a fresh env read: an env re-read could flip
            // `enableSubFrameWrite` mid-session.
            reInitEncodeParams: build_init_params(
                self.codec_guid,
                preset_guid(self.preset),
                self.width,
                self.height,
                self.fps,
                &mut cfg,
                self.split_mode,
                self.session_async,
                self.subframe_on,
            ),
            ..Default::default()
        };
        // Keep RC state and the reference chain: no reset, no IDR.
        params.set_resetEncoder(0);
        params.set_forceIDR(0);
        // SAFETY: the live session (above); `params` and the `cfg` it points at outlive this
        // sync call. A submit-side call: the retrieve thread only locks bitstreams.
        match unsafe { (self.api.reconfigure_encoder)(self.encoder, &mut params) }.nv_ok() {
            Ok(()) => {
                self.bitrate_bps = bps;
                true
            }
            Err(e) => {
                tracing::warn!(status = ?e, mbps = bps / 1_000_000,
                    "nvEncReconfigureEncoder rejected — falling back to a rebuild");
                false
            }
        }
    }

    /// First half of a teardown: join the retrieve thread (it finishes queued jobs against the
    /// still-live session), then unmap every in-flight input. The session stays open for the
    /// backend to unregister its inputs. Idempotent. The join is untimed: for a retrieve loop
    /// that bounds its own waits (Windows completion events).
    pub fn stop(&mut self) {
        if let Some(rt) = self.async_rt.take() {
            rt.stop(None);
        }
        self.unmap_pending();
    }

    /// [`Self::stop`] for a retrieve loop that blocks in the driver with no timeout of its own
    /// (Linux: `nvEncLockBitstream` returns only when the driver completes or fails the
    /// encode). `false` = the thread did not leave the lock within `limit`. It still holds the
    /// session and a pool bitstream, so nothing is unmapped or destroyed: the session is
    /// forgotten as it stands, and its handle, its pool and the thread leak. The backend
    /// leaks the inputs it registered with them.
    pub fn stop_within(&mut self, limit: Duration) -> bool {
        if let Some(rt) = self.async_rt.take() {
            if !rt.stop(Some(limit)) {
                self.live = Live::default();
                return false;
            }
        }
        self.unmap_pending();
        true
    }

    /// Unmap every in-flight input. The session is live and no retrieve thread runs.
    fn unmap_pending(&mut self) {
        let (api, enc) = (self.api, self.encoder);
        for p in &mut self.pending {
            let map = std::mem::replace(&mut p.map, std::ptr::null_mut());
            if !map.is_null() {
                // SAFETY: the entry's mapping on the live session (a `pending` entry implies
                // it); nulled above, so no later retire unmaps it again.
                let _ = unsafe { (api.unmap_input_resource)(enc, map) };
            }
        }
    }

    /// Second half, after the backend's unregisters: [`Self::stop`] (a no-op if the backend
    /// ran it), destroy the bitstream pool, hand the session to `destroy`, and drop everything
    /// the session owned ([`Live`]). The next open starts with an IDR and an empty DPB, so
    /// RFI range, anchor, distrust and wave state go too.
    pub fn close(&mut self, destroy: impl FnOnce(*mut c_void)) {
        if self.encoder.is_null() {
            return;
        }
        // The retrieve thread locks pool bitstreams: joined before any of them is destroyed.
        self.stop();
        for &bs in &self.bitstreams {
            // SAFETY: a pool bitstream of the live session, destroyed once: dropped below.
            let _ = unsafe { (self.api.destroy_bitstream_buffer)(self.encoder, bs) };
        }
        destroy(self.encoder);
        self.live = Live::default();
    }
}

/// One frame's long-term step: the slot this frame marks, and the `(slot, wire)` it uses.
type LtrStep = (Option<usize>, Option<(usize, i64)>);

/// Session-side calls need an open session; a backend that skipped its open fails here.
fn ensure_open(enc: *mut c_void) -> Result<()> {
    if enc.is_null() {
        bail!("NVENC session call before the session opened");
    }
    Ok(())
}

#[cfg(test)]
mod ladder_tests {
    use super::{open_at_ceiling, Opened, DISABLE};
    use crate::nvenc_status::call_err;
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::{NVENCSTATUS, NV_ENC_SPLIT_ENCODE_MODE as M};
    use std::cell::RefCell;

    const TWO: u32 = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

    /// What the driver returns above the codec level's bitrate.
    fn reject() -> anyhow::Error {
        call_err("initialize_encoder", NVENCSTATUS::NV_ENC_ERR_INVALID_PARAM)
    }

    /// A fake driver that opens at or below `ceiling` bps and rejects any split unless
    /// `split_ok`. Checks the ladder leaves exactly the returned session live.
    fn run(ceiling: u64, split_ok: bool, requested: u64, cached: Option<u64>) -> Opened<u64> {
        let live = RefCell::new(Vec::new());
        let opened = open_at_ceiling(
            requested,
            TWO,
            cached,
            |bps, split| {
                if bps > ceiling || (split != DISABLE && !split_ok) {
                    return Err(reject());
                }
                live.borrow_mut().push(bps);
                Ok(bps)
            },
            |h| live.borrow_mut().retain(|&l| l != h),
        )
        .expect("the ladder opens");
        assert_eq!(
            *live.borrow(),
            [opened.handle],
            "one live session: the returned one"
        );
        opened
    }

    #[test]
    fn a_rate_the_gpu_accepts_opens_as_asked() {
        let o = run(800_000_000, true, 300_000_000, None);
        assert_eq!((o.bps, o.split_mode, o.ceiling), (300_000_000, TWO, None));
    }

    #[test]
    fn an_overshoot_bisects_to_a_proven_ceiling() {
        let o = run(300_000_000, true, 800_000_000, None);
        assert_eq!(o.split_mode, TWO);
        assert!((280_000_000..=300_000_000).contains(&o.bps), "{}", o.bps);
        assert_eq!(o.ceiling, Some(o.bps));
    }

    #[test]
    fn a_rejected_split_opens_split_disabled_at_the_request() {
        let o = run(800_000_000, false, 300_000_000, None);
        assert_eq!(
            (o.bps, o.split_mode, o.ceiling),
            (300_000_000, DISABLE, None)
        );
    }

    /// Dropping split is what opened the floor, so the floor says nothing about the bitrate
    /// this GPU accepts: caching it would pin every later open of the key at 10 Mbps.
    #[test]
    fn a_split_fallback_at_the_floor_caches_no_ceiling() {
        let o = run(10_000_000, false, 800_000_000, None);
        assert_eq!(
            (o.bps, o.split_mode, o.ceiling),
            (10_000_000, DISABLE, None)
        );
    }

    /// The cache is advisory: a stale ceiling that no longer opens retries the request.
    #[test]
    fn a_stale_cached_ceiling_retries_the_request() {
        let asked = RefCell::new(Vec::new());
        let o = open_at_ceiling(
            500_000_000,
            TWO,
            Some(100_000_000),
            |bps, _| {
                asked.borrow_mut().push(bps);
                if bps == 100_000_000 {
                    return Err(reject());
                }
                Ok(bps)
            },
            |_| {},
        )
        .expect("the request opens");
        assert_eq!(o.bps, 500_000_000);
        assert_eq!(*asked.borrow(), [100_000_000, 500_000_000]);
    }

    /// Only a param rejection means "above the ceiling": anything else ends the search and
    /// releases the session the bisection held.
    #[test]
    fn a_transient_failure_mid_search_propagates_and_releases() {
        let live = RefCell::new(Vec::new());
        let res = open_at_ceiling(
            800_000_000,
            DISABLE,
            None,
            |bps, _| match bps {
                b if b > 300_000_000 => Err(reject()),
                b if b >= 250_000_000 => Err(call_err(
                    "initialize_encoder",
                    NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY,
                )),
                b => {
                    live.borrow_mut().push(b);
                    Ok(b)
                }
            },
            |h| live.borrow_mut().retain(|&l| l != h),
        );
        assert!(res.is_err());
        assert!(live.borrow().is_empty(), "the held session was destroyed");
    }
}

/// The session's own arbitration (`invalidate_ref_frames`, `encode`, the long-term step) on a
/// driver that accepts every call, held to the NVENC table and model in `rfi::plan_tests`.
#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::rfi::plan_tests::{check, nvenc, sweep, Au, Model, NVENC, NVENC_NO_WAVE};
    use crate::Acked;

    macro_rules! accept {
        ($($name:ident($($arg:ty),*);)*) => {$(
            unsafe extern "C" fn $name($(_: $arg),*) -> nv::NVENCSTATUS {
                nv::NVENCSTATUS::NV_ENC_SUCCESS
            }
        )*};
    }
    accept! {
        open(*mut nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, *mut *mut c_void);
        init(*mut c_void, *mut nv::NV_ENC_INITIALIZE_PARAMS);
        reconfigure(*mut c_void, *mut nv::NV_ENC_RECONFIGURE_PARAMS);
        destroy(*mut c_void);
        caps(*mut c_void, nv::GUID, *mut nv::NV_ENC_CAPS_PARAM, *mut core::ffi::c_int);
        guid_count(*mut c_void, *mut u32);
        guids(*mut c_void, *mut nv::GUID, u32, *mut u32);
        preset(
            *mut c_void, nv::GUID, nv::GUID, nv::NV_ENC_TUNING_INFO, *mut nv::NV_ENC_PRESET_CONFIG
        );
        create_bitstream(*mut c_void, *mut nv::NV_ENC_CREATE_BITSTREAM_BUFFER);
        release(*mut c_void, *mut c_void);
        lock(*mut c_void, *mut nv::NV_ENC_LOCK_BITSTREAM);
        register(*mut c_void, *mut nv::NV_ENC_REGISTER_RESOURCE);
        map(*mut c_void, *mut nv::NV_ENC_MAP_INPUT_RESOURCE);
        invalidate(*mut c_void, u64);
    }
    #[cfg(windows)]
    accept! { event(*mut c_void, *mut nv::NV_ENC_EVENT_PARAMS); }
    #[cfg(target_os = "linux")]
    accept! { streams(*mut c_void, *mut c_void, *mut c_void); }

    thread_local! {
        /// Pictures the stub driver refuses before it takes one, on this test's thread.
        static REFUSE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    unsafe extern "C" fn picture(_: *mut c_void, _: *mut nv::NV_ENC_PIC_PARAMS) -> nv::NVENCSTATUS {
        match REFUSE.get() {
            0 => nv::NVENCSTATUS::NV_ENC_SUCCESS,
            n => {
                REFUSE.set(n - 1);
                nv::NVENCSTATUS::NV_ENC_ERR_GENERIC
            }
        }
    }

    static API: EncodeApi = EncodeApi {
        open_encode_session_ex: open,
        initialize_encoder: init,
        reconfigure_encoder: reconfigure,
        destroy_encoder: destroy,
        get_encode_caps: caps,
        get_encode_guid_count: guid_count,
        get_encode_guids: guids,
        get_encode_preset_config_ex: preset,
        create_bitstream_buffer: create_bitstream,
        destroy_bitstream_buffer: release,
        lock_bitstream: lock,
        unlock_bitstream: release,
        register_resource: register,
        unregister_resource: release,
        map_input_resource: map,
        unmap_input_resource: release,
        encode_picture: picture,
        #[cfg(windows)]
        register_async_event: event,
        #[cfg(windows)]
        unregister_async_event: event,
        invalidate_ref_frames: invalidate,
        #[cfg(target_os = "linux")]
        set_io_cuda_streams: streams,
    };

    /// A session open on the stub driver. `height` in 32-px rows is the wave's cycle at 60 fps.
    fn stub_session(codec: Codec, height: u32) -> NvSession {
        let mut s = NvSession::new(&API, codec, 128, height, 60, 1_000_000, 1);
        s.encoder = std::ptr::NonNull::dangling().as_ptr();
        s.bitstreams = vec![std::ptr::null_mut(); POOL];
        s.rfi_supported = true;
        s.ltr_frames = 2;
        s
    }

    /// Encode one frame and return what it was queued as.
    fn encode(s: &mut NvSession) -> Result<&PendingEncode> {
        let input = EncodeInput {
            reg: std::ptr::null_mut(),
            pitch: 0,
            event: std::ptr::null_mut(),
            pts_ns: 0,
            hold: None,
        };
        // SAFETY: the session is open on the stub driver, which reads none of its arguments.
        unsafe { s.encode(input) }?;
        Ok(s.pending.back().expect("the frame is queued"))
    }

    /// The real session behind the table's events. The range plan and the client's
    /// confirmations are the event, as in the model: `a` arms the anchor, `+` keeps a
    /// confirmed picture resident.
    struct Session {
        s: NvSession,
        floor: bool,
    }

    impl Model for Session {
        fn event(&mut self, e: u8) -> Option<Au> {
            let s = &mut self.s;
            match e {
                b'k' => s.force_kf = true,
                b'+' | b'-' => self.floor = e == b'+',
                // No session before the first frame: the caller keyframes.
                b'a' | b'l' if s.frame_idx == 0 => {}
                b'l' => {
                    s.invalidate_ref_frames(0, 0);
                }
                b'a' if s.wave.is_some() => {
                    s.invalidate_ref_frames(s.frame_idx - 1, s.frame_idx - 1);
                }
                b'a' => s.pending_anchor = true,
                _ => return Some(self.frame()),
            }
            None
        }
    }

    impl Session {
        fn frame(&mut self) -> Au {
            let s = &mut self.s;
            let last = s.frame_idx - 1;
            s.reference_floor = self.floor.then_some(Acked {
                last,
                mask: u16::MAX,
            });
            if self.floor {
                s.ltr_slots = vec![Some(last), None];
            }
            let (forced, armed, before) = (s.force_kf, s.pending_anchor, s.wave);
            let p = encode(s).expect("the stub driver accepts every picture");
            let wave = before.filter(|_| !forced);
            Au {
                refs: match (p.idr_hint, p.anchor, armed) {
                    (true, ..) => FrameRef::Idr,
                    (_, true, true) => FrameRef::Anchor,
                    (_, true, false) => FrameRef::Acked,
                    _ => FrameRef::Chain,
                },
                wave,
                mark: p.mark,
                // There is no per-picture bit; a close is clean when it carries its mark.
                dirty: wave.is_some_and(|w| !w.closes() || p.mark != WaveMark::Close),
            }
        }
    }

    /// Model and session at one cycle: the height in 32-px rows, or AV1 for no wave.
    fn pair(cycle: Option<u32>) -> (impl Model, Session) {
        let s = match cycle {
            Some(rows) => stub_session(Codec::H265, rows * 32),
            None => stub_session(Codec::Av1, 128),
        };
        assert_eq!(s.wave_cycle(), cycle.unwrap_or(0));
        (nvenc(cycle).0, Session { s, floor: false })
    }

    /// A wave's start frame the driver refuses: the wave must not move on without its sweep,
    /// and the forced IDR, the anchor and the long-term slots are not spent either.
    #[test]
    fn a_refused_picture_spends_nothing() {
        let mut s = stub_session(Codec::H265, 128);
        encode(&mut s).expect("the opening IDR");
        assert!(s.invalidate_ref_frames(0, 0), "no anchor: a wave is armed");
        let state = |s: &NvSession| (s.frame_idx, s.wave, s.force_kf, s.pending_anchor, s.next);
        let before = state(&s);
        REFUSE.set(1);
        assert!(encode(&mut s).is_err());
        assert_eq!(state(&s), before);
        let retry = encode(&mut s).expect("the driver takes the retry");
        assert_eq!(retry.mark, WaveMark::Start, "the retry starts the wave");

        s.force_kf = true;
        s.pending_anchor = true;
        s.distrusted = true;
        REFUSE.set(1);
        assert!(encode(&mut s).is_err());
        assert!(s.force_kf && s.pending_anchor && s.distrusted);
        assert!(encode(&mut s).expect("the retry").idr_hint);
        assert!(!s.force_kf && !s.pending_anchor && !s.distrusted);

        // A plain frame under confirmed references marks a long-term slot.
        s.reference_floor = Some(Acked {
            last: s.frame_idx - 1,
            mask: u16::MAX,
        });
        REFUSE.set(1);
        assert!(encode(&mut s).is_err());
        assert_eq!(
            s.ltr_slots,
            [None, None],
            "no slot holds a picture that was never queued"
        );
    }

    /// Right after an IDR no long-term slot is held, so the short-term window still reaches
    /// the IDR, and a loss reported two frames late anchors there instead of waving.
    #[test]
    fn a_loss_after_the_idr_anchors_on_it() {
        let mut s = stub_session(Codec::H265, 128);
        for _ in 0..3 {
            encode(&mut s).expect("the stub driver accepts every picture");
        }
        assert!(s.invalidate_ref_frames(1, 1));
        assert!(
            s.wave.is_none() && s.pending_anchor,
            "an RFI anchor, not a wave"
        );
    }

    /// Confirmations trail by six frames, and a loss is reported past the short-term window:
    /// the kept long-term picture the client confirmed anchors the next frame, not a wave.
    #[test]
    fn a_late_loss_anchors_on_the_kept_picture() {
        let mut s = stub_session(Codec::H265, 128);
        for _ in 0..40 {
            s.confirmed = Some(Acked {
                last: s.frame_idx - 6,
                mask: u16::MAX,
            });
            encode(&mut s).expect("the stub driver accepts every picture");
        }
        let lost = s.frame_idx - 4;
        assert!(s.invalidate_ref_frames(lost, lost));
        assert!(s.wave.is_none(), "a kept picture answers, not a wave");
        assert!(encode(&mut s).expect("the anchor").anchor);
        assert!(
            s.ltr_slots
                .iter()
                .flatten()
                .all(|&w| w < lost || w == s.frame_idx - 1),
            "no slot holds a picture that chained from the loss"
        );
    }

    /// A retrieve loop that takes a job and never answers: the blocking lock on a wedged driver.
    fn stuck(_: usize, jobs: mpsc::Receiver<RetrieveJob>, _done: mpsc::Sender<RetrieveDone>) {
        let _job = jobs.recv();
        loop {
            std::thread::park();
        }
    }

    fn answers(_: usize, jobs: mpsc::Receiver<RetrieveJob>, done: mpsc::Sender<RetrieveDone>) {
        for job in jobs {
            let result = Ok((Vec::new(), false));
            let _ = done.send(RetrieveDone { bs: job.bs, result });
        }
    }

    /// A stuck retrieve thread costs the stop its limit, not the encode thread: the session
    /// is forgotten undestroyed. A thread that answers is joined and the session stays open.
    #[test]
    fn a_stuck_retrieve_thread_is_left_behind() {
        let mut s = stub_session(Codec::H265, 128);
        s.start_retrieve("pf-test-stuck", stuck).expect("spawn");
        encode(&mut s).expect("queued behind the stuck lock");
        let t0 = Instant::now();
        assert!(!s.stop_within(Duration::from_millis(50)));
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert!(s.handle().is_null() && s.pending().is_empty() && !s.retrieving());

        let mut s = stub_session(Codec::H265, 128);
        s.start_retrieve("pf-test-answers", answers).expect("spawn");
        encode(&mut s).expect("queued");
        assert!(s.stop_within(Duration::from_secs(5)));
        assert!(!s.handle().is_null() && !s.retrieving());
    }

    /// `close` drops what the session owned and keeps what the next open needs: the wire
    /// index, an owed IDR, the confirmed floor and the sub-frame verdict.
    #[test]
    fn close_drops_the_session_and_keeps_the_settings() {
        let mut s = stub_session(Codec::H265, 128);
        encode(&mut s).expect("the opening IDR");
        assert!(s.invalidate_ref_frames(0, 0), "a wave is armed");
        encode(&mut s).expect("the wave's start");
        (s.pending_anchor, s.distrusted, s.session_async) = (true, true, true);
        (s.force_kf, s.subframe_broken) = (true, true);
        s.close(|_| ());
        assert!(s.handle().is_null() && !s.inited() && s.opening());
        assert!(s.pending().is_empty() && s.bitstreams().is_empty());
        assert!(s.wave.is_none() && s.wave_span.is_none() && s.ltr_slots.is_empty());
        assert!(!s.pending_anchor && !s.distrusted && !s.session_async);
        assert_eq!(s.frame_idx, 2, "the next open continues the wire index");
        assert!(s.force_kf && s.subframe_broken);
    }

    /// The client's slice count is the encoded count; AV1 has tiles, not slices. A
    /// single-slice client never gets split encode, which emits a slice per engine.
    #[test]
    fn the_session_slices_as_the_client_asked() {
        let shape = |codec, client| {
            let mut s = NvSession::new(&API, codec, 3840, 2160, 120, 80_000_000, client);
            let split = s.resolve_shape();
            (s.slices, split)
        };
        assert_eq!(shape(Codec::H265, 1), (1, DISABLE));
        assert_eq!(shape(Codec::H265, 8).0, 8);
        assert_eq!(shape(Codec::H264, 2).0, 2);
        assert_eq!(shape(Codec::Av1, 8).0, 1);
        assert_eq!(shape(Codec::H265, 0), (1, DISABLE));
        assert_eq!(shape(Codec::H265, 64).0, 32);
    }

    #[test]
    fn the_session_follows_the_nvenc_table() {
        check(NVENC, || pair(Some(4)));
        check(NVENC_NO_WAVE, || pair(None));
        for cycle in [None, Some(2), Some(3)] {
            sweep(|| pair(cycle));
        }
    }
}
