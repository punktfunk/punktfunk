//! The control loop: read one request from the host, act, reply. One thread, one request at a
//! time; the capture's arrival handler and the encode thread run beside it.
//!
//! The host is SYSTEM and this process is the user's, so a request is trusted as far as its
//! shape. A frame that does not parse ends the process: the host starts another.

use std::ffi::c_void;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytemuck::Pod;
use pf_driver_proto::encode::{self as wire, EncodeCtlRequest, SetEncodeReply, SetEncodeRequest};
use pf_driver_proto::worker::{self as proto, kind};
use pf_encode_session::lock;
use pf_encode_session::open::fail_reply;
use pf_encode_session::section::{AuSection, Ctl, EncodeSession};
use windows::Win32::Foundation::{CloseHandle, HANDLE};

use crate::session::{EncodeThread, ThreadCtx};
use crate::source::Source;
use crate::view::View;

/// How long `SET_ENCODE` waits for the thread's open, as the driver does. The host's own bound
/// on the request is longer, so it reads this reply and not its own timeout.
const OPEN_BOUND: Duration = Duration::from_secs(5);

/// The worker → host pipe. Replies come from the control thread and events from the capture's,
/// so every message is one write under this lock.
pub struct Tx(Mutex<File>);

impl Tx {
    fn send<T: Pod>(&self, kind: u32, seq: u32, body: &T) -> std::io::Result<()> {
        lock(&self.0).write_all(&proto::encode(kind, seq, body))
    }
}

/// One whole message, or `None` once the host closed its end.
fn read(rx: &mut File) -> Result<Option<(proto::Frame, Vec<u8>)>> {
    let mut head = [0u8; 16];
    match rx.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe) => {
            return Ok(None)
        }
        Err(e) => return Err(e).context("read the control pipe"),
    }
    let frame = match proto::header(&head) {
        Ok(f) => f,
        Err(e) => bail!("control pipe carried no message header: {e:?}"),
    };
    let mut body = vec![0u8; frame.len as usize];
    rx.read_exact(&mut body).context("read a message body")?;
    Ok(Some((frame, body)))
}

fn tag(name: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    let n = name.len().min(16);
    out[..n].copy_from_slice(&name.as_bytes()[..n]);
    out
}

/// Close a handle value the host duplicated into this process.
fn close_value(value: u64) {
    if value != 0 {
        // SAFETY: `CloseHandle` validates the value against this process's handle table.
        let _ = unsafe { CloseHandle(HANDLE(value as usize as *mut c_void)) };
    }
}

struct Worker {
    tx: Arc<Tx>,
    source: Option<Arc<Source>>,
    /// The live encode session. There is one source, so there is one.
    live: Option<Arc<EncodeSession>>,
    generation: u32,
}

impl Worker {
    fn stop_session(&mut self) {
        if let Some(session) = self.live.take() {
            session.stop();
        }
    }

    /// `OPEN_SOURCE`: replace whatever is captured now. The session on the old capture stops
    /// first; the host opens a new one on the reply.
    fn open_source(&mut self, req: &proto::OpenSource) -> proto::SourceReply {
        self.stop_session();
        self.source = None;
        let tx = self.tx.clone();
        let format = if req.flags & proto::OPEN_FP16 != 0 {
            proto::FORMAT_FP16
        } else {
            proto::FORMAT_BGRA8
        };
        let on_resize = Box::new(move |width, height| {
            tracing::info!(width, height, "capture source changed size");
            let changed = proto::SourceChanged {
                width,
                height,
                format,
            };
            let _ = tx.send(kind::SOURCE_CHANGED, 0, &changed);
        });
        let tx = self.tx.clone();
        let on_gone = Box::new(move || {
            tracing::info!("the captured monitor is gone");
            let gone = proto::SourceGone {
                reason: proto::GONE_MONITOR,
            };
            let _ = tx.send(kind::SOURCE_GONE, 0, &gone);
        });
        match Source::open(req, on_resize, on_gone) {
            Ok(source) => {
                let (width, height) = source.size();
                self.source = Some(Arc::new(source));
                proto::SourceReply {
                    status: proto::SOURCE_OK,
                    error: 0,
                    stage: [0; 16],
                    width,
                    height,
                    format,
                }
            }
            Err((status, error, stage)) => proto::SourceReply {
                status,
                error,
                stage: tag(stage),
                width: 0,
                height: 0,
                format: 0,
            },
        }
    }

