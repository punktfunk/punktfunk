//! GNOME/Mutter virtual-display backend via Mutter's *direct* D-Bus APIs
//! (the gnome-remote-desktop path; not the xdg portal, which needs an
//! interactive grant).
//!
//! Handshake: RemoteDesktop.CreateSession → ScreenCast.CreateSession
//! (anchored on that SessionId) → RecordVirtual → Start, then
//! PipeWireStreamAdded. Size is PipeWire format negotiation
//! ([`VirtualOutput::preferred_mode`]), not a D-Bus argument. Sessions
//! die with the connection, so a keepalive thread owns it.
//!
//! Needs a live Mutter (`gnome-shell`, or `gnome-shell --headless`) on
//! the session bus. Detected via `XDG_CURRENT_DESKTOP=GNOME` or
//! `PUNKTFUNK_COMPOSITOR=mutter`.
//!
//! GNOME cannot rematch a virtual monitor (Mutter mints a fresh EDID
//! serial per RecordVirtual). Scale is host-persisted
//! ([`identity::ScaleMap`](crate::identity)) and reapplied at connect.

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::zbus;
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const BUS_RD: &str = "org.gnome.Mutter.RemoteDesktop";
const BUS_SC: &str = "org.gnome.Mutter.ScreenCast";
const BUS_DC: &str = "org.gnome.Mutter.DisplayConfig";
/// `ApplyMonitorsConfig` method 1 = temporary. Auto-reverts on the next
/// monitor change (our virtual output going away), so the layout never
/// lands in monitors.xml.
const APPLY_TEMPORARY: u32 = 1;

/// Mutter cursor-mode 2: pointer as `SPA_META_Cursor`, not burned into
/// frames. Use whenever `set_hw_cursor` is on. Embedded painting on a
/// virtual stream is suppressed whenever any physical head uses a
/// hardware cursor, so dmabuf frames have no pointer and cursor-only
/// motion does not re-record.
const CURSOR_METADATA: u32 = 2;
/// Mutter cursor-mode 1: compositor paints the pointer into frames.
/// Fallback when `set_hw_cursor` is off (the encode backend cannot
/// composite metadata). On a virtual stream it only paints the MemFd/SHM
/// path, and only on unrelated damage.
const CURSOR_EMBEDDED: u32 = 1;

/// Process-wide mutex around every add/remove/apply of a virtual
/// monitor. Concurrent rebuilds SIGSEGV gnome-shell inside
/// `meta_monitor_manager_rebuild`. One mutation at a time also keeps
/// [`wait_virtual_connector`]'s "connector absent from MY pre-snapshot"
/// from naming a sibling. Sessions run on dedicated threads, so blocking
/// a std mutex across that thread's single-future awaits is safe.
///
/// D-Bus calls return while Mutter is still rebuilding (and, for
/// `APPLY_TEMPORARY`, still auto-reverting). Every locked section ends
/// with [`settle_topology`] before the guard drops. [`StopGuard`]'s Drop
/// waits for Stop + settle: a fire-and-forget flag let the next
/// `RecordVirtual` take the lock while the doomed session still stood.
static TOPOLOGY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Mutter virtual-display driver. Each `create` spins a keepalive thread
/// that owns the D-Bus sessions behind the virtual monitor.
pub struct MutterDisplay {
    /// First display of the group. A later sibling extends into the
    /// already-exclusive desktop; re-applying the sole-monitor config
    /// would disable the first session's virtual. Defaults true.
    first_in_group: bool,
    /// `set_hw_cursor`: metadata cursor-mode when on; embedded when off
    /// (see [`CURSOR_EMBEDDED`]).
    hw_cursor: bool,
    /// Connecting client's cert fingerprint, set before `create`. Keys
    /// the host-persisted scale (Mutter mints a fresh EDID serial, so
    /// `monitors.xml` never rematches).
    client_fp: Option<[u8; 32]>,
    /// Identity slot the last `create` resolved. Reported via
    /// [`last_identity_slot`](VirtualDisplay::last_identity_slot) for
    /// group arrangement and `/display/state`.
    last_slot: Option<u32>,
    /// `mode_conflict: join` admitted this session.
    join_live: bool,
    /// The last `create`'s connector (`Meta-N`), set by its session thread once
    /// identified. A joiner records that connector.
    join_name: Option<crate::backend::JoinName>,
}

impl MutterDisplay {
    pub fn new() -> Result<Self> {
        Ok(MutterDisplay {
            first_in_group: true,
            hw_cursor: false,
            client_fp: None,
            last_slot: None,
            join_live: false,
            join_name: None,
        })
    }
}

/// Cheap env check that the host is in a GNOME session. Fallback only:
/// [`crate::available`] treats a live `gnome-shell` `/proc` scan as
/// authority, because a host launched from systemd/TTY/ssh never
/// inherited this var.
///
/// Only `XDG_CURRENT_DESKTOP`: [`crate::apply_session_env`] writes and
/// scrubs that key. Sniffing `DESKTOP_SESSION` / `XDG_SESSION_DESKTOP`
/// would revive a stale `gnome` after a shell crash and route the next
/// client into a dead session.
///
/// The read takes [`crate::with_env_lock`]: this runs on a management
/// worker concurrently with `apply_session_env`'s `set_var`/`remove_var`,
/// and a glibc `getenv` racing that is the `environ` realloc race
/// ENV_LOCK exists for. Read-then-drop; the lock is not reentrant.
pub fn is_available() -> bool {
    crate::with_env_lock(|| std::env::var("XDG_CURRENT_DESKTOP"))
        .map(|d| d.to_ascii_uppercase().contains("GNOME"))
        .unwrap_or(false)
}

