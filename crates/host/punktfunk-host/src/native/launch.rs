//! A session's library launch, settled before its display opens: the title asked for or the
//! profile's home, its command, and the claim, prep and hold `session_launch` takes.

#[cfg(target_os = "linux")]
use super::plays_seat_mode;
use super::{opens_big_picture, Admitted, Agreed, SessionScope};
use anyhow::{Context, Result};
use punktfunk_core::quic::{Hello, Welcome};
use std::sync::atomic::{AtomicBool, Ordering};

/// The session's library launch, before the display opens: the title (the one asked for, else
/// the profile's home), the command the display runs for it, then reprieve, claim, prep and
/// the launch hold. A title whose files never arrived is dropped and the client told why on
/// `outcome`; the returned [`Prepared`](crate::session_launch::Prepared) has `declined` taken.
pub(super) async fn prepare_launch(
    hello: &Hello,
    welcome: &Welcome,
    profile: &crate::profiles::Resolved,
    owner: &crate::session_launch::LaunchOwner,
    outcome: &crate::gamelease::OutcomeTx,
    stop: &AtomicBool,
) -> Result<(
    Option<crate::library::LaunchTarget>,
    Option<String>,
    crate::session_launch::Prepared,
)> {
    let target = match resolve_launch(hello.launch.as_deref(), outcome).await? {
        Some(t) => Some(t),
        None => home_launch(hello.launch.as_deref(), profile),
    };
    #[cfg(target_os = "windows")]
    let command = target.as_ref().and(hello.launch.clone());
    #[cfg(not(target_os = "windows"))]
    let command = target.as_ref().and_then(|t| t.command.clone());
    let (prep_cmds, prep_env) = launch_prep(hello, welcome, owner.preset.as_ref());
    // `block_in_place`: operator code is blocking and this is a multi-thread runtime.
    let mut prepared = tokio::task::block_in_place(|| {
        crate::session_launch::prepare(target.as_ref(), owner, &prep_cmds, &prep_env, &|| {
            stop.load(Ordering::Relaxed)
        })
    });
    // The title's files never arrived: stream without it, and say why.
    match prepared.declined.take() {
        Some(sentence) => {
            let _ = outcome.send(punktfunk_core::quic::LaunchOutcome::new(
                punktfunk_core::quic::LaunchOutcomeKind::Refused,
                &sentence,
            ));
            Ok((None, None, prepared))
        }
        None => Ok((target, command, prepared)),
    }
}

/// One library lookup: command, title, process identity. The client picks an existing id,
/// never a command. An id this host can't start is refused onto `outcome`. Clients list the
/// box's whole library, so a seat is asked for titles it never could. Blocking: plugin entries
/// ask over loopback from this async context.
async fn resolve_launch(
    launch: Option<&str>,
    outcome: &crate::gamelease::OutcomeTx,
) -> Result<Option<crate::library::LaunchTarget>> {
    let Some(id) = launch else {
        return Ok(None);
    };
    let owned = id.to_string();
    let found = tokio::task::spawn_blocking(move || crate::library::resolve_launch(&owned))
        .await
        .context("resolve the session's library launch")?;
    match &found {
        Some(t) => tracing::info!(
            launch_id = id,
            title = %t.game.title,
            command = t.command.as_deref().unwrap_or("-"),
            "resolved library launch for this session"
        ),
        None => {
            tracing::warn!(
                launch_id = id,
                "client requested a title this host can't start — ignoring"
            );
            let sentence = if pf_paths::seat::box_library_dir().is_some() {
                "Couldn't start that title — this desktop doesn't have its emulator, launcher \
                 or game files."
            } else {
                "Couldn't start that title — this host doesn't have it in its library any more."
            };
            let _ = outcome.send(punktfunk_core::quic::LaunchOutcome::new(
                punktfunk_core::quic::LaunchOutcomeKind::Refused,
                sentence,
            ));
        }
    }
    Ok(found)
}

