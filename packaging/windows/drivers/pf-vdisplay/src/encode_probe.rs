//! Spike S5 (`--features encode-probe`, design/windows-video-plane-overhaul.md §3): the
//! encoder backends opened and driven INSIDE WUDFHost. `IOCTL_ENCODE_PROBE_ARM` parks a
//! request; the drain worker then copies each acquired surface into a three-slot ring in the
//! desktop's own format ([`offer`]: one `CopyResource`, one `SetEvent`) and the `pf-vd-probe`
//! thread does the rest — converts through [`Targets`], submits, polls, files the AUs.
//! `IOCTL_ENCODE_PROBE_STATUS` reads the tally. The request's `flags` pick the depth and chroma,
//! so the 10-bit and full-chroma inputs are reachable here too. A thin client of
//! [`crate::encode`]; never shippable.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pf_driver_proto::control::{self, EncodeProbeReply, EncodeProbeRequest};
use pf_encode_win::{ChromaFormat, EncodedFrame, Encoder};
use wdk_sys::NTSTATUS;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::System::Threading::{SetEvent, WaitForMultipleObjects};
use windows62::Win32::Graphics::Direct3D11 as d3d;

use crate::direct_3d_device::{Direct3DDevice, pooled_device};
use crate::encode::convert::{
    AdapterId, Fail, InputKind, Targets, adapter_of, bridge, source_format,
};
use crate::registry::lock;
use crate::worker::{OwnedHandle, Worker};
use crate::{STATUS_INVALID_PARAMETER, STATUS_SUCCESS};
use pf_encode_session::open::{
    OpenSpec, codec_from_wire, open_backend, qpc_frequency, qpc_now, qpc_to_ns,
};

const STATUS_UNSUCCESSFUL: NTSTATUS = 0xC000_0001u32 as NTSTATUS;
const STATUS_DEVICE_BUSY: NTSTATUS = 0x8000_0011u32 as NTSTATUS;

const ST_ARMED: u32 = 1;
const ST_RUNNING: u32 = 2;
const ST_DONE: u32 = 3;
const ST_FAILED: u32 = 4;

/// Three slots so the drain worker can bank two frames while one encodes (host OUT_RING).
const SLOTS: usize = 3;
/// Submits allowed ahead of the oldest AU — the host's pipeline depth.
const MAX_INFLIGHT: usize = 2;
use pf_driver_proto::encode::backend::NAMES as BACKENDS;
use pf_driver_proto::encode::codec::NAMES as CODECS;
use pf_driver_proto::encode::{backend, codec};

/// What the first acquired surface told the hook: the ring's shape and the GPU behind it.
#[derive(Clone, Copy)]
struct Primed {
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    adapter: AdapterId,
}

enum Ring {
    /// No surface seen yet: the next one is described, not copied.
    Priming,
    /// Described; the probe thread is building the ring.
    Described(Primed),
    Live {
        slots: Vec<ID3D11Texture2D>,
        free: Vec<usize>,
        /// `(slot, PresentDisplayQPCTime)` in acquire order.
        full: VecDeque<(usize, u64)>,
    },
}

/// What the drain hook and the probe thread share. Dropped by whichever holder is last, so
/// the event closes only after any hook call still holding an `Arc` has returned.
struct Shared {
    target_id: u32,
    /// Auto-reset, signalled once per copied frame.
    event: OwnedHandle,
    ring: Mutex<Ring>,
    /// Ring device epoch: a hook on a different (recreated) device skips the copy.
    device_epoch: AtomicU32,
    drops: AtomicU32,
}

struct Probe {
    reply: EncodeProbeReply,
    worker: Option<Worker>,
}

const ZERO_REPLY: EncodeProbeReply = EncodeProbeReply {
    state: 0,
    backend_opened: 0,
    frames_submitted: 0,
    aus: 0,
    bytes: 0,
    open_us: 0,
    first_au_us: 0,
    mean_submit_to_au_us: 0,
    max_submit_to_au_us: 0,
    drops: 0,
    error: 0,
    name: [0; 32],
};

