//! Touchpad frames → `0xCC/0x06` datagrams. One task stamps `seq`, so frames leave in the order
//! the embedder sent them. A still finger produces no frames, so while a contact or the button
//! is down the last frame goes again [`TOUCHPAD_REPEAT_MS`] after the previous send, with the
//! same `seq`: the host's gate drops the copy and its timeout clock still sees it. A frame with
//! a tombstone goes once more after [`TOMBSTONE_ECHO`] unless a newer one went first. Nothing
//! leaves without the pointer grant.

use super::*;
use crate::quic::{TouchpadFrame, GRANT_POINTER, TOUCHPAD_REPEAT_MS};

/// One lost datagram must not turn a retracted tap into a click; the second copy trails the
/// first by about a frame at 60 Hz.
const TOMBSTONE_ECHO: Duration = Duration::from_millis(20);

pub(super) async fn run(
    conn: ClientConn,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<TouchpadFrame>,
    shared: Arc<ClientShared>,
) {
    let period = Duration::from_millis(u64::from(TOUCHPAD_REPEAT_MS));
    let send = |f: &TouchpadFrame| {
        if shared.access_grants.load(Ordering::Relaxed) & GRANT_POINTER != 0 {
            let _ = conn.send_datagram(f.encode());
        }
    };
    let mut seq = 0u16;
    let mut held: Option<TouchpadFrame> = None;
    let mut echo: Option<TouchpadFrame> = None;
    let repeat = tokio::time::sleep(period);
    let echo_at = tokio::time::sleep(TOMBSTONE_ECHO);
    tokio::pin!(repeat, echo_at);
    loop {
        tokio::select! {
            f = rx.recv() => {
                let Some(mut f) = f else { break };
                f.seq = seq;
                seq = seq.wrapping_add(1);
                send(&f);
                held = f.is_active().then_some(f);
                echo = f.has_tombstone().then_some(f);
                let now = tokio::time::Instant::now();
                repeat.as_mut().reset(now + period);
                echo_at.as_mut().reset(now + TOMBSTONE_ECHO);
            }
            () = &mut echo_at, if echo.is_some() => {
                if let Some(f) = echo.take() {
                    send(&f);
                }
            }
            () = &mut repeat, if held.is_some() => {
                if let Some(f) = &held {
                    send(f);
                }
                repeat.as_mut().reset(tokio::time::Instant::now() + period);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::TouchpadContact;

    async fn next_frame(host: &quinn::Connection, wait_ms: u64) -> Option<TouchpadFrame> {
        let d = tokio::time::timeout(Duration::from_millis(wait_ms), host.read_datagram())
            .await
            .ok()?
            .unwrap();
        match crate::quic::v2::dgram::decode(&d) {
            Some(crate::quic::v2::dgram::Dgram::InputState(p)) => TouchpadFrame::decode(p),
            other => panic!("not an input datagram: {other:?}"),
        }
    }

    fn frame(contacts: &[TouchpadContact]) -> TouchpadFrame {
        TouchpadFrame::new(0, false, 700, 1500, contacts).unwrap()
    }

    fn shared(grants: u32) -> Arc<ClientShared> {
        let c = ClientShared::new(crate::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        });
        c.access_grants.store(grants, Ordering::Relaxed);
        Arc::new(c)
    }

    fn spawn(
        client_conn: quinn::Connection,
        grants: u32,
    ) -> (
        tokio::sync::mpsc::UnboundedSender<TouchpadFrame>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(ClientConn::new(client_conn), rx, shared(grants)));
        (tx, task)
    }

    const TOMB: TouchpadContact = TouchpadContact {
        id: 3,
        tip: false,
        confident: false,
        x: 5,
        y: 5,
    };

    /// A held finger repeats with its own `seq`; an empty frame takes the next one and stops it.
    #[tokio::test]
    async fn a_held_finger_repeats_until_it_lifts() {
        let (_server, _client, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;
        let (tx, task) = spawn(client_conn, crate::quic::GRANT_ALL);
        let down = frame(&[TouchpadContact {
            id: 0,
            tip: true,
            confident: true,
            x: 10,
            y: 20,
        }]);
        tx.send(down).unwrap();
        assert_eq!(next_frame(&host_conn, 2000).await, Some(down));
        let again = next_frame(&host_conn, 400).await.expect("a repeat");
        assert_eq!(again, down, "same seq");
        tx.send(frame(&[])).unwrap();
        let mut lifted = next_frame(&host_conn, 400).await.unwrap();
        while lifted.is_active() {
            lifted = next_frame(&host_conn, 400).await.unwrap();
        }
        assert_eq!(lifted.seq, 1);
        assert_eq!(
            next_frame(&host_conn, 300).await,
            None,
            "no repeat once lifted"
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_tombstone_frame_goes_twice_unless_overtaken() {
        let (_server, _client, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;
        let (tx, task) = spawn(client_conn, crate::quic::GRANT_ALL);
        let tomb = frame(&[TOMB]);
        tx.send(tomb).unwrap();
        assert_eq!(next_frame(&host_conn, 2000).await, Some(tomb));
        let sent = std::time::Instant::now();
        assert_eq!(next_frame(&host_conn, 400).await, Some(tomb), "the echo");
        assert!(sent.elapsed() < Duration::from_millis(200));
        assert_eq!(
            next_frame(&host_conn, 300).await,
            None,
            "once, and inactive"
        );

        tx.send(tomb).unwrap();
        tx.send(frame(&[])).unwrap();
        assert_eq!(next_frame(&host_conn, 2000).await.unwrap().seq, 1);
        assert_eq!(next_frame(&host_conn, 400).await.unwrap().seq, 2);
        assert_eq!(
            next_frame(&host_conn, 300).await,
            None,
            "no echo of the overtaken one"
        );
        task.abort();
    }

    #[tokio::test]
    async fn no_frame_leaves_without_the_pointer_grant() {
        let (_server, _client, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;
        let (tx, task) = spawn(client_conn, crate::quic::GRANT_ALL & !GRANT_POINTER);
        tx.send(frame(&[])).unwrap();
        assert_eq!(next_frame(&host_conn, 300).await, None);
        task.abort();
    }
}
