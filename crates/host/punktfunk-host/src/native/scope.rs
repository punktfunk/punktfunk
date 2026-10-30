//! One session's scope: what it starts, what it holds, and its one teardown.
//!
//! A session spawns its side threads ([`SessionScope::thread`]) and tasks
//! ([`SessionScope::task`]) here and hands over its RAII guards ([`SessionScope::hold`]).
//! [`SessionScope::finish`] tears it down in this order, steps 3–7 on a blocking thread:
//!
//! 1. end the lifecycle: `host_ended` on `Ok`, `host_error` on `Err`; the first reason wins;
//! 2. close the link `0 "done"` or `1 "error"`, which ends every task; a session that failed
//!    before it streamed is left open for its plane's typed refusal (`setup_failed_sentence`);
//! 3. join the side threads, and a display prep the stream never took over
//!    ([`SessionScope::prep_done`]), within [`SIDE_THREAD_JOIN_GRACE`], else detach them;
//! 4. enter [`Phase::Done`];
//! 5. drop the gamescope hold: the last one out hands the TV's gaming session back;
//! 6. drop the planes, after the input thread that routes through them;
//! 7. drop the held guards, newest first: the slot permit, held first, goes last.
//!
//! A scope dropped before [`Phase::Done`] (a panic, a cancelled session) ends `host_error`,
//! closes `1 "error"` and runs 3–7 off the dropping task.

use super::{link, planes::SessionPlanes};
use crate::compositor_route::GamescopeHold;
use crate::events::SessionEndReason;
use crate::lifecycle::{Lifecycle, Phase};

/// Audio/input join after close. Both poll `stop`: audio every ≤100 ms, input every ≤25 ms.
/// A display prep sees `stop` between build attempts, after the one in flight. This only
/// catches a wedge.
const SIDE_THREAD_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) struct SessionScope {
    life: Lifecycle,
    conn: link::SessionLink,
    held: Held,
}

/// Everything steps 3–7 of the teardown let go of.
#[derive(Default)]
struct Held {
    threads: Vec<std::thread::JoinHandle<()>>,
    /// The other end of [`SessionScope::prep_done`].
    prep: Option<std::sync::mpsc::Receiver<()>>,
    gamescope: Option<GamescopeHold>,
    planes: Option<SessionPlanes>,
    guards: Vec<Box<dyn Send>>,
}

impl SessionScope {
    /// Takes the gamescope hold before the handshake resolves the compositor, so a session that
    /// fails there still hands the TV back.
    pub(super) fn new(life: Lifecycle, conn: link::SessionLink) -> Self {
        let held = Held {
            gamescope: Some(GamescopeHold::new(life.clone())),
            ..Held::default()
        };
        SessionScope { life, conn, held }
    }

    pub(super) fn life(&self) -> &Lifecycle {
        &self.life
    }

    pub(super) fn conn(&self) -> &link::SessionLink {
        &self.conn
    }

    /// A side thread the teardown joins.
    pub(super) fn thread(
        &mut self,
        name: &str,
        f: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        let thread = std::thread::Builder::new().name(name.into()).spawn(f)?;
        self.held.threads.push(thread);
        Ok(())
    }

    /// The end a Welcome-time display prep holds while it may own the session's display. Its
    /// send says the stream took the prep over, and the stream thread joins it; until then,
    /// or until every sender drops, step 3 waits for it as for a side thread.
    pub(super) fn prep_done(&mut self) -> std::sync::mpsc::SyncSender<()> {
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        self.held.prep = Some(wait);
        done
    }

    /// A task of this session. It ends on the link's close, which the teardown makes, or on a
    /// lane the stream side drops; none is joined.
    pub(super) fn task(&self, f: impl std::future::Future<Output = ()> + Send + 'static) {
        tokio::spawn(f);
    }

    /// Held to step 7 of the teardown.
    pub(super) fn hold<G: Send + 'static>(&mut self, guard: G) {
        self.held.guards.push(Box::new(guard));
    }

    /// Held to step 6 of the teardown, after the input thread that routes through them.
    pub(super) fn set_planes(&mut self, planes: SessionPlanes) {
        self.held.planes = Some(planes);
    }

    pub(super) fn planes(&self) -> &SessionPlanes {
        self.held
            .planes
            .as_ref()
            .expect("a session mints its planes before it reads them")
    }

    /// The teardown, steps 1–7. The join is bounded: a stuck side thread must not hold the
    /// permit or the admission entry.
    pub(super) async fn finish(mut self, result: &anyhow::Result<()>) {
        let ok = result.is_ok();
        // The plane that admitted a session refuses one that never streamed, in its own words;
        // a close here would pre-empt that typed close.
        let refused_by_plane = !ok && self.life.phase() < Phase::Streaming;
        self.life.end(if ok {
            SessionEndReason::HostEnded
        } else {
            SessionEndReason::HostError
        });
        if !refused_by_plane {
            self.conn
                .close(if ok { 0 } else { 1 }, if ok { b"done" } else { b"error" });
        }
        let (held, life) = (std::mem::take(&mut self.held), self.life.clone());
        let _ = tokio::task::spawn_blocking(move || held.release(&life)).await;
    }
}

