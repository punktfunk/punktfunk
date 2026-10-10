//! Isolated GPU-import worker (`punktfunk-host zerocopy-worker`).
//!
//! Owns the headless EGLDisplay + GL context, the CUDA context, and the Vulkan bridge so a
//! driver fault on a producer-invalidated dmabuf kills this process, not the streaming host.
//! The host sees the dead socket, fails the frame, and the capture-loss rebuild takes over.
//!
//! One worker per capture (spawned per `pipewire_thread`). Exits on socket EOF, which the host
//! sends only after the capturer and every in-flight frame are gone, so pooled device memory
//! is never freed under a live mapping.
//!
//! Design: `design/zerocopy-worker-isolation.md`.

use super::cuda::{self, CUdeviceptr, DeviceBuffer};
use super::egl::{DmabufPlane, EglImporter};
use super::ipc;
use super::proto::{
    BufferDesc, ConvertOut, ConvertSrc, CursorRect, ImportKind, Reply, Request, PROTO_VERSION,
};
use anyhow::{bail, Context, Result};
use pf_dmabuf::{ReadMap, Share};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

/// PipeWire pools are ≤ ~16; 64 only applies if a producer churns fds without renegotiating.
const FD_CACHE_CAP: usize = 64;

/// Hidden `zerocopy-worker` subcommand. `--fd N` (default 3) is the socket the host `dup2`'d in.
pub fn run_from_args(args: &[String]) -> Result<()> {
    // Host execs via pinned exe fd (`ipc::self_exe`); kernel comm is that path's basename (an
    // fd number). Rename so `top`/`pkill` see the worker.
    // SAFETY: `PR_SET_NAME` copies at most 16 bytes from the given pointer; the C-string literal
    // is valid, NUL-terminated, and short enough. No pointer is retained past the call.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c"pf-zerocopy".as_ptr());
    }
    run(ipc::adopt_spawned_socket(args)?)
}

fn run(sock: OwnedFd) -> Result<()> {
    let importer = match EglImporter::new() {
        Ok(i) => i,
        Err(e) => {
            // Init failure is an answer, not a crash: the host falls back to the CPU path.
            let _ = ipc::send(
                sock.as_fd(),
                &Reply::InitErr {
                    message: format!("{e:#}"),
                },
                None,
            );
            return Ok(());
        }
    };
    ipc::send(
        sock.as_fd(),
        &Reply::Ready {
            version: PROTO_VERSION,
        },
        None,
    )
    .context("send Ready")?;
    tracing::info!(pid = std::process::id(), "zerocopy import worker ready");
    let mut backend = EglBackend::new(importer);
    serve(&sock, &mut backend)
}