impl VirtualDisplay for MutterDisplay {
    fn name(&self) -> &'static str {
        "mutter"
    }

    fn set_first_in_group(&mut self, first: bool) {
        self.first_in_group = first;
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    fn last_join_name(&self) -> Option<crate::backend::JoinName> {
        self.join_name.clone()
    }

    /// `RecordMonitor` of the owner's virtual connector, in a session of its own.
    fn join_cast(
        &mut self,
        name: &str,
        _node_id: u32,
    ) -> Result<Option<crate::backend::SessionCastParts>> {
        Ok(Some(
            stream_existing_output(name, self.hw_cursor)?.into_cast(),
        ))
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // RecordVirtual owns EDID identity, so the slot never lands on
        // the monitor. Host-persist scale under `scale_key` instead.
        self.last_slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        let scale_key = crate::identity::scale_key(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        // What the desktop last settled on for this device, else what the operator set for
        // it (§6.3 P2). Learned beats configured: once someone has scaled the screen by
        // hand, that is the answer, and Mutter mints a fresh EDID serial every session so
        // its own monitors.xml never rematches to say so.
        let remembered_scale = crate::identity::scales()
            .lock()
            .unwrap()
            .get(&scale_key)
            .or_else(|| {
                crate::policy::prefs()
                    .get()
                    .scale_for(crate::policy::fp_hex(self.client_fp).as_deref())
            });
        if let Some(scale) = remembered_scale {
            tracing::info!(scale, "mutter: reapplying the client's saved display scale");
        }
        let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        // Sender lives as long as the session thread. Drop waits on
        // `Disconnected` for Stop + settle — the happens-before that
        // orders "old monitor gone" before "next monitor created".
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let first_in_group = self.first_in_group;
        let hw_cursor = self.hw_cursor;
        let client_fp = self.client_fp;
        let join_name = crate::backend::JoinName::default();
        self.join_name = Some(join_name.clone());
        thread::Builder::new()
            .name("punktfunk-mutter-vout".into())
            .spawn(move || {
                // Signals `done_rx` on every exit path.
                let _done = done_tx;
                session_thread(
                    setup_tx,
                    stop_thread,
                    mode,
                    first_in_group,
                    hw_cursor,
                    scale_key,
                    remembered_scale,
                    client_fp,
                    join_name,
                )
            })
            .context("spawn Mutter virtual-output thread")?;

        // Built before the wait so every error arm sets the flag. A
        // thread that finishes after we gave up parks at most one 200 ms
        // tick. `report_node` is the primary stop; this covers a thread
        // still elsewhere when the timeout fires.
        let guard = StopGuard {
            stop,
            done: done_rx,
        };

        // 45 s: a session queued on TOPOLOGY_LOCK must outwait a sibling
        // (~10 s stream + 6 s connector + apply) plus its own handshake.
        let node_id = match setup_rx.recv_timeout(Duration::from_secs(45)) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => bail!("Mutter virtual monitor failed: {e}"),
            Err(_) => bail!("timed out creating the Mutter virtual monitor"),
        };
        tracing::info!(
            node_id,
            w = mode.width,
            h = mode.height,
            "Mutter virtual monitor ready"
        );
        Ok(VirtualOutput::owned(
            node_id,
            Some((mode.width, mode.height, mode.refresh_hz)),
            Box::new(guard),
        ))
    }
}

/// Dropping this ends the keepalive thread and closes the D-Bus
/// connection; Mutter then tears the sessions and virtual monitor down.
///
/// Drop is synchronous and bounded: it waits for Stop +
/// [`settle_topology`] under [`TOPOLOGY_LOCK`]. Callers that re-create
/// immediately need that wait — a fire-and-forget flag let the next
/// `RecordVirtual` take the lock while this monitor still stood.
struct StopGuard {
    stop: Arc<AtomicBool>,
    /// `Disconnected` when the session thread (owner of the sender) returns.
    done: std::sync::mpsc::Receiver<()>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // One 200 ms park + Stop + ≤4 s settle, plus a sibling holding
        // TOPOLOGY_LOCK (~16 s of stream + connector waits). Timeout is
        // degraded-but-safe: the next mutation still queues; only the
        // wake-up ordering is lost.
        match self.done.recv_timeout(Duration::from_secs(20)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => tracing::warn!(
                "mutter: virtual-output teardown did not confirm within 20 s — proceeding; the \
                 next topology mutation may race the shell's rebuild"
            ),
        }
    }
}

/// Keepalive: D-Bus handshake on a private runtime, report the PipeWire
/// node, hold the connection until stopped. `first_in_group` gates the
/// topology apply (a later sibling extends rather than re-clobbering).
/// `scale_key` / `remembered_scale` reapply and record per-client scale
/// (GNOME cannot persist it — see [`identity::ScaleMap`](crate::identity)).
// Held across setup/teardown awaits: this OS thread owns a single-future
// runtime, so the guard blocks sibling session threads, not a shared
// executor (see TOPOLOGY_LOCK).
#[allow(clippy::await_holding_lock, clippy::too_many_arguments)]
fn session_thread(
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
    mode: Mode,
    first_in_group: bool,
    hw_cursor: bool,
    scale_key: String,
    remembered_scale: Option<f64>,
    // Whose display this is, for the per-device topology (§6.1). `None` is the host policy.
    client_fp: Option<[u8; 32]>,
    // Filled with the connector once identified, so a `join` session can record it.
    join_name: crate::backend::JoinName,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        // Setup is one RMW on Mutter's monitor state. Hold TOPOLOGY_LOCK
        // across it so concurrent sessions cannot interleave rebuilds or
        // poison connector diffs. Dropped before the keepalive park.
        let topology_guard = TOPOLOGY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Extend: no config change. Primary: virtual primary, physicals
        // kept. Exclusive: virtual sole output. `Auto` is resolved upstream.
        use crate::policy::Topology;
        let topo = crate::effective_topology(client_fp);
        let topo_policy = matches!(topo, Topology::Primary | Topology::Exclusive);
        // Only the first display of the group applies topology. A later
        // sibling extends: Mutter connectors are un-nameable, so a config
        // that keeps every group virtual cannot be built. Skipping is the
        // safe choice; APPLY_TEMPORARY revert of the first is residual.
        let want_config = first_in_group && topo_policy;
        if topo_policy && !first_in_group {
            tracing::info!(
                "mutter: joining an existing display group — extending (the first session owns the \
                 exclusive/primary topology)"
            );
        }
        let exclusive = matches!(topo, Topology::Exclusive);
        // Pre-virtual snapshot: the new connector is "present now, absent
        // then". Taken even when we will not touch topology (scale
        // tracking). Failure degrades to no-topology + no-scale persist.
        let dc_pre = match display_config().await {
            Ok(dc) => match get_state(&dc).await {
                Ok(state) => Some((dc, state)),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "mutter: GetCurrentState (pre) failed; topology + scale persistence off");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "mutter: DisplayConfig unavailable; topology + scale persistence off");
                None
            }
        };

        let session = match connect(mode, hw_cursor, remembered_scale).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                // RecordVirtual may have added the monitor even if the
                // node-id wait failed. Connections dropped in `connect`,
                // so Mutter is removing it — settle before the lock drops.
                settle_topology(None, None).await;
                return;
            }
        };
        // Stop if nobody is listening. Everything below mutates the
        // desktop on behalf of a session that could no longer undo it.
        if !report_node(&setup_tx, &session).await {
            // Monitor is already being removed. Settle under the lock.
            drop(session);
            settle_topology(None, None).await;
            return;
        }
        // The send can also land as `create`'s recv_timeout fires: the
        // value sits in the queue, send reports ok, StopGuard drops.
        // Check the flag before topology work so a doomed session never
        // applies a sole-monitor config.
        if stop.load(Ordering::Relaxed) {
            tracing::warn!(
                "mutter: the opener gave up as the handshake completed — stopping without touching \
                 the desktop topology"
            );
            let _ = session.rd_session.call_method("Stop", &()).await;
            drop(session);
            settle_topology(None, None).await;
            return;
        }

        // Name the new connector, then — if this session owns topology —
        // make it primary so the shell lands on the streamed surface.
        // Without this a physical-attached host streams wallpaper.
        // Best-effort: failure logs and streaming continues.
        let mut tracked: Option<(zbus::Proxy<'static>, CurrentState, String)> = None;
        if let Some((dc, pre)) = dc_pre {
            match wait_virtual_connector(&dc, &pre, mode).await {
                Ok((vconn, state)) => {
                    if want_config {
                        match make_virtual_primary(
                            &dc,
                            mode,
                            &pre,
                            &state,
                            &vconn,
                            exclusive,
                            remembered_scale,
                        )
                        .await
                        {
                            Ok(()) => tracing::info!(
                                exclusive,
                                "mutter: virtual output set as the primary monitor (physicals {})",
                                if exclusive { "disabled" } else { "kept" }
                            ),
                            Err(e) => tracing::warn!(
                                error = %format!("{e:#}"),
                                "mutter: virtual output not made primary — streaming continues, but the desktop may render on the physical monitor"
                            ),
                        }
                    }
                    let _ = join_name.set(vconn.clone());
                    tracked = Some((dc, pre, vconn));
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "mutter: virtual connector not identified; topology + scale persistence off"
                ),
            }
        }

        // Rebuilds this setup caused (RecordVirtual, ApplyMonitorsConfig,
        // auto-revert of a sibling's temporary config) must finish before
        // the lock drops. Cheap when already quiet: one read + 150 ms.
        settle_topology(tracked.as_ref().map(|(dc, _, _)| dc), None).await;
        drop(topology_guard);

        // Keep `session` (and its zbus connection) alive. Every ~5 s
        // (25 × 200 ms) persist a mid-stream scale change; teardown-only
        // would lose it on a host crash.
        let mut known = remembered_scale.unwrap_or(1.0);
        let mut ticks: u32 = 0;
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            ticks = ticks.wrapping_add(1);
            if ticks % 25 == 0 {
                if let Some((dc, _, vconn)) = &tracked {
                    persist_scale_change(dc, vconn, &scale_key, &mut known).await;
                }
            }
        }
        // Before Stop: the virtual output must still exist to be read.
        if let Some((dc, _, vconn)) = &tracked {
            persist_scale_change(dc, vconn, &scale_key, &mut known).await;
        }

        // Stop the screencast; do not ApplyMonitorsConfig — a reconfig
        // during this teardown SIGSEGVs gnome-shell. APPLY_TEMPORARY
        // reverts once the output and our DisplayConfig proxy close.
        // Hold TOPOLOGY_LOCK until settle: the next create often follows.
        let _topology_guard = TOPOLOGY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = session.rd_session.call_method("Stop", &()).await;
        let vconn = tracked.as_ref().map(|(_, _, v)| v.clone());
        // APPLY_TEMPORARY revert waits on this DisplayConfig proxy
        // closing. Drop ours, then observe settle on a fresh connection.
        drop(tracked);
        drop(session);
        settle_topology(None, vconn.as_deref()).await;
    });
}

