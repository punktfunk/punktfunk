//! In-driver encode: the AU section the driver's encode thread publishes into, delivered with
//! `IOCTL_SET_ENCODE`, and the [`Encoder`] the stream loop drives — every control call
//! forwarded over `IOCTL_ENCODE_CTL`. The only Windows video transport.
//!
//! Sealing and handle duplication follow `open.rs` / `channel.rs`: unnamed, SYSTEM-only DACL,
//! duplicated into WUDFHost by a [`ChannelBroker`], adopt-on-success. The reader over the
//! mapped section is [`crate::au_reader`].

use super::*;
use crate::au_reader::{AuReader, AuView, Taken};
use crate::{DriverEndpoint, EncodeCtlSender, SetEncodeSender};
use pf_driver_proto::encode::{self, au, EncodeCtlRequest, SetEncodeRequest};
use pf_encode_win::{AuChunk, EncodedFrame, Encoder, EncoderCaps};

/// What the host asks the driver to open — the session plan as `SET_ENCODE` numbers it.
#[derive(Clone, Copy, Debug)]
pub struct DriverEncodeParams {
    /// 1 H264, 2 HEVC, 3 AV1, 4 PyroWave.
    pub codec: u32,
    /// `0` 4:2:0, `1` 4:4:4.
    pub chroma: u32,
    pub bit_depth: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub hdr: bool,
    pub hdr_meta: Option<pf_frame::HdrMeta>,
    /// Slice-chunk target; `0` = whole access units.
    pub wire_chunk_bytes: u32,
    /// Ordered preference, 0-terminated: 1 NVENC, 2 AMF, 3 QSV, 4 PyroWave.
    pub backends: [u32; 4],
    /// First `wire_seq` the driver stamps — the host's current `au_seq`.
    pub wire_seq_base: u32,
}

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

fn backend_name(b: u32) -> &'static str {
    pf_driver_proto::encode::backend::name(b).unwrap_or("?")
}

fn nul_tag(name: &[u8; 32]) -> String {
    let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    String::from_utf8_lossy(&name[..end]).into_owned()
}

/// `pf_frame::HdrMeta` as the 28 bytes `SET_ENCODE` and `SET_HDR_META` carry: its `repr(C)`
/// image, all zero for `None`.
fn hdr_meta_bytes(meta: Option<pf_frame::HdrMeta>) -> [u8; 28] {
    match meta {
        // SAFETY: `HdrMeta` is `repr(C)`, 28 bytes of integers with no padding, so every byte
        // of the image is initialised.
        Some(m) => unsafe { std::mem::transmute::<pf_frame::HdrMeta, [u8; 28]>(m) },
        None => [0; 28],
    }
}

fn caps_from_wire(w: &encode::EncoderCapsWire) -> EncoderCaps {
    EncoderCaps {
        supports_rfi: w.supports_rfi != 0,
        chroma_444: w.chroma_444 != 0,
        intra_refresh: w.intra_refresh != 0,
        intra_refresh_recovery: w.intra_refresh_recovery != 0,
        intra_refresh_period: w.intra_refresh_period,
        blends_cursor: w.blends_cursor != 0,
        // The driver encodes what DWM composes at the monitor's own size.
        downscales_input: false,
        crops_input: false,
    }
}

/// A driver `qpc_pts` as the wire's epoch-nanosecond `pts_ns`: now minus the stamp's age.
/// `0` (no stamp) reads as now.
fn pts_from_qpc(qpc: u64) -> u64 {
    if qpc == 0 {
        return now_ns();
    }
    now_ns().saturating_sub(IddPushCapturer::qpc_age_us(qpc).saturating_mul(1000))
}

/// Access units to disk while [`diag_dir`] is on, in the framing the S5 encode probe wrote —
/// `[u32 len][u64 pts_ns][u8 keyframe][bytes]` per chunk — so a field failure replays through
/// the same corpus tooling. A write error drops the dump; the stream never fails for it.
struct AuDump(std::io::BufWriter<std::fs::File>);

