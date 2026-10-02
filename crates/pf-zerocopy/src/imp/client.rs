//! Isolated zero-copy GPU import: spawn `zerocopy-worker` on [`super::ipc`],
//! mirror [`super::egl::EglImporter`] over [`super::proto`], and open each
//! pooled CUDA IPC handle once in this process as the pool recycles.
//!
//! Worker death is an `Err` with [`RemoteImporter::dead`] set, never a host
//! fault. Design: `design/zerocopy-worker-isolation.md`.

use super::cuda::{self, CUdeviceptr, DeviceBuffer, CU_IPC_HANDLE_SIZE};
use super::egl::DmabufPlane;
use super::ipc;
use super::proto::{
    BufferDesc, ConvertOut, ConvertSrc, CursorRect, ImportKind, Reply, Request, PROTO_VERSION,
};
use anyhow::{bail, Context, Result};
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 20 s: EGL/CUDA bring-up is ~200 ms; a cold driver load can take seconds.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// 10 s: an import is a few ms of GPU work. Miss = wedged (GPU fault) = dead.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Socket + CUDA IPC mappings, Arc-shared with in-flight [`DeviceBuffer`]s.
/// Last drop closes mappings then the socket, which is the idle worker's EOF.
struct Shared {
    sock: OwnedFd,
    mappings: Mutex<HashMap<u32, MapEntry>>,
    dead: AtomicBool,
}

#[derive(Clone, Copy)]
struct Mapping {
    y: CUdeviceptr,
    y_pitch: usize,
    uv: Option<(CUdeviceptr, usize)>,
    width: u32,
    height: u32,
}

/// A [`Mapping`] with in-flight [`DeviceBuffer`] refs and a retired flag from
/// [`RemoteImporter::clear_cache`]. Retired-but-referenced entries stay until
/// last release; otherwise each renegotiation pins host VA to peer memory the
/// worker already freed. Ids never reuse (`next_id` only counts up).
struct MapEntry {
    m: Mapping,
    refs: u32,
    retired: bool,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Last Arc gone: no DeviceBuffer still points into current or retired mappings.
        for (_, e) in self.mappings.lock().unwrap().drain() {
            close_mapping(&e.m);
        }
    }
}

fn close_mapping(m: &Mapping) {
    cuda::ipc_close(m.y);
    if let Some((uv, _)) = m.uv {
        cuda::ipc_close(uv);
    }
}

/// Isolated importer, one per capture. Same surface as [`super::egl::EglImporter`].
pub struct RemoteImporter {
    shared: Arc<Shared>,
    child: Option<Child>,
    /// All replies are on the single capture thread.
    rbuf: Vec<u8>,
    /// `st_ino` keys whose fd the worker already holds — pass the fd once.
    sent_keys: HashSet<u64>,
}

impl RemoteImporter {
    /// Spawn this host binary as the worker and handshake. Exec is through the
    /// pinned [`ipc::self_exe`] fd so a replaced install cannot change the
    /// image. `Err` means no isolated zero-copy — same fallback as
    /// `EglImporter::new()`. Do not copy this onto the capability-carrying
    /// encode worker: a shared inode shares the file capability.
    pub fn spawn() -> Result<RemoteImporter> {
        match ipc::self_exe() {
            Some(exe) => Self::spawn_exe(&exe.exec_path()),
            None => Self::spawn_exe(
                &std::env::current_exe().context("resolve /proc/self/exe for the worker")?,
            ),
        }
    }

    fn spawn_exe(exe: &Path) -> Result<RemoteImporter> {
        // `exe` is normally `/proc/self/fd/<n>`; argv[0] is what `ps` shows.
        let (host_end, child) =
            ipc::spawn_worker(exe, "punktfunk-host", &["zerocopy-worker", "--fd", "3"])
                .context("spawn zerocopy-worker")?;
        Self::from_socket(host_end, Some(child))
    }