/// Import surface for [`serve`]. Split out so the dispatch loop is unit-testable without a GPU.
pub(crate) trait ImportBackend {
    fn modifiers(&mut self, fourcc: u32) -> Vec<u64>;
    /// The fused-convert lane; a backend without one answers `Err` and the host keeps importing.
    fn register_slot(&mut self, _id: u32, _size: u64, _fd: Option<OwnedFd>) -> Reply {
        Reply::Err {
            message: "no convert lane".into(),
        }
    }
    fn forget_slots(&mut self) {}
    fn set_cursor(
        &mut self,
        _serial: u64,
        _w: u32,
        _h: u32,
        _len: u32,
        _fd: Option<OwnedFd>,
    ) -> Reply {
        Reply::Err {
            message: "no convert lane".into(),
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn convert(
        &mut self,
        _key: u64,
        _has_fd: bool,
        _src: ConvertSrc,
        _slot: u32,
        _out: ConvertOut,
        _cursor: Option<CursorRect>,
        _fd: Option<OwnedFd>,
    ) -> Reply {
        Reply::Err {
            message: "no convert lane".into(),
        }
    }
    fn convert_timeline(&mut self) -> (Reply, Option<OwnedFd>) {
        (
            Reply::Err {
                message: "no convert lane".into(),
            },
            None,
        )
    }
    /// Desc only on first delivery of that id. [`Reply::NeedFd`]: host resends the fd once.
    fn import(&mut self, req: &ImportReq, fd: Option<OwnedFd>) -> Reply;
    fn release(&mut self, id: u32);
    fn clear_cache(&mut self);
}

pub(crate) struct ImportReq {
    pub key: u64,
    pub kind: ImportKind,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: Option<u64>,
    pub offset: u32,
    pub stride: u32,
    pub has_fd: bool,
}

/// `Ok(())` on host EOF (normal end); any other socket error kills the process.
pub(crate) fn serve(sock: &OwnedFd, backend: &mut dyn ImportBackend) -> Result<()> {
    let mut buf = Vec::new();
    loop {
        let (req, fd) = match ipc::recv::<Request>(sock.as_fd(), &mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e).context("worker recv"),
        };
        match req {
            Request::Modifiers { fourcc } => {
                let reply = Reply::Modifiers {
                    modifiers: backend.modifiers(fourcc),
                };
                if send_or_eof(sock, &reply, None)? {
                    return Ok(());
                }
            }
            Request::Import {
                key,
                kind,
                width,
                height,
                fourcc,
                modifier,
                offset,
                stride,
                has_fd,
            } => {
                let req = ImportReq {
                    key,
                    kind,
                    width,
                    height,
                    fourcc,
                    modifier,
                    offset,
                    stride,
                    has_fd,
                };
                let reply = backend.import(&req, fd);
                if send_or_eof(sock, &reply, None)? {
                    return Ok(());
                }
            }
            Request::Release { id } => backend.release(id),
            Request::ClearCache => backend.clear_cache(),
            Request::RegisterSlot { id, size } => {
                let reply = backend.register_slot(id, size, fd);
                if send_or_eof(sock, &reply, None)? {
                    return Ok(());
                }
            }
            Request::ForgetSlots => backend.forget_slots(),
            Request::SetCursor {
                serial,
                width,
                height,
                len,
            } => {
                let reply = backend.set_cursor(serial, width, height, len, fd);
                if send_or_eof(sock, &reply, None)? {
                    return Ok(());
                }
            }
            Request::Convert {
                key,
                has_fd,
                src,
                slot,
                out,
                cursor,
            } => {
                let reply = backend.convert(key, has_fd, src, slot, out, cursor, fd);
                if send_or_eof(sock, &reply, None)? {
                    return Ok(());
                }
            }
            Request::ConvertTimeline => {
                let (reply, fd) = backend.convert_timeline();
                if send_or_eof(sock, &reply, fd.as_ref().map(|f| f.as_fd()))? {
                    return Ok(());
                }
            }
        }
    }
}

/// `Ok(true)`: host is gone (EPIPE); the loop should end quietly.
fn send_or_eof(sock: &OwnedFd, reply: &Reply, fd: Option<BorrowedFd>) -> Result<bool> {
    match ipc::send(sock.as_fd(), reply, fd) {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(true),
        Err(e) => Err(e).context("worker send"),
    }
}

struct EglBackend {
    importer: EglImporter,
    /// Dmabuf fd per host key (`st_ino`). Tiled re-imports every frame (`eglCreateImage`); LINEAR
    /// caches per fd inside the Vulkan bridge.
    fds: HashMap<u64, OwnedFd>,
    fd_lru: VecDeque<u64>,
    /// The `DeviceBuffer` keeps pool `Arc`s alive while the host encodes.
    inflight: HashMap<u32, DeviceBuffer>,
    /// Valid for one pool generation: a VA cannot repeat until a size change replaces the pool
    /// (`note_dims` / `ClearCache` clear this). Ids never reuse; `next_id` only counts up.
    ids: HashMap<CUdeviceptr, u32>,
    next_id: u32,
    /// A change means the importer replaced its pool, so the VA→id map is invalid (see [`Self::ids`]).
    last_shape: Option<(ImportKind, u32, u32)>,
}

impl EglBackend {
    fn new(importer: EglImporter) -> EglBackend {
        EglBackend {
            importer,
            fds: HashMap::new(),
            fd_lru: VecDeque::new(),
            inflight: HashMap::new(),
            ids: HashMap::new(),
            next_id: 0,
            last_shape: None,
        }
    }