impl Held {
    /// Steps 3–7. Blocking: the joins wait, and the planes' mic release is a PipeWire round trip.
    fn release(self, life: &Lifecycle) {
        let Held {
            threads,
            mut prep,
            gamescope,
            planes,
            mut guards,
        } = self;
        let deadline = std::time::Instant::now() + SIDE_THREAD_JOIN_GRACE;
        while (threads.iter().any(|t| !t.is_finished()) || prep_owns_display(&mut prep))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if prep_owns_display(&mut prep) {
            tracing::warn!(
                grace_s = SIDE_THREAD_JOIN_GRACE.as_secs(),
                "display prep did not return after the session stopped — detaching it; the \
                 display it builds stays up until it does"
            );
        }
        if threads.iter().any(|t| !t.is_finished()) {
            // Input thread still owns the virtual pads (Windows: devnode + pad-index mailbox).
            // The next create on that index fails as already-owned until this thread returns.
            tracing::warn!(
                grace_s = SIDE_THREAD_JOIN_GRACE.as_secs(),
                "audio/input threads did not exit after the session stopped — detaching them. \
                 This session's virtual gamepads are STILL HELD by the detached input thread \
                 (devnode + pad-index mailbox on Windows), so a pad create on the same index \
                 will be refused as already-owned until it returns"
            );
        }
        for thread in threads.into_iter().filter(|t| t.is_finished()) {
            let _ = thread.join();
        }
        life.enter(Phase::Done);
        drop(gamescope);
        drop(planes);
        while let Some(guard) = guards.pop() {
            drop(guard);
        }
    }
}

/// Whether a display prep may still own the session's display: neither taken over by the
/// stream nor gone.
fn prep_owns_display(prep: &mut Option<std::sync::mpsc::Receiver<()>>) -> bool {
    use std::sync::mpsc::TryRecvError;
    if let Some(Err(TryRecvError::Empty)) = prep.as_ref().map(|p| p.try_recv()) {
        return true;
    }
    *prep = None;
    false
}

impl Drop for SessionScope {
    fn drop(&mut self) {
        if self.life.phase() == Phase::Done {
            return;
        }
        self.life.end(SessionEndReason::HostError);
        self.conn.close(1, b"error");
        let (held, life) = (std::mem::take(&mut self.held), self.life.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => drop(rt.spawn_blocking(move || held.release(&life))),
            Err(_) => held.release(&life),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    /// A host link to a loopback client, the client's end, and the endpoints both live on.
    async fn loopback() -> (
        link::SessionLink,
        quinn::Connection,
        (quinn::Endpoint, quinn::Endpoint),
    ) {
        use punktfunk_core::quic::endpoint;
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).expect("server");
        let addr = server.local_addr().expect("server address");
        let client = endpoint::client_insecure().expect("client");
        let (host, peer) = tokio::join!(
            async { server.accept().await.expect("incoming").await },
            async { client.connect(addr, "punktfunk").expect("dial").await },
        );
        let host = host.expect("host side connects");
        let media = std::net::UdpSocket::bind("127.0.0.1:0").expect("media socket");
        let link = link::SessionLink::QuicV2(
            host.clone(),
            Arc::new(link::V2Link::new(host, Arc::new(media))),
        );
        (link, peer.expect("client side connects"), (server, client))
    }

    /// Records its name when dropped.
    struct Mark(&'static str, Arc<Mutex<Vec<&'static str>>>);

    impl Drop for Mark {
        fn drop(&mut self) {
            self.1.lock().unwrap().push(self.0);
        }
    }

    /// A scope with a guard and a side thread that outlives `stop` by 50 ms, and the order
    /// the two went in.
    fn scope_with_a_slow_thread(
        conn: link::SessionLink,
        life: &Lifecycle,
    ) -> (SessionScope, Arc<Mutex<Vec<&'static str>>>) {
        let mut scope = SessionScope::new(life.clone(), conn);
        let order = Arc::new(Mutex::new(Vec::new()));
        scope.hold(Mark("guard", order.clone()));
        let (stop, seen) = (life.stop_flag().clone(), order.clone());
        scope
            .thread("pf-scope-test", move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                seen.lock().unwrap().push("thread");
            })
            .unwrap();
        (scope, order)
    }

