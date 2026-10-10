//! Portal ScreenCast of one named output, for the backends whose portal picks
//! its source from a per-user file: Hyprland (xdph's custom picker) and
//! wlroots (xdpw's chooser). Neither has a headless source-selection API.
//!
//! [`cast`] writes the file, runs the handshake under [`SELECTION_LOCK`] and
//! removes the file when the handshake is over. The returned [`StopGuard`]
//! closes the ScreenCast session; the backend drops it before it removes the
//! output.

use crate::portal_cursor::Mode;
use anyhow::{anyhow, bail, Context, Result};
use pf_portal::{close_session, finish_or_close, within, CAST_CLOSE_BUDGET, HANDSHAKE_BUDGET};
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// How one portal is pointed at an output.
pub(crate) struct Selector {
    /// Per-user file the picker reads, under `$XDG_RUNTIME_DIR`.
    pub file: fn() -> String,
    /// The file's contents that name `output`.
    pub line: fn(&str) -> String,
    /// Points the portal at the picker (restarts the portal when that changed).
    pub ensure_config: fn() -> Result<()>,
    /// Portal thread name.
    pub thread: &'static str,
    /// Portal name for logs and errors: `xdph`, `xdpw`.
    pub portal: &'static str,
}

/// Serializes write-the-selection → complete-the-handshake, process-wide. The
/// file is one per user: a write between ours and the portal's read steers
/// capture at the other session's output.
static SELECTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The selection file, removed when the handshake it steers is over.
///
/// The picker reads it once inside [`cast`]'s critical section. Left behind, it
/// names an output that may be gone and shadows the portal's own fallback.
/// Tying removal to the cast would delete a sibling session's selection.
struct SelectionFile(String);

impl Drop for SelectionFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %self.0, error = %e, "portal selection file not removed");
            }
        }
    }
}

/// Ends the cast: signals the portal thread, then waits for `Session.Close` so
/// the caller may remove the output afterwards.
///
/// xdph and xdpw destroy a session only on explicit `Session.Close`; neither
/// watches for a vanished peer. Removing the output while the portal still
/// captures it wedges the portal's frame loop (an unbounded `pw_loop_iterate`
/// spin). Waiting for Close means the output removed next is one nobody captures.
pub(crate) struct StopGuard {
    stop: Arc<AtomicBool>,
    /// Signalled once the portal thread has closed the session. `None` when no
    /// cast was established: nothing to close, and no budget to burn.
    closed: Option<Receiver<()>>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(closed) = self.closed.take() else {
            return;
        };
        match closed.recv_timeout(CAST_CLOSE_BUDGET) {
            // Disconnected: the thread is gone, so nothing holds the cast.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
            Err(RecvTimeoutError::Timeout) => tracing::warn!(
                budget_s = CAST_CLOSE_BUDGET.as_secs(),
                "the ScreenCast session did not close in time — removing the output underneath \
                 it; the next cast may find the portal busy"
            ),
        }
    }
}

/// Point `sel`'s portal at `output` and run the ScreenCast handshake. The
/// cursor mode is the one negotiated against what the portal advertises;
/// `hw_cursor` is only the request.
pub(crate) fn cast(
    sel: &Selector,
    output: &str,
    hw_cursor: bool,
) -> Result<(OwnedFd, u32, Mode, StopGuard)> {
    let _lock = SELECTION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    (sel.ensure_config)()?;
    let path = (sel.file)();
    std::fs::write(&path, (sel.line)(output)).with_context(|| format!("write {path}"))?;
    // Every arm below, `?` included, leaves the handshake: the file's only reader.
    let _file = SelectionFile(path);
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<(OwnedFd, u32, Mode), String>>();
    // Fires at the other end of the cast, when `StopGuard::drop` waits for Close.
    let (closed_tx, closed_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let portal = sel.portal;
    std::thread::Builder::new()
        .name(sel.thread.into())
        .spawn(move || portal_thread(portal, setup_tx, closed_tx, stop_thread, hw_cursor))
        .with_context(|| format!("spawn {portal} portal thread"))?;
    // Built before the wait so every error arm sets `stop`: the thread's send can
    // land after `recv_timeout` gives up, then park forever on a live ScreenCast.
    let mut guard = StopGuard { stop, closed: None };
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok((fd, node_id, cursor_mode))) => {
            // A cast exists, so teardown must wait. Only this arm arms it.
            guard.closed = Some(closed_rx);
            Ok((fd, node_id, cursor_mode, guard))
        }
        Ok(Err(e)) => bail!("ScreenCast portal on {output} failed: {e}"),
        Err(_) => bail!("timed out waiting for the ScreenCast portal on {output}"),
    }
}

