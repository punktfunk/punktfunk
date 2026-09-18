//! One-thread capture → encode → playable file. Optional in-process
//! `punktfunk_core` host→client loopback of each AU (FEC, packetize, reassemble).
//!
//! Not the production host path; that is [`crate::pipeline`]. Sources are a
//! synthetic BGRx pattern, a Windows synthetic NV12 GPU texture, the xdg
//! ScreenCast portal, or a platform virtual display.
//!
//! The virtual source is also the readiness gate the multi-seat supervisor runs
//! before it calls a seat healthy, so on Windows it drives the real path: the
//! driver owns the pixels and the encoder, and a run that produces no access
//! unit exits nonzero.

use crate::capture::{self, Capturer, SyntheticCapturer};
use crate::encode::{self, Codec, EncodedFrame, Encoder};
use anyhow::{anyhow, Context, Result};
use punktfunk_core::packet::{FLAG_PIC, FLAG_SOF};
use punktfunk_core::{Config, Role, Session};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Moving BGRx test pattern. No capture session.
    Synthetic,
    /// Windows-only moving NV12 GPU texture. AMF / D3D11 zero-copy encoders need
    /// this; the CPU `Synthetic` source cannot feed them.
    SyntheticNv12,
    /// Live monitor via the xdg ScreenCast portal + PipeWire.
    Portal,
    /// Platform virtual display at `width`×`height`: a compositor output on
    /// Linux, a pf-vdisplay monitor with in-driver encode on Windows.
    Virtual,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub source: Source,
    /// Synthetic / virtual-output size. Portal uses the PipeWire-negotiated size.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub seconds: u32,
    pub codec: Codec,
    /// Ask for HDR capture and a 10-bit encode. The gate the seat supervisor runs.
    pub hdr: bool,
    pub bitrate_bps: u64,
    /// Annex-B elementary-stream path (`.h265`/`.h264`/`.obu`).
    pub out: PathBuf,
    pub loopback: bool,
    /// Shard payload for [`Encoder::set_wire_chunking`]. `None` is one packet per AU.
    /// Set this and `PUNKTFUNK_PYROWAVE_STREAMED_AU=1` to drain via `poll_chunk` and
    /// the streamed-AU loopback; without both, that path cannot run here.
    pub wire_chunk: Option<usize>,
}