    /// Forget a replaced or evicted fd in the Vulkan bridge first so its per-fd import cannot go stale.
    fn store_fd(&mut self, key: u64, fd: OwnedFd) {
        if let Some(old) = self.fds.insert(key, fd) {
            self.importer.forget_linear_fd(old.as_raw_fd());
            self.fd_lru.retain(|k| *k != key);
        }
        self.fd_lru.push_back(key);
        while self.fds.len() > FD_CACHE_CAP {
            let Some(oldest) = self.fd_lru.pop_front() else {
                break;
            };
            if let Some(old) = self.fds.remove(&oldest) {
                self.importer.forget_linear_fd(old.as_raw_fd());
            }
        }
    }

    /// The dmabuf fd for `key`, storing `fd` first when it rode along. `Err` is the reply to
    /// send instead: `what` claimed an fd that did not arrive, or [`Reply::NeedFd`] after an LRU
    /// eviction or cache desync, so the host resends rather than failing the frame.
    fn resolve_fd(
        &mut self,
        key: u64,
        has_fd: bool,
        fd: Option<OwnedFd>,
        what: &str,
    ) -> Result<i32, Reply> {
        if let Some(fd) = fd {
            self.store_fd(key, fd);
        } else if has_fd {
            return Err(Reply::Err {
                message: format!("{what} said has_fd but no fd arrived"),
            });
        }
        self.fds
            .get(&key)
            .map(|f| f.as_raw_fd())
            .ok_or(Reply::NeedFd)
    }

    fn note_dims(&mut self, kind: ImportKind, width: u32, height: u32) {
        if self.last_shape != Some((kind, width, height)) {
            self.last_shape = Some((kind, width, height));
            self.ids.clear();
        }
    }
}

impl ImportBackend for EglBackend {
    fn modifiers(&mut self, fourcc: u32) -> Vec<u64> {
        self.importer.supported_modifiers(fourcc)
    }

    fn import(&mut self, req: &ImportReq, fd: Option<OwnedFd>) -> Reply {
        let raw = match self.resolve_fd(req.key, req.has_fd, fd, "Import") {
            Ok(raw) => raw,
            Err(reply) => return reply,
        };
        match self.import_inner(req, raw) {
            Ok((id, desc)) => Reply::Frame { id, desc },
            Err(e) => Reply::Err {
                message: format!("{e:#}"),
            },
        }
    }

    fn release(&mut self, id: u32) {
        if self.inflight.remove(&id).is_none() {
            tracing::warn!(id, "release for a frame not in flight (host/worker desync)");
        }
    }