    /// Handshake on an already-connected socket (tests use a mock peer).
    fn from_socket(sock: OwnedFd, child: Option<Child>) -> Result<RemoteImporter> {
        let mut importer = RemoteImporter {
            shared: Arc::new(Shared {
                sock,
                mappings: Mutex::new(HashMap::new()),
                dead: AtomicBool::new(false),
            }),
            child,
            rbuf: Vec::new(),
            sent_keys: HashSet::new(),
        };
        ipc::set_recv_timeout(importer.shared.sock.as_fd(), Some(HANDSHAKE_TIMEOUT))?;
        let ready = ipc::recv::<Reply>(importer.shared.sock.as_fd(), &mut importer.rbuf);
        ipc::set_recv_timeout(importer.shared.sock.as_fd(), Some(REPLY_TIMEOUT))?;
        match ready {
            Ok((Reply::Ready { version }, _)) if version == PROTO_VERSION => {
                tracing::info!(
                    pid = importer.child.as_ref().map(|c| c.id()),
                    "zero-copy GPU import isolated in a worker process"
                );
                Ok(importer)
            }
            Ok((Reply::Ready { version }, _)) => {
                importer.mark_dead();
                bail!(
                    "zerocopy worker protocol mismatch (worker v{version}, host v{})",
                    PROTO_VERSION
                )
            }
            Ok((Reply::InitErr { message }, _)) => {
                // Worker exits after reporting; this is "no GPU", not a death.
                bail!("zerocopy worker init failed: {message}")
            }
            Ok((other, _)) => {
                importer.mark_dead();
                bail!("unexpected zerocopy worker handshake: {other:?}")
            }
            Err(e) => {
                importer.mark_dead();
                Err(e).context("zerocopy worker handshake (died on startup?)")
            }
        }
    }

    /// Transport-level failure: worker gone or wedged. Further calls fail fast.
    pub fn dead(&self) -> bool {
        self.shared.dead.load(Ordering::Relaxed)
    }

    fn mark_dead(&self) {
        self.shared.dead.store(true, Ordering::Relaxed);
    }