    /// A guard outlives the side thread that may still use it: the join comes first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_joins_the_side_threads_before_it_drops_a_guard() {
        let (conn, peer, _endpoints) = loopback().await;
        let life = Lifecycle::new();
        let (scope, order) = scope_with_a_slow_thread(conn, &life);
        scope.finish(&Ok(())).await;
        assert_eq!(*order.lock().unwrap(), ["thread", "guard"]);
        assert_eq!(life.reason(), Some(SessionEndReason::HostEnded));
        assert_eq!(life.phase(), Phase::Done);
        assert_eq!(peer_close(&peer).await, (0, b"done".to_vec()));
    }

    /// The host's application close as the client reads it: code and reason.
    async fn peer_close(peer: &quinn::Connection) -> (u64, Vec<u8>) {
        match peer.closed().await {
            quinn::ConnectionError::ApplicationClosed(c) => {
                (u64::from(c.error_code), c.reason.to_vec())
            }
            other => panic!("expected the host's close, got {other:?}"),
        }
    }

    /// A bring-up that fails leaves the link open for the typed refusal of the plane that
    /// admitted it, which carries the user's sentence. A stream that fails closes `1 "error"`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_bring_up_leaves_the_close_to_its_plane() {
        for (phase, closes) in [(Phase::BringUp, false), (Phase::Streaming, true)] {
            let (conn, peer, _endpoints) = loopback().await;
            let life = Lifecycle::new();
            life.enter(phase);
            let scope = SessionScope::new(life.clone(), conn.clone());
            scope.finish(&Err(anyhow::anyhow!("no display"))).await;
            assert_eq!(
                life.reason(),
                Some(SessionEndReason::HostError),
                "{phase:?}"
            );
            assert_eq!(conn.close_reason().is_some(), closes, "{phase:?}");
            let setup_failed = punktfunk_core::reject::SETUP_FAILED_CLOSE_CODE;
            conn.close(setup_failed, b"The host's screen is asleep");
            let said = peer_close(&peer).await;
            match closes {
                true => assert_eq!(said, (1, b"error".to_vec())),
                false => assert_eq!(said.0, u64::from(setup_failed)),
            }
        }
    }

    /// A scope dropped unfinished (a panic, a cancelled session) ends as the host's error,
    /// closes the link, and still joins its thread before its guard goes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_scope_tears_down_as_an_error() {
        let (conn, peer, _endpoints) = loopback().await;
        let life = Lifecycle::new();
        let (scope, order) = scope_with_a_slow_thread(conn, &life);
        drop(scope);
        assert_eq!(life.reason(), Some(SessionEndReason::HostError));
        assert_eq!(peer_close(&peer).await, (1, b"error".to_vec()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while order.lock().unwrap().len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(*order.lock().unwrap(), ["thread", "guard"]);
        assert_eq!(life.phase(), Phase::Done);
    }

    /// A display prep the stream never took over holds the teardown until its thread lets the
    /// display go; one the stream took over is the stream thread's to join.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn finish_waits_for_a_prep_only_while_it_owns_the_display() {
        for taken_over in [false, true] {
            let (conn, _peer, _endpoints) = loopback().await;
            let life = Lifecycle::new();
            let mut scope = SessionScope::new(life.clone(), conn);
            let order = Arc::new(Mutex::new(Vec::new()));
            scope.hold(Mark("guard", order.clone()));
            let (done, stop, seen) = (scope.prep_done(), life.stop_flag().clone(), order.clone());
            let (release, released) = std::sync::mpsc::channel::<()>();
            let prep = std::thread::spawn(move || {
                if taken_over {
                    done.send(()).unwrap();
                    let _ = released.recv();
                } else {
                    while !stop.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                seen.lock().unwrap().push("prep");
                drop(done);
            });
            scope.finish(&Ok(())).await;
            let want: &[&str] = if taken_over {
                &["guard"]
            } else {
                &["prep", "guard"]
            };
            assert_eq!(*order.lock().unwrap(), want, "taken over: {taken_over}");
            drop(release);
            prep.join().unwrap();
        }
    }
}
