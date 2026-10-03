//! Host clipboard coordinator (`design/clipboard-and-file-transfer.md`).
//!
//! One task per streaming session: the platform [`super::HostClipboard`] on one
//! side, the QUIC clipboard plane on the other. Host copy becomes a [`ClipOffer`]
//! on `offer_tx`; inbound `accept_bi` fetches read the host selection;
//! [`ClipCoordCmd::RemoteOffer`] installs a lazy host source; [`ClipEvent::Paste`]
//! opens an outbound fetch and fills the [`PasteResponder`].
//!
//! Backend-agnostic. The control loop talks [`ClipCoordCmd`] so it compiles on
//! every host platform.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use punktfunk_core::clipboard::CLIP_FETCH_CAP;
use punktfunk_core::quic::{
    clipstream, ClipFetch, ClipFetchHdr, ClipKind, ClipOffer, CLIP_FETCH_OK, CLIP_FETCH_STALE,
    CLIP_FETCH_UNAVAILABLE, CLIP_FILE_INDEX_NONE,
};

use super::{ClipEvent, HostClipboard, PasteResponder};
use crate::ClipCoordCmd;

/// 60 s: a silent client must not hang the host app's paste pipe; the paste then goes empty.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// `true` when a backend is bound. `false` drops the channels; the caller reports
/// `CLIP_REASON_BACKEND_UNAVAILABLE` and declines fetches.
pub async fn start(
    conn: quinn::Connection,
    clip_enabled: Arc<AtomicBool>,
    target: crate::ClipTarget,
    cmd_rx: UnboundedReceiver<ClipCoordCmd>,
    offer_tx: UnboundedSender<ClipOffer>,
) -> bool {
    match HostClipboard::open(target).await {
        Ok((backend, clip_rx)) => {
            tokio::spawn(run(
                conn,
                Arc::new(backend),
                clip_rx,
                clip_enabled,
                cmd_rx,
                offer_tx,
            ));
            true
        }
        Err(e) => {
            tracing::info!(error = %format!("{e:#}"), "clipboard backend unavailable — fetches will be declined");
            false
        }
    }
}

async fn run(
    conn: quinn::Connection,
    backend: Arc<HostClipboard>,
    mut clip_rx: UnboundedReceiver<ClipEvent>,
    clip_enabled: Arc<AtomicBool>,
    mut cmd_rx: UnboundedReceiver<ClipCoordCmd>,
    offer_tx: UnboundedSender<ClipOffer>,
) {
    // Last announced host offer. A fetch naming another seq is stale.
    let host_seq = Arc::new(AtomicU32::new(0));
    let mut next_seq: u32 = 1;
    // Client offer seq, echoed on the outbound fetch when a host app pastes.
    let mut client_seq: u32 = 0;
    let v2 = clipstream::is_v2(&conn);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    ClipCoordCmd::SetEnabled(true) => {
                        // Push the current host selection so enable is not a silent empty clipboard.
                        let mimes = backend.current_wire_mimes();
                        if !mimes.is_empty() {
                            let _ = offer_tx.send(build_offer(&mut next_seq, &host_seq, mimes));
                        }
                    }
                    ClipCoordCmd::SetEnabled(false) => {
                        if let Err(e) = backend.clear_offer() {
                            tracing::debug!(error = %e, "clipboard clear_offer failed");
                        }
                    }
                    ClipCoordCmd::RemoteOffer { seq, mimes } => {
                        // Re-check here: lifecycle can clear the flag after this offer was queued.
                        if clip_enabled.load(Ordering::SeqCst) {
                            client_seq = seq;
                            let res = if mimes.is_empty() {
                                backend.clear_offer()
                            } else {
                                backend.set_offer(&mimes)
                            };
                            if let Err(e) = res {
                                tracing::debug!(error = %e, "clipboard apply remote offer failed");
                            }
                        }
                    }
                }
            }
            ev = clip_rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    ClipEvent::Selection { mimes } => {
                        if clip_enabled.load(Ordering::SeqCst) {
                            let _ = offer_tx.send(build_offer(&mut next_seq, &host_seq, mimes));
                        }
                    }
                    ClipEvent::Paste { mime, responder } => {
                        // Off-task: the loop must keep serving. Snapshot `enabled` so a
                        // revoke racing this paste cannot fetch from the client.
                        tokio::spawn(fetch_into_pipe(
                            conn.clone(),
                            client_seq,
                            mime,
                            responder,
                            clip_enabled.load(Ordering::SeqCst),
                        ));
                    }
                    ClipEvent::Closed => break,
                }
            }
            accepted = conn.accept_bi() => {
                let Ok((send, recv)) = accepted else { break };
                // Handshake already took the control stream; every accept here is a fetch.
                // Off-task: `read_current` blocks on the source app's pipe.
                tokio::spawn(serve_fetch(
                    send,
                    recv,
                    Arc::clone(&backend),
                    Arc::clone(&host_seq),
                    clip_enabled.load(Ordering::SeqCst),
                    v2,
                ));
            }
        }
    }
    // Do not leave our lazy source as the compositor's selection after the session ends.
    let _ = backend.clear_offer();
}

