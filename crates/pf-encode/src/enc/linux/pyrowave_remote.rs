//! Host half of `punktfunk-encode-worker`
//! (design: `design/gpu-priority-capability-worker.md`). The worker half and the vocabulary
//! they share are [`super::worker`].
//!
//! [`RemotePyroWave`] is an `Encoder` that forwards a PyroWave session to the
//! capability-carrying worker. It sits under `TrackedEncoder`, so session accounting, the
//! encode-stall watchdog, encoder recovery and the forwarding rot-guard apply unchanged.
//!
//! A PyroWave open is only reached after the codec was negotiated, so a hard error here is a
//! dead stream. Every rung therefore falls back to the in-process encoder at default GPU
//! priority: env-off, missing binary, spawn, handshake, version skew, and encoder-init return
//! the bare in-process encoder; a non-dmabuf frame, a worker encode error, or socket EOF
//! mid-session pin a live proxy in-process (EOF gets one respawn first).
//!
//! Fallback logs share the clause *"encoding in-process at default GPU priority"* so one
//! `grep` finds every rung.

use super::worker::{self, FromWorker, PriorityOutcome, ToWorker, WireCursor};
use crate::pyrowave_wire::AuStream;
use crate::{AuChunk, ChromaFormat, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload};
use pf_zerocopy::ipc;
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

/// Installed name. The only file that may carry `cap_sys_nice=ep`. Never a hardlink of
/// `punktfunk-host`: a shared inode shares the file capability, KWin cannot identify the host,
/// and the desktop session dies.
const WORKER_BIN: &str = "punktfunk-encode-worker";

/// Handshake budget, matching the zerocopy worker's GPU bring-up. Covers Vulkan instance +
/// device create and the pyrowave object; a cold driver load can take seconds. Blowing it
/// means the driver is wedged, which would wedge the in-process encoder too.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-request budget. A PyroWave encode is 2–5 ms and the encoder's own fence wait is capped at
/// 5 s, so a worker silent for twice that is wedged inside a driver call, not slow.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkerPath {
    /// `PUNKTFUNK_ENCODE_WORKER=off` — debug hatch so worker vs in-process is a one-line change.
    Off,
    Found(PathBuf),
    /// Not beside the host binary and not on `PATH`.
    Missing,
}

/// Resolve the worker binary: `PUNKTFUNK_ENCODE_WORKER` → alongside `/proc/self/exe` → `PATH`.
///
/// The env override is load-bearing on NixOS: a file capability cannot live on a read-only store
/// path, so the module exposes the worker through `security.wrappers` and points this variable at
/// the wrapper. Pure and injected so the table can be tested without mutating process env (races
/// `getenv` in parallel tests).
fn resolve_worker_path_in(
    env: Option<&str>,
    exe_dir: Option<&Path>,
    path_var: Option<&str>,
    exists: &dyn Fn(&Path) -> bool,
) -> WorkerPath {
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        if v.eq_ignore_ascii_case("off") {
            return WorkerPath::Off;
        }
        // Not existence-checked: an operator-named path must fail by that name. Falling through
        // to "beside the host binary" would hide a typo behind a working stream.
        return WorkerPath::Found(PathBuf::from(v));
    }
    if let Some(p) = exe_dir.map(|d| d.join(WORKER_BIN)).filter(|p| exists(p)) {
        return WorkerPath::Found(p);
    }
    if let Some(p) = path_var
        .into_iter()
        .flat_map(|v| v.split(':'))
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(WORKER_BIN))
        .find(|p| exists(p))
    {
        return WorkerPath::Found(p);
    }
    WorkerPath::Missing
}

pub(crate) fn resolve_worker_path() -> WorkerPath {
    // `current_exe()` readlinks `/proc/self/exe`, which becomes `"<path> (deleted)"` after a
    // replace-under-run. Harmless here: only the parent directory is used. The worker we then
    // resolve is pinned by fd before exec, which is where that trap bites.
    let exe_dir = std::env::current_exe().ok();
    let path_var = std::env::var("PATH").ok();
    resolve_worker_path_in(
        std::env::var("PUNKTFUNK_ENCODE_WORKER").ok().as_deref(),
        exe_dir.as_deref().and_then(Path::parent),
        path_var.as_deref(),
        &|p| p.is_file(),
    )
}

/// Session parameters, so an in-process fallback can open mid-session with the same values.
#[derive(Clone, Copy)]
struct Params {
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaFormat,
    /// Negotiated stream depth (8 or 10) and the BT.2020 PQ flag.
    bit_depth: u8,
    hdr: bool,
}

enum Fail {
    /// Transport broke. Surfaces as `submit` `Err` so the host rebuild path runs;
    /// [`RemotePyroWave::reset`] is the one respawn.
    Dead(anyhow::Error),
    /// Worker alive, this frame refused. Pin in-process: the raw-dmabuf degrade latch is a host
    /// process-wide static, and an import failure noted inside the worker dies with it.
    Encode(String),
}

/// Live worker: socket, child, and the caches that keep the steady state free of descriptors.
#[derive(Debug)]
struct Link {
    sock: OwnedFd,
    child: Option<Child>,
    /// Worker's AU return memfd, received once in `Ready`. Bytes cannot ride in the message body
    /// (see [`super::worker`]). `None` only between spawn and handshake so `Drop` can reap on every
    /// failure path.
    au_buf: Option<File>,
    rbuf: Vec<u8>,
    /// Dmabuf keys whose fd the worker already holds; the fd crosses only on first sight.
    sent_keys: HashSet<u64>,
    /// Cursor bitmap `serial` the worker has pixels for. A moving pointer re-sends position only.
    cursor_serial: Option<u64>,
}

impl Drop for Link {
    fn drop(&mut self) {
        // Worker exits on socket EOF (`self.sock` drops after this). Park, don't wait: a worker
        // wedged in a driver ioctl sits in D state and ignores SIGKILL; session teardown must
        // not block on a process that may never die.
        if let Some(child) = self.child.take() {
            ipc::park_child(child);
        }
    }
}

