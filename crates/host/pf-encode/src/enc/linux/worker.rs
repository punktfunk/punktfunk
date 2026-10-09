//! Worker half of `punktfunk-encode-worker` and the host↔worker vocabulary
//! (design: `design/gpu-priority-capability-worker.md`). Host half:
//! [`super::pyrowave_remote`].
//!
//! PyroWave encodes on the same shader cores a game saturates. The only preemption
//! lever is an elevated `VK_KHR_global_priority` queue, granted only to a process
//! holding `CAP_SYS_NICE`. The host must never hold that cap: KWin identifies a
//! client by `readlink /proc/<pid>/exe`, and the kernel refuses the readlink unless
//! the reader's effective set is a superset of the target's PERMITTED set. The cap
//! lives here: no Wayland, no D-Bus, no network, one socket to the parent.
//!
//! Separate executable — never a hardlink of the host, never a subcommand of it.
//! A shared inode shares the file capability and recreates the unidentifiable host.
//!
//! One worker per session; every host→worker message gets exactly one reply. AUs
//! and cursor bitmaps ride memfds (`ipc::MAX_MSG` is 64 KiB; serde_json `Vec<u8>`
//! cannot fit a frame). The AU memfd crosses once in [`FromWorker::Ready`]. Dmabuf
//! fds cache by `key`; the steady state passes zero descriptors.

use anyhow::{Context, Result};
use pf_frame::{CapturedFrame, CursorOverlay, DmabufFrame, FramePayload, PixelFormat};
use pf_zerocopy::ipc;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bumped on any wire change. Host and worker are different files, so a lockstep
/// miss must fall back to the in-process encoder, never a dead session.
pub(crate) const PROTO_VERSION: u32 = 2;

/// Compile-time `CARGO_PKG_VERSION` of this crate. Handshake compares it too: a
/// protocol can stay still while the encoder moves, and a stale worker binary
/// still carries its own older string.
pub(crate) const WORKSPACE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// PipeWire pools are ≤ ~16 buffers. Eviction is recoverable ([`FromWorker::NeedFd`]).
const FD_CACHE_CAP: usize = 64;

/// The encoder's cursor texture: `CURSOR_MAX`² RGBA.
const CURSOR_UPLOAD_MAX: usize = (super::vk_csc::CURSOR_MAX as usize).pow(2) * 4;

/// What `VK_KHR_global_priority` produced. Logged on the host: the worker's
/// `tracing` is inherited stderr, and the in-process INERT wording names the
/// host binary.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PriorityOutcome {
    Granted(GrantedClass),
    /// Normal for an uncapped worker.
    Refused,
    /// `PYROWAVE_QUEUE_PRIORITY=off`, or no global-priority extension. Never warned.
    NotRequested,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantedClass {
    Realtime,
    High,
}

/// Hand-written [`pf_frame::PixelFormat`] mirror: pf-frame has no serde, and the
/// exhaustive `match` makes a new capture format a compile error here.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireFormat {
    Bgrx,
    Rgbx,
    Bgra,
    Rgba,
    Rgb,
    Bgr,
    Rgb10a2,
    Nv12,
    P010,
    Yuv444,
    X2Rgb10,
    X2Bgr10,
}

impl From<PixelFormat> for WireFormat {
    fn from(f: PixelFormat) -> WireFormat {
        match f {
            PixelFormat::Bgrx => WireFormat::Bgrx,
            PixelFormat::Rgbx => WireFormat::Rgbx,
            PixelFormat::Bgra => WireFormat::Bgra,
            PixelFormat::Rgba => WireFormat::Rgba,
            PixelFormat::Rgb => WireFormat::Rgb,
            PixelFormat::Bgr => WireFormat::Bgr,
            PixelFormat::Rgb10a2 => WireFormat::Rgb10a2,
            // Windows-only (IDD-push 10-bit SDR, and the HDR desktop's FP16). The Linux
            // worker never sees them; the wire has no variant.
            PixelFormat::Rgb10a2Sdr | PixelFormat::RgbaF16 => {
                unreachable!("{f:?} is a Windows capture format — the Linux worker never sees it")
            }
            PixelFormat::Nv12 => WireFormat::Nv12,
            PixelFormat::P010 => WireFormat::P010,
            PixelFormat::Yuv444 => WireFormat::Yuv444,
            PixelFormat::X2Rgb10 => WireFormat::X2Rgb10,
            PixelFormat::X2Bgr10 => WireFormat::X2Bgr10,
        }
    }
}