/// What a bare connect opens: Steam's Game Mode on a seat in Game Mode, else Big Picture for a
/// seat profile whose home is `bigpicture`. A connect that named a title opens nothing more.
fn home_launch(
    asked: Option<&str>,
    profile: &crate::profiles::Resolved,
) -> Option<crate::library::LaunchTarget> {
    if asked.is_some() {
        return None;
    }
    #[cfg(target_os = "linux")]
    if opens_game_mode() {
        return crate::library::game_mode_launch();
    }
    if !opens_big_picture(profile) {
        return None;
    }
    #[cfg(not(windows))]
    return crate::library::big_picture_launch();
    #[cfg(windows)]
    None
}

/// A seat in Game Mode, connected to by a profile it serves.
#[cfg(target_os = "linux")]
fn opens_game_mode() -> bool {
    crate::seats::session_switch::in_game_mode() && plays_seat_mode()
}

/// The launched title's prep steps and their environment: `PF_APP_ID` and `PF_STREAM_*`, so a
/// step can set a per-mode FPS cap, and `PF_PRESET_*`, so it can tell a docked session from a
/// handheld one. Empty without a launch.
fn launch_prep(
    hello: &Hello,
    welcome: &Welcome,
    preset: Option<&crate::events::PresetRef>,
) -> (Vec<crate::hooks::PrepCmd>, Vec<(String, String)>) {
    let Some(id) = hello.launch.as_deref() else {
        return (Vec::new(), Vec::new());
    };
    let mut env = vec![("PF_APP_ID".to_string(), id.to_string())];
    if let Some(p) = preset {
        env.push(("PF_PRESET_ID".to_string(), p.id.clone()));
        env.push(("PF_PRESET_NAME".to_string(), p.name.clone()));
    }
    env.extend(crate::hooks::prep_mode_env(
        hello.mode.width,
        hello.mode.height,
        hello.mode.refresh_hz,
        welcome.color.is_hdr(),
    ));
    (crate::library::prep_for(id), env)
}

/// What the launch settled, for the stream: the title, its command, the launch record and
/// the owner it is registered under.
pub(super) struct Launch {
    pub(super) target: Option<crate::library::LaunchTarget>,
    pub(super) command: Option<String>,
    pub(super) claim: Option<crate::launchreg::Claim>,
    pub(super) stamp: Option<f64>,
    pub(super) owner: crate::session_launch::LaunchOwner,
    /// Registry and stats label; an anonymous `--open` client is labelled by its address.
    pub(super) client_label: String,
}

/// The library launch, before the display opens. Its prep guard, waiting row and audio
/// policy are held to the session's end.
pub(super) async fn launch_session(
    scope: &mut SessionScope,
    who: &Admitted,
    agreed: &Agreed,
    controls: &crate::session_status::SessionControls,
    outcome: &crate::gamelease::OutcomeTx,
) -> Result<Launch> {
    let (conn, n) = (scope.conn().clone(), &agreed.n);
    let client_label = super::stream::session_client_label(
        conn.peer_fingerprint().map(hex::encode).as_deref(),
        Some(conn.remote_address().ip()),
    );
    let owner = crate::session_launch::LaunchOwner {
        client: client_label.clone(),
        fingerprint: conn.peer_fingerprint().map(hex::encode),
        plane: conn.plane(),
        preset: n.preset.clone(),
        profile: Some(who.profile_ref.clone()),
        pad: n.welcome.gamepad,
        pad_slots: Some(controls.pad_slots.clone()),
    };
    #[cfg(test)]
    super::tests::fail_at_launch(&conn, scope.life())?;
    let (
        target,
        command,
        crate::session_launch::Prepared {
            claim,
            stamp,
            prep,
            waiting,
            ..
        },
    ) = prepare_launch(
        &n.hello,
        &n.welcome,
        &n.resolved,
        &owner,
        outcome,
        scope.life().stop_flag(),
    )
    .await?;
    scope.hold(prep);
    scope.hold(waiting);
    // The title's `audio.sessions`, over every session on this display. Lifted with the session.
    scope.hold(
        n.hello
            .launch
            .as_deref()
            .and_then(crate::library::audio_sessions_for)
            .map(|policy| crate::session_status::apply_audio_policy(policy, &client_label)),
    );
    Ok(Launch {
        target,
        command,
        claim,
        stamp,
        owner,
        client_label,
    })
}