impl Link {
    /// Send one request and take its single reply. Every host→worker message has exactly one
    /// reply, so the two sides cannot desync on turn-taking.
    fn request(&mut self, msg: &ToWorker, fds: &[BorrowedFd]) -> Result<FromWorker> {
        worker::send_eintr(self.sock.as_fd(), msg, fds).context("send to the encode worker")?;
        let (reply, _) = worker::recv_eintr::<FromWorker>(
            self.sock.as_fd(),
            &mut self.rbuf,
            Some(REPLY_TIMEOUT),
        )
        .context("no reply from the encode worker")?;
        Ok(reply)
    }

    /// Encode one frame. The reply is the buffer-release signal — the same lifetime as
    /// `Encoder::submit`: the caller holds the frame until its AU comes back.
    fn encode(&mut self, frame: &CapturedFrame) -> Result<EncodedFrame, Fail> {
        let FramePayload::Dmabuf(d) = &frame.payload else {
            // `submit` routes every non-dmabuf payload in-process before this.
            return Err(Fail::Encode("not a dmabuf payload".into()));
        };
        let key = dmabuf_key(d.fd.as_fd()).map_err(|e| Fail::Dead(e.into()))?;
        // Built once and re-sent on a `NeedFd` retry: the worker drops every descriptor of a
        // refused frame, so a retry that dropped the cursor would blend a stale pointer.
        // Empty bitmaps upload too: the worker's invariant is "pixels for every serial asked",
        // and skipping empty would make "no upload" mean both new-and-empty and already-sent.
        let upload = match frame.cursor.as_ref() {
            Some(c) if self.cursor_serial != Some(c.serial) => Some(
                worker::cursor_upload(&c.rgba)
                    .map_err(|e| Fail::Dead(anyhow::Error::from(e).context("stage the cursor")))?,
            ),
            _ => None,
        };
        let cursor = frame.cursor.as_ref().map(|c| WireCursor {
            x: c.x,
            y: c.y,
            w: c.w,
            h: c.h,
            serial: c.serial,
            hot_x: c.hot_x,
            hot_y: c.hot_y,
            visible: c.visible,
            upload: upload.as_ref().map(|(_, n)| *n),
        });

        let mut attempts = 0;
        let reply = loop {
            attempts += 1;
            let has_fd = self.sent_keys.insert(key);
            let msg = ToWorker::Frame {
                key,
                has_fd,
                fourcc: d.fourcc,
                modifier: d.modifier,
                offset: d.offset,
                stride: d.stride,
                plane1: d.plane1,
                width: frame.width,
                height: frame.height,
                pts_ns: frame.pts_ns,
                format: frame.format.into(),
                cursor: cursor.clone(),
            };
            // Descriptor order is the protocol: dmabuf first (iff first sight), cursor second.
            let mut fds: Vec<BorrowedFd> = Vec::new();
            if has_fd {
                fds.push(d.fd.as_fd());
            }
            if let Some((f, _)) = upload.as_ref() {
                fds.push(f.as_fd());
            }
            match self.request(&msg, &fds).map_err(Fail::Dead)? {
                // Worker evicted this key (or the caches diverged): forget "already sent" and
                // retry once, with the fd.
                FromWorker::NeedFd if attempts == 1 => {
                    self.sent_keys.remove(&key);
                    continue;
                }
                FromWorker::NeedFd => {
                    return Err(Fail::Dead(anyhow::anyhow!(
                        "the encode worker still lacks the dmabuf fd after a resend (desync)"
                    )))
                }
                other => break other,
            }
        };
        match reply {
            FromWorker::Au {
                len,
                pts_ns,
                keyframe,
                chunk_aligned,
                encode_us,
                ..
            } => {
                let mut data = vec![0u8; len];
                self.au_buf
                    .as_ref()
                    .ok_or_else(|| {
                        Fail::Dead(anyhow::anyhow!("no AU return buffer (no handshake)"))
                    })?
                    .read_exact_at(&mut data, 0)
                    .map_err(|e| {
                        Fail::Dead(anyhow::Error::from(e).context("read the AU return buffer"))
                    })?;
                if let Some(c) = frame.cursor.as_ref() {
                    if upload.is_some() {
                        self.cursor_serial = Some(c.serial);
                    }
                }
                tracing::trace!(len, encode_us, "pyrowave: AU from the encode worker");
                Ok(EncodedFrame {
                    data,
                    pts_ns,
                    keyframe,
                    recovery_anchor: false,
                    recovery_point: false,
                    recovery_close: false,
                    chunk_aligned,
                })
            }
            FromWorker::EncodeErr {
                message,
                capture_rebuild,
            } => {
                if capture_rebuild {
                    super::vk_util::reject_dmabuf(d, &message);
                }
                Err(Fail::Encode(message))
            }
            other => Err(Fail::Dead(anyhow::anyhow!(
                "unexpected encode worker reply to a frame: {other:?}"
            ))),
        }
    }
}

/// Dmabuf identity across frames: its inode, the key the zerocopy importer uses too.
fn dmabuf_key(fd: BorrowedFd) -> io::Result<u64> {
    Ok(pf_zerocopy::fd_identity(fd)?.1)
}

#[derive(Debug)]
struct Handshake {
    link: Link,
    caps: EncoderCaps,
    priority: PriorityOutcome,
    device: String,
}

/// Spawn a worker on `exe` and complete the handshake. Any `Err` is a ladder rung: the caller
/// warns once and encodes in-process.
fn spawn_link(exe: &Path, p: &Params, bitrate_bps: u64) -> Result<Handshake> {
    // Pin the worker's inode, never `self_exe()`: this binary is a different file from the host
    // (it carries the capability the host must never have). Pinning also means a package upgrade
    // between here and exec still runs the build we resolved.
    let pinned =
        ipc::PinnedExe::open(exe).with_context(|| format!("open {} for exec", exe.display()))?;
    let (sock, child) = ipc::spawn_worker(&pinned.exec_path(), WORKER_BIN, &["--fd", "3"])
        .with_context(|| format!("spawn {}", exe.display()))?;
    // Built before the handshake so `Drop` reaps the child on every `?` in `handshake`.
    handshake(
        Link {
            sock,
            child: Some(child),
            au_buf: None,
            rbuf: Vec::new(),
            sent_keys: HashSet::new(),
            cursor_serial: None,
        },
        p,
        bitrate_bps,
    )
}

