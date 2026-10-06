//! Client-side shared-clipboard transport (`design/clipboard-and-file-transfer.md`).
//!
//! Per-session task: accept inbound fetch streams (host pasting what we offered) and
//! drive outbound fetches (client pasting what the host offered). Events surface as
//! poll-style [`ClipEventCore`]. Offers and enable/disable ride the control stream as
//! ordinary [`ClipControl`]/[`ClipOffer`] messages ([`crate::client`] routes those);
//! only bulk bytes flow here, over [`crate::quic::clipstream`] fetch bi-streams.
//!
//! No OS pasteboard. The C ABI (`punktfunk-ffi`) is the event/command seam: a native
//! client polls offers and fetch-requests and answers with bytes.

use std::collections::HashMap;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;

use crate::error::PunktfunkStatus;
use crate::quic::{
    clipstream, ClipFetch, ClipFetchHdr, ClipKind, CLIP_FETCH_DENIED, CLIP_FETCH_OK,
    CLIP_FETCH_STALE, CLIP_FETCH_UNAVAILABLE,
};

/// Per-fetch read cap, bytes (`design/clipboard-and-file-transfer.md`). A holder that
/// streams more is a cap breach: the fetch fails rather than buffering unboundedly.
pub const CLIP_FETCH_CAP: usize = 64 << 20;

/// High bit on inbound-serve `req_id`s so they never collide with outbound `xfer_id`s
/// (those count from 1). [`ClipCommand::Cancel`] then routes on one `id`.
pub const INBOUND_REQ_FLAG: u32 = 0x8000_0000;

/// Stall bound, seconds (`design/clipboard-and-file-transfer.md`). A holder that never
/// answers fails the transfer instead of hanging it.
const FETCH_STALL_SECS: u64 = 60;
/// Event plane for `NativeClient::next_clip` / C ABI `punktfunk_connection_next_clipboard`.
#[derive(Clone, Debug)]
pub enum ClipEventCore {
    /// Host copied. Fetch lazily — only when a local app actually pastes.
    RemoteOffer {
        seq: u32,
        kinds: Vec<ClipKind>,
    },
    /// Host ack / unsolicited policy or backend update, for the toggle UI.
    State {
        enabled: bool,
        policy: u8,
        reason: u8,
    },
    /// Host is pasting content we offered. Answer with `clip_serve(req_id, …)` or
    /// `clip_cancel(req_id)`.
    FetchRequest {
        req_id: u32,
        seq: u32,
        file_index: u32,
        mime: String,
    },
    /// Bytes for a fetch the embedder started (`xfer_id` from `clip_fetch`). One event,
    /// `last = true`.
    Data {
        xfer_id: u32,
        bytes: Vec<u8>,
        last: bool,
    },
    Cancelled {
        id: u32,
    },
    /// A transfer failed; `code` is a [`PunktfunkStatus`] value (negative).
    Error {
        id: u32,
        code: i32,
    },
}

/// Embedder → clipboard task. `ClipControl`/`ClipOffer` are not here — they ride the
/// control stream as ordinary control messages.
pub enum ClipCommand {
    /// Fetch remote offered content. `xfer_id` is client-assigned and echoed on the
    /// resulting [`ClipEventCore::Data`]/`Error`/`Cancelled`.
    Fetch {
        xfer_id: u32,
        seq: u32,
        file_index: u32,
        mime: String,
    },
    /// Provide bytes answering an inbound [`ClipEventCore::FetchRequest`] (`req_id`). Chunks
    /// accumulate; `last` completes the transfer.
    Serve {
        req_id: u32,
        bytes: Vec<u8>,
        last: bool,
    },
    /// Cancel a transfer by id — either an outbound fetch (`xfer_id`) or an inbound serve
    /// (`req_id`, high bit set).
    Cancel { id: u32 },
}

type ServeWaiters = Arc<Mutex<HashMap<u32, oneshot::Sender<Option<Vec<u8>>>>>>;

fn fetch_status_to_code(status: u8) -> i32 {
    let s = match status {
        CLIP_FETCH_STALE => PunktfunkStatus::NoFrame, // stale offer → "nothing to insert"
        CLIP_FETCH_UNAVAILABLE => PunktfunkStatus::Unsupported,
        CLIP_FETCH_DENIED => PunktfunkStatus::InvalidArg,
        _ => PunktfunkStatus::BadPacket,
    };
    s as i32
}

