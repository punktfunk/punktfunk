//! Host-side capture through a capture worker: a monitor the host did not create, captured
//! with Windows Graphics Capture and encoded in a process that runs as the signed-in user.
//!
//! [`WorkerLink`] is the host's end of one worker: its process handle, and the two pipes of
//! [`pf_driver_proto::worker`] behind a [`Link`], which checks and bounds everything the
//! worker sends. [`WorkerTransport`] is the remote source's edge over it: the worker owns the
//! pixels and the encoder, and publishes into the same AU section the driver does, so the
//! source and its encoder proxy are the driver's own.
//!
//! Evidence: `design/windows-wgc-capture.md` §4.3, §4.7, §4.13.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::io::OwnedHandle;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytemuck::Pod;
use pf_driver_proto::encode as wire;
use pf_driver_proto::worker::{self as proto, kind};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
};

use crate::windows::remote::broker::RemoteProcess;
use crate::windows::remote::encoder::nul_tag;
use crate::windows::remote::link::{Link, Message, Peer};
use crate::windows::remote::qpc;
use crate::windows::remote::source::{Geometry, RemoteSource};
use crate::windows::remote::supervisor::Supervision;
use crate::windows::remote::transport::{HandleTarget, RemoteEndpoint, Transport, TransportEvent};
use crate::{Capturer, EncodeCtlSender, EncoderSite, PixelFormat};

/// How long the worker has to say hello. A process's first lines of `main`.
const HELLO_BOUND: Duration = Duration::from_secs(5);
/// How long a capture open may take. Its stages are calls that have hung on some box; a warm
/// open is 55 ms and a slow one 2.5 s.
const SOURCE_BOUND: Duration = Duration::from_secs(5);
/// An encoder open or reset: the worker's own 5 s bound on the backend, plus a thread stop.
const OPEN_BOUND: Duration = Duration::from_secs(8);
/// Every other op is queued in the worker and answered at once.
const CTL_BOUND: Duration = Duration::from_secs(2);
/// How long a worker gets to exit once its pipe closed, before it is ended.
const EXIT_BOUND: Duration = Duration::from_millis(500);
/// The largest picture a worker may report. Past it the reply is not a size.
const MAX_SIDE: u32 = 16_384;
/// A source with no new frame for this long reads as idle on the health surface.
const IDLE_AFTER: Duration = Duration::from_secs(2);

/// A started capture worker, as whoever spawned it hands it over.
pub struct WorkerProcess {
    /// The process, with the rights its creator holds: duplicate into, wait on, end.
    pub process: OwnedHandle,
    pub pid: u32,
    /// Host → worker.
    pub requests: File,
    /// Worker → host: replies and events.
    pub replies: File,
    /// The worker's stderr.
    pub log: File,
}

/// The worker's process, with the rights its creator holds: duplicate into, wait on, end.
impl Peer for RemoteProcess {
    fn kill(&self) {
        // SAFETY: the handle is live while `self` is, with the terminate right its creator
        // holds.
        let _ = unsafe { TerminateProcess(self.raw(), 1) };
    }

    fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        // SAFETY: the handle is live while `self` is; `code` is a live out-param.
        (self.exited() && unsafe { GetExitCodeProcess(self.raw(), &mut code) }.is_ok())
            .then_some(code)
    }
}

/// The host's end of one capture worker. Dropping the last reference closes the request pipe,
/// which is how a worker is told to exit; one that does not is ended.
pub struct WorkerLink {
    link: Link<RemoteProcess>,
    pid: u32,
}

/// The worker's stderr into this process's log, a line at a time and a bounded line at that.
fn relay(log: File, pid: u32) {
    let mut log = BufReader::new(log);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut log).take(4096).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                tracing::info!(pid, "capture worker: {}", text.trim_end());
            }
        }
    }
}

