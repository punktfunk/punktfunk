//! The host's end of one capture worker's control channel: the two pipes of
//! [`pf_driver_proto::worker`] and, as a [`Peer`], the process behind them.
//!
//! [`Link`] keeps one request in flight and waits a bound for it to be written and answered.
//! The worker is less privileged than this process: every reply is checked before use, a worker
//! that takes a request late, answers late or with the wrong bytes is ended, and nothing waits
//! on the worker without a bound. A stream that stops being messages ends the channel, and a
//! request that finds it ended ends a worker that still runs.
//!
//! Generic over the pipes and the process, so the tests stand in for a worker on every
//! target; `wgc.rs` hands over the real ones. Evidence: `design/windows-wgc-capture.md`
//! §4.3, §4.7, §4.13.

use std::io::{Read, Write};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bytemuck::Pod;
use pf_driver_proto::worker::{self as proto, kind};

pub(crate) type Message = (proto::Frame, Vec<u8>);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The worker's process, as its link uses it.
pub(crate) trait Peer {
    /// End the worker now. It holds no state of ours: a session starts another.
    fn kill(&self);
    /// The worker's exit code once it has exited.
    fn exit_code(&self) -> Option<u32>;
}

/// One worker's process and pipes. Closing the request pipe is how a worker is told to exit.
pub(crate) struct Link<P> {
    peer: P,
    /// The request pipe and the last sequence sent; `None` once closed. Held across a whole
    /// round trip, so one request is in flight at a time.
    requests: Mutex<Option<(Requests, u32)>>,
    replies: Mutex<Receiver<Message>>,
    events: Mutex<Receiver<Message>>,
}

/// The request pipe, written on a thread of its own. A worker that stops reading blocks that
/// thread, never the caller; a duplicate of the read end keeps it blocked past the worker's end.
struct Requests {
    inbox: SyncSender<Vec<u8>>,
    written: Receiver<std::io::Result<()>>,
}

impl Requests {
    /// Hand `bytes` to the writer and wait until `deadline` for them to be written.
    fn send(&self, bytes: Vec<u8>, deadline: Instant) -> Result<()> {
        self.inbox
            .try_send(bytes)
            .map_err(|_| anyhow!("the request writer stopped"))?;
        match self
            .written
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(written) => Ok(written?),
            Err(RecvTimeoutError::Timeout) => bail!("not taken before the bound"),
            Err(RecvTimeoutError::Disconnected) => bail!("the request writer stopped"),
        }
    }
}

/// Write each message to the worker's pipe and say so, until the link lets go or a write fails.
fn write_out(
    mut pipe: impl Write,
    inbox: Receiver<Vec<u8>>,
    written: SyncSender<std::io::Result<()>>,
) {
    for bytes in inbox {
        let result = pipe.write_all(&bytes);
        let failed = result.is_err();
        if written.send(result).is_err() || failed {
            return;
        }
    }
}

/// Read whole messages off the worker's pipe until it closes or sends something that is not
/// one. Both queues are bounded: a worker that floods loses messages, not the host its memory.
fn pump(mut pipe: impl Read, replies: SyncSender<Message>, events: SyncSender<Message>) {
    loop {
        let mut head = [0u8; 16];
        if pipe.read_exact(&mut head).is_err() {
            return;
        }
        let frame = match proto::header(&head) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = ?e, "capture worker sent no message header");
                return;
            }
        };
        let mut body = vec![0u8; frame.len as usize];
        if pipe.read_exact(&mut body).is_err() {
            return;
        }
        let answer = frame.kind & proto::REPLY != 0 || frame.kind == kind::HELLO;
        let queue = if answer { &replies } else { &events };
        let _ = queue.try_send((frame, body));
    }
}

impl<P: Peer> Link<P> {
    /// Take a started worker's pipes over: its requests written and its replies pumped, each
    /// on a thread of its own.
    pub(crate) fn open(
        peer: P,
        requests: impl Write + Send + 'static,
        replies: impl Read + Send + 'static,
    ) -> Result<Self> {
        let (reply_tx, reply_rx) = sync_channel(8);
        let (event_tx, event_rx) = sync_channel(64);
        std::thread::Builder::new()
            .name("pf-cw-replies".into())
            .spawn(move || pump(replies, reply_tx, event_tx))
            .context("spawn the capture worker reply thread")?;
        let (inbox, inbox_rx) = sync_channel(1);
        let (written_tx, written) = sync_channel(1);
        std::thread::Builder::new()
            .name("pf-cw-requests".into())
            .spawn(move || write_out(requests, inbox_rx, written_tx))
            .context("spawn the capture worker request thread")?;
        Ok(Self {
            peer,
            requests: Mutex::new(Some((Requests { inbox, written }, 0))),
            replies: Mutex::new(reply_rx),
            events: Mutex::new(event_rx),
        })
    }

