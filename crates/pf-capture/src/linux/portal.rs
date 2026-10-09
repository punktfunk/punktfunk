//! xdg ScreenCast / RemoteDesktop control plane: bounded ashpd handshake on the
//! shared portal runtime, and GNOME's BT.2100 colour-mode probe.
//!
//! Nothing here is per-frame. The handshake runs once; the thread then parks until
//! `PortalSession`'s `Drop` (parent module) fires `quit_rx`, closes the portal
//! session and signals done. ashpd's `Session` has no `Drop`, and the zbus
//! connection is process-global (`pf_portal`), so only `Session.Close`
//! ends the compositor's cast.
//!
//! HDR offer is scoped to `PUNKTFUNK_CAPTURE_MONITOR` when set; unpinned it is
//! "any head in BT.2100". See `design/per-monitor-portal-capture.md`. The probe
//! is one session-bus round-trip; call from control-plane threads only.

use anyhow::{anyhow, Context, Result};
use pf_portal::{
    close_session, finish_or_close, negotiate_cursor_mode, to_ashpd, within, HANDSHAKE_BUDGET,
};
use std::future::Future;
use std::os::fd::OwnedFd;

/// Mutter advertises 10-bit PQ only while the mirrored head is BT.2100.
/// `false` on any error (not GNOME, no colour modes, no monitors) so the
/// caller offers SDR. Blocking session-bus round-trip; control-plane only.
///
/// When `PUNKTFUNK_CAPTURE_MONITOR` is set, only that connector counts — an
/// HDR neighbour must not pull PQ onto an SDR panel. Unpinned: any head.
/// See `design/per-monitor-portal-capture.md`.
pub fn gnome_hdr_monitor_active() -> bool {
    use ashpd::zbus;
    // `color-mode` is on the monitor properties dict, not the logical-monitor one.
    type Mode = (
        String,
        i32,
        i32,
        f64,
        f64,
        Vec<f64>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type Monitor = (
        (String, String, String, String),
        Vec<Mode>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type LogicalMonitor = (
        i32,
        i32,
        f64,
        u32,
        bool,
        Vec<(String, String, String, String)>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type State = (
        u32,
        Vec<Monitor>,
        Vec<LogicalMonitor>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    let probe = || -> Result<bool> {
        // zbus is async-only here (ashpd's tokio). Throwaway current-thread runtime:
        // one round-trip, not the handshake path that needs a pumped reader.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        rt.block_on(async {
            let conn = zbus::Connection::session().await.context("session bus")?;
            let reply = conn
                .call_method(
                    Some("org.gnome.Mutter.DisplayConfig"),
                    "/org/gnome/Mutter/DisplayConfig",
                    Some("org.gnome.Mutter.DisplayConfig"),
                    "GetCurrentState",
                    &(),
                )
                .await
                .context("DisplayConfig.GetCurrentState")?;
            let (_serial, monitors, _logical, _props): State = reply
                .body()
                .deserialize()
                .context("parse GetCurrentState")?;
            // `spec.0` is the connector; "color-mode" 1 is META_COLOR_MODE_BT2100.
            let heads: Vec<(&str, bool)> = monitors
                .iter()
                .map(|(spec, _modes, props)| {
                    let hdr = props
                        .get("color-mode")
                        .and_then(|v| u32::try_from(v).ok())
                        .is_some_and(|mode| mode == 1);
                    (spec.0.as_str(), hdr)
                })
                .collect();
            Ok(hdr_offer_for(
                &heads,
                pf_host_config::config().capture_monitor.as_deref(),
            ))
        })
    };
    match probe() {
        Ok(hdr) => hdr,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "GNOME HDR colour-mode probe failed — SDR");
            false
        }
    }
}

/// Pinned: only that connector's BT.2100 bit. A pin that names no live head is
/// SDR, not "any" — the session is about to fail on that missing monitor, and an
/// HDR offer would be a second wrong answer it would not fail on. Unpinned: any head.
fn hdr_offer_for(heads: &[(&str, bool)], pinned: Option<&str>) -> bool {
    match pinned {
        Some(want) => heads
            .iter()
            .find(|(connector, _)| connector.eq_ignore_ascii_case(want))
            .is_some_and(|(_, hdr)| *hdr),
        None => heads.iter().any(|(_, hdr)| *hdr),
    }
}

type SetupTx = std::sync::mpsc::Sender<Result<(OwnedFd, u32), String>>;

/// Handshake under [`HANDSHAKE_BUDGET`], hand the fd and node over, park
/// on `quit_rx`, then `Session.Close`. ashpd `Session` has no `Drop` and the connection is
/// process-global, so nothing else ends the cast.
///
/// `anchored` selects sources on a RemoteDesktop session (KWin/GNOME), so the single
/// `start` grant — the `kde-authorized` headless bypass, same as libei — covers capture.
/// ScreenCast has no such bypass; a standalone cast would show a dialog.
pub(super) fn portal_thread(
    setup_tx: SetupTx,
    quit_rx: tokio::sync::oneshot::Receiver<()>,
    want_metadata_cursor: bool,
    anchored: bool,
) {
    // Shared, never dropped (`pf_portal`): a per-session runtime took
    // ashpd's process-global D-Bus connection down with it, and every later
    // handshake in the process hung.
    let rt = match pf_portal::portal_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(e));
            return;
        }
    };
    rt.block_on(async move {
        let deadline = tokio::time::Instant::now() + HANDSHAKE_BUDGET;
        let result = if anchored {
            remote_desktop(deadline, want_metadata_cursor, &setup_tx, quit_rx).await
        } else {
            screencast(deadline, want_metadata_cursor, &setup_tx, quit_rx).await
        };
        if let Err(e) = result {
            let _ = setup_tx.send(Err(format!("{e:#}")));
        }
    });
}

