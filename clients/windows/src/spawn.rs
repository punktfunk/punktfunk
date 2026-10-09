//! The shell↔session handoff: streams run in the spawned `punktfunk-session` binary,
//! spawned and supervised by `pf_client_core::orchestrate` like the GTK shell's. This module
//! adds what only this shell needs: CREATE_NO_WINDOW (the session keeps the console subsystem
//! for its stdout contract, and a GUI parent would otherwise pop a console window),
//! `--window-pos`, the log-file tee for the child's stderr, and [`SpawnEvent`]s for the app's
//! navigation closure.

use pf_client_core::orchestrate::{self, CancelHandle, ConnectOutcome, SessionEvent};
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;

/// One event from the session child.
pub(crate) enum SpawnEvent {
    /// The child presented its first frame (its window is up and streaming).
    Ready,
    /// One stats window for the session status page.
    Stats(Box<punktfunk_core::hud::StatsSnapshot>),
    /// The child exited (stdout EOF + reap; a kill lands here too), classified.
    Exited(ConnectOutcome),
}

/// Where a banner sends the user for a cause: the log's real location.
pub(crate) fn log_hint() -> String {
    crate::logfile::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the client log".into())
}

/// Spawn the session binary for a connect with `fp_hex` pinned and feed its lifecycle to
/// `on_event` from a reader thread. `slot` is the handle Disconnect/Cancel kill. `launch`
/// carries a library title id for the host to launch during the handshake; `preset` is a
/// ONE-OFF settings-preset pick. `profile` is the shell's answer for who plays: it replaces
/// the saved pick the plan read, and `None` names none. `Err` = the spawn itself failed (binary missing?) —
/// surfaced as a connect error by the caller.
///
/// The argv and the `--resolved-spec` come from [`orchestrate::session_command`], so this
/// shell's sessions run from the same effective (preset-aware) settings as every other one.
#[allow(clippy::too_many_arguments)] // one cohesive spawn spec (session_params precedent)
pub(crate) fn spawn_session(
    addr: &str,
    port: u16,
    fp_hex: &str,
    connect_timeout_secs: u64,
    launch: Option<&str>,
    preset: Option<&str>,
    profile: Option<&str>,
    slot: CancelHandle,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    use pf_client_core::orchestrate::{ConnectPlan, HostTarget};
    let mut plan = ConnectPlan::for_target(
        HostTarget {
            name: String::new(), // display-only; this shell's screens carry their own copy
            addr: addr.to_string(),
            port,
            fp_hex: Some(fp_hex.to_string()),
            mac: Vec::new(), // wake ran before this spawn (initiate_waking) — not the plan's job
            id: None,
            mgmt_port: None, // the library fetch runs in the shell (`Target`), never off a spawn plan
        },
        launch.map(str::to_string),
        preset.map(str::to_string),
    );
    plan.profile = profile.map(str::to_string);
    plan.connect_timeout_secs = Some(connect_timeout_secs);
    let (cmd, spec_path) = orchestrate::session_command(&plan);
    spawn(cmd, spec_path, &format!("{addr}:{port}"), slot, on_event)
}

/// Spawn the session binary in `--browse` mode: the console home, in the session window —
/// launches run as streams in that same window. The same stdout contract as a connect
/// (`--json-status`): `ready` when the console window presents, `error` on a failed start,
/// EOF on quit.
pub(crate) fn spawn_browse(
    fullscreen: bool,
    slot: CancelHandle,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    let mut cmd = Command::new(orchestrate::session_binary());
    cmd.arg("--browse");
    cmd.arg("--json-status");
    if fullscreen {
        cmd.arg("--fullscreen");
    }
    spawn(cmd, None, "console", slot, on_event)
}

/// Hand the shell window's position to the child (`--window-pos`) so the session window
/// opens on the same monitor, where the shell is — the hide/restore handoff then reads as
/// one window changing content instead of a window jumping displays.
fn add_window_pos(cmd: &mut Command) {
    if let Some((x, y)) = crate::shell_window::position() {
        cmd.arg("--window-pos").arg(format!("{x},{y}"));
    }
}

/// [`orchestrate::spawn_child`] with this shell's window flags and log tee, folding the
/// contract's `error`/`ended` lines and our own kill into [`SpawnEvent::Exited`].
fn spawn(
    mut cmd: Command,
    spec_path: Option<PathBuf>,
    label: &str,
    slot: CancelHandle,
    mut on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    add_window_pos(&mut cmd);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let (mut error, mut ended) = (None::<orchestrate::SessionError>, None::<String>);
    let cancel = slot.clone();
    orchestrate::spawn_child(cmd, spec_path, Some(slot), crate::logfile::Tee, move |ev| {
        match ev {
            SessionEvent::Ready => on_event(SpawnEvent::Ready),
            SessionEvent::Stats(s) => on_event(SpawnEvent::Stats(s)),
            SessionEvent::Error(e) => error = Some(e),
            SessionEvent::Ended(msg) => ended = Some(msg),
            // orchestrate persists the window size on the way past.
            SessionEvent::Window { .. } => {}
            SessionEvent::Exited(code) => on_event(SpawnEvent::Exited(ConnectOutcome::from_exit(
                code,
                error.take(),
                ended.take(),
                cancel.is_cancelled(),
            ))),
        }
    })
    .map_err(|e| {
        tracing::error!(error = %e, "spawning the session binary");
        "The session didn't start. Check the client log.".to_string()
    })?;
    tracing::info!(host = %label, "session binary spawned");
    Ok(())
}