/// Per-session clipboard task. Runs until the connection closes or the command sender
/// is dropped. Owns no clipboard content — the embedder supplies bytes on demand.
pub async fn run(
    conn: quinn::Connection,
    events: SyncSender<ClipEventCore>,
    mut cmd_rx: UnboundedReceiver<ClipCommand>,
) {
    // req_id → oneshot: `Some(bytes)` serves, `None` denies/cancels.
    let serve_waiters: ServeWaiters = Arc::new(Mutex::new(HashMap::new()));
    let mut serve_bufs: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut fetch_cancels: HashMap<u32, oneshot::Sender<()>> = HashMap::new();
    let mut next_req_id: u32 = 1;

    loop {
        tokio::select! {
            accepted = conn.accept_bi() => {
                let Ok((send, recv)) = accepted else { break }; // connection gone
                let req_id = INBOUND_REQ_FLAG | next_req_id;
                next_req_id = next_req_id.wrapping_add(1);
                if next_req_id == 0 {
                    next_req_id = 1;
                }
                let events = events.clone();
                let waiters = serve_waiters.clone();
                tokio::spawn(serve_inbound(send, recv, req_id, events, waiters));
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break }; // NativeClient dropped
                match cmd {
                    ClipCommand::Fetch { xfer_id, seq, file_index, mime } => {
                        // Completed/failed fetches drop their cancel receiver; prune closed
                        // senders or the map grows one dead entry per paste until Cancel.
                        fetch_cancels.retain(|_, tx| !tx.is_closed());
                        let (cancel_tx, cancel_rx) = oneshot::channel();
                        fetch_cancels.insert(xfer_id, cancel_tx);
                        let conn = conn.clone();
                        let events = events.clone();
                        let req = ClipFetch { seq, file_index, mime };
                        tokio::spawn(run_outbound_fetch(conn, xfer_id, req, events, cancel_rx));
                    }
                    ClipCommand::Serve { req_id, bytes, last } => {
                        // Waiter is inserted before FetchRequest is emitted. A stale/unknown
                        // req_id must not pool bytes here for the rest of the session.
                        if !serve_waiters.lock().unwrap().contains_key(&req_id) {
                            serve_bufs.remove(&req_id);
                        } else {
                            let buf = serve_bufs.entry(req_id).or_default();
                            if buf.len().saturating_add(bytes.len()) > CLIP_FETCH_CAP {
                                // Peer already caps the read at CLIP_FETCH_CAP. Fail now:
                                // UNAVAILABLE to the peer, Error to the embedder — not Ok-per-chunk.
                                serve_bufs.remove(&req_id);
                                if let Some(tx) = serve_waiters.lock().unwrap().remove(&req_id) {
                                    let _ = tx.send(None);
                                }
                                let _ = events.try_send(ClipEventCore::Error {
                                    id: req_id,
                                    code: PunktfunkStatus::InvalidArg as i32,
                                });
                            } else {
                                buf.extend_from_slice(&bytes);
                                if last {
                                    let full = serve_bufs.remove(&req_id).unwrap_or_default();
                                    if let Some(tx) = serve_waiters.lock().unwrap().remove(&req_id)
                                    {
                                        let _ = tx.send(Some(full));
                                    }
                                }
                            }
                        }
                    }
                    ClipCommand::Cancel { id } => {
                        if let Some(tx) = fetch_cancels.remove(&id) {
                            let _ = tx.send(());
                        }
                        serve_bufs.remove(&id);
                        if let Some(tx) = serve_waiters.lock().unwrap().remove(&id) {
                            let _ = tx.send(None); // deny — the serving task writes UNAVAILABLE
                        }
                    }
                }
            }
        }
    }
}

async fn serve_inbound(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    req_id: u32,
    events: SyncSender<ClipEventCore>,
    waiters: ServeWaiters,
) {
    let _ = send.set_priority(-1);
    if clipstream::read_stream_header(&mut recv).await.is_err() {
        let _ = send.reset(clipstream::cancelled_code());
        return;
    }
    let req = match clipstream::read_fetch(&mut recv).await {
        Ok(r) => r,
        Err(_) => return,
    };

    // Waiter first — an immediate `clip_serve` must not race the insert.
    let (tx, rx) = oneshot::channel();
    waiters.lock().unwrap().insert(req_id, tx);
    let ev = ClipEventCore::FetchRequest {
        req_id,
        seq: req.seq,
        file_index: req.file_index,
        mime: req.mime,
    };
    if events.try_send(ev).is_err() {
        // Embedder isn't draining (or the session is ending): refuse cleanly.
        waiters.lock().unwrap().remove(&req_id);
        let _ = clipstream::write_fetch_hdr(
            &mut send,
            &ClipFetchHdr {
                status: CLIP_FETCH_UNAVAILABLE,
                total_size: 0,
            },
        )
        .await;
        return;
    }

    // Same stall bound as outbound. An unanswered paste must not hold the waiter and
    // the accepted bi-stream for the rest of the session (bidi budget). `send.stopped()`
    // wakes as soon as the peer gives up.
    let answer = tokio::select! {
        r = rx => r.ok().flatten(),
        _ = send.stopped() => {
            let _ = events.try_send(ClipEventCore::Cancelled { id: req_id });
            None
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(FETCH_STALL_SECS)) => {
            let _ = events.try_send(ClipEventCore::Cancelled { id: req_id });
            None
        }
    };
    // Idempotent — the answering/denying paths already removed it; the stall paths must.
    waiters.lock().unwrap().remove(&req_id);

    match answer {
        Some(bytes) => {
            if clipstream::write_fetch_hdr(
                &mut send,
                &ClipFetchHdr {
                    status: CLIP_FETCH_OK,
                    total_size: bytes.len() as u64,
                },
            )
            .await
            .is_ok()
            {
                let _ = clipstream::write_data(&mut send, &bytes).await;
            }
        }
        // Denied, cancelled, or the waiter was dropped: tell the peer it's gone.
        _ => {
            let _ = clipstream::write_fetch_hdr(
                &mut send,
                &ClipFetchHdr {
                    status: CLIP_FETCH_UNAVAILABLE,
                    total_size: 0,
                },
            )
            .await;
        }
    }
}