/// Handshake split from spawn so the ladder rungs are testable against a plain socket.
fn handshake(mut link: Link, p: &Params, bitrate_bps: u64) -> Result<Handshake> {
    let hello = ToWorker::Hello {
        proto: worker::PROTO_VERSION,
        workspace_version: worker::WORKSPACE_VERSION.to_string(),
        drm_node: pf_gpu::render_node_env().map(|p| p.to_string_lossy().into_owned()),
        width: p.width,
        height: p.height,
        fps: p.fps,
        bitrate_bps,
        chroma444: p.chroma.is_444(),
        bit_depth: p.bit_depth,
        hdr: p.hdr,
        // Resolved here and forwarded. The worker strips this variable from its own environment,
        // so the operator's knob cannot silently mean something different across the boundary.
        priority_intent: std::env::var("PYROWAVE_QUEUE_PRIORITY").ok(),
    };
    // Naming the handshake is load-bearing. A worker that dies during startup races this send:
    // win, and death is the EOF the recv reads; lose, and send takes EPIPE. One cause, one
    // diagnosis — without this context the EPIPE side degrades to a bare "Broken pipe".
    worker::send_eintr(link.sock.as_fd(), &hello, &[])
        .context("encode worker handshake: send Hello (died on startup?)")?;
    let (ready, fds) = worker::recv_eintr::<FromWorker>(
        link.sock.as_fd(),
        &mut link.rbuf,
        Some(HANDSHAKE_TIMEOUT),
    )
    .context("encode worker handshake (died on startup?)")?;
    match ready {
        FromWorker::Ready {
            proto,
            workspace_version,
            priority,
            device,
            chroma444,
            blends_cursor,
        } => {
            // Host and worker are different files, so lockstep skew is a real deployment state
            // and must degrade to the in-process encoder, not a session that cannot decode.
            if proto != worker::PROTO_VERSION || workspace_version != worker::WORKSPACE_VERSION {
                bail!(
                    "encode worker version skew: worker proto {proto} v{workspace_version}, \
                     host proto {} v{} — host and worker must ship lockstep",
                    worker::PROTO_VERSION,
                    worker::WORKSPACE_VERSION
                );
            }
            let au_buf = fds
                .into_iter()
                .next()
                .context("Ready carried no AU return buffer")?;
            link.au_buf = Some(File::from(au_buf));
            Ok(Handshake {
                link,
                caps: EncoderCaps {
                    // The values the worker actually opened. A hardcoded default mis-reports a
                    // 4:4:4 open and fires a spurious "chroma disagrees with Welcome" warn.
                    blends_cursor,
                    chroma_444: chroma444,
                    ..EncoderCaps::default()
                },
                priority,
                device,
            })
        }
        FromWorker::InitErr { message } => {
            bail!("encode worker did not open its encoder: {message}")
        }
        other => bail!("unexpected encode worker handshake: {other:?}"),
    }
}

/// Open a PyroWave session, preferring the capability-carrying worker.
///
/// The one seam Linux `open_video` PyroWave arms go through. Not wired into Windows: that
/// platform has no such worker.
pub(crate) fn open_preferring_worker(
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    chroma: ChromaFormat,
    bit_depth: u8,
    hdr: bool,
) -> Result<Box<dyn Encoder>> {
    let params = Params {
        width,
        height,
        fps,
        chroma,
        bit_depth,
        hdr,
    };
    let inline = || -> Result<Box<dyn Encoder>> {
        super::pyrowave::PyroWaveEncoder::open(
            width,
            height,
            fps,
            bitrate_bps,
            chroma,
            bit_depth,
            hdr,
        )
        .map(|e| Box::new(e) as Box<dyn Encoder>)
    };
    let path = match resolve_worker_path() {
        WorkerPath::Off => {
            tracing::info!(
                "pyrowave: PUNKTFUNK_ENCODE_WORKER=off — encoding in-process at default GPU priority"
            );
            return inline();
        }
        WorkerPath::Missing => {
            tracing::warn!(
                worker = WORKER_BIN,
                "pyrowave: the encode worker was not found beside the host binary or on PATH — \
                 encoding in-process at default GPU priority (the GPU-preemption lever needs the \
                 capability-carrying worker; set PUNKTFUNK_ENCODE_WORKER to its path)"
            );
            return inline();
        }
        WorkerPath::Found(p) => p,
    };
    let hs = match spawn_link(&path, &params, bitrate_bps) {
        Ok(hs) => hs,
        Err(e) => {
            tracing::warn!(
                worker = %path.display(),
                error = %format!("{e:#}"),
                "pyrowave: the encode worker did not come up — encoding in-process at default \
                 GPU priority"
            );
            return inline();
        }
    };
    match hs.priority {
        PriorityOutcome::Granted(class) => tracing::info!(
            priority = ?class,
            device = %hs.device,
            worker = %path.display(),
            "pyrowave: encoding in the capability-carrying worker at an elevated global queue \
             priority (the encode dispatch preempts a GPU-bound game where the driver honors it)"
        ),
        PriorityOutcome::Refused => tracing::warn!(
            device = %hs.device,
            worker = %path.display(),
            "pyrowave: every global queue priority class was refused — encoding at default \
             priority. The GPU-preemption lever is INERT without CAP_SYS_NICE on the encode \
             WORKER binary (never on punktfunk-host — a capability there makes the host \
             unidentifiable to KWin and kills desktop streaming); PYROWAVE_QUEUE_PRIORITY=off \
             silences this"
        ),
        PriorityOutcome::NotRequested => tracing::info!(
            device = %hs.device,
            worker = %path.display(),
            "pyrowave: encoding in the capability-carrying worker, no queue priority requested"
        ),
    }
    Ok(Box::new(RemotePyroWave {
        link: Some(hs.link),
        inline: None,
        params,
        bitrate_bps,
        worker_path: path,
        caps: hs.caps,
        stream: AuStream::default(),
        pending: VecDeque::new(),
        respawn_used: false,
    }))
}