static PROBE: Mutex<Probe> = Mutex::new(Probe {
    reply: ZERO_REPLY,
    worker: None,
});
/// The hook's handle to the run; `None` between runs.
static SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);
/// Fast path for the drain hook: one relaxed load per frame while no run is armed.
static ARMED: AtomicBool = AtomicBool::new(false);

/// `IOCTL_ENCODE_PROBE_ARM`: start a run. `STATUS_DEVICE_BUSY` while one is going.
pub fn arm(req: &EncodeProbeRequest) -> NTSTATUS {
    let mut p = lock(&PROBE);
    if matches!(p.reply.state, ST_ARMED | ST_RUNNING) {
        return STATUS_DEVICE_BUSY;
    }
    // PyroWave is its own backend and codec, or neither; every other pairing is free.
    let valid = (1..=BACKENDS.len() as u32).contains(&req.backend)
        && (1..=CODECS.len() as u32).contains(&req.codec)
        && (req.backend == backend::PYROWAVE) == (req.codec == codec::PYROWAVE);
    if !valid {
        return STATUS_INVALID_PARAMETER;
    }
    // The previous run's thread has exited (its state is done/failed): the join is immediate.
    drop(p.worker.take());
    let Some(event) = OwnedHandle::event(false) else {
        return STATUS_UNSUCCESSFUL;
    };
    let shared = Arc::new(Shared {
        target_id: req.target_id,
        event,
        ring: Mutex::new(Ring::Priming),
        device_epoch: AtomicU32::new(0),
        drops: AtomicU32::new(0),
    });
    let req = *req;
    let thread_shared = shared.clone();
    let Some(worker) = Worker::spawn("pf-vd-probe", move |stop| run(stop, req, thread_shared))
    else {
        return STATUS_UNSUCCESSFUL;
    };
    p.reply = ZERO_REPLY;
    p.reply.state = ST_ARMED;
    p.worker = Some(worker);
    *lock(&SHARED) = Some(shared);
    ARMED.store(true, Ordering::Release);
    dbglog!(
        "[pf-vd] probe: armed backend={} codec={} input={} flags={:#x} frames={} target={}",
        BACKENDS[req.backend as usize - 1],
        CODECS[req.codec as usize - 1],
        req.input,
        req.flags,
        req.frames,
        req.target_id
    );
    STATUS_SUCCESS
}

/// `IOCTL_ENCODE_PROBE_STATUS`: the tally so far.
pub fn status() -> EncodeProbeReply {
    let mut reply = lock(&PROBE).reply;
    if let Some(s) = lock(&SHARED).as_ref() {
        reply.drops = s.drops.load(Ordering::Relaxed);
    }
    reply
}

/// The drain worker's hook, per acquired surface: a `CopyResource` into a free ring slot and
/// a `SetEvent`, or a counted drop. Never blocks — every lock is a `try_lock`.
pub fn offer(device: &Direct3DDevice, tex: &ID3D11Texture2D, display_qpc: u64, target_id: u32) {
    if !ARMED.load(Ordering::Acquire) {
        return;
    }
    let Some(shared) = SHARED.try_lock().ok().and_then(|s| s.clone()) else {
        return;
    };
    if shared.target_id != 0 && shared.target_id != target_id {
        return;
    }
    let Ok(mut ring) = shared.ring.try_lock() else {
        shared.drops.fetch_add(1, Ordering::Relaxed);
        return;
    };
    match &mut *ring {
        Ring::Priming => {
            if let Some(p) = describe(device, tex) {
                shared.device_epoch.store(device.epoch(), Ordering::Release);
                *ring = Ring::Described(p);
                signal(&shared);
            }
        }
        Ring::Described(_) => {}
        Ring::Live { slots, free, full } => {
            if shared.device_epoch.load(Ordering::Acquire) != device.epoch() {
                return;
            }
            let Some(i) = free.pop() else {
                shared.drops.fetch_add(1, Ordering::Relaxed);
                return;
            };
            // SAFETY: `tex` is the live acquired surface and `slots[i]` a same-size, same-format
            // ring texture on the same pooled device, whose immediate context is
            // multithread-protected (`Direct3DDevice`).
            unsafe { device.device_context.CopyResource(&slots[i], tex) };
            full.push_back((i, display_qpc));
            signal(&shared);
        }
    }
}