impl WorkerLink {
    /// Take over a started worker: pump its pipes and trade hellos. A worker of another
    /// build is ended here.
    pub fn start(worker: WorkerProcess) -> Result<Arc<Self>> {
        let (log, pid) = (worker.log, worker.pid);
        let process = RemoteProcess::new(worker.process, "the capture worker");
        let link = Link::open(process, worker.requests, worker.replies)?;
        std::thread::Builder::new()
            .name("pf-cw-log".into())
            .spawn(move || relay(log, pid))
            .context("spawn the capture worker log thread")?;
        let link = Arc::new(Self { link, pid });
        link.link.hello(HELLO_BOUND)?;
        Ok(link)
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn process(&self) -> HANDLE {
        self.link.peer().raw()
    }

    /// The worker's exit code once it has exited.
    pub fn exit_code(&self) -> Option<u32> {
        self.link.peer().exit_code()
    }

    /// End the worker now. It holds no state of ours: a session starts another.
    fn kill(&self) {
        self.link.peer().kill();
    }

    /// Raise the worker's GPU scheduling class from outside: the user's token cannot.
    fn raise_gpu_priority(&self) {
        // SAFETY: `process` is live for the call and carries the set-information right.
        unsafe { pf_frame::dxgi::elevate_gpu_priority_of(self.process(), "capture worker") };
    }

    /// [`Link::request`]: one request, its reply within `bound`, or the worker is ended.
    fn request<Q: Pod, R: Pod>(&self, op: u32, body: &Q, bound: Duration) -> Result<R> {
        self.link.request(op, body, bound)
    }

    /// The next event the worker sent, if any.
    fn event(&self) -> Option<Message> {
        self.link.event()
    }
}

impl Drop for WorkerLink {
    fn drop(&mut self) {
        self.link.close();
        // SAFETY: `process` is this link's live handle; a bounded wait only reads its state.
        let exited = unsafe {
            WaitForSingleObject(self.process(), EXIT_BOUND.as_millis() as u32) == WAIT_OBJECT_0
        };
        if !exited {
            tracing::warn!(pid = self.pid, "capture worker did not exit — ending it");
            self.kill();
        }
    }
}

/// The monitor a worker captures, as the host's display inventory names it now.
#[derive(Clone, Debug)]
pub struct WgcSource {
    /// GDI device name, the name the capture is opened by.
    pub gdi_name: String,
    /// The monitor's display target id.
    pub target_id: u32,
    /// The monitor composes HDR.
    pub hdr: bool,
}

fn side(v: u32) -> Option<u32> {
    (1..=MAX_SIDE).contains(&v).then_some(v)
}

/// The remote source's edge into one worker: its pipes for the encoder's requests, and its
/// events for the source's news. The worker owns both duplicated handles from the moment the
/// request is written, whatever it answers.
struct WorkerTransport {
    link: Arc<WorkerLink>,
    source: WgcSource,
    /// The source's size as the worker last reported it.
    size: (u32, u32),
}

impl Transport for WorkerTransport {
    fn endpoint(&self) -> RemoteEndpoint {
        RemoteEndpoint {
            site: EncoderSite::Worker,
            target_id: self.source.target_id,
        }
    }

    fn name(&self) -> &str {
        &self.source.gdi_name
    }

    fn handles(&self) -> &dyn HandleTarget {
        self.link.link.peer()
    }

    fn set_encode(&self, req: &wire::SetEncodeRequest) -> Result<wire::SetEncodeReply> {
        self.link.request(kind::SET_ENCODE, req, OPEN_BOUND)
    }

    /// The control calls as pipe requests: a reset gets the open's bound, every other op is
    /// answered at once.
    fn encode_ctl(&self) -> EncodeCtlSender {
        let link = self.link.clone();
        Arc::new(move |req: &wire::EncodeCtlRequest| {
            let bound = if req.op == wire::ENCODE_CTL_RESET {
                OPEN_BOUND
            } else {
                CTL_BOUND
            };
            let reply: proto::CtlReply = link.request(kind::ENCODE_CTL, req, bound)?;
            if reply.status != proto::CTL_OK {
                bail!(
                    "capture worker refused control op {}: status {}",
                    req.op,
                    reply.status
                );
            }
            Ok(())
        })
    }

    fn secure_desktop(&self) -> bool {
        pf_win_display::secure_desktop()
    }