    fn clear_cache(&mut self) {
        for (_, fd) in self.fds.drain() {
            self.importer.forget_linear_fd(fd.as_raw_fd());
        }
        self.fd_lru.clear();
        self.importer.clear_linear_cache();
        // Host retires CUDA IPC mappings here (except those still under an in-flight frame).
        // Forget VA→id so the next delivery gets a fresh id WITH its descriptor; reusing an
        // old id would name a mapping the host just closed. `next_id` only counts up, so
        // fresh ids cannot collide with retired ones.
        self.ids.clear();
    }
    fn register_slot(&mut self, id: u32, size: u64, fd: Option<OwnedFd>) -> Reply {
        let Some(fd) = fd else {
            return Reply::Err {
                message: "RegisterSlot without an fd".into(),
            };
        };
        match self.importer.register_slot(id, fd, size) {
            Ok(()) => Reply::Done,
            Err(e) => Reply::Err {
                message: format!("{e:#}"),
            },
        }
    }
    fn forget_slots(&mut self) {
        self.importer.forget_slots();
    }
    fn set_cursor(&mut self, serial: u64, w: u32, h: u32, len: u32, fd: Option<OwnedFd>) -> Reply {
        let Some(fd) = fd else {
            return Reply::Err {
                message: "SetCursor without a memfd".into(),
            };
        };
        let r = ReadMap::new(fd.as_fd(), len as usize, Share::Private)
            .context("map the cursor memfd")
            .and_then(|m| self.importer.set_cursor(serial, w, h, m.bytes()));
        match r {
            Ok(()) => Reply::Done,
            Err(e) => Reply::Err {
                message: format!("{e:#}"),
            },
        }
    }
    fn convert(
        &mut self,
        key: u64,
        has_fd: bool,
        mut src: ConvertSrc,
        slot: u32,
        out: ConvertOut,
        cursor: Option<CursorRect>,
        fd: Option<OwnedFd>,
    ) -> Reply {
        src.fd = match self.resolve_fd(key, has_fd, fd, "Convert") {
            Ok(raw) => raw,
            Err(reply) => return reply,
        };
        match self.importer.convert(&src, slot, &out, cursor) {
            Ok(value) => Reply::Converted { value },
            Err(e) => Reply::Err {
                message: format!("{e:#}"),
            },
        }
    }
    fn convert_timeline(&mut self) -> (Reply, Option<OwnedFd>) {
        match self.importer.convert_timeline_fd() {
            Ok(fd) => (Reply::Timeline, Some(fd)),
            Err(e) => (
                Reply::Err {
                    message: format!("{e:#}"),
                },
                None,
            ),
        }
    }
}

impl EglBackend {
    fn import_inner(&mut self, req: &ImportReq, raw: i32) -> Result<(u32, Option<BufferDesc>)> {
        let plane = DmabufPlane {
            fd: raw,
            offset: req.offset,
            stride: req.stride,
        };
        self.note_dims(req.kind, req.width, req.height);
        let buf = self.importer.import(
            req.kind,
            &plane,
            req.width,
            req.height,
            req.fourcc,
            req.modifier,
        )?;
        cuda::make_current()?;
        let (id, desc) = match self.ids.get(&buf.ptr) {
            Some(&id) => (id, None),
            None => {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                let y_handle = cuda::ipc_export(buf.ptr)?.to_vec();
                let uv = match buf.uv() {
                    Some((uv_ptr, uv_pitch)) => {
                        Some((cuda::ipc_export(uv_ptr)?.to_vec(), uv_pitch))
                    }
                    None => None,
                };
                self.ids.insert(buf.ptr, id);
                (
                    id,
                    Some(BufferDesc {
                        width: buf.width,
                        height: buf.height,
                        y_handle,
                        y_pitch: buf.pitch,
                        uv,
                    }),
                )
            }
        };
        if self.inflight.insert(id, buf).is_some() {
            // A pool never hands out a buffer that has not been recycled; a duplicate id would
            // alias two frames.
            bail!("buffer id {id} already in flight");
        }
        Ok((id, desc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// `st_ino` of an open fd. SCM_RIGHTS preserves identity while re-numbering the descriptor;
    /// the dispatch test asserts the arrived fd is the one the host sent, not just that JSON
    /// claimed one.
    fn fd_ino(fd: impl AsFd) -> u64 {
        crate::imp::fd_identity(fd.as_fd()).expect("fstat").1
    }

    struct MockBackend {
        calls: mpsc::Sender<String>,
        next: u32,
    }

    impl ImportBackend for MockBackend {
        fn modifiers(&mut self, fourcc: u32) -> Vec<u64> {
            let _ = self.calls.send(format!("modifiers:{fourcc}"));
            vec![7, 8, 9]
        }
        fn import(&mut self, req: &ImportReq, fd: Option<OwnedFd>) -> Reply {
            let received = match &fd {
                Some(f) => format!("ino:{}", fd_ino(f)),
                None => "none".into(),
            };
            let _ = self.calls.send(format!(
                "import:key={} kind={:?} fd={received}",
                req.key, req.kind,
            ));
            if req.key == 0xbad {
                return Reply::Err {
                    message: "scripted failure".into(),
                };
            }
            if req.key == 0xfeed && !req.has_fd {
                return Reply::NeedFd;
            }
            let id = self.next;
            self.next += 1;
            let desc = (id == 0).then(|| BufferDesc {
                width: req.width,
                height: req.height,
                y_handle: vec![0u8; 64],
                y_pitch: 256,
                uv: None,
            });
            Reply::Frame { id, desc }
        }
        fn release(&mut self, id: u32) {
            let _ = self.calls.send(format!("release:{id}"));
        }
        fn clear_cache(&mut self) {
            let _ = self.calls.send("clear".into());
        }
    }

    fn start_server() -> (
        OwnedFd,
        mpsc::Receiver<String>,
        std::thread::JoinHandle<Result<()>>,
    ) {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        let (tx, rx) = mpsc::channel();
        let join = std::thread::spawn(move || {
            let mut backend = MockBackend { calls: tx, next: 0 };
            serve(&worker, &mut backend)
        });
        (host, rx, join)
    }

    fn import_req(key: u64, has_fd: bool) -> Request {
        Request::Import {
            key,
            kind: ImportKind::Tiled,
            width: 64,
            height: 64,
            fourcc: 1,
            modifier: None,
            offset: 0,
            stride: 256,
            has_fd,
        }
    }

    #[test]
    fn dispatch_and_eof() {
        let (host, rx, join) = start_server();
        let mut buf = Vec::new();

        ipc::send(host.as_fd(), &Request::Modifiers { fourcc: 42 }, None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(
            reply,
            Reply::Modifiers {
                modifiers: vec![7, 8, 9]
            }
        );

        ipc::send(host.as_fd(), &import_req(1, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        match reply {
            Reply::Frame {
                id: 0,
                desc: Some(_),
            } => {}
            other => panic!("unexpected reply {other:?}"),
        }
        ipc::send(host.as_fd(), &import_req(1, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(reply, Reply::Frame { id: 1, desc: None });

        // SCM_RIGHTS must deliver a live fd with the sender's identity. `serve` dropping it
        // (`backend.import(&req, None)`) is only caught here.
        let (pr, _pw) = std::io::pipe().unwrap();
        let sent_ino = fd_ino(&pr);
        ipc::send(host.as_fd(), &import_req(3, true), Some(pr.as_fd())).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(reply, Reply::Frame { id: 2, desc: None });

        ipc::send(host.as_fd(), &import_req(0xfeed, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(reply, Reply::NeedFd);

        // Failed import is an Err reply, not a dead worker.
        ipc::send(host.as_fd(), &import_req(0xbad, false), None).unwrap();
        let (reply, _) = ipc::recv::<Reply>(host.as_fd(), &mut buf).unwrap();
        match reply {
            Reply::Err { message } => assert!(message.contains("scripted failure")),
            other => panic!("unexpected reply {other:?}"),
        }

        ipc::send(host.as_fd(), &Request::Release { id: 0 }, None).unwrap();
        ipc::send(host.as_fd(), &Request::ClearCache, None).unwrap();

        drop(host);
        join.join().unwrap().unwrap();

        let calls: Vec<String> = rx.iter().collect();
        assert_eq!(
            calls,
            vec![
                "modifiers:42".to_string(),
                "import:key=1 kind=Tiled fd=none".to_string(),
                "import:key=1 kind=Tiled fd=none".to_string(),
                format!("import:key=3 kind=Tiled fd=ino:{sent_ino}"),
                "import:key=65261 kind=Tiled fd=none".to_string(), // 0xfeed
                "import:key=2989 kind=Tiled fd=none".to_string(),  // 0xbad
                "release:0".to_string(),
                "clear".to_string(),
            ]
        );
    }
}