async fn run_outbound_fetch(
    conn: quinn::Connection,
    xfer_id: u32,
    req: ClipFetch,
    events: SyncSender<ClipEventCore>,
    cancel_rx: oneshot::Receiver<()>,
) {
    let transfer = async {
        let (send, mut recv) = clipstream::open_fetch(&conn, &req)
            .await
            .map_err(|_| PunktfunkStatus::Io as i32)?;
        let hdr = clipstream::read_fetch_hdr(&mut recv)
            .await
            .map_err(|_| PunktfunkStatus::Io as i32)?;
        if hdr.status != CLIP_FETCH_OK {
            return Err(fetch_status_to_code(hdr.status));
        }
        let data = clipstream::read_data(&mut recv, CLIP_FETCH_CAP)
            .await
            .map_err(|_| PunktfunkStatus::Io as i32)?;
        drop(send); // done — dropping the send half is a clean FIN-less close on our side
        Ok(data)
    };

    tokio::select! {
        r = transfer => match r {
            Ok(data) => {
                let _ = events.try_send(ClipEventCore::Data { xfer_id, bytes: data, last: true });
            }
            Err(code) => {
                let _ = events.try_send(ClipEventCore::Error { id: xfer_id, code });
            }
        },
        _ = cancel_rx => {
            // The `transfer` future is dropped here; its streams reset on drop.
            let _ = events.try_send(ClipEventCore::Cancelled { id: xfer_id });
        }
        // Stall bound: a holder that never answers must not hang the transfer.
        // Dropping `transfer` resets the streams.
        _ = tokio::time::sleep(std::time::Duration::from_secs(FETCH_STALL_SECS)) => {
            let _ = events.try_send(ClipEventCore::Error {
                id: xfer_id,
                code: PunktfunkStatus::Timeout as i32,
            });
        }
    }
}

// Every test here drives a real connection pair, so it needs `quic` like the module it
// borrows `connect_pair` from. The crate's own code above is feature-free.
#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;
    use crate::quic::test_util::connect_pair;
    use crate::quic::CLIP_FILE_INDEX_NONE;

    /// A single chunk over CLIP_FETCH_CAP fails now: Error to the embedder, UNAVAILABLE
    /// to the peer — not Ok-per-chunk toward a guaranteed rejection. Also pins the
    /// waiter-membership gate (serve only accumulates while the fetch is parked).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_serve_fails_the_transfer_instead_of_buffering() {
        let (_s, _c, host_conn, client_conn) = connect_pair().await;
        let (ev_tx, ev_rx) = std::sync::mpsc::sync_channel(16);
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(run(client_conn, ev_tx, cmd_rx));

        let req = ClipFetch {
            seq: 1,
            file_index: CLIP_FILE_INDEX_NONE,
            mime: "text/plain;charset=utf-8".into(),
        };
        let (_send, mut recv) = clipstream::open_fetch(&host_conn, &req).await.unwrap();

        let (req_id, ev_rx) = tokio::task::spawn_blocking(move || {
            match ev_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
            {
                ClipEventCore::FetchRequest { req_id, .. } => (req_id, ev_rx),
                other => panic!("expected FetchRequest, got {other:?}"),
            }
        })
        .await
        .unwrap();

        cmd_tx
            .send(ClipCommand::Serve {
                req_id,
                bytes: vec![0u8; CLIP_FETCH_CAP + 1],
                last: false,
            })
            .unwrap();
        let ev = tokio::task::spawn_blocking(move || {
            ev_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        })
        .await
        .unwrap();
        match ev {
            ClipEventCore::Error { id, .. } => assert_eq!(id, req_id),
            other => panic!("expected Error, got {other:?}"),
        }
        let hdr = clipstream::read_fetch_hdr(&mut recv).await.unwrap();
        assert_eq!(hdr.status, CLIP_FETCH_UNAVAILABLE);
    }
}