/// Bounded wait for Mutter's topology to go quiet. Called at the end of
/// every [`TOPOLOGY_LOCK`] section, before the guard drops.
///
/// Two phases over `GetCurrentState`: (1) if `gone` names a removed
/// connector, wait until it is absent (`Stop` returns before the shell
/// finishes the rebuild); (2) wait until the config serial holds across
/// two reads — add/remove/apply and auto-revert each bump it.
///
/// `dc` reuses the session proxy; otherwise a short-lived connection
/// (teardown closes its own first — APPLY_TEMPORARY revert waits on
/// that close). Best-effort: a read error usually means the shell is
/// gone; the deadline keeps a hotplug storm from parking forever.
async fn settle_topology(dc: Option<&zbus::Proxy<'_>>, gone: Option<&str>) {
    let fresh;
    let dc = match dc {
        Some(p) => p,
        None => match display_config().await {
            Ok(p) => {
                fresh = p;
                &fresh
            }
            Err(_) => {
                // No DisplayConfig (crashed shell?): a fixed grace still
                // beats returning into the next mutation instantly.
                tokio::time::sleep(Duration::from_millis(300)).await;
                return;
            }
        },
    };
    let started = Instant::now();
    let deadline = started + Duration::from_secs(4);
    if let Some(conn) = gone {
        loop {
            match get_state(dc).await {
                Ok(s) if !connectors(&s).contains(conn) => break,
                Ok(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ => break, // read error (shell gone) or deadline — proceed
            }
        }
    }
    let mut last: Option<u32> = None;
    loop {
        match get_state(dc).await {
            Ok(s) => {
                if last == Some(s.0) {
                    break;
                }
                last = Some(s.0);
            }
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "mutter: the monitor topology did not settle within 4 s — proceeding (a concurrent \
                 hotplug?)"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let waited = started.elapsed();
    if waited > Duration::from_millis(600) {
        tracing::info!(
            waited_ms = waited.as_millis() as u64,
            removed = gone.is_some(),
            "mutter: waited out a monitor-topology rebuild before releasing the lock"
        );
    }
}

/// Record an existing monitor by connector (the monitor-mirror path).
/// Returns the PipeWire node id and a keepalive whose drop stops the
/// recording.
///
/// Same private ScreenCast API as the virtual path, `RecordMonitor`
/// instead of `RecordVirtual` — still Mutter's direct D-Bus, no portal
/// grant. Not under [`TOPOLOGY_LOCK`]: mirroring neither adds/removes a
/// monitor nor applies a config.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let connector_thread = connector.to_string();
    thread::Builder::new()
        .name("punktfunk-mutter-mirror".into())
        .spawn(move || mirror_thread(setup_tx, stop_thread, connector_thread, hw_cursor))
        .context("spawn Mutter monitor-mirror thread")?;
    // Built before the wait so a timeout still signals the thread,
    // rather than leaving a RecordMonitor cast on a real head.
    let guard = crate::proc::StopFlag(stop);
    let node_id = match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => bail!("Mutter monitor mirror failed: {e}"),
        Err(_) => bail!("timed out recording the Mutter output {connector:?}"),
    };
    Ok(crate::mirror::MirrorStream {
        node_id,
        // RecordMonitor's node is on the user's PipeWire daemon.
        remote_fd: None,
        // Not a portal session: cursor-mode was set on RecordMonitor
        // and Mutter honours it, so there is nothing to report back.
        cursor_mode: None,
        keepalive: Box::new(guard),
    })
}

/// D-Bus connection behind a mirrored-monitor cast. Drop of the opener's
/// keepalive is what stops it.
fn mirror_thread(
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
    connector: String,
    hw_cursor: bool,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        let session = match connect_monitor(&connector, hw_cursor).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        if !report_node(&setup_tx, &session).await {
            return;
        }
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // No virtual output was added, so no removal rebuild to settle.
        let _ = session.rd_session.call_method("Stop", &()).await;
    });
}

/// RecordMonitor handshake: RemoteDesktop session → ScreenCast session
/// anchored to it → record `connector` → node id after Start.
async fn connect_monitor(connector: &str, hw_cursor: bool) -> Result<MutterSession> {
    let (conn, rd_session, sc_session) = open_rd_sc().await?;

    // Only cursor-mode: the mode belongs to the monitor's owner, not us.
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert(
        "cursor-mode",
        Value::from(if hw_cursor {
            CURSOR_METADATA
        } else {
            CURSOR_EMBEDDED
        }),
    );
    let stream_path: OwnedObjectPath = sc_session
        .call("RecordMonitor", &(connector, rec))
        .await
        .with_context(|| format!("Session.RecordMonitor({connector:?})"))?;

    let session = start_and_await_node(conn, rd_session, sc_session, stream_path).await?;
    tracing::info!(
        connector,
        node_id = session.node_id,
        "mutter: recording an existing monitor"
    );
    Ok(session)
}