/// Seq 0 is "never offered"; wrap skips it. Published on `host_seq` for staleness checks.
fn build_offer(next_seq: &mut u32, host_seq: &AtomicU32, mimes: Vec<String>) -> ClipOffer {
    let seq = *next_seq;
    *next_seq = next_seq.wrapping_add(1);
    if *next_seq == 0 {
        *next_seq = 1;
    }
    host_seq.store(seq, Ordering::SeqCst);
    let kinds = mimes
        .into_iter()
        .map(|mime| ClipKind { mime, size_hint: 0 })
        .collect();
    ClipOffer { seq, kinds }
}

async fn serve_fetch(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    backend: Arc<HostClipboard>,
    host_seq: Arc<AtomicU32>,
    enabled: bool,
    v2: bool,
) {
    let _ = send.set_priority(-1);
    match clipstream::read_stream_header(&mut recv, v2).await {
        Ok(k) if k == clipstream::CLIP_STREAM_KIND_FETCH => {}
        _ => {
            let _ = send.reset(clipstream::cancelled_code());
            return;
        }
    }
    let req = match clipstream::read_fetch(&mut recv, v2).await {
        Ok(r) => r,
        Err(_) => return,
    };

    let decline = |status: u8| ClipFetchHdr {
        status,
        total_size: 0,
    };
    if !enabled {
        let _ = clipstream::write_fetch_hdr(&mut send, &decline(CLIP_FETCH_UNAVAILABLE), v2).await;
        return;
    }
    if req.seq != host_seq.load(Ordering::SeqCst) {
        let _ = clipstream::write_fetch_hdr(&mut send, &decline(CLIP_FETCH_STALE), v2).await;
        return;
    }

    match backend.read_current(&req.mime).await {
        Ok(data) => {
            let hdr = ClipFetchHdr {
                status: CLIP_FETCH_OK,
                total_size: data.len() as u64,
            };
            if clipstream::write_fetch_hdr(&mut send, &hdr, v2)
                .await
                .is_ok()
            {
                let _ = clipstream::write_data(&mut send, &data).await;
            }
        }
        // Clipboard moved or the read failed: decline UNAVAILABLE, not STALE (seq still matched).
        Err(_) => {
            let _ =
                clipstream::write_fetch_hdr(&mut send, &decline(CLIP_FETCH_UNAVAILABLE), v2).await;
        }
    }
}

/// Any failure or `enabled == false` responds empty so the host paste pipe does not hang.
async fn fetch_into_pipe(
    conn: quinn::Connection,
    seq: u32,
    mime: String,
    responder: PasteResponder,
    enabled: bool,
) {
    if !enabled {
        responder.respond(Vec::new()).await;
        return;
    }
    let req = ClipFetch {
        seq,
        file_index: CLIP_FILE_INDEX_NONE,
        mime,
    };
    let v2 = clipstream::is_v2(&conn);
    let fetched = tokio::time::timeout(FETCH_TIMEOUT, async {
        let (send, mut recv) = clipstream::open_fetch(&conn, &req).await.ok()?;
        let hdr = clipstream::read_fetch_hdr(&mut recv, v2).await.ok()?;
        if hdr.status != CLIP_FETCH_OK {
            return None;
        }
        let data = clipstream::read_data(&mut recv, CLIP_FETCH_CAP)
            .await
            .ok()?;
        drop(send); // Finish our send half; do not `reset`.
        Some(data)
    })
    .await
    .ok()
    .flatten();

    responder.respond(fetched.unwrap_or_default()).await;
}
