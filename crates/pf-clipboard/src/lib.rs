//! Host-side shared clipboard.
//!
//! Wire protocol and the client half live in `punktfunk-core`
//! (`punktfunk_core::quic` + `punktfunk_core::clipboard`). This crate opens the
//! per-OS backends in [`host`] and bridges them to the QUIC clipboard plane
//! through [`host::session`].
//!
//! The orchestrator uses only this portable facade so its control loop compiles
//! on every host platform; the OS split is behind [`start`]. Evidence:
//! `design/clipboard-and-file-transfer.md`.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use punktfunk_core::quic::ClipOffer;

/// Per-OS backends and the backend-agnostic [`host::session`] coordinator.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod host;

/// The host's clipboard setting as a `CLIP_POLICY_*` bitfield. `Off` is `None`
/// (default): the host neither advertises the cap nor starts [`host::session`].
pub fn policy() -> Option<u8> {
    use pf_host_config::ClipboardPolicy;
    use punktfunk_core::quic::{CLIP_POLICY_FILES, CLIP_POLICY_TEXT};
    match pf_host_config::config().clipboard {
        ClipboardPolicy::Off => None,
        ClipboardPolicy::Text => Some(CLIP_POLICY_TEXT),
        ClipboardPolicy::Files => Some(CLIP_POLICY_TEXT | CLIP_POLICY_FILES),
    }
}

pub fn enabled() -> bool {
    policy().is_some()
}

/// Linux still advertises when the compositor has no data-control; enable then
/// returns `BACKEND_UNAVAILABLE` so the client can say why.
pub fn cap_advertised() -> bool {
    enabled() && cfg!(any(target_os = "linux", target_os = "windows"))
}

/// Session-loop → coordinator. Defined here so the control loop compiles on
/// hosts with no backend; the consumer lives only where [`host::session`] does.
pub enum ClipCoordCmd {
    /// Sync toggle. On: (re)announce the current host clipboard. Off: drop any
    /// selection this session owns and stop forwarding host copies.
    SetEnabled(bool),
    /// Client copy: install the offered wire MIMEs as a lazy host selection
    /// (empty = clear). Ignored while sync is off.
    RemoteOffer { seq: u32, mimes: Vec<String> },
}

pub struct ClipCoord {
    /// False when no backend bound. The control loop then answers enable with
    /// `CLIP_REASON_BACKEND_UNAVAILABLE` and [`spawn_decline_loop`] takes fetch.
    pub available: bool,
    pub cmd_tx: tokio::sync::mpsc::UnboundedSender<ClipCoordCmd>,
    pub offer_rx: tokio::sync::mpsc::UnboundedReceiver<ClipOffer>,
}

/// Where a session's clipboard lives.
pub enum ClipTarget {
    /// No display clipboard: the protocol-test source.
    None,
    /// The compositor the process environment names.
    Session,
    /// A gamescope session's Xwayland, once the stream has one.
    Gamescope(GamescopeXwayland),
}

/// The X displays of the session's gamescope, set by the stream once it has a gamescope. The
/// clipboard waits until then: at handshake the gamescope may not exist yet.
#[derive(Clone, Default)]
pub struct GamescopeXwayland(Arc<Mutex<Option<Displays>>>);

type Displays = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

impl GamescopeXwayland {
    /// `displays` lists the session gamescope's X displays, first one preferred.
    pub fn set(&self, displays: impl Fn() -> Vec<String> + Send + Sync + 'static) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(displays));
    }

    /// Empty until [`Self::set`]; a lookup runs outside the lock.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn displays(&self) -> Vec<String> {
        let displays = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        displays.map(|f| f()).unwrap_or_default()
    }
}

/// Open the backend `target` names and spawn [`host::session`], or return an inert handle
/// (`available = false`).
pub async fn start(
    conn: quinn::Connection,
    clip_enabled: Arc<AtomicBool>,
    target: ClipTarget,
) -> ClipCoord {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (offer_tx, offer_rx) = tokio::sync::mpsc::unbounded_channel();
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let available = if !matches!(target, ClipTarget::None) && enabled() {
        host::session::start(conn, clip_enabled, target, cmd_rx, offer_tx).await
    } else {
        drop((conn, clip_enabled, cmd_rx, offer_tx));
        false
    };
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let available = {
        let _ = (conn, clip_enabled, cmd_rx, offer_tx, target);
        false
    };
    ClipCoord {
        available,
        cmd_tx,
        offer_rx,
    }
}

/// `accept_bi` fallback when [`start`] did not bind a backend. Answers
/// `CLIP_FETCH_UNAVAILABLE` instead of hanging. Exactly one consumer of
/// `accept_bi` runs (this or the coordinator). The control stream is already
/// accepted, so this loop only sees clipboard fetch streams.
pub fn spawn_decline_loop(conn: quinn::Connection) {
    tokio::spawn(async move {
        use punktfunk_core::quic::{clipstream, ClipFetchHdr, CLIP_FETCH_UNAVAILABLE};
        let v2 = clipstream::is_v2(&conn);
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            tokio::spawn(async move {
                match clipstream::read_stream_header(&mut recv, v2).await {
                    Ok(k) if k == clipstream::CLIP_STREAM_KIND_FETCH => {}
                    _ => {
                        let _ = send.reset(clipstream::cancelled_code());
                        return;
                    }
                }
                if clipstream::read_fetch(&mut recv, v2).await.is_err() {
                    return;
                }
                let _ = clipstream::write_fetch_hdr(
                    &mut send,
                    &ClipFetchHdr {
                        status: CLIP_FETCH_UNAVAILABLE,
                        total_size: 0,
                    },
                    v2,
                )
                .await;
            });
        }
    });
}
