//! The shell↔session handoff: streams run in the spawned `punktfunk-session` binary,
//! spawned and supervised by `pf_client_core::orchestrate` like the GTK shell's. This module
//! adds what only this shell needs: CREATE_NO_WINDOW (the session keeps the console subsystem
//! for its stdout contract, and a GUI parent would otherwise pop a console window),
//! `--window-pos`, the log-file tee for the child's stderr, and [`SpawnEvent`]s for the app's
//! navigation closure.

use pf_client_core::orchestrate::{
    self, CancelHandle, ConnectOutcome, ConnectPlan, HostTarget, SessionEvent,
};
use punktfunk_core::quic::v2::msg::ProfileUnlock;
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;

/// One event from the session child.
pub(crate) enum SpawnEvent {
    /// The host is asking its operator about this device; the child keeps waiting.
    AwaitingApproval,
    /// The child presented its first frame (its window is up and streaming).
    Ready,
    /// One stats window for the session status page.
    Stats(Box<punktfunk_core::hud::StatsSnapshot>),
    /// The child exited (stdout EOF + reap; a kill lands here too), classified.
    Exited(ConnectOutcome),
}

/// Where a banner sends the user for a cause: the log's real location, else its file name.
pub(crate) fn log_hint() -> String {
    crate::logfile::path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "client.log".into())
}

/// Spawn the session binary for a connect to `host`, its `fp_hex` the pin it dials, and feed
/// its lifecycle to `on_event` from a reader thread. `slot` is the handle Disconnect/Cancel kill. `launch`
/// carries a library title id for the host to launch during the handshake; `preset` is a
/// ONE-OFF settings-preset pick. `profile` is the shell's answer for who plays: it replaces
/// the saved pick the plan read, and `None` names none. `unlock` goes to the child's stdin and
/// is wiped before this returns. `Err` = the spawn itself failed (binary missing?) —
/// surfaced as a connect error by the caller.
///
/// The argv and the `--resolved-spec` come from [`orchestrate::session_command`], so this
/// shell's sessions run from the same effective (preset-aware) settings as every other one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_session(
    host: HostTarget,
    connect_timeout_secs: u64,
    launch: Option<&str>,
    preset: Option<&str>,
    profile: Option<&str>,
    unlock: Option<ProfileUnlock>,
    slot: CancelHandle,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    let label = format!("{}:{}", host.addr, host.port);
    let mut plan =
        ConnectPlan::for_target(host, launch.map(str::to_string), preset.map(str::to_string));
    plan.play_as(profile.map(str::to_string));
    plan.connect_timeout_secs = Some(connect_timeout_secs);
    // This shell prompts for a locked profile's password and spawns again with it.
    plan.prompts_unlock = true;
    let (cmd, spec_path) = orchestrate::session_command(&plan);
    spawn(cmd, spec_path, &label, slot, unlock, on_event)
}

/// Spawn the session binary in `--browse` mode: the console home, in the session window —
/// launches run as streams in that same window. The same stdout contract as a connect:
/// `ready` when the console window presents, `error` on a failed start, EOF on quit.
/// `until_no_pads` quits it once the last controller is gone.
pub(crate) fn spawn_browse(
    fullscreen: bool,
    until_no_pads: bool,
    slot: CancelHandle,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    let cmd = orchestrate::browse_command(fullscreen, until_no_pads);
    spawn(cmd, None, "console", slot, None, on_event)
        .map_err(|_| pf_l10n::client::dial_console_not_started().to_string())
}

/// Hand the shell window's position to the child (`--window-pos`) so the session window
/// opens on the same monitor, where the shell is — the hide/restore handoff then reads as
/// one window changing content instead of a window jumping displays.
fn add_window_pos(cmd: &mut Command) {
    if let Some((x, y)) = crate::shell_window::position() {
        cmd.arg("--window-pos").arg(format!("{x},{y}"));
    }
}

/// [`orchestrate::spawn_child_with_unlock`] with this shell's window flags and log tee, folding
/// the contract's `error`/`ended` lines and our own kill into [`SpawnEvent::Exited`].
fn spawn(
    mut cmd: Command,
    spec_path: Option<PathBuf>,
    label: &str,
    slot: CancelHandle,
    unlock: Option<ProfileUnlock>,
    mut on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    add_window_pos(&mut cmd);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let (mut error, mut ended) = (None::<orchestrate::SessionError>, None::<String>);
    let cancel = slot.clone();
    let tee = crate::logfile::Tee;
    orchestrate::spawn_child_with_unlock(cmd, spec_path, Some(slot), tee, unlock, move |ev| {
        match ev {
            SessionEvent::AwaitingApproval => on_event(SpawnEvent::AwaitingApproval),
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
        pf_l10n::client::dial_session_not_started().to_string()
    })?;
    tracing::info!(host = %label, "session binary spawned");
    Ok(())
}
