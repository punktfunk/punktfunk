//! Touchpad frames → `0xCC/0x06` datagrams. One task stamps `seq`, so frames leave in the order
//! the embedder sent them. A still finger produces no frames, so while a contact or the button
//! is down the last frame goes again [`TOUCHPAD_REPEAT_MS`] after the previous send, with the
//! same `seq`: the host's gate drops the copy and its timeout clock still sees it. Nothing
//! leaves without the pointer grant.

use super::*;
use crate::quic::{TouchpadFrame, GRANT_POINTER, TOUCHPAD_REPEAT_MS};

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
    let repeat = tokio::time::sleep(period);
    tokio::pin!(repeat);
    loop {
        tokio::select! {
            f = rx.recv() => {
                let Some(mut f) = f else { break };
                f.seq = seq;
                seq = seq.wrapping_add(1);
                send(&f);
                held = f.is_active().then_some(f);
            }
            () = &mut repeat, if held.is_some() => {
                if let Some(f) = &held {
                    send(f);
                }
            }
        }
        repeat.as_mut().reset(tokio::time::Instant::now() + period);
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

    /// A held finger repeats with its own `seq`; an empty frame takes the next one and stops it.
    #[tokio::test]
    async fn a_held_finger_repeats_until_it_lifts() {
        let (_server, _client, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(run(
            ClientConn::new(client_conn),
            rx,
            shared(crate::quic::GRANT_ALL),
        ));
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
    async fn no_frame_leaves_without_the_pointer_grant() {
        let (_server, _client, host_conn, client_conn) =
            crate::quic::test_util::connect_pair().await;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let grants = crate::quic::GRANT_ALL & !GRANT_POINTER;
        let task = tokio::spawn(run(ClientConn::new(client_conn), rx, shared(grants)));
        tx.send(frame(&[])).unwrap();
        assert_eq!(next_frame(&host_conn, 300).await, None);
        task.abort();
    }
}