fn signal(shared: &Shared) {
    // SAFETY: the event lives as long as `shared`, which the caller holds an `Arc` of.
    unsafe {
        let _ = SetEvent(shared.event.as_raw());
    }
}

/// The surface's shape plus the pooled device's adapter identity, read once at priming.
fn describe(device: &Direct3DDevice, tex: &ID3D11Texture2D) -> Option<Primed> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `tex` is the live acquired surface; `desc` is a valid local out-param.
    unsafe { tex.GetDesc(&mut desc) };
    Some(Primed {
        width: desc.Width,
        height: desc.Height,
        format: desc.Format,
        adapter: adapter_of(device)?,
    })
}

/// The probe thread. Everything after the hook's copy happens here; the outcome lands in
/// [`PROBE`] and the ring handle is released so the hook goes back to one atomic load.
fn run(stop: HANDLE, req: EncodeProbeRequest, shared: Arc<Shared>) {
    let outcome = drive(stop, &req, &shared);
    ARMED.store(false, Ordering::Release);
    let released = lock(&SHARED).take();
    drop(released);
    let mut p = lock(&PROBE);
    p.reply.drops = shared.drops.load(Ordering::Relaxed);
    match outcome {
        Ok(()) => {
            p.reply.state = ST_DONE;
            let r = &p.reply;
            dbglog!(
                "[pf-vd] probe: done frames={} aus={} bytes={} open_us={} first_au_us={} mean_us={} max_us={} drops={}",
                r.frames_submitted,
                r.aus,
                r.bytes,
                r.open_us,
                r.first_au_us,
                r.mean_submit_to_au_us,
                r.max_submit_to_au_us,
                shared.drops.load(Ordering::Relaxed)
            );
        }
        Err((code, name)) => {
            p.reply.state = ST_FAILED;
            p.reply.error = code;
            p.reply.name = [0; 32];
            let n = name.len().min(32);
            p.reply.name[..n].copy_from_slice(&name.as_bytes()[..n]);
            dbglog!("[pf-vd] probe: FAILED {name} ({code})");
        }
    }
}

/// The chosen input paired with the chroma the opened encoder reports, as the reply's tag:
/// `Rgb10+444` is the HDR full-chroma pairing, `P010+444` would be the reply outrunning the
/// encoder again. Readable from the test's output, so a run needs no driver log.
fn outcome_tag(kind: InputKind, chroma444: bool) -> [u8; 32] {
    let k = match kind {
        InputKind::Bgra => "Bgra",
        InputKind::Nv12 => "Nv12",
        InputKind::P010 => "P010",
        InputKind::P010Sdr => "P010Sdr",
        InputKind::Rgb10 => "Rgb10",
        InputKind::Rgb10Wcg => "Rgb10Wcg",
        InputKind::P010Wcg => "P010Wcg",
        InputKind::Fp16 => "Fp16",
        InputKind::Planar { .. } => "Planar",
    };
    let tag = format!("{k}+{}", if chroma444 { "444" } else { "420" });
    let mut out = [0u8; 32];
    let n = tag.len().min(32);
    out[..n].copy_from_slice(&tag.as_bytes()[..n]);
    out
}