/// Shared steps 1–2: session bus → RemoteDesktop session → ScreenCast
/// session anchored to it. Returns the connection because it IS the
/// sessions' lifetime — drop it and Mutter tears them down.
async fn open_rd_sc() -> Result<(zbus::Connection, zbus::Proxy<'static>, zbus::Proxy<'static>)> {
    let conn = zbus::Connection::session()
        .await
        .context("connect session D-Bus")?;

    // RemoteDesktop session: the ScreenCast anchor, and the input path.
    let rd = zbus::Proxy::new(
        &conn,
        BUS_RD,
        "/org/gnome/Mutter/RemoteDesktop",
        "org.gnome.Mutter.RemoteDesktop",
    )
    .await
    .context("RemoteDesktop proxy (is gnome-shell / `gnome-shell --headless` running?)")?;
    let rd_path: OwnedObjectPath = rd
        .call("CreateSession", &())
        .await
        .context("RemoteDesktop.CreateSession")?;
    let rd_session = zbus::Proxy::new(
        &conn,
        BUS_RD,
        rd_path,
        "org.gnome.Mutter.RemoteDesktop.Session",
    )
    .await?;
    let session_id: String = rd_session
        .get_property("SessionId")
        .await
        .context("read SessionId")?;

    let sc = zbus::Proxy::new(
        &conn,
        BUS_SC,
        "/org/gnome/Mutter/ScreenCast",
        "org.gnome.Mutter.ScreenCast",
    )
    .await
    .context("ScreenCast proxy")?;
    let mut props: HashMap<&str, Value> = HashMap::new();
    props.insert("remote-desktop-session-id", Value::from(session_id));
    let sc_path: OwnedObjectPath = sc
        .call("CreateSession", &(props,))
        .await
        .context("ScreenCast.CreateSession")?;
    let sc_session = zbus::Proxy::new(
        &conn,
        BUS_SC,
        sc_path,
        "org.gnome.Mutter.ScreenCast.Session",
    )
    .await?;
    Ok((conn, rd_session, sc_session))
}

/// Subscribe to PipeWireStreamAdded before Start (the signal can land
/// while we are still subscribing), then start and wait for the node id.
async fn start_and_await_node(
    conn: zbus::Connection,
    rd_session: zbus::Proxy<'static>,
    sc_session: zbus::Proxy<'static>,
    stream_path: OwnedObjectPath,
) -> Result<MutterSession> {
    let stream = zbus::Proxy::new(
        &conn,
        BUS_SC,
        stream_path,
        "org.gnome.Mutter.ScreenCast.Stream",
    )
    .await?;
    let mut added = stream
        .receive_signal("PipeWireStreamAdded")
        .await
        .context("subscribe PipeWireStreamAdded")?;
    rd_session
        .call_method("Start", &())
        .await
        .context("RemoteDesktop.Session.Start")?;
    let msg = tokio::time::timeout(Duration::from_secs(10), added.next())
        .await
        .map_err(|_| anyhow!("PipeWireStreamAdded did not arrive within 10s"))?
        .ok_or_else(|| anyhow!("signal stream ended before PipeWireStreamAdded"))?;
    let (node_id,): (u32,) = msg
        .body()
        .deserialize()
        .context("PipeWireStreamAdded body")?;

    Ok(MutterSession {
        rd_session,
        _sc_session: sc_session,
        _conn: conn,
        node_id,
    })
}

/// Hand the node id back to the opener. A failed send means the opener
/// already gave up (`recv_timeout`), so nothing will drop this keepalive.
/// Unwind here rather than park forever holding the D-Bus connection
/// that *is* the monitor's lifetime. `false` = stop now.
async fn report_node(setup_tx: &Sender<Result<u32, String>>, session: &MutterSession) -> bool {
    if setup_tx.send(Ok(session.node_id)).is_ok() {
        return true;
    }
    tracing::warn!(
        node_id = session.node_id,
        "mutter: the virtual-output opener gave up before the handshake finished — stopping the \
         session instead of parking on it (a parked session keeps the monitor, and its topology, alive)"
    );
    let _ = session.rd_session.call_method("Stop", &()).await;
    false
}

/// Held for the stream's lifetime. `_conn` is the RAII teardown: drop
/// closes the D-Bus connection and Mutter tears the sessions down.
struct MutterSession {
    rd_session: zbus::Proxy<'static>,
    _sc_session: zbus::Proxy<'static>,
    _conn: zbus::Connection,
    node_id: u32,
}

/// Four-step handshake (module docs). `preferred_scale` is the client's
/// remembered desktop scale, passed as the virtual mode's
/// `preferred-scale` so Mutter creates the monitor already scaled
/// (Mutter ≥ 48; older ignores unknown keys). Covers `extend`, where we
/// never ApplyMonitorsConfig ourselves.
async fn connect(
    mode: Mode,
    hw_cursor: bool,
    preferred_scale: Option<f64>,
) -> Result<MutterSession> {
    let (conn, rd_session, sc_session) = open_rd_sc().await?;

    // Pin WxH@Hz via RecordVirtual "modes" (Mutter ≥ 47) when refresh
    // is >60 Hz; at ≤60 Mutter's PipeWire-derived 60 Hz default is
    // already correct. A remembered scale alone still rides that default.
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert(
        "cursor-mode",
        Value::from(if hw_cursor {
            CURSOR_METADATA
        } else {
            CURSOR_EMBEDDED
        }),
    );
    if mode.refresh_hz > 60 || preferred_scale.is_some() {
        let mut vmode: HashMap<&str, Value> = HashMap::new();
        vmode.insert("size", Value::from((mode.width, mode.height)));
        if mode.refresh_hz > 60 {
            vmode.insert("refresh-rate", Value::from(mode.refresh_hz as f64));
        }
        if let Some(scale) = preferred_scale {
            vmode.insert("preferred-scale", Value::from(scale));
        }
        vmode.insert("is-preferred", Value::from(true));
        rec.insert("modes", Value::from(vec![vmode]));
    }
    let stream_path: OwnedObjectPath = sc_session
        .call("RecordVirtual", &(rec,))
        .await
        .context("Session.RecordVirtual")?;

    start_and_await_node(conn, rd_session, sc_session, stream_path).await
}

// Promote the RecordVirtual output via DisplayConfig.ApplyMonitorsConfig.
// Applied APPLY_TEMPORARY — Mutter reverts when the virtual monitor
// disappears and our DisplayConfig connection closes. Never re-assert
// the layout on teardown: that ApplyMonitorsConfig SIGSEGVs gnome-shell.