pub fn run(opts: Options) -> Result<()> {
    let hdr = opts.hdr || std::env::var("PUNKTFUNK_SPIKE_HDR").as_deref() == Ok("1");
    if hdr && matches!(opts.source, Source::Synthetic | Source::SyntheticNv12) {
        anyhow::bail!("--hdr needs a real source: --source portal or --source virtual");
    }
    if hdr && !opts.codec.supports_10bit() {
        anyhow::bail!("--hdr needs a 10-bit codec (h265, av1 or pyrowave)");
    }
    if hdr && !encode::can_encode_10bit(opts.codec) {
        anyhow::bail!(
            "--hdr asked for {:?} at 10 bits and the resolved encoder cannot do it",
            opts.codec
        );
    }
    plat::check_virtual_source(&opts)?;
    let bit_depth: u8 = if hdr { 10 } else { 8 };

    let mut capturer: Box<dyn Capturer> = match opts.source {
        Source::Synthetic => {
            tracing::info!(
                width = opts.width,
                height = opts.height,
                fps = opts.fps,
                "spike source: synthetic BGRx test pattern"
            );
            Box::new(SyntheticCapturer::new(opts.width, opts.height, opts.fps))
        }
        Source::SyntheticNv12 => plat::open_synthetic_nv12(&opts)?,
        Source::Portal => {
            // `PUNKTFUNK_SPIKE_HDR=1` asks the portal for 10-bit PQ dmabufs (GNOME HDR offer).
            let want_hdr = std::env::var("PUNKTFUNK_SPIKE_HDR").as_deref() == Ok("1");
            tracing::info!(
                want_hdr,
                "spike source: xdg ScreenCast portal (live monitor)"
            );
            // Encoder open passes `cursor_blend = false`, so a metadata pointer would composite nowhere.
            capture::open_portal_monitor(want_hdr, false).context("open portal capturer")?
        }
        Source::Virtual => {
            let compositor = plat::spike_compositor()?;
            tracing::info!(
                width = opts.width,
                height = opts.height,
                hdr,
                ?compositor,
                "spike source: platform virtual display"
            );
            let mut vd = crate::vdisplay::open(compositor).context("open virtual display")?;
            vd.set_hdr(hdr);
            let vout = vd
                .create(punktfunk_core::Mode {
                    width: opts.width,
                    height: opts.height,
                    refresh_hz: opts.fps,
                })
                .context("create virtual output")?;
            // `OutputFormat::resolve` hard-codes `pyrowave: false` (GameStream never
            // negotiates it). On Linux that flag is raw-dmabuf passthrough; overwrite
            // from the spike codec so `--codec pyrowave` does not encode off a
            // non-PyroWave capture.
            let mut want = capture::OutputFormat::resolve(hdr, encode::resolved_backend_is_gpu());
            want.pyrowave = opts.codec == Codec::PyroWave;
            capture::capture_virtual_output(
                vout,
                want,
                crate::session_plan::CaptureBackend::resolve(),
                compositor == crate::vdisplay::Compositor::Kwin,
                compositor == crate::vdisplay::Compositor::Gamescope,
            )
            .context("capture virtual output")?
        }
    };

    // Portal/PipeWire delivers frames only while `active`; idle by default so reconnects are cheap.
    capturer.set_active(true);

    let first = capturer.next_frame().context("capture first frame")?;
    let (w, h) = (first.width, first.height);
    tracing::info!(
        width = w,
        height = h,
        format = ?first.format,
        codec = ?opts.codec,
        bit_depth,
        bitrate_bps = opts.bitrate_bps,
        "opening video encoder"
    );
    let driver_encoder =
        plat::open_driver_encoder(&opts, capturer.as_ref(), (w, h), bit_depth, hdr)?;
    let driver_encode = driver_encoder.is_some();
    let mut encoder = if let Some(enc) = driver_encoder {
        enc
    } else {
        encode::open_video(
            opts.codec,
            first.format,
            w,
            h,
            opts.fps,
            opts.bitrate_bps,
            first.is_cuda(),
            bit_depth,
            encode::ChromaFormat::Yuv420,
            false, // no cursor to blend
            4,     // no client decoder; keep the backend multi-slice default
        )
        .context("open encoder")?
    };

    // Also the gate for `supports_chunked_poll()` (needs `PUNKTFUNK_PYROWAVE_STREAMED_AU=1`).
    if let Some(c) = opts.wire_chunk {
        encoder.set_wire_chunking(c);
        tracing::info!(
            shard_payload = c,
            chunked_poll = encoder.supports_chunked_poll(),
            "spike: wire chunking on (chunked_poll=false means PUNKTFUNK_PYROWAVE_STREAMED_AU \
             is not armed — the AU still goes out whole)"
        );
    }

    let mut sink = BufWriter::new(
        File::create(&opts.out).with_context(|| format!("create {}", opts.out.display()))?,
    );

    let mut lb = if opts.loopback {
        Some(Loopback::new().context("build punktfunk-core loopback")?)
    } else {
        None
    };

    let target_frames = (opts.seconds as u64) * (opts.fps as u64);
    let started = Instant::now();
    let mut stats = Stats::default();

    let mut frame = first;
    let deadline = started + std::time::Duration::from_secs(u64::from(opts.seconds));
    loop {
        // The in-driver encoder is fed by the driver, not by this process: there is nothing to
        // submit and nothing buffered to flush, so the run is bounded by the clock and the host
        // only collects what comes out.
        if !driver_encode {
            encoder.submit(&frame).context("encoder submit")?;
            stats.submitted += 1;
        }
        drain_encoder(encoder.as_mut(), &mut sink, lb.as_mut(), &mut stats)?;
        let done = if driver_encode {
            Instant::now() >= deadline
        } else {
            stats.submitted >= target_frames
        };
        if done {
            break;
        }
        frame = capturer.next_frame().context("capture frame")?;
    }

    if !driver_encode {
        // NVENC buffers frames internally even at delay=0 — flush and drain the tail.
        encoder.flush().context("encoder flush")?;
    }
    drain_encoder(encoder.as_mut(), &mut sink, lb.as_mut(), &mut stats)?;
    sink.flush().context("flush output file")?;

    // The readiness question this command answers: did the whole chain actually produce
    // something? A run that captured and encoded nothing has exited 0 until now.
    if stats.encoded == 0 || stats.bytes_out == 0 {
        anyhow::bail!(
            "capture ran for {}s and the encoder produced no access unit — the display or the \
             encoder is not working on this desktop",
            opts.seconds
        );
    }

    let elapsed = started.elapsed().as_secs_f64();
    tracing::info!(
        submitted = stats.submitted,
        encoded = stats.encoded,
        keyframes = stats.keyframes,
        bytes_out = stats.bytes_out,
        out = %opts.out.display(),
        elapsed_s = format!("{elapsed:.2}"),
        encode_fps = format!("{:.1}", stats.encoded as f64 / elapsed.max(1e-9)),
        // 0 = whole-AU drain; > encoded = streamed drain cut AUs into pieces.
        chunks = stats.chunks,
        chunks_per_au = format!(
            "{:.1}",
            stats.chunks as f64 / (stats.encoded.max(1)) as f64
        ),
        "spike capture→encode→file complete"
    );

    if let Some(lb) = lb {
        lb.report();
        if lb.mismatches > 0 || lb.recovered != lb.submitted {
            return Err(anyhow!(
                "punktfunk-core loopback verification FAILED: {} mismatches, {}/{} AUs recovered",
                lb.mismatches,
                lb.recovered,
                lb.submitted
            ));
        }
    }
    Ok(())
}