    /// Empty on any failure so capture falls back like an importless negotiation.
    pub fn supported_modifiers(&mut self, fourcc: u32) -> Vec<u64> {
        if self.dead() {
            return Vec::new();
        }
        if let Err(e) = ipc::send(
            self.shared.sock.as_fd(),
            &Request::Modifiers { fourcc },
            None,
        ) {
            tracing::warn!(error = %e, "zerocopy worker modifier query failed");
            self.mark_dead();
            return Vec::new();
        }
        match ipc::recv::<Reply>(self.shared.sock.as_fd(), &mut self.rbuf) {
            Ok((Reply::Modifiers { modifiers }, _)) => modifiers,
            Ok((other, _)) => {
                tracing::warn!(?other, "unexpected zerocopy worker reply to Modifiers");
                self.mark_dead();
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "zerocopy worker modifier reply failed");
                self.mark_dead();
                Vec::new()
            }
        }
    }

    /// One import round trip; see [`super::EglImporter::import`] for `kind`.
    pub fn import(
        &mut self,
        kind: ImportKind,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        if self.dead() {
            bail!("zerocopy worker is dead");
        }
        // SAFETY: `plane.fd` is the dmabuf fd of the PipeWire buffer the capture thread still
        // holds for this callback (`consume_frame`'s contract), so it is open and stays open
        // for this synchronous call; the `BorrowedFd` never outlives it (the key and the `send`).
        let fd = unsafe { BorrowedFd::borrow_raw(plane.fd) };
        let key = dmabuf_key(fd)?;
        // One retry: NeedFd (worker evicted this key) clears sent_keys so the resend carries the fd.
        let mut attempts = 0;
        let reply = loop {
            attempts += 1;
            let has_fd = self.sent_keys.insert(key);
            let pass = has_fd.then_some(fd);
            let req = Request::Import {
                key,
                kind,
                width,
                height,
                fourcc,
                modifier,
                offset: plane.offset,
                stride: plane.stride,
                has_fd,
            };
            if let Err(e) = ipc::send(self.shared.sock.as_fd(), &req, pass) {
                self.mark_dead();
                return Err(e).context("zerocopy worker died (send)");
            }
            let reply = match ipc::recv::<Reply>(self.shared.sock.as_fd(), &mut self.rbuf) {
                Ok((reply, _)) => reply,
                Err(e) => {
                    self.mark_dead();
                    return Err(e).context("zerocopy worker died (no reply)");
                }
            };
            match reply {
                Reply::NeedFd if attempts == 1 => {
                    self.sent_keys.remove(&key);
                    continue;
                }
                Reply::NeedFd => {
                    self.mark_dead();
                    bail!("zerocopy worker still lacks the fd after a resend (desync)");
                }
                other => break other,
            }
        };
        match reply {
            Reply::Frame { id, desc } => {
                if let Some(desc) = desc {
                    let mapping = open_mapping(&desc).with_context(|| {
                        // Unopenable mapping poisons this buffer; mark dead so capture rebuilds.
                        self.mark_dead();
                        format!("open CUDA IPC mapping for worker buffer {id}")
                    })?;
                    self.shared.mappings.lock().unwrap().insert(
                        id,
                        MapEntry {
                            m: mapping,
                            refs: 0,
                            retired: false,
                        },
                    );
                }
                let m = {
                    let mut g = self.shared.mappings.lock().unwrap();
                    let entry = g.get_mut(&id).ok_or_else(|| {
                        self.mark_dead();
                        anyhow::anyhow!("worker delivered unknown buffer id {id} (desync)")
                    })?;
                    entry.refs += 1;
                    entry.m
                };
                let shared = self.shared.clone();
                let release = Box::new(move || {
                    // A retired mapping closes with its last ref BEFORE the worker hears
                    // Release, which may free the allocation: CUDA leaves a free under an open
                    // importer undefined. Recycle is fire-and-forget (EPIPE if dead); this Arc
                    // keeps mapping and socket alive until the last frame drops.
                    {
                        let mut g = shared.mappings.lock().unwrap();
                        if let Some(entry) = g.get_mut(&id) {
                            entry.refs = entry.refs.saturating_sub(1);
                            if entry.retired && entry.refs == 0 {
                                let entry = g.remove(&id).expect("entry exists");
                                close_mapping(&entry.m);
                            }
                        }
                    }
                    let _ = ipc::send(shared.sock.as_fd(), &Request::Release { id }, None);
                });
                // Wire has no plane format; layout is the ImportKind we asked for.
                let yuv444 = kind.layout() == cuda::PlaneLayout::Yuv444;
                // SAFETY: `m` is the IPC mapping `open_mapping` opened for this id, with the
                // worker's layout; the ref taken above keeps it open until `release` drops it.
                Ok(unsafe {
                    DeviceBuffer::remote(m.y, m.y_pitch, m.width, m.height, m.uv, yuv444, release)
                })
            }
            Reply::Err { message } => bail!("zerocopy worker import failed: {message}"),
            other => {
                self.mark_dead();
                bail!("unexpected zerocopy worker reply: {other:?}")
            }
        }
    }

    /// Stream renegotiated: drop fd-cache keys and retire this generation's CUDA
    /// IPC mappings. The worker replaces its pool; live host mappings would pin
    /// VA to peer memory already freed. Unreferenced close now; in-flight wait
    /// for release.
    /// Send one request and wait for its reply; a transport failure marks the worker dead.
    fn call(&mut self, req: &Request, fd: Option<BorrowedFd>) -> Result<Reply> {
        self.call_fd(req, fd).map(|(reply, _)| reply)
    }

    /// [`call`](Self::call), keeping the descriptor the reply carries.
    fn call_fd(
        &mut self,
        req: &Request,
        fd: Option<BorrowedFd>,
    ) -> Result<(Reply, Option<OwnedFd>)> {
        if self.dead() {
            bail!("zerocopy worker is dead");
        }
        if let Err(e) = ipc::send(self.shared.sock.as_fd(), req, fd) {
            self.mark_dead();
            return Err(e).context("zerocopy worker died (send)");
        }
        match ipc::recv::<Reply>(self.shared.sock.as_fd(), &mut self.rbuf) {
            Ok(reply) => Ok(reply),
            Err(e) => {
                self.mark_dead();
                Err(e).context("zerocopy worker died (no reply)")
            }
        }
    }

    /// Hand the worker the host's NVENC slot `id` (an OPAQUE_FD export of `size` bytes).
    pub fn register_slot(&mut self, id: u32, fd: BorrowedFd, size: u64) -> Result<()> {
        match self.call(&Request::RegisterSlot { id, size }, Some(fd))? {
            Reply::Done => Ok(()),
            Reply::Err { message } => bail!("register slot: {message}"),
            other => {
                self.mark_dead();
                bail!("unexpected reply to RegisterSlot: {other:?}")
            }
        }
    }

    /// The host rebuilt its ring. Fire-and-forget, like `ClearCache`.
    pub fn forget_slots(&mut self) {
        if self.dead() {
            return;
        }
        if ipc::send(self.shared.sock.as_fd(), &Request::ForgetSlots, None).is_err() {
            self.mark_dead();
        }
    }

    /// Ship a straight-alpha RGBA8 cursor bitmap through a memfd (pixels never ride the socket).
    pub fn set_cursor(&mut self, serial: u64, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        let len = u32::try_from(rgba.len()).context("cursor bitmap too large")?;
        let memfd = memfd_with(rgba)?;
        match self.call(
            &Request::SetCursor {
                serial,
                width,
                height,
                len,
            },
            Some(memfd.as_fd()),
        )? {
            Reply::Done => Ok(()),
            Reply::Err { message } => bail!("set cursor: {message}"),
            other => {
                self.mark_dead();
                bail!("unexpected reply to SetCursor: {other:?}")
            }
        }
    }

    /// One fused pass of `src` into slot `slot`; returns the timeline value it signals. The
    /// dmabuf fd crosses once per key, like an import; a worker that lost it asks again.
    pub fn convert(
        &mut self,
        src: &ConvertSrc,
        slot: u32,
        out: &ConvertOut,
        cursor: Option<CursorRect>,
    ) -> Result<u64> {
        // SAFETY: `src.fd` is the caller's live dmabuf for the duration of this call.
        let fd = unsafe { BorrowedFd::borrow_raw(src.fd) };
        let key = dmabuf_key(fd)?;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let has_fd = self.sent_keys.insert(key);
            let pass = has_fd.then_some(fd);
            let req = Request::Convert {
                key,
                has_fd,
                src: *src,
                slot,
                out: *out,
                cursor,
            };
            match self.call(&req, pass)? {
                Reply::Converted { value } => return Ok(value),
                Reply::NeedFd if attempts == 1 => {
                    self.sent_keys.remove(&key);
                    continue;
                }
                Reply::NeedFd => {
                    self.mark_dead();
                    bail!("zerocopy worker still lacks the fd after a resend (desync)");
                }
                Reply::Err { message } => bail!("convert: {message}"),
                other => {
                    self.mark_dead();
                    bail!("unexpected reply to Convert: {other:?}")
                }
            }
        }
    }

    /// The worker's convert timeline as an OPAQUE_FD, for the host's CUDA import.
    pub fn convert_timeline(&mut self) -> Result<OwnedFd> {
        match self.call_fd(&Request::ConvertTimeline, None)? {
            (Reply::Timeline, Some(fd)) => Ok(fd),
            (Reply::Err { message }, _) => bail!("convert timeline: {message}"),
            (other, _) => {
                self.mark_dead();
                bail!("unexpected reply to ConvertTimeline: {other:?}")
            }
        }
    }

    pub fn clear_cache(&mut self) {
        self.sent_keys.clear();
        {
            let mut g = self.shared.mappings.lock().unwrap();
            g.retain(|_, entry| {
                if entry.refs == 0 {
                    close_mapping(&entry.m);
                    false
                } else {
                    entry.retired = true;
                    true
                }
            });
        }
        if !self.dead() {
            if let Err(e) = ipc::send(self.shared.sock.as_fd(), &Request::ClearCache, None) {
                tracing::warn!(error = %e, "zerocopy worker ClearCache failed");
                self.mark_dead();
            }
        }
    }
}