impl AuDump {
    /// One file per encoder open, named by pid and target so two sessions never share one.
    fn create(target_id: u32) -> Option<Self> {
        let dir = diag_dir()?;
        let path = dir.join(format!("pfvd-au-{}-{target_id}.bin", std::process::id()));
        match std::fs::File::create(&path) {
            Ok(f) => {
                tracing::info!(path = %path.display(), "driver encode: dumping access units");
                Some(Self(std::io::BufWriter::new(f)))
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "driver encode: no AU dump");
                None
            }
        }
    }

    /// Append one chunk; `false` means the caller drops the dump.
    fn write(&mut self, c: &AuChunk) -> bool {
        use std::io::Write;
        let r = self
            .0
            .write_all(&(c.data.len() as u32).to_le_bytes())
            .and_then(|()| self.0.write_all(&c.pts_ns.to_le_bytes()))
            .and_then(|()| self.0.write_all(&[u8::from(c.keyframe)]))
            .and_then(|()| self.0.write_all(&c.data));
        if let Err(e) = r {
            tracing::warn!(error = %e, "driver encode: AU dump write failed — dump disabled");
            return false;
        }
        true
    }
}

/// The mapped AU section and its ready event, alive for the encoder's life. The driver holds
/// its own duplicates; dropping this unmaps and closes only the host's handles.
struct AuSection {
    section: MappedSection,
    event: OwnedHandle,
    bytes: usize,
}

impl AuSection {
    /// Create the sealed section + auto-reset event and stamp the header, magic last.
    ///
    /// `generation` is the DRIVER's word: `EncodeSession::new` stores its own over whatever
    /// is here, and `published_for_us` reads that. The host does not seed it.
    fn create(heap_bytes: u32, wire_seq_base: u32) -> Result<Self> {
        let bytes = au::section_bytes(heap_bytes) as usize;
        // SAFETY: as the ring's section in `open.rs`: every create is `?`-checked, `sa` lives
        // across them, the view is `bytes` long and page-aligned, and the header write stays
        // inside it with magic stored last, Release, so the driver sees a whole header.
        unsafe {
            let sa = open::SharedObjectSa::new()?;
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa.as_ptr()),
                PAGE_READWRITE,
                0,
                bytes as u32,
                PCWSTR::null(),
            )
            .context("CreateFileMapping(AU section)")?;
            let map = OwnedHandle::from_raw_handle(map.0 as _);
            let view = MapViewOfFile(
                HANDLE(map.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                bytes,
            );
            if view.Value.is_null() {
                bail!("MapViewOfFile failed for the AU section");
            }
            let section = MappedSection { handle: map, view };
            let header = section.ptr::<au::AuHeader>();
            std::ptr::write_bytes(header.cast::<u8>(), 0, bytes);
            (*header).version = au::AU_VERSION;
            (*header).heap_offset = au::HEAP_OFFSET as u32;
            (*header).heap_bytes = heap_bytes;
            (*header).slot_table_offset = au::SLOT_TABLE_OFFSET as u32;
            (*header).slot_count = au::AU_SLOTS;
            (*header).wire_seq_base = wire_seq_base;
            let event = CreateEventW(Some(sa.as_ptr()), false, false, PCWSTR::null())
                .context("CreateEvent(AU section)")?;
            let event = OwnedHandle::from_raw_handle(event.0 as _);
            std::sync::atomic::fence(Ordering::Release);
            (*(std::ptr::addr_of!((*header).magic) as *const AtomicU32))
                .store(au::AU_MAGIC, Ordering::Release);
            Ok(Self {
                section,
                event,
                bytes,
            })
        }
    }

    fn view(&self) -> AuView {
        // SAFETY: the mapping is `bytes` long and page-aligned, and outlives the view because
        // the proxy owns both.
        unsafe { AuView::new(self.section.ptr::<u8>(), self.bytes) }
    }

    /// Wait for a publish, at most `timeout_ms`. The auto-reset event is the wakeup only; the
    /// header is the truth, so a consumed signal is never a lost chunk.
    fn wait(&self, timeout_ms: u32) {
        // SAFETY: `event` is this section's live handle; the bounded wait only reads it.
        let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), timeout_ms) };
    }
}

