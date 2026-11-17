//! A PipeWire cast of one niri output through niri's in-process `org.gnome.Mutter.ScreenCast`.
//!
//! niri 26.04 serves the ScreenCast half of Mutter's API (the `xdp-gnome-screencast` feature,
//! on by default) but not RemoteDesktop, so the session stands alone: `CreateSession({})` →
//! `RecordMonitor(output)` → `Start` → `PipeWireStreamAdded`. The node lives on the user's
//! PipeWire daemon, so there is no remote fd to carry. `hw_cursor` asks for the pointer as
//! metadata; otherwise niri paints it into the frames.

use anyhow::{anyhow, bail, Context, Result};
use ashpd::zbus;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, Value};

const BUS_SC: &str = "org.gnome.Mutter.ScreenCast";
/// Same cursor-mode values as Mutter's: 1 paints the pointer, 2 sends it as metadata.
const CURSOR_EMBEDDED: u32 = 1;
const CURSOR_METADATA: u32 = 2;
/// How long a cast's drop waits for the session's `Stop`. A miss leaves the cast thread to send
/// it alone, after whatever drops next.
const STOP_WAIT: Duration = Duration::from_secs(1);

/// The keepalive of one cast. Drop raises `stop` and waits up to [`STOP_WAIT`] for the cast
/// thread's `Stop` to return, so what drops after it, the output included, goes after the
/// session.
struct CastStop {
    stop: Arc<AtomicBool>,
    /// Answered once `Stop` returned. `None` until the cast is up: a failed start has no
    /// session to wait for.
    stopped: Option<Receiver<()>>,
}

impl Drop for CastStop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(stopped) = self.stopped.take() else {
            return;
        };
        if let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(STOP_WAIT) {
            tracing::warn!("niri screencast stop timed out — the output may go first");
        }
    }
}

/// Start a cast of `output`; the node id and the keepalive whose drop stops it.
pub(crate) fn start(output: &str, hw_cursor: bool) -> Result<(u32, Box<dyn Send>)> {
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let (stopped_tx, stopped_rx) = std::sync::mpsc::channel::<()>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let output_thread = output.to_string();
    thread::Builder::new()
        .name("punktfunk-niri-cast".into())
        .spawn(move || cast_thread(setup_tx, stopped_tx, stop_thread, output_thread, hw_cursor))
        .context("spawn the niri cast thread")?;
    // Built before the wait so a timeout still stops the thread.
    let mut guard = CastStop {
        stop,
        stopped: None,
    };
    match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(node_id)) => {
            guard.stopped = Some(stopped_rx);
            Ok((node_id, Box::new(guard)))
        }
        Ok(Err(e)) => bail!("niri screencast of {output}: {e}"),
        Err(_) => bail!("timed out starting the niri screencast of {output}"),
    }
}

/// Runs one cast until `stop`, then sends `Stop` and answers `stopped`. 20 ms poll: the
/// keepalive's drop waits on the `Stop` that follows.
fn cast_thread(
    setup_tx: Sender<Result<u32, String>>,
    stopped: Sender<()>,
    stop: Arc<AtomicBool>,
    output: String,
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
        let session = match connect(&output, hw_cursor).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        // A failed send means the opener gave up: stop now rather than hold a cast nobody reads.
        if setup_tx.send(Ok(session.node_id)).is_ok() {
            while !stop.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        if let Err(e) = session.session.call_method("Stop", &()).await {
            tracing::debug!(output = %output, error = %e, "niri screencast stop");
        }
        let _ = stopped.send(());
    });
}

/// Held for the cast's lifetime; the connection is what niri ties the session to.
struct Session {
    session: zbus::Proxy<'static>,
    _conn: zbus::Connection,
    node_id: u32,
}

async fn connect(output: &str, hw_cursor: bool) -> Result<Session> {
    // The stored session's bus, as every other D-Bus user in the crate: the process env may
    // name a bus from an earlier login.
    let conn = pf_portal::session_bus()
        .await
        .context("connect session D-Bus")?;
    let sc = zbus::Proxy::new(
        &conn,
        BUS_SC,
        "/org/gnome/Mutter/ScreenCast",
        "org.gnome.Mutter.ScreenCast",
    )
    .await
    .context("niri ScreenCast proxy (is niri built with xdp-gnome-screencast?)")?;
    let props: HashMap<&str, Value> = HashMap::new();
    let path: OwnedObjectPath = sc
        .call("CreateSession", &(props,))
        .await
        .context("ScreenCast.CreateSession")?;
    let session =
        zbus::Proxy::new(&conn, BUS_SC, path, "org.gnome.Mutter.ScreenCast.Session").await?;

    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert(
        "cursor-mode",
        Value::from(if hw_cursor {
            CURSOR_METADATA
        } else {
            CURSOR_EMBEDDED
        }),
    );
    let stream_path: OwnedObjectPath = session
        .call("RecordMonitor", &(output, rec))
        .await
        .with_context(|| format!("Session.RecordMonitor({output:?})"))?;
    let stream = zbus::Proxy::new(
        &conn,
        BUS_SC,
        stream_path,
        "org.gnome.Mutter.ScreenCast.Stream",
    )
    .await?;
    // Subscribe before Start: the signal can land while the subscription is still being made.
    let mut added = stream
        .receive_signal("PipeWireStreamAdded")
        .await
        .context("subscribe PipeWireStreamAdded")?;
    session
        .call_method("Start", &())
        .await
        .context("ScreenCast.Session.Start")?;
    let msg = tokio::time::timeout(Duration::from_secs(10), added.next())
        .await
        .map_err(|_| anyhow!("PipeWireStreamAdded did not arrive within 10s"))?
        .ok_or_else(|| anyhow!("signal stream ended before PipeWireStreamAdded"))?;
    let (node_id,): (u32,) = msg
        .body()
        .deserialize()
        .context("PipeWireStreamAdded body")?;
    tracing::info!(output, node_id, "niri: screencast started");
    Ok(Session {
        session,
        _conn: conn,
        node_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    /// The keepalive's drop returns only once the cast thread's `Stop` did, so the output that
    /// drops next goes after the session; a thread that never answers costs [`STOP_WAIT`].
    #[test]
    fn a_cast_keepalive_drops_after_its_stop() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, cast_log) = (stop.clone(), log.clone());
        let cast = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(5));
            }
            thread::sleep(Duration::from_millis(300));
            cast_log.lock().unwrap().push("Stop");
            stopped_tx.send(()).unwrap();
        });
        drop(CastStop {
            stop,
            stopped: Some(stopped_rx),
        });
        log.lock().unwrap().push("RemoveVirtualOutput");
        cast.join().unwrap();
        assert_eq!(*log.lock().unwrap(), ["Stop", "RemoveVirtualOutput"]);

        let (_silent, never) = std::sync::mpsc::channel::<()>();
        let begun = Instant::now();
        drop(CastStop {
            stop: Arc::new(AtomicBool::new(false)),
            stopped: Some(never),
        });
        let waited = begun.elapsed();
        assert!(waited >= STOP_WAIT && waited < STOP_WAIT * 2, "{waited:?}");
    }
}