impl Drop for RemoteImporter {
    fn drop(&mut self) {
        // Worker exits on socket EOF (last Shared: this importer or the last
        // in-flight frame). Reap if already gone; park the rest for the sweep.
        if let Some(mut child) = self.child.take() {
            if !matches!(child.try_wait(), Ok(Some(_))) {
                ipc::park_child(child);
            }
        }
        ipc::sweep_reaper();
    }
}

/// dma-buf identity, stable across frames and SCM_RIGHTS re-numbering: the
/// kernel gives each dma-buf a unique inode for its lifetime. Worker fd-cache
/// key, so the fd itself is passed once.
fn dmabuf_key(fd: BorrowedFd<'_>) -> Result<u64> {
    Ok(super::fd_identity(fd).context("fstat dmabuf fd")?.1)
}

fn open_mapping(desc: &BufferDesc) -> Result<Mapping> {
    cuda::make_current()?;
    let y_handle: [u8; CU_IPC_HANDLE_SIZE] = desc
        .y_handle
        .as_slice()
        .try_into()
        .context("worker sent a malformed Y IPC handle")?;
    let y = cuda::ipc_open(&y_handle).context("open Y plane IPC handle")?;
    let uv = match &desc.uv {
        Some((handle, pitch)) => {
            let handle: [u8; CU_IPC_HANDLE_SIZE] = handle
                .as_slice()
                .try_into()
                .context("worker sent a malformed UV IPC handle")?;
            match cuda::ipc_open(&handle) {
                Ok(ptr) => Some((ptr, *pitch)),
                Err(e) => {
                    // Close Y if UV open fails; otherwise the mapping leaks.
                    cuda::ipc_close(y);
                    return Err(e).context("open UV plane IPC handle");
                }
            }
        }
        None => None,
    };
    Ok(Mapping {
        y,
        y_pitch: desc.y_pitch,
        uv,
        width: desc.width,
        height: desc.height,
    })
}