/// Open the driver's encoder for `endpoint` and hand back the stream loop's [`Encoder`].
/// Every open re-raises the WUDFHost's GPU scheduling class, which a driver reload resets.
/// Structured failure: [`DriverEncodeOpenError`] when the driver walked the list and none
/// opened, else the delivery error. There is no fallback.
pub fn open_driver_encoder(
    endpoint: DriverEndpoint,
    params: &DriverEncodeParams,
    set_encode: SetEncodeSender,
    encode_ctl: EncodeCtlSender,
) -> Result<Box<dyn Encoder>> {
    let heap = au::heap_bytes_for(params.bitrate_kbps, params.fps);
    let section = AuSection::create(heap, params.wire_seq_base)?;
    let broker = ChannelBroker::open(endpoint.wudf_pid)?;
    broker.raise_gpu_priority();
    // SAFETY: both handles are live members of `section`, borrowed for the duplication.
    let (section_v, event_v) = unsafe {
        let s = broker.dup_into(
            HANDLE(section.section.handle.as_raw_handle()),
            Some(SECTION_MAP_RW),
        )?;
        match broker.dup_into(
            HANDLE(section.event.as_raw_handle()),
            Some(EVENT_MODIFY_STATE),
        ) {
            Ok(e) => (s, e),
            Err(e) => {
                broker.close_remote(s);
                return Err(e);
            }
        }
    };
    let req = SetEncodeRequest {
        target_id: endpoint.target_id,
        _pad: 0,
        section: section_v,
        event: event_v,
        section_bytes: section.bytes as u32,
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
        flags: 0,
        _pad_tail: 0,
        // The host process carries `host.env`, so its environment is the knob source; the
        // driver reads these instead of WUDFHost's own (machine) environment.
        knobs: pf_encode_win::knobs::dev_override::from_env(),
    };
    let reply = match set_encode(&req) {
        Ok(r) => r,
        Err(e) => {
            // A failed IOCTL adopted nothing: the duplicates are ours to reap. One that
            // completed with a short reply did adopt them, and closing would double-close.
            if e.downcast_ref::<encode::ReplyTooShort>().is_none() {
                broker.close_remote(section_v);
                broker.close_remote(event_v);
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
    let view = section.view();
    let header = view.header();
    if !au::au_readable(&header) {
        // SET_ENCODE already succeeded, so the driver holds an open encode session — and no
        // `EncoderProxy` exists yet to close it on drop. Send the same CLOSE that `Drop` does,
        // or the session stays open on a section nobody will ever drain.
        let _ = encode_ctl(&EncodeCtlRequest {
            target_id: endpoint.target_id,
            op: encode::ENCODE_CTL_CLOSE,
            arg0: header.generation,
            arg1: 0,
            payload: [0; 28],
        });
        bail!("AU section header failed its layout gate after SET_ENCODE: {header:?}");
    }
    let caps = caps_from_wire(&reply.caps);
    tracing::info!(
        target_id = endpoint.target_id,
        backend = backend_name(reply.backend_opened),
        ?caps,
        applied_kbps = reply.applied_bitrate_kbps,
        heap_bytes = heap,
        generation = header.generation,
        wire_seq_base = params.wire_seq_base,
        "driver encode: encoder open, AU section adopted"
    );
    Ok(Box::new(EncoderProxy {
        reader: AuReader::new(view, params.wire_seq_base),
        section,
        target_id: endpoint.target_id,
        ctl: encode_ctl,
        caps,
        applied_bps: u64::from(reply.applied_bitrate_kbps) * 1000,
        retarget_after: 0,
        hdr_meta: params.hdr_meta,
        wire_chunk: params.wire_chunk_bytes as usize,
        wire_chunk_warned: false,
        last_wire_seq: 0,
        last_source_seq: 0,
        last_arrival: None,
        last_split: None,
        dump: AuDump::create(endpoint.target_id),
        opened_at: Instant::now(),
        backend: backend_name(reply.backend_opened),
    }))
}

/// The driver's stamps on one slot as a split. `None` from a driver that wrote none (a zero
/// submit) or stamps out of order. A present stamp of `0`, or one past the submit (a head that
/// names the vblank the frame is for), leaves the pool wait unmeasured rather than negative.
fn driver_split(t: &Taken) -> Option<pf_frame::health::DriverSplit> {
    if t.qpc_submit == 0 || t.qpc_published < t.qpc_submit {
        return None;
    }
    let span = |ticks: u64| Duration::from_micros(IddPushCapturer::qpc_ticks_us(ticks));
    Some(pf_frame::health::DriverSplit {
        pool: (t.qpc_pts != 0 && t.qpc_submit >= t.qpc_pts).then(|| span(t.qpc_submit - t.qpc_pts)),
        encode: span(t.qpc_published - t.qpc_submit),
        ipc: Duration::from_micros(IddPushCapturer::qpc_age_us(t.qpc_published)),
    })
}

/// The stream loop's [`Encoder`] for a driver-encoded session. Nothing is submitted: the
/// driver encodes what DWM composes, and [`Encoder::ready_aus`] tells the loop how many
/// access units it owes wire indexes for. Every control call is one synchronous
/// `ENCODE_CTL`; `set_hdr_meta` only on a change, since the loop repeats it every tick.
pub struct EncoderProxy {
    reader: AuReader,
    section: AuSection,
    target_id: u32,
    ctl: EncodeCtlSender,
    caps: EncoderCaps,
    /// The rate this side last asked for. It stands until the driver has published past
    /// [`Self::retarget_after`], because its stamp until then describes the rate before the ask.
    applied_bps: u64,
    /// `published_total` when the last bitrate ctl was queued.
    retarget_after: u64,
    hdr_meta: Option<pf_frame::HdrMeta>,
    /// Decided at `SET_ENCODE`; a later `set_wire_chunking` that differs is logged once.
    wire_chunk: usize,
    wire_chunk_warned: bool,
    /// The last taken chunk's `wire_seq` / `source_seq`: AU progress, the supervisor's second
    /// ground-truth clock (`progress`).
    last_wire_seq: u32,
    last_source_seq: u32,
    /// Age of the last taken chunk's `qpc_pts` when the host took it — present→arrival.
    last_arrival: Option<Duration>,
    /// The driver's own split of that span, when its slot carried the stamps.
    last_split: Option<pf_frame::health::DriverSplit>,
    /// Bitstream capture while `PUNKTFUNK_IDD_DIAG` is on; dropped on the first write error.
    dump: Option<AuDump>,
    /// Stands in for `last_au_qpc` until the first publish, so a never-producing encoder is
    /// silent from a known instant rather than invisible.
    opened_at: Instant,
    /// The backend the driver opened, for the status surface.
    backend: &'static str,
}

// SAFETY: `!Send` only through the mapping's raw pointers. Built on the prep thread, used on
// the stream thread, one owner at a time; the driver's writes arrive through atomics.
unsafe impl Send for EncoderProxy {}

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
    /// A slice never takes this long; past it the access unit is truncated and the loop's
    /// stall path resets the encoder.
    const CHUNK_WAIT: Duration = Duration::from_millis(500);

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

    /// One taken slot as a wire chunk, recording the progress clocks on the way through:
    /// the sequence pair the supervisor reads as encoder progress, and how old the OS present
    /// stamp already is — the ground truth for "late, not missing". The diagnostic dump takes
    /// its copy here, so a chunked session writes the same bytes in the same order.
    fn chunk(&mut self, t: Taken) -> AuChunk {
        self.last_wire_seq = t.wire_seq;
        self.last_source_seq = t.source_seq;
        self.last_arrival =
            (t.qpc_pts != 0).then(|| Duration::from_micros(IddPushCapturer::qpc_age_us(t.qpc_pts)));
        self.last_split = driver_split(&t);
        let chunk = AuChunk {
            data: t.data,
            pts_ns: pts_from_qpc(t.qpc_pts),
            keyframe: t.flags & au::AU_KEYFRAME != 0,
            recovery_anchor: t.flags & au::AU_RECOVERY_ANCHOR != 0,
            recovery_point: t.flags & au::AU_RECOVERY_POINT != 0,
            recovery_close: t.flags & au::AU_RECOVERY_CLOSE != 0,
            chunk_aligned: t.flags & au::AU_CHUNK_ALIGNED != 0,
            first: t.flags & au::AU_FIRST != 0,
            last: t.flags & au::AU_LAST != 0,
        };
        if self.dump.as_mut().is_some_and(|d| !d.write(&chunk)) {
            self.dump = None;
        }
        chunk
    }

    /// The header telemetry the driver keeps (`encoder_state`, `detached`, `last_au_qpc`,
    /// `drain_heartbeat_qpc`, `source_seq`, …); [`Encoder::telemetry`] is its supervisor cut.
    pub fn snapshot(&self) -> au::AuHeader {
        self.reader.view().header()
    }

    /// `(wire_seq, source_seq)` of the last chunk taken: AU progress against the header's
    /// `source_seq`, the "encoder wedged" signal (§2.5) the supervisor reads from Phase 4 on.
    #[allow(dead_code)]
    pub fn progress(&self) -> (u32, u32) {
        (self.last_wire_seq, self.last_source_seq)
    }
}

impl Encoder for EncoderProxy {
    fn submit(&mut self, _frame: &CapturedFrame) -> Result<()> {
        // Unreachable by the loop: `ready_aus` answers `Some`, so it owes records instead of
        // submitting. A caller that lands here is a bug worth one visible reset.
        bail!("driver encode: the driver encodes; nothing to submit")
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
        if meta == self.hdr_meta {
            return;
        }
        match self.ctl(encode::ENCODE_CTL_SET_HDR_META, 0, 0, hdr_meta_bytes(meta)) {
            Ok(()) => self.hdr_meta = meta,
            Err(e) => tracing::warn!(
                target_id = self.target_id,
                error = %format!("{e:#}"),
                "driver encode: HDR metadata update refused"
            ),
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

    fn distrust_references(&mut self) {
        self.ctl_logged(
            "reference distrust",
            encode::ENCODE_CTL_DISTRUST_REFERENCES,
            0,
            0,
        );
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Whole access units: chunks concatenate in order until LAST closes it.
        let Some(first) = self.poll_chunk()? else {
            return Ok(None);
        };
        let mut frame = EncodedFrame {
            data: first.data,
            pts_ns: first.pts_ns,
            keyframe: first.keyframe,
            recovery_anchor: first.recovery_anchor,
            recovery_point: first.recovery_point,
            recovery_close: first.recovery_close,
            chunk_aligned: first.chunk_aligned,
        };
        let mut last = first.last;
        while !last {
            let c = self
                .poll_chunk()?
                .context("driver encode: access unit ended without its LAST chunk")?;
            frame.data.extend_from_slice(&c.data);
            frame.keyframe |= c.keyframe;
            last = c.last;
        }
        Ok(Some(frame))
    }

    fn supports_chunked_poll(&self) -> bool {
        true
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        // Non-blocking between access units; inside one, the next chunk is owed and waited for.
        let deadline = self
            .reader
            .mid_au()
            .then(|| Instant::now() + Self::CHUNK_WAIT);
        loop {
            if let Some(t) = self.reader.take_next()? {
                return Ok(Some(self.chunk(t)));
            }
            let Some(deadline) = deadline else {
                return Ok(None);
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "driver encode: no further chunk of wire frame {} within {:?}",
                    self.reader.next_wire_seq(),
                    Self::CHUNK_WAIT
                );
            }
            self.section.wait(left.as_millis().clamp(1, 16) as u32);
        }
    }

    /// The classifier's two clocks straight from the AU header: the drain worker's heartbeat and
    /// the encoder's publish stamp, both QPC ages turned into local instants.
    fn telemetry(&self) -> Option<pf_frame::health::EncoderTelemetry> {
        let h = self.snapshot();
        let stamp = |qpc: u64| {
            (qpc != 0)
                .then(|| Duration::from_micros(IddPushCapturer::qpc_age_us(qpc)))
                .and_then(|age| Instant::now().checked_sub(age))
        };
        Some(pf_frame::health::EncoderTelemetry {
            // Before the first access unit the encoder's open time stands in, so a
            // never-producing encoder is silent from a known instant.
            last_au: stamp(h.last_au_qpc).unwrap_or(self.opened_at),
            published_total: h.published_total,
            detached: h.detached,
            source_seq: h.source_seq,
            dropped_total: h.dropped_total,
            drain_heartbeat: stamp(h.drain_heartbeat_qpc),
            present_to_arrival: self.last_arrival,
            driver_split: self.last_split,
            state: h.encoder_state,
            backend: self.backend,
        })
    }

    fn ready_aus(&mut self, deadline: Instant) -> Option<usize> {
        loop {
            let n = self.reader.ready_aus();
            if n > 0 {
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
        // The domain restarts where this reader stands, so the loop's `au_seq` keeps matching.
        // A half-read AU already spent its index on the wire: the loop closes that frame at
        // the next FIRST, so the domain restarts past it.
        let base = self
            .reader
            .next_wire_seq()
            .wrapping_add(u32::from(self.reader.mid_au()));
        if !self.ctl_logged("reset", encode::ENCODE_CTL_RESET, base, 0) {
            return false;
        }
        self.reader.rebase(base);
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

    fn flush(&mut self) -> Result<()> {
        self.ctl(encode::ENCODE_CTL_FLUSH, 0, 0, [0; 28])
            .context("driver encode: flush")
    }
}