async fn screencast(
    deadline: tokio::time::Instant,
    want_metadata_cursor: bool,
    setup_tx: &SetupTx,
    quit_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    use ashpd::desktop::screencast::Screencast;
    let proxy = within(deadline, Screencast::new())
        .await
        .context("connect ScreenCast portal")?;
    let session = within(deadline, proxy.create_session(Default::default()))
        .await
        .context("create_session")?;
    let start = async {
        Ok::<_, anyhow::Error>(
            proxy
                .start(&session, None, Default::default())
                .await
                .context("start cast")?
                .response()
                .context("start response (chooser cancelled? portal misconfigured?)")?
                .streams()
                .to_vec(),
        )
    };
    let steps = cast(
        &proxy,
        &session,
        want_metadata_cursor,
        "screencast",
        std::future::ready(Ok(())),
        start,
    );
    serve(deadline, &session, steps, setup_tx, quit_rx).await
}

async fn remote_desktop(
    deadline: tokio::time::Instant,
    want_metadata_cursor: bool,
    setup_tx: &SetupTx,
    quit_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions};
    use ashpd::desktop::screencast::Screencast;
    use ashpd::desktop::PersistMode;
    let remote = within(deadline, RemoteDesktop::new())
        .await
        .context("connect RemoteDesktop portal")?;
    let screencast = within(deadline, Screencast::new())
        .await
        .context("connect ScreenCast portal")?;
    let session = within(deadline, remote.create_session(Default::default()))
        .await
        .context("create RemoteDesktop session")?;
    // Device selection is required even though this session never `connect_to_eis`
    // (inject has its own). Without it, `start` is not the grant `kde-authorized` covers.
    let select_devices = async {
        remote
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .context("select_devices")?
            .response()
            .context("select_devices rejected")
    };
    let start = async {
        Ok::<_, anyhow::Error>(
            remote
                .start(&session, None, Default::default())
                .await
                .context("start RemoteDesktop+ScreenCast")?
                .response()
                .context("start response (grant not pre-authorized / headless dialog?)")?
                .streams()
                .to_vec(),
        )
    };
    let steps = cast(
        &screencast,
        &session,
        want_metadata_cursor,
        "remote-desktop",
        select_devices,
        start,
    );
    serve(deadline, &session, steps, setup_tx, quit_rx).await
}

/// The cast on a created session: `prepare`, the cursor mode, one monitor source, `start`,
/// then the PipeWire remote and the first stream's node id.
async fn cast<S: ashpd::desktop::screencast::IsScreencastSession>(
    screencast: &ashpd::desktop::screencast::Screencast,
    session: &ashpd::desktop::Session<S>,
    want_metadata_cursor: bool,
    backend: &str,
    prepare: impl Future<Output = Result<()>>,
    start: impl Future<Output = Result<Vec<ashpd::desktop::screencast::Stream>>>,
) -> Result<(OwnedFd, u32)> {
    use ashpd::desktop::screencast::{SelectSourcesOptions, SourceType};
    use ashpd::desktop::PersistMode;
    use ashpd::enumflags2::BitFlags;
    prepare.await?;
    let cursor_mode = negotiate_cursor_mode(screencast, want_metadata_cursor, backend).await;
    screencast
        .select_sources(
            session,
            SelectSourcesOptions::default()
                .set_cursor_mode(to_ashpd(cursor_mode))
                // wlroots advertises MONITOR only (`AvailableSourceTypes=1`).
                // Asking for an unsupported type invalidates the session.
                .set_sources(BitFlags::from_flag(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .context("select_sources")?
        .response()
        .context("select_sources rejected (unsupported source type / cursor mode?)")?;
    let node_id = start
        .await?
        .first()
        .context("portal returned no streams")?
        .pipe_wire_node_id();
    let fd = screencast
        .open_pipe_wire_remote(session, Default::default())
        .await
        .context("open_pipe_wire_remote")?;
    Ok((fd, node_id))
}

/// Run `steps` under the deadline, hand the result over, then hold `session` until
/// `quit_rx` and close it. The zbus connection is process-global, so only that Close ends
/// the compositor's cast.
async fn serve<S: ashpd::desktop::SessionPortal>(
    deadline: tokio::time::Instant,
    session: &ashpd::desktop::Session<S>,
    steps: impl Future<Output = Result<(OwnedFd, u32)>>,
    setup_tx: &SetupTx,
    quit_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<()> {
    let setup = finish_or_close(deadline, steps, || session.close()).await?;
    setup_tx
        .send(Ok(setup))
        .map_err(|_| anyhow!("capturer dropped before setup completed"))?;
    let _ = quit_rx.await;
    close_session(session.close()).await;
    Ok(())
}

#[cfg(test)]
mod hdr_offer_tests {
    use super::hdr_offer_for;

    #[test]
    fn unpinned_keeps_the_any_monitor_heuristic() {
        assert!(hdr_offer_for(&[("DP-1", false), ("HDMI-A-1", true)], None));
        assert!(!hdr_offer_for(&[("DP-1", false)], None));
    }

    #[test]
    fn a_pin_ignores_an_hdr_neighbour() {
        let heads = [("DP-1", false), ("HDMI-A-1", true)];
        assert!(!hdr_offer_for(&heads, Some("DP-1")));
        assert!(hdr_offer_for(&heads, Some("HDMI-A-1")));
    }

    #[test]
    fn a_pin_matches_case_insensitively_like_the_resolver() {
        assert!(hdr_offer_for(&[("HDMI-A-1", true)], Some("hdmi-a-1")));
    }

    #[test]
    fn a_pin_naming_no_live_head_reports_sdr() {
        assert!(!hdr_offer_for(&[("DP-1", true)], Some("DP-9")));
    }
}