impl From<WireFormat> for PixelFormat {
    fn from(f: WireFormat) -> PixelFormat {
        match f {
            WireFormat::Bgrx => PixelFormat::Bgrx,
            WireFormat::Rgbx => PixelFormat::Rgbx,
            WireFormat::Bgra => PixelFormat::Bgra,
            WireFormat::Rgba => PixelFormat::Rgba,
            WireFormat::Rgb => PixelFormat::Rgb,
            WireFormat::Bgr => PixelFormat::Bgr,
            WireFormat::Rgb10a2 => PixelFormat::Rgb10a2,
            WireFormat::Nv12 => PixelFormat::Nv12,
            WireFormat::P010 => PixelFormat::P010,
            WireFormat::Yuv444 => PixelFormat::Yuv444,
            WireFormat::X2Rgb10 => PixelFormat::X2Rgb10,
            WireFormat::X2Bgr10 => PixelFormat::X2Bgr10,
        }
    }
}

/// [`pf_frame::CursorOverlay`] without pixels. `upload: Some(len)` is a fresh
/// memfd of `len` bytes (bitmap `serial` changed); `None` reuses the cached
/// bitmap for `serial`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct WireCursor {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub serial: u64,
    pub hot_x: u32,
    pub hot_y: u32,
    pub visible: bool,
    pub upload: Option<usize>,
}

/// host → worker. Every variant has exactly one reply.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) enum ToWorker {
    /// Open the encoder. [`FromWorker::Ready`] (AU memfd) or [`FromWorker::InitErr`].
    ///
    /// `priority_intent` is the host-resolved `PYROWAVE_QUEUE_PRIORITY` (`None` =
    /// REALTIME→HIGH, HIGH on NVIDIA). Forwarded explicitly: the worker strips the
    /// variable so one knob cannot mean two things across the process boundary.
    Hello {
        proto: u32,
        workspace_version: String,
        /// Host `PUNKTFUNK_RENDER_NODE` (`None` = unset). Log-only: device
        /// selection ignores every node anchor (`pyrowave.rs::select_physical_device`).
        drm_node: Option<String>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma444: bool,
        /// Negotiated stream depth (8 or 10) and the BT.2020 PQ flag — the encoder's
        /// CSC shader, plane formats, and colour stamp all follow them.
        bit_depth: u8,
        hdr: bool,
        priority_intent: Option<String>,
    },
    /// Encode one frame. The dmabuf fd rides as `SCM_RIGHTS` only on first sight
    /// of `key` (`has_fd`); a cursor upload, when present, is the fd after it.
    Frame {
        key: u64,
        has_fd: bool,
        fourcc: u32,
        modifier: u64,
        offset: u32,
        stride: u32,
        plane1: Option<(u32, u32)>,
        width: u32,
        height: u32,
        pts_ns: u64,
        format: WireFormat,
        cursor: Option<WireCursor>,
    },
    /// `Encoder::set_wire_chunking`. Crosses because it changes AU bytes and the
    /// rate budget; streamed-AU cutting stays host-side.
    SetWireChunking {
        shard_payload: usize,
    },
    Reconfigure {
        bitrate_bps: u64,
    },
    /// `Encoder::reset` inside the worker so the priority-elevated device survives.
    /// Why a message, not a respawn: [`super::pyrowave_remote::RemotePyroWave::reset`].
    Reset,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub(crate) enum FromWorker {
    /// Encoder is open. Carries the AU memfd as its single `SCM_RIGHTS` descriptor.
    ///
    /// `proto` and `workspace_version` are the first two fields and must never be
    /// renamed: a version-skewed pair diagnoses itself from those.
    Ready {
        proto: u32,
        workspace_version: String,
        priority: PriorityOutcome,
        device: String,
        /// Encoder's real chroma and cursor blend (`EncoderCaps`). The proxy must
        /// not guess: a hardcoded default mis-reports a 4:4:4 open.
        chroma444: bool,
        blends_cursor: bool,
    },
    /// Open failed — an answer, not a crash. Host falls back in-process.
    InitErr {
        message: String,
    },
    /// One access unit at offset 0 of the AU memfd. Also the buffer-release
    /// signal: 1:1 with `Encoder::submit`'s lifetime (caller holds the frame
    /// until its AU returns from `poll`).
    Au {
        key: u64,
        len: usize,
        pts_ns: u64,
        keyframe: bool,
        chunk_aligned: bool,
        encode_us: u32,
    },
    /// No cached fd for `key`. Host forgets "already sent" and retries once with the fd.
    NeedFd,
    /// This frame failed; the worker is still alive. `capture_rebuild` feeds a
    /// dmabuf submit failure to the host's identity-scoped fallback.
    EncodeErr {
        message: String,
        capture_rebuild: bool,
    },
    Ack {
        ok: bool,
    },
}