/// The request's backend, codec, depth/chroma flags and input A/B as one `open` spec. The
/// kind comes from [`InputKind::choose`], the same call `SET_ENCODE` makes, so a probe run
/// exercises the shipping decision rather than a copy of it.
fn spec(req: &EncodeProbeRequest, w: u32, h: u32) -> Result<OpenSpec, Fail> {
    let hdr = (req.flags & control::PROBE_FLAG_HDR) != 0;
    let chroma444 = (req.flags & control::PROBE_FLAG_444) != 0;
    let kind = match (req.backend, req.input) {
        // No 10-bit-SDR probe flag: `ten_bit` follows `hdr`, so the probe's input matches the
        // shipping SDR (Nv12/Bgra) and HDR (P010/Fp16) decisions, never the AMF P010Sdr path.
        // Without the FP16 SDR flag every backend has an input.
        (4, _) => InputKind::choose(4, hdr, hdr, chroma444, false).ok_or((-4, "input"))?,
        // The colour A/B: BGRA→NV12 on the video engine instead of the backend's own CSC.
        (_, 1) => InputKind::Nv12,
        _ => InputKind::choose(req.backend, hdr, hdr, chroma444, false).ok_or((-4, "input"))?,
    };
    Ok(OpenSpec {
        backend: req.backend,
        codec: codec_from_wire(req.codec).ok_or((-4, "codec"))?,
        kind,
        width: w,
        height: h,
        fps: if req.fps == 0 { 60 } else { req.fps },
        bitrate_bps: u64::from(if req.bitrate_kbps == 0 {
            20_000
        } else {
            req.bitrate_kbps
        }) * 1000,
        bit_depth: if hdr { 10 } else { 8 },
        chroma: if chroma444 {
            ChromaFormat::Yuv444
        } else {
            ChromaFormat::Yuv420
        },
    })
}