#[derive(Default)]
struct Stats {
    submitted: u64,
    encoded: u64,
    keyframes: u64,
    bytes_out: u64,
    /// Streamed-AU drain only. 1 per AU means the cut never engaged.
    chunks: u64,
}

fn drain_encoder(
    encoder: &mut dyn Encoder,
    sink: &mut impl Write,
    mut lb: Option<&mut Loopback>,
    stats: &mut Stats,
) -> Result<()> {
    // Re-query `supports_chunked_poll` each drain; the trait does not promise a stable answer.
    if encoder.supports_chunked_poll() {
        return drain_encoder_chunked(encoder, sink, lb, stats);
    }
    while let Some(au) = encoder.poll().context("encoder poll")? {
        sink.write_all(&au.data).context("write AU to file")?;
        stats.encoded += 1;
        stats.bytes_out += au.data.len() as u64;
        if au.keyframe {
            stats.keyframes += 1;
        }
        if let Some(lb) = lb.as_deref_mut() {
            lb.submit(&au)?;
        }
    }
    Ok(())
}

/// Seal each chunk as it is polled. Concatenate only for the file sink and the
/// loopback byte-compare against what `poll()` would have produced.
fn drain_encoder_chunked(
    encoder: &mut dyn Encoder,
    sink: &mut impl Write,
    mut lb: Option<&mut Loopback>,
    stats: &mut Stats,
) -> Result<()> {
    let mut whole: Vec<u8> = Vec::new();
    let mut chunks = 0u32;
    while let Some(c) = encoder.poll_chunk().context("encoder poll_chunk")? {
        if c.first {
            whole.clear();
            chunks = 0;
            if let Some(lb) = lb.as_deref_mut() {
                lb.streamed_begin(c.pts_ns, c.keyframe)?;
            }
        }
        whole.extend_from_slice(&c.data);
        chunks += 1;
        if let Some(lb) = lb.as_deref_mut() {
            lb.streamed_chunk(&c.data)?;
        }
        if !c.last {
            continue;
        }
        sink.write_all(&whole).context("write AU to file")?;
        stats.encoded += 1;
        stats.bytes_out += whole.len() as u64;
        stats.chunks += chunks as u64;
        if c.keyframe {
            stats.keyframes += 1;
        }
        if let Some(lb) = lb.as_deref_mut() {
            lb.streamed_finish(&whole)?;
        }
    }
    Ok(())
}

/// In-process host↔client `punktfunk_core` pair. Each AU is FEC-protected,
/// packetized, sent, reassembled, and byte-compared to the original.
struct Loopback {
    host: Session,
    client: Session,
    submitted: u64,
    recovered: u64,
    mismatches: u64,
    bytes: u64,
    /// Open streamed AU. `Some` only between `streamed_begin` and `streamed_finish`.
    open: Option<punktfunk_core::packet::StreamedAu>,
    /// Explicit wire-frame index for the streamed path. `submit_frame` uses the
    /// packetizer's counter; `begin_streamed_frame_at` takes this. A session
    /// must use one style.
    next_index: u32,
}