/// Stream a head the compositor already has. The keepalive stops the cast only;
/// the head is the compositor's, not ours.
pub(crate) fn stream_existing_output(
    sel: &Selector,
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let (fd, node_id, cursor_mode, stop) = cast(sel, connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        remote_fd: Some(fd),
        cursor_mode: Some(cursor_mode),
        keepalive: Box::new(stop),
    })
}

/// Handshake under [`HANDSHAKE_BUDGET`], report fd + node id + cursor mode, park
/// until `stop`, then `Session.Close` and signal `closed_tx`. The picker answers
/// source selection, so no dialog shows.
fn portal_thread(
    portal: &'static str,
    setup_tx: Sender<Result<(OwnedFd, u32, Mode), String>>,
    closed_tx: Sender<()>,
    stop: Arc<AtomicBool>,
    hw_cursor: bool,
) {
    use ashpd::desktop::screencast::{Screencast, SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;

    // Shared, never dropped ([`pf_portal`]): a per-cast runtime kills
    // ashpd's process-global cached connection and every later handshake hangs.
    let rt = match pf_portal::portal_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(e));
            return;
        }
    };
    let err_tx = setup_tx.clone();

    rt.block_on(async move {
        let result: Result<()> = async {
            let deadline = tokio::time::Instant::now() + HANDSHAKE_BUDGET;
            // An orphaned cached connection hangs here, before any handshake call.
            let proxy = within(deadline, Screencast::new())
                .await
                .with_context(|| format!("connect ScreenCast portal (is {portal} running?)"))?;
            let session = within(deadline, proxy.create_session(Default::default()))
                .await
                .context("create_session")?;
            let steps = async {
                // Negotiated against what the portal advertises: an unadvertised
                // mode fails the call. Neither xdph nor xdpw advertises Metadata.
                let cursor_mode = crate::portal_cursor::negotiate(&proxy, hw_cursor, portal).await;
                proxy
                    .select_sources(
                        &session,
                        SelectSourcesOptions::default()
                            .set_cursor_mode(pf_portal::to_ashpd(cursor_mode))
                            // Both offer MONITOR; the picker selects our output.
                            .set_sources(BitFlags::from_flag(SourceType::Monitor))
                            .set_multiple(false)
                            .set_persist_mode(PersistMode::DoNot),
                    )
                    .await
                    .context("select_sources")?
                    .response()
                    .context("select_sources rejected")?;
                let streams = proxy
                    .start(&session, None, Default::default())
                    .await
                    .context("start cast")?
                    .response()
                    .with_context(|| {
                        format!(
                            "start response (picker declined? check the {portal} config and the \
                             selection file)"
                        )
                    })?;
                let stream = streams
                    .streams()
                    .first()
                    .context("portal returned no streams")?
                    .clone();
                let fd = proxy
                    .open_pipe_wire_remote(&session, Default::default())
                    .await
                    .context("open_pipe_wire_remote")?;
                Ok::<_, anyhow::Error>((fd, stream.pipe_wire_node_id(), cursor_mode))
            };
            let (fd, node_id, cursor_mode) =
                finish_or_close(deadline, steps, || session.close()).await?;

            setup_tx
                .send(Ok((fd, node_id, cursor_mode)))
                .map_err(|_| anyhow!("virtual-output opener went away"))?;

            // Keep `proxy` + `session` alive until stopped. 20 ms poll: teardown
            // waits on the Close that follows.
            let _keep_alive = (&proxy, &session);
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            close_session(session.close()).await;
            // Best-effort: the receiver is gone if the caller already gave up.
            let _ = closed_tx.send(());
            Ok(())
        }
        .await;

        if let Err(e) = result {
            let _ = err_tx.send(Err(format!("{e:#}")));
        }
    });
}