fn drive(stop: HANDLE, req: &EncodeProbeRequest, shared: &Arc<Shared>) -> Result<(), Fail> {
    let primed = wait_primed(stop, shared)?;
    dbglog!(
        "[pf-vd] probe: primed {}x{} fmt={:?} luid={:08x}:{:08x} gpu={:04x}:{:04x}",
        primed.width,
        primed.height,
        primed.format,
        primed.adapter.luid.HighPart,
        primed.adapter.luid.LowPart,
        primed.adapter.vendor_id,
        primed.adapter.device_id
    );
    let (w, h) = (primed.width, primed.height);
    let spec = spec(req, w, h)?;
    // The ring holds DWM's surface as it comes, so the input the run asked for must be the one
    // this desktop can feed: FP16 for an HDR run, BGRA otherwise. Flip advanced colour first.
    let want = source_format(spec.kind);
    if primed.format.0 != want.0 {
        dbglog!(
            "[pf-vd] probe: desktop presents {:?}, this input needs {want:?}",
            primed.format
        );
        return Err((-4, "fmt"));
    }
    // The adapter id carries the session's 0.62 LUID; the pooled device is keyed by ours.
    let luid = windows::Win32::Foundation::LUID {
        LowPart: primed.adapter.luid.LowPart,
        HighPart: primed.adapter.luid.HighPart,
    };
    let dev = pooled_device(luid).ok_or((-5, "device"))?;
    shared.device_epoch.store(dev.epoch(), Ordering::Release);
    let slots = (0..SLOTS)
        .map(|_| make_slot(&dev, w, h, primed.format))
        .collect::<Result<Vec<_>, _>>()?;
    let dev62: d3d::ID3D11Device = bridge(&dev.device)?;
    let ctx62: d3d::ID3D11DeviceContext = bridge(&dev.device_context)?;
    let slots62 = slots
        .iter()
        .map(|s| bridge::<d3d::ID3D11Texture2D>(s))
        .collect::<Result<Vec<_>, _>>()?;
    let mut targets = Targets::new(spec.kind, &dev62, &ctx62, (w, h), SLOTS)?;

    let t0 = Instant::now();
    // `dev62` is the device the ring's slots live on, so NVENC opens its session against the
    // one it will encode from; `open_us` therefore covers that session, not just the handle.
    let mut enc = open_backend(&spec, &primed.adapter, &dev62, &crate::log::knob)?;
    let open_us = t0.elapsed().as_micros() as u32;
    // The opened chroma, not the requested one: an input that cannot carry 4:4:4 reads false
    // here, which is the whole point of running the probe at 10-bit.
    dbglog!(
        "[pf-vd] probe: backend open OK in {open_us} us (input {:?}, 4:4:4 {})",
        spec.kind,
        enc.caps().chroma_444
    );
    {
        let mut p = lock(&PROBE);
        p.reply.backend_opened = req.backend;
        p.reply.open_us = open_us;
        p.reply.state = ST_RUNNING;
        p.reply.name = outcome_tag(spec.kind, enc.caps().chroma_444);
    }
    *lock(&shared.ring) = Ring::Live {
        slots,
        free: (0..SLOTS).collect(),
        full: VecDeque::new(),
    };

    let path = std::env::temp_dir().join(format!(
        "pfvd-probe-{}-{}.bin",
        BACKENDS[req.backend as usize - 1],
        CODECS[req.codec as usize - 1]
    ));
    let file = std::fs::File::create(&path).map_err(|_| (-6, "file"))?;
    let mut sink = Sink {
        shared: shared.clone(),
        file: std::io::BufWriter::new(file),
        inflight: VecDeque::new(),
        aus: 0,
        lat_sum: 0,
    };
    let frames = if req.frames == 0 { 300 } else { req.frames };
    let qpc_hz = qpc_frequency();
    for n in 0..frames {
        let (slot, qpc) = wait_frame(stop, shared)?;
        let pts_ns = qpc_to_ns(if qpc == 0 { qpc_now() } else { qpc }, qpc_hz);
        targets.pass(&slots62[slot], slot, false)?;
        let frame = targets.frame(slot, pts_ns, None)?;
        let submitted = Instant::now();
        enc.submit(&frame).map_err(|e| {
            dbglog!("[pf-vd] probe: submit #{n} failed: {e:#}");
            (-1, "submit")
        })?;
        sink.inflight.push_back((slot, submitted));
        lock(&PROBE).reply.frames_submitted = n + 1;
        sink.collect(enc.as_mut(), MAX_INFLIGHT)?;
    }
    if let Err(e) = enc.flush() {
        dbglog!("[pf-vd] probe: flush failed: {e:#}");
    }
    // A backend that keeps its last AU past flush is a note, not a failed run.
    if let Err((_, why)) = sink.collect(enc.as_mut(), 1) {
        dbglog!(
            "[pf-vd] probe: drain ended {why} with {} AU(s) owed",
            sink.inflight.len()
        );
    }
    drop(enc);
    let _ = sink.file.flush();
    dbglog!("[pf-vd] probe: AUs written to {}", path.display());
    Ok(())
}

/// The AU side of the run: matches AUs to submits in order, frees their ring slots, files
/// the bytes and keeps the reply's counters current.
struct Sink {
    shared: Arc<Shared>,
    file: std::io::BufWriter<std::fs::File>,
    /// `(slot, submit time)` of every frame whose AU is still owed.
    inflight: VecDeque<(usize, Instant)>,
    aus: u32,
    lat_sum: u64,
}

impl Sink {
    /// Poll until fewer than `keep` frames are in flight. A backend that owes an AU and
    /// produces none for 2 s is a stall.
    fn collect(&mut self, enc: &mut dyn Encoder, keep: usize) -> Result<(), Fail> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if self.inflight.len() < keep {
                return Ok(());
            }
            match enc.poll() {
                Ok(Some(au)) => self.on_au(au)?,
                Ok(None) => {
                    if Instant::now() > deadline {
                        return Err((-3, "stalled"));
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                Err(e) => {
                    dbglog!("[pf-vd] probe: poll failed: {e:#}");
                    return Err((-1, "poll"));
                }
            }
        }
    }