impl Loopback {
    fn new() -> Result<Loopback> {
        let (host_tx, client_tx) = punktfunk_core::transport::loopback_pair(0, 0);
        let host = Session::new(Config::p1_defaults(Role::Host), Box::new(host_tx))
            .map_err(|e| anyhow!("host session: {e:?}"))?;
        let client = Session::new(Config::p1_defaults(Role::Client), Box::new(client_tx))
            .map_err(|e| anyhow!("client session: {e:?}"))?;
        Ok(Loopback {
            host,
            client,
            submitted: 0,
            recovered: 0,
            mismatches: 0,
            bytes: 0,
            open: None,
            next_index: 0,
        })
    }

    /// Client needs no opt-in; a streamed frame surfaces as one `Frame`, same as a whole AU.
    fn streamed_begin(&mut self, pts_ns: u64, keyframe: bool) -> Result<()> {
        if self.open.is_some() {
            return Err(anyhow!(
                "streamed AU still open at begin — a previous AU never sent its `last` chunk"
            ));
        }
        let mut flags = FLAG_PIC as u32;
        if keyframe {
            flags |= FLAG_SOF as u32;
        }
        let idx = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);
        self.open = Some(
            self.host
                .begin_streamed_frame_at(pts_ns, flags, idx)
                .map_err(|e| anyhow!("begin_streamed_frame_at: {e:?}"))?,
        );
        Ok(())
    }

    /// Seal + send one encoder chunk. An empty batch is normal: the sealer waits for a full FEC block.
    fn streamed_chunk(&mut self, data: &[u8]) -> Result<()> {
        let au = self
            .open
            .as_mut()
            .ok_or_else(|| anyhow!("streamed chunk with no open AU"))?;
        let wires = self
            .host
            .seal_streamed_chunk(au, data, false)
            .map_err(|e| anyhow!("seal_streamed_chunk: {e:?}"))?;
        self.send(wires)
    }

    /// Final block carries the real totals. Then verify the client reassembly.
    fn streamed_finish(&mut self, expect: &[u8]) -> Result<()> {
        let au = self
            .open
            .take()
            .ok_or_else(|| anyhow!("streamed finish with no open AU"))?;
        let wires = self
            .host
            .seal_streamed_finish(au)
            .map_err(|e| anyhow!("seal_streamed_finish: {e:?}"))?;
        self.send(wires)?;
        self.submitted += 1;
        self.bytes += expect.len() as u64;
        self.verify(expect)
    }

    fn send(&mut self, wires: Vec<Vec<u8>>) -> Result<()> {
        if wires.is_empty() {
            return Ok(());
        }
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        self.host
            .send_sealed(&refs)
            .map_err(|e| anyhow!("send_sealed: {e:?}"))?;
        drop(refs);
        self.host.reclaim_wires(wires);
        Ok(())
    }

    fn verify(&mut self, expect: &[u8]) -> Result<()> {
        loop {
            match self.client.poll_frame() {
                Ok(frame) => {
                    self.recovered += 1;
                    if frame.data != expect {
                        self.mismatches += 1;
                        tracing::warn!(
                            recovered = self.recovered,
                            got = frame.data.len(),
                            expected = expect.len(),
                            complete = frame.complete,
                            "loopback AU mismatch"
                        );
                    }
                }
                Err(punktfunk_core::PunktfunkError::NoFrame) => break,
                Err(e) => return Err(anyhow!("client poll_frame: {e:?}")),
            }
        }
        Ok(())
    }

    fn submit(&mut self, au: &EncodedFrame) -> Result<()> {
        let mut flags = FLAG_PIC as u32;
        if au.keyframe {
            flags |= FLAG_SOF as u32;
        }
        self.host
            .submit_frame(&au.data, au.pts_ns, flags)
            .map_err(|e| anyhow!("host submit_frame: {e:?}"))?;
        self.submitted += 1;
        self.bytes += au.data.len() as u64;

        // Lossless in-order loopback: each submit yields exactly the AU just sent.
        loop {
            match self.client.poll_frame() {
                Ok(frame) => {
                    self.recovered += 1;
                    if frame.data != au.data {
                        self.mismatches += 1;
                        tracing::warn!(
                            recovered = self.recovered,
                            got = frame.data.len(),
                            expected = au.data.len(),
                            "loopback AU mismatch"
                        );
                    }
                }
                Err(punktfunk_core::PunktfunkError::NoFrame) => break,
                Err(e) => return Err(anyhow!("client poll_frame: {e:?}")),
            }
        }
        Ok(())
    }

    fn report(&self) {
        tracing::info!(
            submitted = self.submitted,
            recovered = self.recovered,
            mismatches = self.mismatches,
            bytes = self.bytes,
            "punktfunk-core loopback: AUs FEC-packetized → sent → reassembled & verified"
        );
    }
}