/// A memfd holding `bytes`, for a bitmap that must not ride the socket. Written once, then
/// sealed against any change, so the worker's mapping of it cannot fault on a shrink.
fn memfd_with(bytes: &[u8]) -> Result<OwnedFd> {
    use rustix::fs::{MemfdFlags, SealFlags};
    use std::io::Write as _;
    let fd = rustix::fs::memfd_create(
        c"punktfunk-cursor",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .context("memfd_create(cursor)")?;
    let mut f = std::fs::File::from(fd);
    f.write_all(bytes).context("write cursor memfd")?;
    rustix::fs::fcntl_add_seals(
        &f,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    )
    .context("seal cursor memfd")?;
    Ok(OwnedFd::from(f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::thread;

    fn handshake_server(reply: Reply) -> OwnedFd {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        ipc::send(worker.as_fd(), &reply, None).unwrap();
        // Handshake is already queued; dropping the worker end still delivers it.
        drop(worker);
        host
    }

    #[test]
    fn handshake_ready_and_version_gate() {
        let host = handshake_server(Reply::Ready {
            version: PROTO_VERSION,
        });
        let imp = RemoteImporter::from_socket(host, None).unwrap();
        assert!(!imp.dead());

        let host = handshake_server(Reply::Ready { version: 999 });
        assert!(RemoteImporter::from_socket(host, None).is_err());
    }

    #[test]
    fn handshake_init_err() {
        let host = handshake_server(Reply::InitErr {
            message: "no GPU".into(),
        });
        let Err(err) = RemoteImporter::from_socket(host, None) else {
            panic!("InitErr handshake must fail")
        };
        assert!(format!("{err:#}").contains("no GPU"), "{err:#}");
    }

    #[test]
    fn handshake_eof_is_an_error() {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        drop(worker);
        assert!(RemoteImporter::from_socket(host, None).is_err());
    }

    #[test]
    fn spawning_a_non_worker_fails_cleanly() {
        // `true` exits with no handshake → EOF, same fallback as a GPU-less box.
        let Err(err) = RemoteImporter::spawn_exe(Path::new("true")) else {
            panic!("spawning a non-worker must fail")
        };
        assert!(format!("{err:#}").contains("handshake"), "{err:#}");
    }

    #[test]
    fn spawn_execs_the_pinned_self_exe() {
        // `spawn()` execs the pinned `/proc/self/fd/…` image. Here that is the
        // libtest harness, which rejects `--fd` and exits without a handshake
        // — so "handshake" in the error proves exec succeeded (an exec failure
        // would say "spawn zerocopy-worker").
        let Err(err) = RemoteImporter::spawn() else {
            panic!("the test harness is not a worker; spawn must fail at the handshake")
        };
        assert!(format!("{err:#}").contains("handshake"), "{err:#}");
    }

    /// Request as the peer saw it, plus `st_ino` of any SCM_RIGHTS fd. `has_fd`
    /// in the JSON is a claim; tests assert the descriptor actually arrived.
    type SeenRequest = (Request, Option<u64>);

    fn scripted_server(
        replies: Vec<Reply>,
    ) -> (RemoteImporter, thread::JoinHandle<Vec<SeenRequest>>) {
        let (host, worker) = ipc::socketpair_seqpacket().unwrap();
        ipc::send(
            worker.as_fd(),
            &Reply::Ready {
                version: PROTO_VERSION,
            },
            None,
        )
        .unwrap();
        let join = thread::spawn(move || {
            let mut buf = Vec::new();
            let mut seen = Vec::new();
            let mut replies = replies.into_iter();
            while let Ok((req, fd)) = ipc::recv::<Request>(worker.as_fd(), &mut buf) {
                let needs_reply = matches!(req, Request::Modifiers { .. } | Request::Import { .. });
                let ino = fd
                    .as_ref()
                    .map(|f| dmabuf_key(f.as_fd()).expect("fstat received fd"));
                seen.push((req, ino));
                if needs_reply {
                    match replies.next() {
                        Some(r) => ipc::send(worker.as_fd(), &r, None).unwrap(),
                        None => break, // close → client sees a dead worker
                    }
                }
            }
            seen
        });
        let imp = RemoteImporter::from_socket(host, None).unwrap();
        (imp, join)
    }

    #[test]
    fn modifiers_round_trip() {
        let (mut imp, join) = scripted_server(vec![Reply::Modifiers {
            modifiers: vec![1, 2, 3],
        }]);
        assert_eq!(imp.supported_modifiers(0x3432_5258), vec![1, 2, 3]);
        assert!(!imp.dead());
        drop(imp);
        let seen = join.join().unwrap();
        assert_eq!(
            seen,
            vec![(
                Request::Modifiers {
                    fourcc: 0x3432_5258
                },
                None
            )]
        );
    }

    #[test]
    fn need_fd_triggers_one_resend_with_the_fd() {
        let (mut imp, join) = scripted_server(vec![
            Reply::Err {
                message: "one".into(),
            },
            Reply::NeedFd,
            Reply::Err {
                message: "two".into(),
            },
        ]);
        let (pr, _pw) = std::io::pipe().unwrap();
        let plane = DmabufPlane {
            fd: pr.as_fd().as_raw_fd(),
            offset: 0,
            stride: 256,
        };
        // First sight of the key: fd rides along. Err keeps the key marked sent
        // (worker cached the fd before failing).
        assert!(imp
            .import(ImportKind::Tiled, &plane, 64, 64, 1, Some(2))
            .is_err());
        assert!(imp
            .import(ImportKind::Tiled, &plane, 64, 64, 1, Some(2))
            .is_err());
        assert!(!imp.dead(), "NeedFd handling must not mark the worker dead");
        // SCM_RIGHTS re-numbers the fd; st_ino of the open file survives.
        let key = dmabuf_key(pr.as_fd()).unwrap();
        drop(imp);
        let fd_sends: Vec<(bool, Option<u64>)> = join
            .join()
            .unwrap()
            .iter()
            .map(|(r, ino)| match r {
                Request::Import { has_fd, .. } => (*has_fd, *ino),
                other => panic!("unexpected request {other:?}"),
            })
            .collect();
        // Assert the descriptor crossed, not just has_fd. `pass = None` with
        // has_fd=true is the regression this would miss.
        assert_eq!(
            fd_sends,
            vec![(true, Some(key)), (false, None), (true, Some(key))]
        );
    }

    #[test]
    fn import_error_reply_keeps_worker_alive_and_death_is_detected() {
        let (mut imp, join) = scripted_server(vec![Reply::Err {
            message: "EGL_BAD_MATCH".into(),
        }]);
        // Stand-in fd; only the inode is used.
        let (pr, _pw) = std::io::pipe().unwrap();
        let plane = DmabufPlane {
            fd: pr.as_fd().as_raw_fd(),
            offset: 0,
            stride: 256,
        };
        let Err(err) = imp.import(ImportKind::Tiled, &plane, 64, 64, 1, Some(2)) else {
            panic!("scripted Err reply must fail the import")
        };
        assert!(format!("{err:#}").contains("EGL_BAD_MATCH"));
        assert!(!imp.dead(), "an Err reply must not mark the worker dead");

        // Replies exhausted → server closes → next import dies.
        let Err(err) = imp.import(ImportKind::Tiled, &plane, 64, 64, 1, Some(2)) else {
            panic!("a closed worker must fail the import")
        };
        assert!(format!("{err:#}").contains("died"), "{err:#}");
        assert!(imp.dead());
        let key = dmabuf_key(pr.as_fd()).unwrap();
        drop(imp);
        let seen = join.join().unwrap();
        match (&seen[0], &seen[1]) {
            (
                (
                    Request::Import {
                        has_fd: true,
                        kind: ImportKind::Tiled,
                        ..
                    },
                    Some(ino),
                ),
                (Request::Import { has_fd: false, .. }, None),
            ) => assert_eq!(*ino, key),
            other => panic!("unexpected requests {other:?}"),
        }
    }
}