    /// Trade hellos. A worker of another build is ended here.
    pub(crate) fn hello(&self, bound: Duration) -> Result<()> {
        let ours = proto::Hello::new(env!("CARGO_PKG_VERSION"));
        let theirs: proto::Hello = self.request(kind::HELLO, &ours, bound)?;
        if theirs != ours {
            self.peer.kill();
            bail!(
                "capture worker is another build ({:?})",
                String::from_utf8_lossy(&theirs.build).trim_end_matches('\0')
            );
        }
        Ok(())
    }

    pub(crate) fn peer(&self) -> &P {
        &self.peer
    }

    /// Send one request and wait `bound` for it to be written and answered. A hello is
    /// answered by a hello and is unnumbered on both sides. A worker that does not take the
    /// request, does not answer, answers with the wrong bytes, or still runs once its reply
    /// channel ended, is ended. One that does not take it also closes the link: the write may
    /// still be under way.
    pub(crate) fn request<Q: Pod, R: Pod>(&self, op: u32, body: &Q, bound: Duration) -> Result<R> {
        let mut requests = lock(&self.requests);
        let Some((pipe, last)) = requests.as_mut() else {
            bail!("capture worker is closed");
        };
        let (answer, seq) = if op == kind::HELLO {
            (kind::HELLO, 0)
        } else {
            // `0` is an event's sequence.
            *last = last.wrapping_add(1).max(1);
            (op | proto::REPLY, *last)
        };
        let deadline = Instant::now() + bound;
        if let Err(e) = pipe.send(proto::encode(op, seq, body), deadline) {
            *requests = None;
            self.peer.kill();
            return Err(e).context("write to the capture worker");
        }
        let replies = lock(&self.replies);
        loop {
            match replies.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok((frame, bytes)) if frame.kind == answer && frame.seq == seq => {
                    return proto::body(&bytes).ok_or_else(|| {
                        self.peer.kill();
                        anyhow!("capture worker answered {op:#x} with {} bytes", bytes.len())
                    });
                }
                // The answer to a request nobody waits for any more.
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    self.peer.kill();
                    bail!("capture worker did not answer {op:#x} within {bound:?}");
                }
                // The pump stopped: the pipe closed, or carried what is not a message.
                Err(RecvTimeoutError::Disconnected) => {
                    let Some(code) = self.peer.exit_code() else {
                        self.peer.kill();
                        bail!("capture worker's reply channel ended before it answered {op:#x}");
                    };
                    bail!("capture worker exited (code {code})");
                }
            }
        }
    }

    /// The next event the worker sent, if any.
    pub(crate) fn event(&self) -> Option<Message> {
        lock(&self.events).try_recv().ok()
    }

    /// Close the request pipe: the worker exits when it does.
    pub(crate) fn close(&self) {
        drop(lock(&self.requests).take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;
    use std::collections::VecDeque;
    use std::io;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{channel, Sender};
    use std::sync::Arc;

    /// A bound a silent worker runs out.
    const SOON: Duration = Duration::from_millis(30);
    /// A bound no answered request reaches: a test that waits it out fails, it does not hang.
    const AMPLE: Duration = Duration::from_secs(10);

    /// The reply pipe. The pump blocks on it as on a pipe, and reads end-of-file once the
    /// worker's end drops.
    struct Pipe {
        rx: Receiver<Vec<u8>>,
        buf: VecDeque<u8>,
    }

    impl Read for Pipe {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            while self.buf.is_empty() {
                match self.rx.recv() {
                    Ok(bytes) => self.buf.extend(bytes),
                    Err(_) => return Ok(0),
                }
            }
            self.buf.read(out)
        }
    }

    /// The request pipe as the worker reads it, kept for the test to look at.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    impl Write for Sent {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A request pipe the worker stopped draining: a write blocks until the test lets go.
    struct Stuck(Receiver<()>);

    impl Write for Stuck {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A worker process that counts how often it was ended. It still runs unless `exited`
    /// holds its code.
    #[derive(Default)]
    struct Worker {
        kills: AtomicU32,
        exited: Option<u32>,
    }

    impl Peer for Worker {
        fn kill(&self) {
            self.kills.fetch_add(1, Ordering::Relaxed);
        }

        fn exit_code(&self) -> Option<u32> {
            self.exited
        }
    }

    /// A link to `process` over `requests` as its request pipe, and the worker's end of its
    /// reply pipe.
    fn link_to(
        process: Worker,
        requests: impl Write + Send + 'static,
    ) -> (Link<Worker>, Sender<Vec<u8>>) {
        let (worker, rx) = channel();
        let buf = VecDeque::new();
        let link = Link::open(process, requests, Pipe { rx, buf }).unwrap();
        (link, worker)
    }

    /// A link to a worker that runs.
    fn link(requests: impl Write + Send + 'static) -> (Link<Worker>, Sender<Vec<u8>>) {
        link_to(Worker::default(), requests)
    }

    fn hello() -> proto::Hello {
        proto::Hello::new(env!("CARGO_PKG_VERSION"))
    }

    /// The reply to the `seq`th `OPEN_SOURCE`, its width marking which one it is.
    fn opened(seq: u32, width: u32) -> Vec<u8> {
        let reply = proto::SourceReply {
            width,
            ..Zeroable::zeroed()
        };
        proto::encode(kind::OPEN_SOURCE | proto::REPLY, seq, &reply)
    }

    fn open_source(link: &Link<Worker>, bound: Duration) -> Result<proto::SourceReply> {
        link.request(kind::OPEN_SOURCE, &proto::OpenSource::zeroed(), bound)
    }

    fn refusal<T>(r: Result<T>) -> String {
        format!("{:#}", r.err().expect("a refusal"))
    }

    fn kills(link: &Link<Worker>) -> u32 {
        link.peer().kills.load(Ordering::Relaxed)
    }

    #[test]
    fn a_matching_hello_opens_the_link_and_requests_count_from_one() {
        let sent = Sent::default();
        let (link, worker) = link(sent.clone());
        worker
            .send(proto::encode(kind::HELLO, 0, &hello()))
            .unwrap();
        link.hello(AMPLE).unwrap();
        for seq in 1..=2 {
            worker.send(opened(seq, seq)).unwrap();
            assert_eq!(open_source(&link, AMPLE).unwrap().width, seq);
        }
        assert_eq!(kills(&link), 0);
        drop(link);
        // What the worker was sent: each message's kind and number.
        let mut heads = Vec::new();
        let sent = lock(&sent.0);
        let mut rest = &sent[..];
        while let Some((head, tail)) = rest.split_first_chunk::<16>() {
            let frame = proto::header(head).unwrap();
            heads.push((frame.kind, frame.seq));
            rest = &tail[frame.len as usize..];
        }
        assert_eq!(
            heads,
            [
                (kind::HELLO, 0),
                (kind::OPEN_SOURCE, 1),
                (kind::OPEN_SOURCE, 2)
            ]
        );
    }

    /// Wrong magic, a body over `MAX_BODY`, and a worker that closes mid-frame: the pump
    /// stops at the first and parses nothing behind it. The worker still runs, so the request
    /// ends it and names the channel, not an exit.
    #[test]
    fn a_stream_that_is_not_messages_ends_the_channel() {
        let good = proto::encode(kind::HELLO, 0, &hello());
        let mut foreign = good.clone();
        foreign[0] ^= 0xFF;
        foreign.extend(&good);
        let long = proto::Frame {
            magic: proto::MAGIC,
            kind: kind::HELLO,
            seq: 0,
            len: proto::MAX_BODY + 1,
        };
        let mut oversized = bytemuck::bytes_of(&long).to_vec();
        oversized.resize(16 + long.len as usize, 0);
        oversized.extend(&good);
        for (what, bytes, closes) in [
            ("wrong magic", foreign, false),
            ("a body over the cap", oversized, false),
            ("closed mid-header", good[..7].to_vec(), true),
            ("closed mid-body", good[..26].to_vec(), true),
        ] {
            let (link, worker) = link(io::sink());
            worker.send(bytes).unwrap();
            let worker = (!closes).then_some(worker);
            let said = refusal(link.hello(AMPLE));
            let ended = "reply channel ended before it answered 0x1";
            assert!(said.contains(ended), "{what}: {said}");
            assert_eq!(kills(&link), 1, "{what}");
            drop(worker);
        }
    }

    /// A worker that exited is named as that, with its code, and is not ended again.
    #[test]
    fn a_worker_that_exited_is_reported_with_its_code() {
        let exited = Worker {
            exited: Some(3),
            ..Worker::default()
        };
        let (link, worker) = link_to(exited, io::sink());
        drop(worker);
        let said = refusal(link.hello(AMPLE));
        assert!(said.contains("capture worker exited (code 3)"), "{said}");
        assert_eq!(kills(&link), 0);
    }

    #[test]
    fn a_hello_that_differs_in_one_field_is_another_build() {
        let ours = hello();
        let mut build = ours;
        build.build[0] ^= 1;
        let protocol = proto::Hello {
            protocol: ours.protocol + 1,
            ..ours
        };
        let pad = proto::Hello { _pad: 1, ..ours };
        for theirs in [protocol, pad, build] {
            let (link, worker) = link(io::sink());
            worker.send(proto::encode(kind::HELLO, 0, &theirs)).unwrap();
            let said = refusal(link.hello(AMPLE));
            assert!(said.contains("is another build"), "{theirs:?}: {said}");
            assert_eq!(kills(&link), 1, "{theirs:?}");
        }
    }

    #[test]
    fn a_worker_that_never_answers_is_ended_at_the_bound() {
        let (link, _worker) = link(io::sink());
        let said = refusal(link.hello(SOON));
        assert!(said.contains("did not answer 0x1"), "{said}");
        assert_eq!(kills(&link), 1);
    }

    /// The answer to a request that already ran out is not the next request's, and neither
    /// is a reply to another op under the right number.
    #[test]
    fn a_reply_past_its_deadline_answers_nothing() {
        let (link, worker) = link(io::sink());
        let said = refusal(open_source(&link, SOON));
        assert!(said.contains("did not answer 0x2"), "{said}");
        worker.send(opened(1, 1)).unwrap();
        let other = proto::CtlReply {
            status: proto::CTL_OK,
        };
        worker
            .send(proto::encode(kind::ENCODE_CTL | proto::REPLY, 2, &other))
            .unwrap();
        worker.send(opened(2, 2)).unwrap();
        assert_eq!(open_source(&link, AMPLE).unwrap().width, 2);
        assert_eq!(kills(&link), 1, "ended once, for the late reply");
    }

    #[test]
    fn a_reply_of_the_wrong_size_ends_the_worker() {
        let (link, worker) = link(io::sink());
        let short = proto::CtlReply {
            status: proto::CTL_OK,
        };
        worker
            .send(proto::encode(kind::OPEN_SOURCE | proto::REPLY, 1, &short))
            .unwrap();
        let said = refusal(open_source(&link, AMPLE));
        assert!(said.contains("answered 0x2 with 4 bytes"), "{said}");
        assert_eq!(kills(&link), 1);
    }

    /// Nothing drains the events during the flood, so the queue's bound is what is left.
    #[test]
    fn a_flood_of_events_costs_messages_not_memory() {
        let (link, worker) = link(io::sink());
        let gone = proto::SourceGone {
            reason: proto::GONE_MONITOR,
        };
        for _ in 0..1000 {
            worker
                .send(proto::encode(kind::SOURCE_GONE, 0, &gone))
                .unwrap();
        }
        worker.send(opened(1, 1)).unwrap();
        open_source(&link, AMPLE).expect("the pump is past the flood");
        assert_eq!(std::iter::from_fn(|| link.event()).count(), 64);
    }

    /// A request pipe that takes no byte, as an empty buffer does.
    #[test]
    fn a_request_the_worker_cannot_take_ends_it() {
        let (link, _worker) = link(io::Cursor::new([0u8; 0]));
        let said = refusal(open_source(&link, AMPLE));
        assert!(said.contains("write to the capture worker"), "{said}");
        assert_eq!(kills(&link), 1);
    }

    /// A worker that never drains its request pipe is ended at the bound, and the link sends
    /// nothing behind the write still under way.
    #[test]
    fn a_worker_that_stops_taking_requests_is_ended_at_the_bound() {
        let (unstick, stuck) = channel();
        let (link, _worker) = link(Stuck(stuck));
        let said = refusal(open_source(&link, SOON));
        assert!(said.contains("write to the capture worker"), "{said}");
        assert!(said.contains("not taken before the bound"), "{said}");
        assert_eq!(kills(&link), 1);
        let said = refusal(open_source(&link, AMPLE));
        assert!(said.contains("capture worker is closed"), "{said}");
        drop(unstick);
    }

    #[test]
    fn a_closed_link_sends_nothing() {
        let sent = Sent::default();
        let (link, _worker) = link(sent.clone());
        link.close();
        let said = refusal(open_source(&link, AMPLE));
        assert!(said.contains("capture worker is closed"), "{said}");
        drop(link);
        assert!(lock(&sent.0).is_empty());
    }
}