/// What the spike does differently on Windows: the seat gate (`--source virtual` must reach
/// a GPU encoder, and IDD-push keeps the pixels in the driver), and the NV12 GPU source.
#[cfg(target_os = "windows")]
mod plat {
    use super::*;

    /// The virtual source proves a seat can stream, so a software encoder is a failure
    /// rather than a fallback: the driver is what encodes, and it needs a GPU.
    pub(super) fn check_virtual_source(opts: &Options) -> Result<()> {
        if opts.source == Source::Virtual && !encode::resolved_backend_is_gpu() {
            anyhow::bail!(
                "--source virtual is the Windows seat gate and needs a GPU encoder; the resolved \
                 backend is software"
            );
        }
        Ok(())
    }

    pub(super) fn open_synthetic_nv12(opts: &Options) -> Result<Box<dyn Capturer>> {
        tracing::info!(
            width = opts.width,
            height = opts.height,
            fps = opts.fps,
            "spike source: synthetic NV12 GPU texture (moving luma ramp)"
        );
        Ok(Box::new(
            capture::synthetic_nv12::SyntheticNv12Capturer::new(opts.width, opts.height, opts.fps)
                .context("open synthetic NV12 capturer")?,
        ))
    }

    /// Windows has one virtual-display backend.
    pub(super) fn spike_compositor() -> Result<crate::vdisplay::Compositor> {
        Ok(crate::vdisplay::Compositor::Windows)
    }

    /// IDD-push keeps the pixels in the driver and hands this process access units, so the
    /// frame carries no bytes and an in-process encoder would encode nothing. The driver
    /// encoder is the real path, and it is what the seat gate has to exercise.
    pub(super) fn open_driver_encoder(
        opts: &Options,
        capturer: &dyn Capturer,
        size: (u32, u32),
        bit_depth: u8,
        hdr: bool,
    ) -> Result<Option<Box<dyn Encoder>>> {
        let driver_encode = opts.source == Source::Virtual
            && crate::session_plan::CaptureBackend::resolve()
                == crate::session_plan::CaptureBackend::IddPush;
        if !driver_encode {
            return Ok(None);
        }
        let plan = crate::session_plan::SessionPlan::resolve(
            bit_depth,
            hdr,
            encode::ChromaFormat::Yuv420,
            opts.codec,
            false, // IDD composites the pointer
            false, // no mid-stream cursor-forward flip in a spike
            false, // no client decoder to slice for
        );
        capture::open_driver_encoder(
            &plan,
            capturer,
            size,
            opts.fps,
            opts.bitrate_bps,
            bit_depth,
            None,
            0,
        )
        .context("open the in-driver encoder")
        .map(Some)
    }
}
#[cfg(not(target_os = "windows"))]
mod plat {
    use super::*;

    pub(super) fn check_virtual_source(_opts: &Options) -> Result<()> {
        Ok(())
    }

    pub(super) fn open_synthetic_nv12(_opts: &Options) -> Result<Box<dyn Capturer>> {
        anyhow::bail!("--source synthetic-nv12 is Windows-only (native AMF / D3D11 encoders)")
    }

    pub(super) fn spike_compositor() -> Result<crate::vdisplay::Compositor> {
        crate::vdisplay::detect().context("detect compositor")
    }

    pub(super) fn open_driver_encoder(
        _opts: &Options,
        _capturer: &dyn Capturer,
        _size: (u32, u32),
        _bit_depth: u8,
        _hdr: bool,
    ) -> Result<Option<Box<dyn Encoder>>> {
        Ok(None)
    }
}