    /// `SET_ENCODE`: open an encoder on the capture and publish into the host's section,
    /// replacing any session there is. Both handle values are this process's from the moment
    /// the request arrives: every path below closes them or hands them to the session.
    fn set_encode(&mut self, req: &SetEncodeRequest) -> SetEncodeReply {
        let listed = req.backends[0] != 0 && req.backends.iter().all(|&b| wire::backend::listed(b));
        let valid = req.section != 0
            && req.event != 0
            && wire::codec::valid(req.codec)
            && req.width != 0
            && req.height != 0
            && listed;
        let refuse = |status, fail| {
            close_value(req.section);
            close_value(req.event);
            fail_reply(status, fail)
        };
        if !valid {
            return refuse(wire::SET_ENCODE_BAD_SECTION, (-9, "request"));
        }
        let Some(source) = self.source.clone() else {
            return refuse(wire::SET_ENCODE_NO_MONITOR, (-6, "nosource"));
        };
        let section = HANDLE(req.section as usize as *mut c_void);
        let Some(view) = View::map(section, req.section_bytes as usize) else {
            return refuse(wire::SET_ENCODE_BAD_SECTION, (-9, "map"));
        };
        let event = req.event as usize as *mut c_void;
        // SAFETY: `event` is the ready event the host duplicated into this process for this
        // section; the writer is its sole closer from here. A refusal adopts nothing, and the
        // view unmaps as it drops.
        let section = match unsafe { AuSection::adopt(Box::new(view), req.section_bytes, event) } {
            Ok(s) => s,
            Err(fail) => return refuse(wire::SET_ENCODE_BAD_SECTION, fail),
        };
        // The view keeps the section alive, so the duplicated handle can close now.
        close_value(req.section);
        // The host's `host.env` knobs, before any backend reads them.
        pf_encode_win::knobs::set(req.knobs);
        self.stop_session();
        self.generation = self.generation.wrapping_add(1);
        let session = Arc::new(EncodeSession::new(*req, section, self.generation));
        match self.start(&session, source) {
            Ok(reply) => {
                self.live = Some(session);
                reply
            }
            Err(reply) => reply,
        }
    }

    /// Run `session` on a new thread and wait for its open. `Err` is the failed open's reply,
    /// with the thread stopped.
    fn start(
        &self,
        session: &Arc<EncodeSession>,
        source: Arc<Source>,
    ) -> Result<SetEncodeReply, SetEncodeReply> {
        let (opened, rx) = sync_channel(1);
        let Some(thread) = EncodeThread::spawn(ThreadCtx {
            session: session.clone(),
            source,
            opened,
        }) else {
            return Err(fail_reply(wire::SET_ENCODE_THREAD, (-7, "spawn")));
        };
        let reply = match rx.recv_timeout(OPEN_BOUND) {
            Ok(reply) => reply,
            Err(RecvTimeoutError::Timeout) => fail_reply(wire::SET_ENCODE_TIMEOUT, (-3, "open")),
            Err(RecvTimeoutError::Disconnected) => {
                fail_reply(wire::SET_ENCODE_THREAD, (-7, "exit"))
            }
        };
        drop(session.set_thread(Box::new(thread)));
        if reply.status != wire::SET_ENCODE_OK {
            session.stop();
            return Err(reply);
        }
        Ok(reply)
    }

    /// `ENCODE_CTL`. `reset` and `close` act here; everything else is queued for the encode
    /// thread and wakes it.
    fn encode_ctl(&mut self, req: &EncodeCtlRequest) -> u32 {
        let Some(session) = self.live.clone() else {
            return proto::CTL_NOT_FOUND;
        };
        match req.op {
            wire::ENCODE_CTL_RESET => {
                let Some(source) = self.source.clone() else {
                    return proto::CTL_NOT_FOUND;
                };
                session.stop();
                session.rebase(req.arg0);
                let reopened = self.start(&session, source);
                tracing::info!(
                    wire_seq = req.arg0,
                    status = ?reopened.as_ref().map_or_else(|r| r.status, |r| r.status),
                    "encoder reset"
                );
                if reopened.is_ok() {
                    proto::CTL_OK
                } else {
                    proto::CTL_FAILED
                }
            }
            // A stale proxy names an older generation and stops nothing.
            wire::ENCODE_CTL_CLOSE => {
                if session.generation == req.arg0 {
                    self.stop_session();
                }
                proto::CTL_OK
            }
            _ => match Ctl::queued(req) {
                Some(op) => {
                    session.push_ctl(op);
                    if let Some(source) = &self.source {
                        source.wake();
                    }
                    proto::CTL_OK
                }
                None => proto::CTL_INVALID,
            },
        }
    }
}

/// Serve the host until it closes its end. `rx` and `tx` are this process's two pipe ends.
pub fn serve(mut rx: File, tx: File) -> Result<()> {
    let tx = Arc::new(Tx(Mutex::new(tx)));
    let ours = proto::Hello::new(env!("CARGO_PKG_VERSION"));
    tx.send(kind::HELLO, 0, &ours).context("send hello")?;
    let theirs = match read(&mut rx)? {
        Some((frame, body)) if frame.kind == kind::HELLO => proto::body::<proto::Hello>(&body),
        _ => None,
    };
    if theirs != Some(ours) {
        bail!("the host is another build: {theirs:?}");
    }
    let mut worker = Worker {
        tx: tx.clone(),
        source: None,
        live: None,
        generation: 0,
    };
    while let Some((frame, body)) = read(&mut rx)? {
        let reply = frame.kind | proto::REPLY;
        let sent = match frame.kind {
            kind::OPEN_SOURCE => {
                let req = proto::body(&body).context("OPEN_SOURCE body")?;
                tx.send(reply, frame.seq, &worker.open_source(&req))
            }
            kind::SET_ENCODE => {
                let req = proto::body(&body).context("SET_ENCODE body")?;
                tx.send(reply, frame.seq, &worker.set_encode(&req))
            }
            kind::ENCODE_CTL => {
                let req = proto::body(&body).context("ENCODE_CTL body")?;
                let status = worker.encode_ctl(&req);
                tx.send(reply, frame.seq, &proto::CtlReply { status })
            }
            other => bail!("unknown control message {other:#x}"),
        };
        sent.context("reply on the control pipe")?;
    }
    // The host is gone or done with this session: stop cleanly, in order.
    worker.stop_session();
    worker.source = None;
    Ok(())
}