/// [`ipc::recv_fds`] that survives a signal.
///
/// A timeout-armed socket returns **EINTR**, not `ERESTARTSYS`; `SA_RESTART`
/// does not apply. pf-zerocopy maps any recv error to "the worker died", so
/// one signal would drop a healthy session. Retry on the remaining budget —
/// resetting the full timeout would let a periodic signal hang forever.
/// `budget = None` blocks until the host speaks or closes.
pub(crate) fn recv_eintr<T: DeserializeOwned>(
    sock: BorrowedFd,
    buf: &mut Vec<u8>,
    budget: Option<Duration>,
) -> io::Result<(T, Vec<OwnedFd>)> {
    let deadline = budget.map(|d| Instant::now() + d);
    loop {
        if let Some(deadline) = deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            // Under a microsecond `set_recv_timeout` clears the timeout: that is spent too.
            if left < Duration::from_micros(1) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "encode worker did not answer within its budget",
                ));
            }
            ipc::set_recv_timeout(sock, Some(left))?;
        }
        match ipc::recv_fds::<T>(sock, buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// [`ipc::send_fds`] that survives a signal. A small body on a socket whose peer
/// is actively reading does not block, so this normally retries never.
pub(crate) fn send_eintr<T: Serialize>(
    sock: BorrowedFd,
    msg: &T,
    fds: &[BorrowedFd],
) -> io::Result<()> {
    loop {
        match ipc::send_fds(sock, msg, fds) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// Grows on `pwrite`; callers never size it.
fn memfd(name: &CStr) -> io::Result<File> {
    Ok(rustix::fs::memfd_create(name, rustix::fs::MemfdFlags::CLOEXEC)?.into())
}

pub(crate) fn cursor_upload(rgba: &[u8]) -> io::Result<(File, usize)> {
    let n = rgba.len().min(CURSOR_UPLOAD_MAX);
    let f = memfd(c"pf-encode-cursor")?;
    f.write_all_at(&rgba[..n], 0)?;
    Ok((f, n))
}

pub fn run_from_args(args: &[String]) -> Result<()> {
    // Core dumps ON, and first: the kernel clears `PR_SET_DUMPABLE` when a process
    // gains a file capability. This process fronts nothing, so a GPU-driver crash
    // is exactly what we want a core for. Dumpable is not the KWin identifiability
    // gate; the PERMITTED set is, and this process never speaks Wayland.

    // SAFETY: `prctl(PR_SET_DUMPABLE, 1)` takes integers by value, touches no Rust memory and
    // affects only this process.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 1);
    }
    // The host execs via a pinned `/proc/self/fd/<n>`, so the kernel would name us
    // after a meaningless fd number.

    // SAFETY: `PR_SET_NAME` copies at most 16 bytes from the given pointer; the C-string literal is
    // valid, NUL-terminated and short enough, and no pointer is retained past the call.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"pf-encode-wk".as_ptr());
    }
    sanitize_env();
    // Real `setpriority`: without `CAP_SYS_NICE`/`RLIMIT_NICE` it is a silent
    // no-op. The worker is single-threaded, so this is the encode thread.
    pf_frame::thread_qos::boost_thread_priority(true);

    run(ipc::adopt_spawned_socket(args)?)
}

/// Drop env this process must not act on. A denylist, not an allowlist: the
/// Vulkan loader finds ICDs through the environment, so a strict allowlist
/// leaves the worker with no GPU (NixOS). Strip the priority intent (it arrives
/// in `Hello`) and the worker path (nothing here spawns a worker).
fn sanitize_env() {
    for k in ["PYROWAVE_QUEUE_PRIORITY", "PUNKTFUNK_ENCODE_WORKER"] {
        // SAFETY: single-threaded — this runs before anything in this process creates a thread,
        // which is the one situation where mutating the environment is sound (the `getenv` race
        // `remove_var`'s contract is about needs a second thread).
        unsafe { std::env::remove_var(k) };
    }
}

fn run(sock: OwnedFd) -> Result<()> {
    let mut buf = Vec::new();
    // No timeout on the worker's receives: the host owns the clock. A worker that
    // gave up on its own would look exactly like a crash.
    let (hello, _) = recv_eintr::<ToWorker>(sock.as_fd(), &mut buf, None).context("recv Hello")?;
    let ToWorker::Hello {
        proto,
        workspace_version,
        drm_node,
        width,
        height,
        fps,
        bitrate_bps,
        chroma444,
        bit_depth,
        hdr,
        priority_intent,
    } = hello
    else {
        anyhow::bail!("first message was not Hello");
    };
    if proto != PROTO_VERSION || workspace_version != WORKSPACE_VERSION {
        // Answer, don't crash: the host warns and encodes in-process.
        let _ = send_eintr(
            sock.as_fd(),
            &FromWorker::InitErr {
                message: format!(
                    "version skew: worker proto {PROTO_VERSION} v{WORKSPACE_VERSION}, \
                     host proto {proto} v{workspace_version}"
                ),
            },
            &[],
        );
        return Ok(());
    }

    let enc = match super::pyrowave::PyroWaveEncoder::open_in_worker(
        width,
        height,
        fps,
        bitrate_bps,
        chroma444,
        bit_depth,
        hdr,
        priority_intent.as_deref(),
    ) {
        Ok(e) => e,
        Err(e) => {
            let _ = send_eintr(
                sock.as_fd(),
                &FromWorker::InitErr {
                    message: format!("{e:#}"),
                },
                &[],
            );
            return Ok(());
        }
    };
    let au_buf = memfd(c"pf-encode-au").context("create the AU return buffer")?;
    let caps = crate::Encoder::caps(&enc);
    let ready = FromWorker::Ready {
        proto: PROTO_VERSION,
        workspace_version: WORKSPACE_VERSION.to_string(),
        priority: enc.priority_outcome(),
        device: enc.device_name().to_string(),
        chroma444: caps.chroma_444,
        blends_cursor: caps.blends_cursor,
    };
    send_eintr(sock.as_fd(), &ready, &[au_buf.as_fd()]).context("send Ready")?;
    tracing::info!(
        pid = std::process::id(),
        device = %enc.device_name(),
        priority = ?enc.priority_outcome(),
        host_render_node = ?drm_node,
        "punktfunk-encode-worker ready"
    );
    serve(&sock, enc, &au_buf)
}

/// Request loop. `Ok(())` on host EOF; any other socket error exits, which the
/// host reads as a death.
fn serve(sock: &OwnedFd, mut enc: super::pyrowave::PyroWaveEncoder, au_buf: &File) -> Result<()> {
    use crate::Encoder as _;
    // AUs are laid out in the return buffer itself; `encode_one` only reports the length.
    enc.set_au_arena(au_buf.try_clone().context("dup the AU return buffer")?);
    let mut buf = Vec::new();
    let mut fds: HashMap<u64, OwnedFd> = HashMap::new();
    let mut fd_order: VecDeque<u64> = VecDeque::new();
    // Last uploaded cursor bitmap, by `serial`. A moving pointer re-sends only position.
    let mut cursor_rgba: Option<(u64, Arc<Vec<u8>>)> = None;
    loop {
        let (msg, got) = match recv_eintr::<ToWorker>(sock.as_fd(), &mut buf, None) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e).context("worker recv"),
        };
        let reply = match msg {
            ToWorker::Hello { .. } => FromWorker::EncodeErr {
                message: "duplicate Hello".into(),
                capture_rebuild: false,
            },
            ToWorker::SetWireChunking { shard_payload } => {
                enc.set_wire_chunking(shard_payload);
                FromWorker::Ack { ok: true }
            }
            ToWorker::Reconfigure { bitrate_bps } => FromWorker::Ack {
                ok: enc.reconfigure_bitrate(bitrate_bps),
            },
            ToWorker::Reset => FromWorker::Ack { ok: enc.reset() },
            ToWorker::Frame {
                key,
                has_fd,
                fourcc,
                modifier,
                offset,
                stride,
                plane1,
                width,
                height,
                pts_ns,
                format,
                cursor,
            } => {
                // Sender order: dmabuf (iff `has_fd`), then cursor upload (iff the
                // bitmap changed). Drain before any early return so an extra
                // descriptor is closed with the `Vec` rather than leaked.
                let mut got = got.into_iter();
                let dmabuf = if has_fd { got.next() } else { None };
                let cursor_fd = cursor.as_ref().and_then(|c| c.upload.map(|_| got.next()));
                if let Some(fd) = dmabuf {
                    if fds.insert(key, fd).is_none() {
                        fd_order.push_back(key);
                    }
                    while fd_order.len() > FD_CACHE_CAP {
                        if let Some(old) = fd_order.pop_front() {
                            fds.remove(&old);
                        }
                    }
                }
                match encode_one(
                    &mut enc,
                    au_buf,
                    &fds,
                    &mut cursor_rgba,
                    FrameReq {
                        key,
                        fourcc,
                        modifier,
                        offset,
                        stride,
                        plane1,
                        width,
                        height,
                        pts_ns,
                        format,
                        cursor,
                    },
                    cursor_fd.flatten(),
                ) {
                    Ok(reply) => reply,
                    Err(e) => FromWorker::EncodeErr {
                        message: format!("{e:#}"),
                        capture_rebuild: false,
                    },
                }
            }
        };
        match send_eintr(sock.as_fd(), &reply, &[]) {
            Ok(()) => {}
            // Host vanished between recv and send — same end-of-life as EOF.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(e) => return Err(e).context("worker send"),
        }
    }
}