/// `GetCurrentState` reply shapes (Mutter DisplayConfig XML):
///   monitors:         `a((ssss)a(siiddada{sv})a{sv})`
///   logical_monitors: `a(iiduba(ssss)a{sv})`
type MonitorSpec = (String, String, String, String); // connector, vendor, product, serial
type DbusMode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type MonitorInfo = (MonitorSpec, Vec<DbusMode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
type CurrentState = (
    u32,
    Vec<MonitorInfo>,
    Vec<LogicalMonitor>,
    HashMap<String, OwnedValue>,
);

/// `ApplyMonitorsConfig` logical-monitor shape: `(iiduba(ssa{sv}))`, monitor = `(ssa{sv})`.
type ApplyMon = (String, String, HashMap<String, Value<'static>>); // connector, mode_id, props
type ApplyLogical = (i32, i32, f64, u32, bool, Vec<ApplyMon>);

/// DisplayConfig proxy on its own session-bus connection, independent of
/// the RemoteDesktop/ScreenCast connection so it can outlive them.
async fn display_config() -> Result<zbus::Proxy<'static>> {
    let conn = zbus::Connection::session()
        .await
        .context("connect session D-Bus (DisplayConfig)")?;
    zbus::Proxy::new(
        &conn,
        BUS_DC,
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )
    .await
    .context("DisplayConfig proxy")
}

async fn get_state(dc: &zbus::Proxy<'_>) -> Result<CurrentState> {
    dc.call("GetCurrentState", &())
        .await
        .context("DisplayConfig.GetCurrentState")
}

fn connectors(state: &CurrentState) -> HashSet<String> {
    state.1.iter().map(|m| m.0 .0.clone()).collect()
}

fn mode_flag(md: &DbusMode, key: &str) -> bool {
    matches!(md.6.get(key).map(|v| &**v), Some(&Value::Bool(true)))
}

fn current_mode_full(state: &CurrentState, connector: &str) -> Option<(String, i32, i32, f64)> {
    let mon = state.1.iter().find(|m| m.0 .0 == connector)?;
    let pick = mon
        .1
        .iter()
        .find(|md| mode_flag(md, "is-current"))
        .or_else(|| mon.1.iter().find(|md| mode_flag(md, "is-preferred")))
        .or_else(|| mon.1.first())?;
    Some((pick.0.clone(), pick.1, pick.2, pick.3))
}

/// [`current_mode_full`] without refresh (callers that place by width).
fn current_mode(state: &CurrentState, connector: &str) -> Option<(String, i32, i32)> {
    current_mode_full(state, connector).map(|(id, w, h, _)| (id, w, h))
}

/// Mode-pick for a kept physical (unit-tested). `pre_mode` is the
/// physical's pre-connect `(id, w, h, refresh)`; `None` if the connector
/// is new. `state_modes` is the post-virtual list.
///
/// Mutter re-derives layout when RecordVirtual appears and can drop a
/// high-refresh panel to its EDID-preferred 60 Hz, so post-virtual
/// `is-current` is already wrong. Prefer the pre mode (real refresh)
/// resolved to an id valid at apply time; fall back to post-virtual
/// current rather than inventing an id ApplyMonitorsConfig would reject.
///
/// Height is not decoration: a 90°/270° head is as wide on the desktop
/// as its mode is tall.
fn pick_keep_mode(
    pre_mode: Option<(String, i32, i32, f64)>,
    state_modes: &[(String, i32, i32, f64, bool, bool)],
) -> Option<(String, i32, i32)> {
    let state_current = || {
        state_modes
            .iter()
            .find(|m| m.4)
            .or_else(|| state_modes.iter().find(|m| m.5))
            .or_else(|| state_modes.first())
            .map(|m| (m.0.clone(), m.1, m.2))
    };
    let Some((pre_id, w, h, hz)) = pre_mode else {
        return state_current();
    };
    if state_modes.iter().any(|m| m.0 == pre_id) {
        return Some((pre_id, w, h));
    }
    // Same geometry + refresh under a new id (still the real refresh).
    if let Some(m) = state_modes
        .iter()
        .find(|m| m.1 == w && m.2 == h && (m.3 - hz).abs() < 0.5)
    {
        return Some((m.0.clone(), m.1, m.2));
    }
    state_current()
}

/// `(mode_id, width, height)` to re-apply on a kept physical: its
/// pre-connect mode, preserved across Mutter's layout re-derive.
fn physical_keep_mode(
    pre: &CurrentState,
    state: &CurrentState,
    conn: &str,
) -> Option<(String, i32, i32)> {
    let pre_mode = current_mode_full(pre, conn);
    let state_modes: Vec<(String, i32, i32, f64, bool, bool)> = state
        .1
        .iter()
        .find(|m| m.0 .0 == conn)
        .map(|mon| {
            mon.1
                .iter()
                .map(|md| {
                    (
                        md.0.clone(),
                        md.1,
                        md.2,
                        md.3,
                        mode_flag(md, "is-current"),
                        mode_flag(md, "is-preferred"),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    pick_keep_mode(pre_mode, &state_modes)
}

/// Wait for the virtual output to appear in DisplayConfig (size follows
/// PipeWire negotiation, shortly after the node id) and return its
/// connector (present now, absent in the pre-snapshot) plus that state.
async fn wait_virtual_connector(
    dc: &zbus::Proxy<'_>,
    pre: &CurrentState,
    mode: Mode,
) -> Result<(String, CurrentState)> {
    let pre_conns = connectors(pre);
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let state = get_state(dc).await?;
        // New connectors. TOPOLOGY_LOCK keeps a sibling out, but a
        // physical hotplug is not ours to serialise. Do not pick first:
        // that aims topology + scale at the operator's new panel.
        let chosen = {
            let fresh: Vec<&MonitorInfo> = state
                .1
                .iter()
                .filter(|m| !pre_conns.contains(&m.0 .0))
                .collect();
            let pick = pick_virtual(&fresh, mode);
            if fresh.len() > 1 {
                tracing::warn!(
                    candidates = ?fresh.iter().map(|m| m.0 .0.as_str()).collect::<Vec<_>>(),
                    chosen = pick.map(|m| m.0 .0.as_str()),
                    want = format!("{}x{}", mode.width, mode.height),
                    "mutter: more than one connector appeared while waiting for the virtual monitor \
                     (a physical hotplug?) — picking the one advertising the client's mode"
                );
            }
            pick.map(|m| m.0 .0.clone())
        };
        if let Some(vconn) = chosen {
            return Ok((vconn, state));
        }
        if Instant::now() >= deadline {
            bail!("the virtual monitor did not appear in DisplayConfig within 6s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Which of the connectors that appeared since the pre-snapshot is ours.
///
/// Disambiguate on the client's exact WxH. Fallback "first new connector"
/// when size is omitted (older Mutter) or there is only one. Split out
/// so the choice is testable without a session bus.
fn pick_virtual<'a>(fresh: &[&'a MonitorInfo], mode: Mode) -> Option<&'a MonitorInfo> {
    fresh
        .iter()
        .find(|m| {
            m.1.iter()
                .any(|md| md.1 == mode.width as i32 && md.2 == mode.height as i32)
        })
        .or(fresh.first())
        .copied()
}

/// Make the virtual output primary — sole (`exclusive`: physicals
/// disabled) or with physicals kept as secondaries — so focus stays on
/// the streamed surface. Applied at `remembered_scale` (validated against
/// supported scales; 1.0 if none). Reverted by Mutter (APPLY_TEMPORARY).
async fn make_virtual_primary(
    dc: &zbus::Proxy<'_>,
    mode: Mode,
    pre: &CurrentState,
    state: &CurrentState,
    vconn: &str,
    exclusive: bool,
    remembered_scale: Option<f64>,
) -> Result<()> {
    let vmode = state
        .1
        .iter()
        .find(|m| m.0 .0 == vconn)
        .and_then(|m| {
            m.1.iter()
                .find(|md| md.1 == mode.width as i32 && md.2 == mode.height as i32)
                .map(|md| md.0.clone())
        })
        .or_else(|| current_mode(state, vconn).map(|(id, _, _)| id));
    let Some(vmode) = vmode else {
        bail!("virtual monitor {vconn} has no usable mode yet");
    };
    // Prefer the scale Mutter derived from RecordVirtual preferred-scale;
    // do not force 1.0 (that clobbers it). Older Mutter stays at 1.0:
    // snap to an integral logical size (no supported-scales on virtuals)
    // and retry at derived if the apply is rejected.
    let derived = logical_scale(state, vconn)
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(1.0);
    let mut scale = match remembered_scale {
        Some(want) if (want - derived).abs() > 1e-3 => {
            snap_integral_scale(want, mode.width, mode.height)
        }
        _ => derived,
    };
    loop {
        // Exclusive: virtual alone. Primary: virtual at (0,0) plus
        // physicals as secondaries. Headless, the two are identical.
        let config = if exclusive {
            build_exclusive_config(state, vconn, &vmode, mode.width as i32, scale)
        } else {
            build_primary_keeping_physicals(pre, state, vconn, &vmode, mode.width as i32, scale)
        };
        let res: zbus::Result<()> = dc
            .call(
                "ApplyMonitorsConfig",
                &(
                    state.0,
                    APPLY_TEMPORARY,
                    config,
                    HashMap::<String, Value<'static>>::new(),
                ),
            )
            .await;
        match res {
            Ok(()) => return Ok(()),
            Err(e) if (scale - derived).abs() > 1e-3 => {
                tracing::warn!(
                    scale,
                    derived,
                    error = %format!("{e:#}"),
                    "mutter: ApplyMonitorsConfig at the remembered scale failed — retrying at the derived scale"
                );
                scale = derived;
            }
            Err(e) => {
                return Err(e).context("DisplayConfig.ApplyMonitorsConfig (set virtual primary)")
            }
        }
    }
}

/// Snap `want` to a scale that gives the mode an integral logical size.
/// Mutter rejects fractional scales where `width/scale` or `height/scale`
/// is not an integer, and virtual monitors report no `supported-scales`.
/// Searches nearby logical widths that keep the aspect; falls back to
/// `want` (caller retries at derived). Pure, unit-tested.
fn snap_integral_scale(want: f64, width: u32, height: u32) -> f64 {
    if !want.is_finite() || want <= 0.0 {
        return 1.0;
    }
    let (w, h) = (width as i64, height as i64);
    let target = (w as f64 / want).round() as i64;
    (target - 8..=target + 8)
        .filter(|lw| *lw >= 1 && (h * lw) % w == 0)
        .map(|lw| w as f64 / lw as f64)
        .min_by(|a, b| (a - want).abs().total_cmp(&(b - want).abs()))
        .unwrap_or(want)
}

/// `(scale, transform)` of the logical monitor carrying `connector`.
/// `None` means no logical monitor carries it — Mutter's report of a
/// head the operator has disabled; [`keep_head_layout`] leaves it off.
fn logical_placement(state: &CurrentState, connector: &str) -> Option<(f64, u32)> {
    state
        .2
        .iter()
        .find(|l| l.5.iter().any(|spec| spec.0 == connector))
        .map(|l| (l.2, l.3))
}

fn logical_scale(state: &CurrentState, connector: &str) -> Option<f64> {
    logical_placement(state, connector).map(|(scale, _)| scale)
}

/// Whether a kept physical is re-applied, and at what `(scale, transform)`.
/// Pure and unit-tested: getting it wrong is invisible on a headless box.
///
/// Carry pre-connect scale/transform when the head was on. Omit it when
/// the connector existed pre-connect with no logical monitor (disabled
/// on purpose). A connector not in the snapshot (hotplug during connect)
/// stays on at whatever Mutter just derived.
fn keep_head_layout(
    existed_pre: bool,
    pre_logical: Option<(f64, u32)>,
    state_logical: Option<(f64, u32)>,
) -> Option<(f64, u32)> {
    // A non-finite or non-positive scale fails the whole ApplyMonitorsConfig.
    let sane = |(scale, transform): (f64, u32)| {
        (
            if scale.is_finite() && scale > 0.0 {
                scale
            } else {
                1.0
            },
            transform,
        )
    };
    match (pre_logical, existed_pre) {
        (Some(l), _) => Some(sane(l)),
        (None, true) => None,
        (None, false) => Some(sane(state_logical.unwrap_or((1.0, 0)))),
    }
}

/// Every head Mutter reports, for [`crate::monitors::list`].
///
/// A pure GetCurrentState on a short-lived connection — no session, no
/// ApplyMonitorsConfig, so it never contends [`TOPOLOGY_LOCK`]. Geometry
/// is logical-monitor space (`state.2`); a monitor absent from every
/// logical monitor is disabled and reported at the origin, not dropped.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime (monitor enumeration)")?;
    let state = rt.block_on(async {
        let dc = display_config().await?;
        get_state(&dc).await
    })?;
    let mut out: Vec<_> = state
        .1
        .iter()
        .map(|(spec, _modes, _props)| {
            let (connector, vendor, product, _serial) = spec;
            let logical = state
                .2
                .iter()
                .find(|l| l.5.iter().any(|s| &s.0 == connector));
            let (w, h, refresh) = current_mode_full(&state, connector)
                .map(|(_id, w, h, hz)| (w.max(0) as u32, h.max(0) as u32, (hz * 1000.0) as u32))
                .unwrap_or((0, 0, 0));
            crate::monitors::PhysicalMonitor {
                connector: connector.clone(),
                description: crate::monitors::describe(vendor, product, connector),
                width: w,
                height: h,
                refresh_mhz: refresh,
                x: logical.map(|l| l.0).unwrap_or(0),
                y: logical.map(|l| l.1).unwrap_or(0),
                scale: logical.map(|l| l.2).filter(|s| *s > 0.0).unwrap_or(1.0),
                primary: logical.map(|l| l.4).unwrap_or(false),
                enabled: logical.is_some(),
                // Our RecordVirtual outputs are `Meta-N`; the console draws each once.
                managed: is_virtual_connector(connector),
            }
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Persist a mid-stream scale change (GNOME Settings) under `scale_key`
/// so the next connect reapplies it. Read failures are skipped.
async fn persist_scale_change(dc: &zbus::Proxy<'_>, vconn: &str, scale_key: &str, known: &mut f64) {
    let Ok(state) = get_state(dc).await else {
        return;
    };
    let Some(cur) = logical_scale(&state, vconn) else {
        return;
    };
    if (cur - *known).abs() > 1e-3 {
        crate::identity::scales()
            .lock()
            .unwrap()
            .set(scale_key, cur);
        *known = cur;
        tracing::info!(
            scale = cur,
            "mutter: persisted the client's display scale for the next connect"
        );
    }
}

/// Exclusive: the virtual output as the sole primary. Physicals omitted
/// so Mutter disables them. A kept physical as secondary lets relative
/// pointer motion wander onto it (cursor vanishes on the client).
///
/// Every other virtual monitor stays listed, to the right. A resize
/// creates the next monitor while the old one still streams, and a
/// config that leaves a live virtual monitor unassigned SIGSEGVs Mutter
/// 50 (`meta_virtual_monitor_get_crtc_mode`, GNOME/mutter#5007).
fn build_exclusive_config(
    state: &CurrentState,
    vconn: &str,
    vmode: &str,
    virt_width: i32,
    scale: f64,
) -> Vec<ApplyLogical> {
    let mut logicals: Vec<ApplyLogical> = vec![(
        0,
        0,
        scale,
        0,
        true,
        vec![(vconn.to_string(), vmode.to_string(), HashMap::new())],
    )];
    let physical_layout = matches!(
        state.3.get("layout-mode").map(|v| &**v),
        Some(&Value::U32(2))
    );
    let footprint = |w: i32, s: f64| {
        if physical_layout {
            w.max(0)
        } else {
            ((w as f64 / s).round() as i32).max(0)
        }
    };
    let mut x = footprint(virt_width, scale);
    for mon in &state.1 {
        let conn = &mon.0 .0;
        if conn == vconn || !is_virtual_connector(conn) {
            continue;
        }
        let Some((mode_id, w, _)) = current_mode(state, conn) else {
            continue;
        };
        let s = logical_scale(state, conn)
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or(1.0);
        logicals.push((
            x,
            0,
            s,
            0,
            false,
            vec![(conn.clone(), mode_id, HashMap::new())],
        ));
        x += footprint(w, s);
    }
    logicals
}

/// Mutter names `RecordVirtual` connectors `Meta-N`; a physical head
/// carries its DRM connector name.
fn is_virtual_connector(conn: &str) -> bool {
    conn.starts_with("Meta-")
}

/// Primary: virtual at `(0, 0)`, every physical the operator had enabled
/// kept as a secondary (left-to-right, each at its pre-connect mode,
/// scale, and transform). Headless this equals [`build_exclusive_config`].
///
/// Read kept-head facts from `pre`. The post-virtual `state` is already
/// contaminated: Mutter re-derives layout and can drop a 120 Hz panel
/// to 60 Hz ([`physical_keep_mode`]). Scale/transform/enabled-ness from
/// the same snapshot ([`keep_head_layout`]).
fn build_primary_keeping_physicals(
    pre: &CurrentState,
    state: &CurrentState,
    vconn: &str,
    vmode: &str,
    virt_width: i32,
    scale: f64,
) -> Vec<ApplyLogical> {
    let mut logicals: Vec<ApplyLogical> = vec![(
        0,
        0,
        scale,
        0,
        true,
        vec![(vconn.to_string(), vmode.to_string(), HashMap::new())],
    )];
    // Physicals to the right of the virtual, at pre-connect mode.
    // Offsets are logical pixels on Wayland (virtual footprint is
    // width/scale), physical pixels only under layout-mode 2.
    let physical_layout = matches!(
        state.3.get("layout-mode").map(|v| &**v),
        Some(&Value::U32(2))
    );
    let virt_logical_width = if physical_layout {
        virt_width
    } else {
        ((virt_width as f64 / scale).round() as i32).max(1)
    };
    let mut x = virt_logical_width.max(0);
    for mon in &state.1 {
        let conn = &mon.0 .0;
        if conn == vconn {
            continue;
        }
        let existed_pre = pre.1.iter().any(|m| m.0 .0 == *conn);
        let Some((head_scale, transform)) = keep_head_layout(
            existed_pre,
            logical_placement(pre, conn),
            logical_placement(state, conn),
        ) else {
            // Omitted ⇒ Mutter leaves it disabled. Listing it would
            // switch their dark head on for the session.
            tracing::debug!(
                connector = %conn,
                "mutter: this head was disabled before the session — leaving it disabled"
            );
            continue;
        };
        if let Some((mode_id, w, h)) = physical_keep_mode(pre, state, conn) {
            logicals.push((
                x,
                0,
                head_scale,
                transform,
                false,
                vec![(conn.clone(), mode_id, HashMap::new())],
            ));
            // Advance by this head's logical footprint, same space as
            // the virtual. A 90°/270° head (transform 1/3/5/7) is as
            // wide as its mode is tall. Advancing by raw mode width
            // overlaps or gaps once real scale is preserved.
            let rotated = matches!(transform, 1 | 3 | 5 | 7);
            let footprint = if rotated { h } else { w };
            x += if physical_layout {
                footprint.max(0)
            } else {
                ((footprint as f64 / head_scale).round() as i32).max(0)
            };
        }
    }
    logicals
}

#[cfg(test)]
mod tests {
    use super::{
        keep_head_layout, pick_keep_mode, pick_virtual, snap_integral_scale, HashMap, Mode,
        MonitorInfo,
    };

    // (id, w, h, refresh, is_current, is_preferred)
    fn m(
        id: &str,
        w: i32,
        h: i32,
        hz: f64,
        cur: bool,
        pref: bool,
    ) -> (String, i32, i32, f64, bool, bool) {
        (id.to_string(), w, h, hz, cur, pref)
    }

    #[test]
    fn keep_mode_prefers_pre_refresh_over_downgraded_state() {
        // Pre 2560x1440@120; post-virtual current is 60 Hz. Re-apply 120.
        let pre = Some(("M120".to_string(), 2560, 1440, 120.0));
        let state = vec![
            m("M120", 2560, 1440, 120.0, false, false),
            m("M60", 2560, 1440, 60.0, true, true),
        ];
        assert_eq!(
            pick_keep_mode(pre, &state),
            Some(("M120".to_string(), 2560, 1440))
        );
    }

    #[test]
    fn keep_mode_rekeyed_id_matches_by_geometry_and_refresh() {
        // Pre id gone (re-keyed list); match 120 Hz by geometry + refresh.
        let pre = Some(("old-120".to_string(), 2560, 1440, 120.0));
        let state = vec![
            m("new-120", 2560, 1440, 119.998, false, false),
            m("new-60", 2560, 1440, 60.0, true, true),
        ];
        assert_eq!(
            pick_keep_mode(pre, &state),
            Some(("new-120".to_string(), 2560, 1440))
        );
    }

    #[test]
    fn keep_mode_falls_back_to_state_current_when_pre_mode_gone() {
        // Pre mode gone; never invent an id — use post-virtual current.
        let pre = Some(("gone-165".to_string(), 3440, 1440, 165.0));
        let state = vec![
            m("s-100", 3440, 1440, 100.0, true, false),
            m("s-60", 3440, 1440, 60.0, false, true),
        ];
        assert_eq!(
            pick_keep_mode(pre, &state),
            Some(("s-100".to_string(), 3440, 1440))
        );
    }

    #[test]
    fn snap_integral_scale_keeps_valid_scales_and_snaps_odd_ones() {
        // 1920/1.5 = 1280, 1080/1.5 = 720.
        assert_eq!(snap_integral_scale(1.5, 1920, 1080), 1.5);
        // GNOME 1.6666… on 3840x2400 (logical 2304x1440).
        let s = snap_integral_scale(1.666_666_6, 3840, 2400);
        assert!((s - 3840.0 / 2304.0).abs() < 1e-9, "got {s}");
        // 16:9 logical widths are multiples of 16 → 1.3 snaps to 1920/1472.
        let s = snap_integral_scale(1.3, 1920, 1080);
        assert!((s - 1920.0 / 1472.0).abs() < 1e-9, "got {s}");
        // Junk input degrades to 1.0.
        assert_eq!(snap_integral_scale(f64::NAN, 1920, 1080), 1.0);
        assert_eq!(snap_integral_scale(-2.0, 1920, 1080), 1.0);
    }

    #[test]
    fn keep_mode_no_pre_uses_state_current_then_preferred() {
        // Connector new since the snapshot: is-current, else is-preferred.
        let state = vec![
            m("A", 1920, 1080, 60.0, true, false),
            m("B", 1920, 1080, 144.0, false, true),
        ];
        assert_eq!(
            pick_keep_mode(None, &state),
            Some(("A".to_string(), 1920, 1080))
        );

        let no_current = vec![
            m("A", 1920, 1080, 60.0, false, false),
            m("B", 1920, 1080, 144.0, false, true),
        ];
        assert_eq!(
            pick_keep_mode(None, &no_current),
            Some(("B".to_string(), 1920, 1080))
        );
    }

    /// A kept physical comes back as the operator had it: pre-connect
    /// scale/transform, stay off if it was off, stay on if hotplugged.
    #[test]
    fn a_kept_head_carries_its_pre_connect_scale_and_transform() {
        // Rotated + 2×, as before the virtual appeared.
        assert_eq!(
            keep_head_layout(true, Some((2.0, 1)), Some((1.0, 0))),
            Some((2.0, 1))
        );
        // Disabled on purpose — stays off.
        assert_eq!(keep_head_layout(true, None, Some((1.0, 0))), None);
        // Hotplugged during connect: keep on at Mutter's derived layout.
        assert_eq!(
            keep_head_layout(false, None, Some((1.5, 2))),
            Some((1.5, 2))
        );
        assert_eq!(keep_head_layout(false, None, None), Some((1.0, 0)));
        // Junk scale would fail the whole ApplyMonitorsConfig.
        assert_eq!(keep_head_layout(true, Some((0.0, 3)), None), Some((1.0, 3)));
        assert_eq!(
            keep_head_layout(true, Some((f64::NAN, 0)), None),
            Some((1.0, 0))
        );
    }

    fn mon(connector: &str, modes: &[(i32, i32)]) -> MonitorInfo {
        (
            (
                connector.to_string(),
                "vendor".into(),
                "product".into(),
                "serial".into(),
            ),
            modes
                .iter()
                .map(|&(w, h)| {
                    (
                        format!("{w}x{h}"),
                        w,
                        h,
                        60.0,
                        1.0,
                        vec![1.0],
                        HashMap::new(),
                    )
                })
                .collect(),
            HashMap::new(),
        )
    }

    const M: Mode = Mode {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };

    /// A resize promotes the new virtual monitor while the old one still
    /// streams: it stays listed (secondary, to the right), physicals do not.
    #[test]
    fn an_exclusive_config_keeps_a_sibling_virtual_monitor_assigned() {
        let state = (
            1u32,
            vec![
                mon("Meta-0", &[(5120, 1440)]),
                mon("Meta-1", &[(1246, 1124)]),
                mon("eDP-1", &[(1920, 1080)]),
            ],
            vec![],
            HashMap::new(),
        );
        let cfg = super::build_exclusive_config(&state, "Meta-1", "1246x1124", 1246, 1.0);
        assert_eq!(cfg.len(), 2, "{cfg:?}");
        assert!(cfg[0].4 && cfg[0].0 == 0 && cfg[0].5[0].0 == "Meta-1");
        assert!(!cfg[1].4 && cfg[1].0 == 1246 && cfg[1].5[0].0 == "Meta-0");
        assert_eq!(cfg[1].5[0].1, "5120x1440");
        assert!(cfg.iter().all(|l| l.5.iter().all(|m| m.0 != "eDP-1")));
    }

    /// One new connector is ours whether or not the size matches (older
    /// Mutter may not advertise it).
    #[test]
    fn a_lone_new_connector_is_ours() {
        let only = mon("VIRTUAL-1", &[(3840, 2160)]);
        assert_eq!(
            pick_virtual(&[&only], M).map(|m| m.0 .0.as_str()),
            Some("VIRTUAL-1")
        );
    }

    /// A physical hotplug can land in the same window. The client's
    /// exact mode tells the two apart; first-wins aims topology at the
    /// operator's panel.
    #[test]
    fn a_hotplug_does_not_steal_the_identity() {
        let hotplug = mon("DP-3", &[(2560, 1440), (3840, 2160)]);
        let ours = mon("VIRTUAL-1", &[(1920, 1080)]);
        assert_eq!(
            pick_virtual(&[&hotplug, &ours], M).map(|m| m.0 .0.as_str()),
            Some("VIRTUAL-1"),
            "the connector advertising the client's mode must win regardless of order"
        );
    }

    /// Nothing advertises the client's size. First-wins rather than
    /// failing the session; the caller logs a warning.
    #[test]
    fn with_no_mode_match_the_first_still_wins() {
        let a = mon("DP-3", &[(2560, 1440)]);
        let b = mon("VIRTUAL-1", &[(3840, 2160)]);
        assert_eq!(
            pick_virtual(&[&a, &b], M).map(|m| m.0 .0.as_str()),
            Some("DP-3")
        );
    }

    #[test]
    fn nothing_new_is_nothing_to_pick() {
        assert!(pick_virtual(&[], M).is_none());
    }

    /// Live GNOME create/drop. Needs gnome-shell on the session bus.
    /// `PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY=0` unless Exclusive is the point.
    #[test]
    #[ignore = "needs a live gnome-shell on the session bus; run with --ignored"]
    fn live_mutter_create_drop() {
        use super::{MutterDisplay, VirtualDisplay};

        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let mut vd = MutterDisplay::new().expect("construct the Mutter backend");
        let out = vd.create(mode).expect("create the Mutter virtual monitor");
        assert!(out.node_id > 0, "a real PipeWire node id");
        assert_eq!(
            out.preferred_mode,
            Some((mode.width, mode.height, mode.refresh_hz))
        );
        drop(out);
    }
}