    /// The worker's exit, then its events. A gone source or a reply that is not a size ends
    /// the source.
    fn poll(&mut self, _now: Instant, _last_fresh: Instant) -> Result<Vec<TransportEvent>> {
        if let Some(code) = self.link.exit_code() {
            bail!("capture worker exited (code {code})");
        }
        let mut news = Vec::new();
        while let Some((frame, body)) = self.link.event() {
            match frame.kind {
                kind::SOURCE_CHANGED => {
                    let changed = proto::body::<proto::SourceChanged>(&body)
                        .and_then(|c| Some((side(c.width)?, side(c.height)?)));
                    let Some((width, height)) = changed else {
                        bail!("capture worker reported a source change that is not a size");
                    };
                    tracing::info!(
                        gdi = %self.source.gdi_name,
                        from = format!("{}x{}", self.size.0, self.size.1),
                        to = format!("{width}x{height}"),
                        "capture worker: the source changed size"
                    );
                    self.size = (width, height);
                    news.push(TransportEvent::Resized { width, height });
                }
                kind::SOURCE_GONE => bail!("capture source {} is gone", self.source.gdi_name),
                other => tracing::debug!(kind = other, "capture worker: unknown event"),
            }
        }
        Ok(news)
    }

    fn alive(&self) -> bool {
        self.link.exit_code().is_none()
    }
}

/// Open `source` in `worker` and hand back its capturer. `want_hdr` is the session's: the
/// capture is FP16 only for an HDR session on a monitor that composes HDR. `fps` is the
/// stream's rate. `own_priority` gives the worker the GPU scheduling class a session's encoder
/// gets; a viewer of someone else's display leaves it off. `keepalive` is held for the
/// capturer's life.
pub fn open_wgc(
    worker: WorkerProcess,
    source: WgcSource,
    want_hdr: bool,
    fps: u32,
    own_priority: bool,
    keepalive: Box<dyn Send>,
) -> Result<Box<dyn Capturer>> {
    let link = WorkerLink::start(worker)?;
    if own_priority {
        link.raise_gpu_priority();
    }
    let fp16 = want_hdr && source.hdr;
    let gdi_name = proto::gdi_name(&source.gdi_name)
        .with_context(|| format!("monitor name {:?} is not a device name", source.gdi_name))?;
    let open = proto::OpenSource {
        flags: proto::OPEN_CURSOR | if fp16 { proto::OPEN_FP16 } else { 0 },
        frame_interval_100ns: 10_000_000 / fps.max(1),
        gdi_name,
    };
    let reply: proto::SourceReply = link.request(kind::OPEN_SOURCE, &open, SOURCE_BOUND)?;
    if reply.status != proto::SOURCE_OK {
        bail!(
            "open capture source {}: status {}, error {:#010x}, at '{}'",
            source.gdi_name,
            reply.status,
            reply.error as u32,
            nul_tag(&reply.stage)
        );
    }
    let asked = if fp16 {
        proto::FORMAT_FP16
    } else {
        proto::FORMAT_BGRA8
    };
    let (Some(width), Some(height), true) =
        (side(reply.width), side(reply.height), reply.format == asked)
    else {
        link.kill();
        bail!(
            "capture worker opened {} as {}x{} format {}",
            source.gdi_name,
            reply.width,
            reply.height,
            reply.format
        );
    };
    tracing::info!(
        gdi = %source.gdi_name,
        target_id = source.target_id,
        width,
        height,
        fp16,
        worker_pid = link.pid(),
        "capture worker: source open"
    );
    // The capture is FP16 scRGB and the stream HDR, or BGRA8 and SDR, for the session's life.
    let format = if fp16 {
        PixelFormat::P010
    } else {
        PixelFormat::Nv12
    };
    Ok(Box::new(RemoteSource::new(
        WorkerTransport {
            link,
            source,
            size: (width, height),
        },
        Geometry {
            width,
            height,
            format,
        },
        // The capture delivers only what changes, so a quiet source is idle, never a stall,
        // and no recovery rung hangs off it.
        Supervision::Classify {
            idle_after: IDLE_AFTER,
        },
        qpc::CLOCKS,
        keepalive,
    )))
}