/// A PyroWave session encoded in `punktfunk-encode-worker`, with the in-process encoder as the
/// floor. See the module docs for the ladder.
pub(crate) struct RemotePyroWave {
    /// Live worker, or `None` once this session is pinned in-process.
    link: Option<Link>,
    /// In-process encoder, opened lazily on any mid-session rung. Once open it serves the rest of
    /// the session — the worker is not re-attempted per frame.
    inline: Option<super::pyrowave::PyroWaveEncoder>,
    params: Params,
    /// Live rate, so a fallback opens at the bitrate ABR last set, not the session start rate.
    bitrate_bps: u64,
    worker_path: PathBuf,
    caps: EncoderCaps,
    /// Datagram-aligned boundary, mirrored here as well as forwarded (it changes the AU bytes),
    /// and the streamed-AU cursor — the proxy's in both modes, so exactly one can ever be open
    /// (see [`Self::poll_chunk`]).
    stream: AuStream,
    /// AUs the worker returned that the caller has not polled. Empty in in-process mode except
    /// leftovers from the fallback, which [`Self::poll_whole`] drains first.
    pending: VecDeque<EncodedFrame>,
    /// One respawn per session. After that, in-process: a worker that dies twice will keep dying,
    /// and burning the host's five-reset budget on it costs the session.
    respawn_used: bool,
}

impl RemotePyroWave {
    /// Drop the worker and encode in-process for the rest of the session.
    fn pin_inline(&mut self, reason: &str) {
        if self.link.take().is_some() {
            tracing::warn!(
                worker = %self.worker_path.display(),
                reason,
                "pyrowave: leaving the encode worker — encoding in-process at default GPU \
                 priority for the rest of this session"
            );
        }
    }

    /// In-process encoder, opened on demand at the session's current parameters with trait-set
    /// state replayed onto it.
    fn inline_mut(&mut self) -> Result<&mut super::pyrowave::PyroWaveEncoder> {
        if self.inline.is_none() {
            let mut e = super::pyrowave::PyroWaveEncoder::open(
                self.params.width,
                self.params.height,
                self.params.fps,
                self.bitrate_bps,
                self.params.chroma,
                self.params.bit_depth,
                self.params.hdr,
            )
            .context("open the in-process PyroWave encoder after leaving the worker")?;
            // Replay: the boundary changes the AU bytes, so a fallback that forgot it would ship
            // dense AUs flagged as datagram-aligned. (Bitrate needs no replay — it is an open
            // parameter above.)
            if let Some(shard) = self.stream.wire_chunk {
                e.set_wire_chunking(shard);
            }
            self.inline = Some(e);
        }
        Ok(self.inline.as_mut().expect("just opened"))
    }

    fn poll_whole(&mut self) -> Result<Option<EncodedFrame>> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        match self.inline.as_mut() {
            Some(e) => e.poll(),
            None => Ok(None),
        }
    }
}

impl Encoder for RemotePyroWave {
    /// Submit-driven: the worker encodes what `submit` hands it, so the loop submits this tick's
    /// frame rather than draining AUs the backend already holds. Written out because a silently
    /// inherited default here would disable the feature for every worker-backed session.
    fn ready_aus(&mut self, _deadline: std::time::Instant) -> Option<usize> {
        None
    }