    fn on_au(&mut self, au: EncodedFrame) -> Result<(), Fail> {
        let latency_us = match self.inflight.pop_front() {
            Some((slot, submitted)) => {
                if let Ring::Live { free, .. } = &mut *lock(&self.shared.ring) {
                    free.push(slot);
                }
                submitted.elapsed().as_micros() as u32
            }
            None => 0,
        };
        let len = au.data.len() as u32;
        self.file
            .write_all(&len.to_le_bytes())
            .and_then(|()| self.file.write_all(&au.pts_ns.to_le_bytes()))
            .and_then(|()| self.file.write_all(&[u8::from(au.keyframe)]))
            .and_then(|()| self.file.write_all(&au.data))
            .map_err(|_| (-6, "file"))?;
        self.aus += 1;
        let mut p = lock(&PROBE);
        p.reply.aus = self.aus;
        p.reply.bytes += u64::from(len);
        if self.aus == 1 {
            p.reply.first_au_us = latency_us;
            dbglog!(
                "[pf-vd] probe: first AU after {latency_us} us ({len} bytes, keyframe={})",
                au.keyframe
            );
        } else {
            self.lat_sum += u64::from(latency_us);
            p.reply.mean_submit_to_au_us = (self.lat_sum / u64::from(self.aus - 1)) as u32;
            p.reply.max_submit_to_au_us = p.reply.max_submit_to_au_us.max(latency_us);
        }
        Ok(())
    }
}

/// Wait for the hook to describe the first surface (10 s: DWM composes a fresh monitor at
/// arrival, but a stale ARM on a monitor nobody drains must still end).
fn wait_primed(stop: HANDLE, shared: &Shared) -> Result<Primed, Fail> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ring::Described(p) = &*lock(&shared.ring) {
            return Ok(*p);
        }
        wait(stop, shared, deadline, "prime")?;
    }
}

/// The oldest full slot; 5 s without one is a starved desktop, not a slow encoder.
fn wait_frame(stop: HANDLE, shared: &Shared) -> Result<(usize, u64), Fail> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ring::Live { full, .. } = &mut *lock(&shared.ring)
            && let Some(f) = full.pop_front()
        {
            return Ok(f);
        }
        wait(stop, shared, deadline, "starved")?;
    }
}

/// One bounded wait on `{stop, frame event}`. The event is auto-reset, so a signal raised
/// while nobody waits latches for the next call — a queue checked before each wait loses none.
fn wait(
    stop: HANDLE,
    shared: &Shared,
    deadline: Instant,
    starved: &'static str,
) -> Result<(), Fail> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err((-3, starved));
    }
    let ms = left.as_millis().min(1000) as u32;
    let handles = [stop, shared.event.as_raw()];
    // SAFETY: `stop` is the worker's stop event, alive until the worker joins this thread;
    // the frame event lives as long as `shared`.
    let waited = unsafe { WaitForMultipleObjects(&handles, false, ms) };
    if waited == WAIT_OBJECT_0 {
        return Err((-7, "stop"));
    }
    Ok(())
}

/// One BGRA ring slot on the pooled 0.58 device: the hook's copy target, the thread's source.
fn make_slot(
    dev: &Direct3DDevice,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D, Fail> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // SRV for the shader kinds' pass; RT is what NVENC registers against.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` is a fully-initialized local; `t` a valid out-param checked below.
    let hr = unsafe { dev.device.CreateTexture2D(&desc, None, Some(&mut t)) };
    match (hr, t) {
        (Ok(()), Some(t)) => Ok(t),
        (r, _) => {
            dbglog!("[pf-vd] probe: ring slot CreateTexture2D failed: {r:?}");
            Err((-2, "pool"))
        }
    }
}