struct FrameReq {
    key: u64,
    fourcc: u32,
    modifier: u64,
    offset: u32,
    stride: u32,
    plane1: Option<(u32, u32)>,
    width: u32,
    height: u32,
    pts_ns: u64,
    format: WireFormat,
    cursor: Option<WireCursor>,
}

fn encode_one(
    enc: &mut super::pyrowave::PyroWaveEncoder,
    au_buf: &File,
    fds: &HashMap<u64, OwnedFd>,
    cursor_rgba: &mut Option<(u64, Arc<Vec<u8>>)>,
    req: FrameReq,
    cursor_fd: Option<OwnedFd>,
) -> Result<FromWorker> {
    use crate::Encoder as _;
    let Some(cached) = fds.get(&req.key) else {
        return Ok(FromWorker::NeedFd);
    };
    // Dup per frame, not a borrow: `DmabufFrame` owns its fd, and the cache must
    // keep the original so the steady state passes no descriptors.
    let fd = cached.try_clone().context("dup the cached dmabuf fd")?;

    let cursor = match req.cursor {
        Some(c) => {
            match (c.upload, cursor_fd) {
                (Some(len), Some(f)) => {
                    let mut px = vec![0u8; len];
                    File::from(f)
                        .read_exact_at(&mut px, 0)
                        .context("read the cursor upload")?;
                    *cursor_rgba = Some((c.serial, Arc::new(px)));
                }
                // Announced upload with no descriptor. Error, not a shrug: the host
                // marks the serial "sent" on a successful AU, so blending nothing
                // would leave the pointer invisible for the rest of that bitmap.
                (Some(_), None) => anyhow::bail!("cursor upload announced but no descriptor came"),
                (None, _) => {}
            }
            // Serial with no pixels: the host only omits the upload for a serial
            // it has seen acknowledged, so a miss is a desync.
            let Some(rgba) = cursor_rgba
                .as_ref()
                .filter(|(serial, _)| *serial == c.serial)
                .map(|(_, px)| px.clone())
            else {
                anyhow::bail!("no cursor bitmap cached for serial {}", c.serial);
            };
            Some(CursorOverlay {
                x: c.x,
                y: c.y,
                w: c.w,
                h: c.h,
                rgba,
                serial: c.serial,
                hot_x: c.hot_x,
                hot_y: c.hot_y,
                visible: c.visible,
            })
        }
        None => None,
    };
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: req.width,
        height: req.height,
        pts_ns: req.pts_ns,
        format: req.format.into(),
        payload: FramePayload::Dmabuf(DmabufFrame {
            fd,
            fourcc: req.fourcc,
            modifier: req.modifier,
            plane1: req.plane1,
            offset: req.offset,
            stride: req.stride,
            // Hold and the authoritative rebuild signal stay host-side.
            hold: None,
            health: pf_zerocopy::zero_copy_health(0),
            rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
        cursor,
    };
    // submit then poll in one breath: encode is synchronous at depth 1, so the
    // AU is ready when `poll` returns and `frame` is alive across both halves.
    let t0 = Instant::now();
    if let Err(e) = enc.submit(&frame) {
        // Only an import refusal: an OOM or a device loss must not latch the host's capture.
        let rejected = e.downcast_ref::<super::vk_util::ImportRejected>().is_some();
        return Ok(FromWorker::EncodeErr {
            message: format!("{e:#}"),
            capture_rebuild: rejected,
        });
    }
    let Some(au) = enc.poll()? else {
        anyhow::bail!("encoder returned no AU for a submitted frame");
    };
    let encode_us = t0.elapsed().as_micros() as u32;
    // On the arena the AU is already in the buffer; a CPU-side AU still comes as bytes.
    let len = match enc.take_arena_len() {
        Some(n) => n,
        None => {
            au_buf
                .write_all_at(&au.data, 0)
                .context("write the AU into the return buffer")?;
            au.data.len()
        }
    };
    Ok(FromWorker::Au {
        key: req.key,
        len,
        pts_ns: au.pts_ns,
        keyframe: au.keyframe,
        chunk_aligned: au.chunk_aligned,
        encode_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn hello() -> ToWorker {
        ToWorker::Hello {
            proto: PROTO_VERSION,
            workspace_version: WORKSPACE_VERSION.to_string(),
            drm_node: Some("/dev/dri/renderD128".into()),
            width: 3840,
            height: 2160,
            fps: 60,
            bitrate_bps: 400_000_000,
            chroma444: true,
            bit_depth: 10,
            hdr: true,
            priority_intent: Some("realtime".into()),
        }
    }

    /// Pins the message types and the fd order the frame path depends on.
    /// Framing (EOF, timeouts, descriptor cap) is pf-zerocopy's `ipc` tests.
    #[test]
    fn proto_round_trip_both_directions() {
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        ipc::send(a.as_fd(), &hello(), None).unwrap();
        let (got, fds) = ipc::recv_fds::<ToWorker>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, hello());
        assert!(fds.is_empty());

        let frame = ToWorker::Frame {
            key: 0xdead_beef,
            has_fd: true,
            fourcc: 0x3432_5258,
            modifier: 0x0300_0000_0000_1234,
            offset: 0,
            stride: 3840 * 4,
            plane1: None,
            width: 3840,
            height: 2160,
            pts_ns: 1_234_567_890,
            format: WireFormat::Bgrx,
            cursor: Some(WireCursor {
                x: 10,
                y: 20,
                w: 32,
                h: 32,
                serial: 7,
                hot_x: 1,
                hot_y: 2,
                visible: true,
                upload: Some(32 * 32 * 4),
            }),
        };
        let (dma, cur) = (memfd(c"t-dma").unwrap(), memfd(c"t-cur").unwrap());
        ipc::send_fds(a.as_fd(), &frame, &[dma.as_fd(), cur.as_fd()]).unwrap();
        let (got, fds) = ipc::recv_fds::<ToWorker>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, frame);
        assert_eq!(fds.len(), 2);

        let ready = FromWorker::Ready {
            proto: PROTO_VERSION,
            workspace_version: WORKSPACE_VERSION.to_string(),
            priority: PriorityOutcome::Granted(GrantedClass::Realtime),
            device: "NVIDIA GeForce RTX 5070 Ti".into(),
            chroma444: true,
            blends_cursor: true,
        };
        ipc::send(b.as_fd(), &ready, Some(dma.as_fd())).unwrap();
        let (got, fd) = ipc::recv::<FromWorker>(a.as_fd(), &mut buf).unwrap();
        assert_eq!(got, ready);
        assert!(fd.is_some(), "Ready carries the AU return buffer");

        for reply in [
            FromWorker::Au {
                key: 1,
                len: 830_000,
                pts_ns: 5,
                keyframe: true,
                chunk_aligned: true,
                encode_us: 4400,
            },
            FromWorker::NeedFd,
            FromWorker::Ack { ok: true },
            FromWorker::EncodeErr {
                message: "boom".into(),
                capture_rebuild: false,
            },
        ] {
            ipc::send(b.as_fd(), &reply, None).unwrap();
            let (got, _) = ipc::recv::<FromWorker>(a.as_fd(), &mut buf).unwrap();
            assert_eq!(got, reply);
        }
    }

    /// An AU never rides in the JSON: even a modest per-frame budget is already
    /// `MAX_MSG`, and serde_json renders a byte as up to four characters.
    #[test]
    fn an_inline_au_would_not_fit_a_message() {
        // 1080p60 at 40 Mb/s — smallest AU the encoder will actually produce.
        let au = vec![0xABu8; 40_000_000 / (8 * 60)];
        let body = serde_json::to_vec(&au).unwrap();
        assert!(
            body.len() > ipc::MAX_MSG,
            "a {}-byte AU serialized to {} bytes, which would (wrongly) fit MAX_MSG {}",
            au.len(),
            body.len(),
            ipc::MAX_MSG
        );
    }

    #[test]
    fn oversized_messages_are_refused_not_truncated() {
        let (a, _b) = ipc::socketpair_seqpacket().unwrap();
        let ToWorker::Hello {
            proto,
            drm_node,
            width,
            height,
            fps,
            bitrate_bps,
            chroma444,
            bit_depth,
            hdr,
            priority_intent,
            ..
        } = hello()
        else {
            unreachable!("hello() builds a Hello");
        };
        let huge = ToWorker::Hello {
            proto,
            // Over `MAX_MSG` on its own. Enum variants take no functional-update
            // syntax, so the rest is destructured above rather than `..hello()`.
            workspace_version: "x".repeat(ipc::MAX_MSG),
            drm_node,
            width,
            height,
            fps,
            bitrate_bps,
            chroma444,
            bit_depth,
            hdr,
            priority_intent,
        };
        let err = ipc::send(a.as_fd(), &huge, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Catches a mis-typed arm; a new capture format is already a compile error.
    #[test]
    fn pixel_formats_round_trip() {
        for f in [
            PixelFormat::Bgrx,
            PixelFormat::Rgbx,
            PixelFormat::Bgra,
            PixelFormat::Rgba,
            PixelFormat::Rgb,
            PixelFormat::Bgr,
            PixelFormat::Rgb10a2,
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::Yuv444,
            PixelFormat::X2Rgb10,
            PixelFormat::X2Bgr10,
        ] {
            assert_eq!(PixelFormat::from(WireFormat::from(f)), f);
        }
    }

    #[test]
    fn memfd_round_trips_bytes_across_a_descriptor() {
        let f = memfd(c"pf-encode-test").unwrap();
        let au = vec![0x5Au8; 900_000];
        f.write_all_at(&au, 0).unwrap();
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        ipc::send(a.as_fd(), &FromWorker::NeedFd, Some(f.as_fd())).unwrap();
        let (_, fd) = ipc::recv::<FromWorker>(b.as_fd(), &mut buf).unwrap();
        let mut back = vec![0u8; au.len()];
        File::from(fd.unwrap()).read_exact_at(&mut back, 0).unwrap();
        assert_eq!(back, au);
    }

    /// The arena lays out exactly the AU `build_au` would, in the file the host reads, and
    /// regrows for a frame past its mapping.
    #[test]
    fn the_arena_lays_out_the_au_the_host_reads() {
        let f = memfd(c"pf-encode-arena").unwrap();
        let mut arena = super::super::pyrowave::AuArena::new(f.try_clone().unwrap());
        let bs: Vec<u8> = (0..3000u32).map(|i| (i * 13) as u8).collect();
        let pkts = [(0usize, 700usize), (700, 900), (1600, 20)];
        let n = arena.build(&pkts, &bs, Some(1408)).unwrap();
        let mut back = vec![0u8; n];
        f.read_exact_at(&mut back, 0).unwrap();
        assert_eq!(back, crate::pyrowave_wire::build_au(&pkts, &bs, Some(1408)));
        let big = vec![7u8; 3 << 20];
        let n2 = arena.build(&[(0, big.len())], &big, Some(1408)).unwrap();
        assert!(n2 > big.len());
        assert!(f.metadata().unwrap().len() >= n2 as u64);
    }

    #[test]
    fn cursor_upload_clamps_to_the_blend_texture() {
        let (_, n) = cursor_upload(&vec![0u8; CURSOR_UPLOAD_MAX * 4]).unwrap();
        assert_eq!(n, CURSOR_UPLOAD_MAX);
        let (_, n) = cursor_upload(&vec![0u8; 64 * 64 * 4]).unwrap();
        assert_eq!(n, 64 * 64 * 4);
    }

    /// EINTR must not read as a dead worker. `SIGURG` (default-ignored) on a
    /// thread parked in `recv` with `SO_RCVTIMEO` returns EINTR — `SA_RESTART`
    /// does not apply to a timeout-armed socket.
    #[test]
    fn recv_survives_a_signal() {
        let (a, b) = ipc::socketpair_seqpacket().unwrap();
        let b = std::sync::Arc::new(b);
        let waiter = {
            let b = b.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                recv_eintr::<FromWorker>(b.as_fd(), &mut buf, Some(Duration::from_secs(10)))
            })
        };
        // Park in `recvmsg` first; then interrupt while the message is still absent.
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..5 {
            // SAFETY: `pthread_kill` takes the live thread's id by value and a signal number;
            // SIGURG's default disposition is "ignore", so delivery cannot kill the process.
            unsafe {
                libc::pthread_kill(
                    std::os::unix::thread::JoinHandleExt::as_pthread_t(&waiter),
                    libc::SIGURG,
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ipc::send(a.as_fd(), &FromWorker::Ack { ok: true }, None).unwrap();
        let (got, _) = waiter.join().unwrap().expect("EINTR must not surface");
        assert_eq!(got, FromWorker::Ack { ok: true });
    }

    /// Retry must not defeat the deadline: an unread socket still times out.
    #[test]
    fn recv_still_times_out() {
        let (a, _b) = ipc::socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let err = recv_eintr::<FromWorker>(a.as_fd(), &mut buf, Some(Duration::from_millis(80)))
            .unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error kind: {err:?}"
        );
    }
}