    /// Submit-driven, so the loop's own access-unit watch catches this backend's stalls; there is
    /// no driver-side clock to report. Written out for the same reason as `ready_aus`.
    fn telemetry(&self) -> Option<pf_frame::health::EncoderTelemetry> {
        None
    }

    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // CPU-backed frames reach this encoder (4:4:4 with zero-copy off; the raw-dmabuf degrade
        // latch). 1080p BGRA is ~8 MB (~480 MB/s at 60 fps) and must not cross the socket; the
        // in-process encoder uploads into its own device.
        if self.link.is_some() && !matches!(frame.payload, FramePayload::Dmabuf(_)) {
            self.pin_inline(
                "capture delivered a non-dmabuf frame, which the worker path cannot take",
            );
        }
        if self.link.is_some() {
            match self
                .link
                .as_mut()
                .expect("checked just above")
                .encode(frame)
            {
                Ok(au) => {
                    self.pending.push_back(au);
                    return Ok(());
                }
                Err(Fail::Dead(e)) => {
                    // Worker gone. Surface it so the host rebuild path runs; `reset` is the one
                    // respawn.
                    self.link = None;
                    return Err(e.context("pyrowave encode worker"));
                }
                Err(Fail::Encode(message)) => {
                    // Worker alive but refused this frame. Re-run in-process: the recovery
                    // machinery (raw-dmabuf degrade latch is a host-process static) lives here.
                    self.pin_inline(&format!("the worker failed a frame: {message}"));
                }
            }
        }
        self.inline_mut()?.submit(frame)
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        // Every PyroWave AU is a keyframe, so there is no per-frame reference bookkeeping to pin
        // to the wire index.
        let _ = wire_index;
        self.submit(frame)
    }

    fn caps(&self) -> EncoderCaps {
        // Once in-process, the live encoder is authoritative; before that, the values the worker
        // reported for the encoder it opened.
        match self.inline.as_ref() {
            Some(e) => e.caps(),
            None => self.caps,
        }
    }

    fn request_keyframe(&mut self) {
        // Intra-only: every AU is already a keyframe.
    }

    fn set_hdr_meta(&mut self, _meta: Option<pf_frame::HdrMeta>) {
        // PyroWave carries no VUI/SEI grade — the colour contract is the CSC shader's.
    }

    fn invalidate_ref_frames(&mut self, _first_frame: i64, _last_frame: i64) -> bool {
        // No references to invalidate.
        false
    }

    fn distrust_references(&mut self) {
        // Intra-only: no RFI anchor trust to withdraw. Written out rather than inherited because
        // this is the proxy — an unforwarded default would mean the worker never hears the call.
        // The trait-coverage test below exists to catch that. The no-op is a visible decision.
    }

    fn set_reference_floor(&mut self, _acked_wire: Option<i64>) {
        // Intra-only: no frame references another.
    }

    fn set_pipelined(&mut self, _on: bool) -> bool {
        // Encode is synchronous; no pipelined-retrieve mode.
        false
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.stream.check_whole_poll()?;
        self.poll_whole()
    }

    /// Encode is synchronous on both rungs, so `poll` never blocks on hardware and there is
    /// no handle to park on. Written out for the same reason as `ready_aus`.
    fn ready_event(&self) -> Option<isize> {
        None
    }

    fn supports_chunked_poll(&self) -> bool {
        self.stream.supports_chunked_poll()
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        // Streamed-AU cut needs no protocol. `submit` is synchronous, so an AU in `pending` is
        // complete and the same cut runs here. The cursor is the proxy's in both modes (the
        // fallback's `poll_chunk` is never called), so exactly one can be open.
        if let Some(chunk) = self.stream.next_open() {
            return Ok(Some(chunk));
        }
        Ok(self.poll_whole()?.and_then(|f| self.stream.cut(f)))
    }

    fn reset(&mut self) -> bool {
        // A rebuild forfeits every in-flight frame, including a half-handed-out AU — drop the
        // cursor first so the next `poll_chunk` cannot splice a dead AU's tail onto a fresh one.
        self.stream.reset();
        self.pending.clear();
        if let Some(link) = self.link.as_mut() {
            // A Reset message, not a respawn: the expensive thing is the priority-elevated
            // device. A respawn would re-run the global-priority ladder and could land on a
            // different class than this session was measured at. A wedged worker times this
            // out and falls through to the one-respawn rung below.
            match link.request(&ToWorker::Reset, &[]) {
                Ok(FromWorker::Ack { ok }) => return ok,
                Ok(other) => {
                    tracing::warn!(?other, "pyrowave: unexpected encode worker reply to Reset");
                    self.link = None;
                }
                Err(e) => {
                    tracing::warn!(
                        worker = %self.worker_path.display(),
                        error = %format!("{e:#}"),
                        "pyrowave: the encode worker died mid-session — rebuilding the encoder"
                    );
                    self.link = None;
                }
            }
        }
        if self.link.is_none() && self.inline.is_none() && !self.respawn_used {
            self.respawn_used = true;
            match spawn_link(&self.worker_path, &self.params, self.bitrate_bps) {
                Ok(hs) => {
                    self.caps = hs.caps;
                    self.link = Some(hs.link);
                    if let Some(shard) = self.stream.wire_chunk {
                        // Same replay the in-process fallback does: the boundary changes AU bytes.
                        self.set_wire_chunking(shard);
                    }
                    tracing::info!(
                        worker = %self.worker_path.display(),
                        priority = ?hs.priority,
                        "pyrowave: respawned the encode worker after a mid-session death"
                    );
                    return true;
                }
                Err(e) => tracing::warn!(
                    worker = %self.worker_path.display(),
                    error = %format!("{e:#}"),
                    "pyrowave: the encode worker would not respawn — encoding in-process at \
                     default GPU priority for the rest of this session"
                ),
            }
        }
        let already_open = self.inline.is_some();
        match self.inline_mut() {
            // A freshly opened in-process encoder is the rebuild the caller asked for.
            Ok(e) => {
                if already_open {
                    e.reset()
                } else {
                    true
                }
            }
            Err(e) => {
                tracing::error!(
                    error = %format!("{e:#}"),
                    "pyrowave: no encoder left after the worker went away — the session cannot \
                     recover"
                );
                false
            }
        }
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Kept regardless of which half is live: a later fallback opens at the rate ABR settled
        // on, not the session start rate.
        self.bitrate_bps = bps;
        if let Some(link) = self.link.as_mut() {
            match link.request(&ToWorker::Reconfigure { bitrate_bps: bps }, &[]) {
                Ok(FromWorker::Ack { ok }) => return ok,
                Ok(other) => {
                    tracing::warn!(
                        ?other,
                        "pyrowave: unexpected encode worker reply to Reconfigure"
                    )
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "pyrowave: the encode worker did not accept a bitrate retarget"
                ),
            }
            // Report failure and let ABR use its rebuild path, which lands on `reset`.
            return false;
        }
        match self.inline.as_mut() {
            Some(e) => e.reconfigure_bitrate(bps),
            // Nothing is open yet; the new rate is now the one the open will use.
            None => true,
        }
    }

    fn bitrate_retarget_is_synchronous(&self) -> bool {
        // `Reconfigure` blocks for the worker's Ack, which it sends only after its own
        // `reconfigure_bitrate` returns; the in-process fallback applies the rate in-call.
        true
    }

    fn retarget_settled(&self) -> bool {
        // Synchronous, as above: a retarget has settled once `reconfigure_bitrate` returns.
        true
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        // PyroWave applies the requested rate as a per-frame byte budget with no internal clamp
        // to report.
        None
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        // Same floor as the in-process impl, so mirrored state and the worker's cannot disagree
        // about whether chunking is on.
        if !self.stream.set_chunking(shard_payload) {
            return;
        }
        if let Some(link) = self.link.as_mut() {
            // Must cross: it changes the packetize boundary and the rate budget (AU bytes).
            // Only the streamed-AU cut stays host-side.
            if let Err(e) = link.request(&ToWorker::SetWireChunking { shard_payload }, &[]) {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "pyrowave: the encode worker did not accept the datagram-aligned boundary"
                );
            }
            // Not the in-process sentence: the worker emits that one itself, and two identical
            // lines from two processes read as a bug. This is the host-ring copy.
            tracing::info!(
                shard_payload,
                "pyrowave: datagram-aligned packetization forwarded to the encode worker \
                 (partial-frame loss mode)"
            );
        }
        if let Some(e) = self.inline.as_mut() {
            e.set_wire_chunking(shard_payload);
        }
    }

    fn set_send_spread_us(&mut self, _us: u32) {
        // Only the direct-NVENC split arbitration consumes this; PyroWave never splits.
    }

    fn set_input_ring_depth(&mut self, _depth: usize) {
        // The encoder imports the capture dmabuf and CSCs it into its own images, so the
        // capturer's ring depth constrains nothing here.
    }

    fn set_input_crop(&mut self, _rect: [u32; 4]) -> Result<()> {
        // `crops_input` is false: the host never asks a worker to crop.
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        // Nothing is in flight across the socket: `submit` returns only once the AU is in
        // `pending`. Unlike the in-process encoder, whose `submit`/`poll` split leaves a fence.
        match self.inline.as_mut() {
            Some(e) => e.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::worker::GrantedClass;
    use super::*;
    use pf_frame::{CursorOverlay, DmabufFrame, PixelFormat};
    use std::os::fd::AsFd;
    use std::sync::Arc;

    fn params() -> Params {
        Params {
            width: 1920,
            height: 1080,
            fps: 60,
            chroma: ChromaFormat::Yuv420,
            bit_depth: 8,
            hdr: false,
        }
    }

    /// A `Link` wired to a socket the test drives itself — the only way to exercise the per-frame
    /// path without a GPU.
    fn mock_link(sock: OwnedFd, au_buf: File) -> Link {
        Link {
            sock,
            child: None,
            au_buf: Some(au_buf),
            rbuf: Vec::new(),
            sent_keys: HashSet::new(),
            cursor_serial: None,
        }
    }

    /// [`handshake`] against a socket instead of a spawned child.
    fn handshake_on(sock: OwnedFd) -> Result<Handshake> {
        handshake(mock_link(sock, File::from(memfd())), &params(), 40_000_000)
    }

    /// A dmabuf-shaped frame whose "dmabuf" is a memfd: the host half only fstats the descriptor
    /// and passes it, so this exercises the real key/cache/`SCM_RIGHTS` path.
    fn frame(fd: OwnedFd, cursor: Option<CursorOverlay>) -> CapturedFrame {
        CapturedFrame {
            provenance: Default::default(),
            width: 1920,
            height: 1080,
            pts_ns: 42,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Dmabuf(DmabufFrame {
                fd,
                fourcc: 0x3432_5258,
                modifier: 0,
                plane1: None,
                offset: 0,
                stride: 1920 * 4,
                hold: None,
                health: pf_zerocopy::zero_copy_health(42),
                rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            cursor,
        }
    }

    fn memfd() -> OwnedFd {
        let (f, _) = worker::cursor_upload(&[]).unwrap();
        OwnedFd::from(f)
    }

    #[test]
    fn path_resolution_table() {
        let here = Path::new("/opt/punktfunk/bin");
        let installed = here.join(WORKER_BIN);
        let on_path = Path::new("/usr/bin").join(WORKER_BIN);
        let exists = {
            let (a, b) = (installed.clone(), on_path.clone());
            move |p: &Path| p == a || p == b
        };

        // `off`, in every spelling the row promises.
        for v in ["off", "OFF", " Off "] {
            assert_eq!(
                resolve_worker_path_in(Some(v), Some(here), Some("/usr/bin"), &exists),
                WorkerPath::Off
            );
        }
        // An explicit path wins over both discoveries — the NixOS wrapper case.
        assert_eq!(
            resolve_worker_path_in(
                Some("/run/wrappers/bin/punktfunk-encode-worker"),
                Some(here),
                Some("/usr/bin"),
                &exists
            ),
            WorkerPath::Found("/run/wrappers/bin/punktfunk-encode-worker".into())
        );
        // Not existence-checked, so a typo surfaces as a spawn failure naming the path.
        assert_eq!(
            resolve_worker_path_in(
                Some("/nope/pf-worker"),
                Some(here),
                Some("/usr/bin"),
                &exists
            ),
            WorkerPath::Found("/nope/pf-worker".into())
        );
        // Empty/whitespace reads as unset, not as a path.
        assert_eq!(
            resolve_worker_path_in(Some("  "), Some(here), Some("/usr/bin"), &exists),
            WorkerPath::Found(installed.clone())
        );
        // Beside the host binary beats PATH.
        assert_eq!(
            resolve_worker_path_in(None, Some(here), Some("/usr/bin"), &exists),
            WorkerPath::Found(installed)
        );
        // Then PATH, entry by entry, skipping empties.
        assert_eq!(
            resolve_worker_path_in(
                None,
                Some(Path::new("/nowhere")),
                Some(":/nope:/usr/bin"),
                &exists
            ),
            WorkerPath::Found(on_path)
        );
        // Nothing anywhere: the missing-binary rung.
        assert_eq!(
            resolve_worker_path_in(None, Some(Path::new("/nowhere")), Some("/nope"), &exists),
            WorkerPath::Missing
        );
        assert_eq!(
            resolve_worker_path_in(None, None, None, &exists),
            WorkerPath::Missing
        );
    }

    /// A binary that exists, execs, and exits at once. Resolved off `PATH` rather than hardcoded
    /// to `/bin/false`: NixOS ships only `/bin/sh` in `/bin`, and `PinnedExe::open` needs a real
    /// path (a bare name would fail OPEN and take the spawn-failure rung).
    fn a_binary_that_exits_immediately() -> PathBuf {
        std::env::var_os("PATH")
            .as_deref()
            .map(std::env::split_paths)
            .into_iter()
            .flatten()
            .map(|d| d.join("false"))
            .find(|p| p.is_file())
            .expect("a `false` binary on PATH")
    }

    /// Ladder rung: the binary exists and runs but is not a worker — the same rung a worker that
    /// dies during Vulkan bring-up takes. Which half of the handshake reports the death is a
    /// race this test does not pin: send-then-EOF vs EPIPE. What is pinned is that both name
    /// the handshake.
    #[test]
    fn a_worker_that_exits_immediately_is_a_handshake_failure() {
        let err =
            spawn_link(&a_binary_that_exits_immediately(), &params(), 40_000_000).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("handshake"),
            "the rung must name the handshake: {text}"
        );
    }

    /// EPIPE half of the rung above, without the race: the peer is closed before Hello, so the
    /// send cannot succeed. Fails if the send's context is trimmed to a bare "send Hello".
    #[test]
    fn a_worker_that_died_before_hello_still_names_the_handshake() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        drop(peer); // the worker is gone before the host writes a byte
        let err = handshake_on(host).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("handshake"),
            "the rung must name the handshake even when the death beats the Hello: {text}"
        );
    }

    /// Ladder rung: a binary that does not exist — operator-set path typo, or a half-installed
    /// package.
    #[test]
    fn a_missing_binary_is_a_spawn_failure() {
        let err = spawn_link(
            Path::new("/nonexistent/punktfunk-encode-worker"),
            &params(),
            40_000_000,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("punktfunk-encode-worker"));
    }

    /// Ladder rung: the worker started, spoke, and did not open its encoder. An answer, not a
    /// death, so the host encodes in-process.
    #[test]
    fn an_init_error_fails_the_handshake_without_looking_like_a_death() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None);
            worker::send_eintr(
                peer.as_fd(),
                &FromWorker::InitErr {
                    message: "GPU lacks pyrowave-required Vulkan features".into(),
                },
                &[],
            )
            .unwrap();
        });
        let err = handshake_on(host).unwrap_err();
        server.join().unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains("did not open its encoder") && text.contains("Vulkan features"),
            "the rung must carry the worker's own diagnosis: {text}"
        );
    }

    /// Ladder rung: a dead worker gets one respawn from `reset`, then the session is in-process.
    /// Driven with a dead socket and `/bin/false` so the respawn attempt is real and fails. A
    /// proxy that retried every `reset` would burn the host's five-reset recovery budget.
    #[test]
    fn reset_respawns_the_worker_at_most_once() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        drop(peer); // the worker is already gone
        let mut proxy = RemotePyroWave {
            link: Some(mock_link(host, File::from(memfd()))),
            inline: None,
            params: params(),
            bitrate_bps: 40_000_000,
            // Spawns, then exits without a handshake — the respawn attempt runs for real and
            // fails, which is the case this budget exists for.
            worker_path: PathBuf::from("/bin/false"),
            caps: EncoderCaps::default(),
            stream: AuStream::default(),
            pending: VecDeque::new(),
            respawn_used: false,
        };
        // Reply never comes: the link dies, the one respawn is spent and fails, session in-process.
        let _ = proxy.reset();
        assert!(proxy.respawn_used, "the one respawn must have been spent");
        assert!(proxy.link.is_none(), "/bin/false cannot become a worker");
        // Every later reset stays in-process — the budget is spent, not renewed.
        let _ = proxy.reset();
        assert!(proxy.respawn_used);
        assert!(proxy.link.is_none());
    }

    /// Ladder rung: a CPU-backed frame. It reaches this encoder (4:4:4 with zero-copy off, or
    /// after the raw-dmabuf degrade latch), and ~8 MB per frame must never cross the socket.
    /// Asserts the classification (link dropped), not the submit result, which needs a Vulkan
    /// device for the in-process encoder.
    #[test]
    fn a_cpu_frame_leaves_the_worker_for_the_rest_of_the_session() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let mut proxy = RemotePyroWave {
            link: Some(mock_link(host, File::from(memfd()))),
            inline: None,
            params: Params {
                width: 64,
                height: 64,
                fps: 60,
                chroma: ChromaFormat::Yuv420,
                bit_depth: 8,
                hdr: false,
            },
            bitrate_bps: 5_000_000,
            worker_path: PathBuf::from("/usr/bin/punktfunk-encode-worker"),
            caps: EncoderCaps::default(),
            stream: AuStream::default(),
            pending: VecDeque::new(),
            respawn_used: false,
        };
        let cpu = CapturedFrame {
            provenance: Default::default(),
            width: 64,
            height: 64,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(vec![0u8; 64 * 64 * 4]),
            cursor: None,
        };
        let _ = proxy.submit(&cpu);
        assert!(
            proxy.link.is_none(),
            "a non-dmabuf payload must pin the session in-process"
        );
        // Nothing was ever asked of the worker: the rung fires before the socket is touched.
        drop(proxy);
        let mut buf = Vec::new();
        assert_eq!(
            worker::recv_eintr::<ToWorker>(
                peer.as_fd(),
                &mut buf,
                Some(Duration::from_millis(200))
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    /// Ladder rung: proto/workspace skew. Host and worker are different files, so this is a real
    /// deployment state and must land on the in-process encoder rather than a broken session.
    #[test]
    fn version_skew_fails_the_handshake() {
        for (proto, version) in [
            (
                worker::PROTO_VERSION + 1,
                worker::WORKSPACE_VERSION.to_string(),
            ),
            (worker::PROTO_VERSION, "0.0.1-stale".to_string()),
        ] {
            let (host, peer) = ipc::socketpair_seqpacket().unwrap();
            let server = std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None);
                worker::send_eintr(
                    peer.as_fd(),
                    &FromWorker::Ready {
                        proto,
                        workspace_version: version,
                        priority: PriorityOutcome::Granted(GrantedClass::Realtime),
                        device: "mock".into(),
                        chroma444: false,
                        blends_cursor: true,
                    },
                    &[],
                )
                .unwrap();
            });
            let err = handshake_on(host).unwrap_err();
            server.join().unwrap();
            assert!(
                format!("{err:#}").contains("version skew"),
                "unexpected error: {err:#}"
            );
        }
    }

    /// Per-frame path, host side: the fd crosses once, the cursor bitmap only when its serial
    /// changes, and the AU comes back through the memfd rather than the message body.
    #[test]
    fn frames_pass_the_fd_once_and_the_au_comes_back_through_the_buffer() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let au_buf = File::from(memfd());
        let au = vec![0x7Eu8; 300_000];
        let expect = au.clone();
        let server_buf = au_buf.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut seen: Vec<(bool, Option<usize>)> = Vec::new();
            for _ in 0..3 {
                let (msg, fds) =
                    worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None).unwrap();
                let ToWorker::Frame {
                    key,
                    has_fd,
                    cursor,
                    ..
                } = msg
                else {
                    panic!("expected a frame");
                };
                let want = usize::from(has_fd)
                    + usize::from(cursor.as_ref().is_some_and(|c| c.upload.is_some()));
                assert_eq!(fds.len(), want, "descriptor count must match the flags");
                seen.push((has_fd, cursor.and_then(|c| c.upload)));
                server_buf.write_all_at(&expect, 0).unwrap();
                worker::send_eintr(
                    peer.as_fd(),
                    &FromWorker::Au {
                        key,
                        len: expect.len(),
                        pts_ns: 42,
                        keyframe: true,
                        chunk_aligned: false,
                        encode_us: 4400,
                    },
                    &[],
                )
                .unwrap();
            }
            seen
        });

        let mut link = mock_link(host, au_buf);
        let cursor = |serial: u64| {
            Some(CursorOverlay {
                x: 1,
                y: 2,
                w: 32,
                h: 32,
                rgba: Arc::new(vec![9u8; 32 * 32 * 4]),
                serial,
                hot_x: 0,
                hot_y: 0,
                visible: true,
            })
        };
        // Same buffer three times (one memfd, re-passed), with the cursor bitmap changing once.
        let dmabuf = memfd();
        for c in [cursor(1), cursor(1), cursor(2)] {
            let f = frame(dmabuf.try_clone().unwrap(), c);
            let got = match link.encode(&f) {
                Ok(au) => au,
                Err(Fail::Dead(e)) => panic!("worker died: {e:#}"),
                Err(Fail::Encode(m)) => panic!("encode error: {m}"),
            };
            assert_eq!(got.data, au, "the AU must arrive byte-for-byte");
            assert_eq!(got.pts_ns, 42);
            assert!(got.keyframe);
        }
        let seen = server.join().unwrap();
        assert_eq!(
            seen,
            vec![
                (true, Some(32 * 32 * 4)),
                (false, None),
                (false, Some(32 * 32 * 4))
            ],
            "the dmabuf fd crosses once; the cursor crosses only on a serial change"
        );
    }

    /// A `NeedFd` (worker evicted the key) is answered by one retry carrying the fd again.
    #[test]
    fn need_fd_resends_the_descriptor_once() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let au_buf = File::from(memfd());
        let server_buf = au_buf.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut flags = Vec::new();
            for i in 0..2 {
                let (msg, fds) =
                    worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None).unwrap();
                let ToWorker::Frame { key, has_fd, .. } = msg else {
                    panic!("expected a frame");
                };
                flags.push((has_fd, fds.len()));
                let reply = if i == 0 {
                    FromWorker::NeedFd
                } else {
                    server_buf.write_all_at(&[1, 2, 3], 0).unwrap();
                    FromWorker::Au {
                        key,
                        len: 3,
                        pts_ns: 0,
                        keyframe: true,
                        chunk_aligned: false,
                        encode_us: 1,
                    }
                };
                worker::send_eintr(peer.as_fd(), &reply, &[]).unwrap();
            }
            flags
        });
        let mut link = mock_link(host, au_buf);
        // Pretend the key already crossed, so the first attempt sends no descriptor.
        let dmabuf = memfd();
        link.sent_keys.insert(dmabuf_key(dmabuf.as_fd()).unwrap());
        let f = frame(dmabuf, None);
        let au = match link.encode(&f) {
            Ok(au) => au,
            Err(Fail::Dead(e)) => panic!("worker died: {e:#}"),
            Err(Fail::Encode(m)) => panic!("encode error: {m}"),
        };
        assert_eq!(au.data, vec![1, 2, 3]);
        assert_eq!(server.join().unwrap(), vec![(false, 0), (true, 1)]);
    }

    /// Ladder rung: the worker dies mid-stream. The transport error must classify as `Dead`
    /// (host rebuild, then one respawn) and never as a per-frame encode error.
    #[test]
    fn a_worker_that_dies_mid_stream_is_a_transport_death() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        drop(peer);
        let mut link = mock_link(host, File::from(memfd()));
        let f = frame(memfd(), None);
        match link.encode(&f) {
            Err(Fail::Dead(_)) => {}
            Err(Fail::Encode(m)) => panic!("a dead socket must not read as an encode error: {m}"),
            Ok(_) => panic!("a dead socket must not produce an AU"),
        }
    }

    /// A live worker refusal stays an encode error and forwards its capture-rebuild verdict.
    #[test]
    fn a_refused_frame_is_an_encode_error() {
        let (host, peer) = ipc::socketpair_seqpacket().unwrap();
        let server = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = worker::recv_eintr::<ToWorker>(peer.as_fd(), &mut buf, None).unwrap();
            worker::send_eintr(
                peer.as_fd(),
                &FromWorker::EncodeErr {
                    message: "unsupported dmabuf fourcc".into(),
                    capture_rebuild: true,
                },
                &[],
            )
            .unwrap();
        });
        let mut link = mock_link(host, File::from(memfd()));
        let mut f = frame(memfd(), None);
        let FramePayload::Dmabuf(d) = &mut f.payload else {
            unreachable!()
        };
        d.modifier = 7;
        let rebuild = d.rebuild.clone();
        match link.encode(&f) {
            Err(Fail::Encode(m)) => assert!(m.contains("fourcc")),
            Err(Fail::Dead(e)) => panic!("a live worker's refusal must not read as death: {e:#}"),
            Ok(_) => panic!("a refusal must not produce an AU"),
        }
        assert!(rebuild.load(std::sync::atomic::Ordering::Relaxed));
        server.join().unwrap();
    }

    /// Every `Encoder` trait method must be written here. The proxy has two backends, so an
    /// unforwarded default silently disables a feature only on worker-backed sessions.
    #[test]
    fn the_proxy_writes_every_trait_method() {
        crate::smoke_pattern::assert_writes_every_encoder_method(
            include_str!("pyrowave_remote.rs"),
            "impl Encoder for RemotePyroWave {",
        );
    }
}
