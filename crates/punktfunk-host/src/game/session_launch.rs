//! The launch sequence both planes run around a session's display
//! (`design/session-game-lifetime.md`): [`prepare`] claims and preps before the display
//! opens, [`spawn`] starts the title once capture is live, [`lease`] ties the game to the
//! session. Each plane supplies its own prep steps and env, and native adds the lease's
//! window, scope and outcome.

use crate::events::{Plane, PresetRef};
use crate::library::LaunchTarget;

/// Who launches: the lease, the launch hold and the game events name this client.
pub(crate) struct LaunchOwner {
    /// Stats and lease label: native's fingerprint prefix or peer IP, GameStream's peer IP.
    pub client: String,
    /// Hex fingerprint; `None` for an anonymous client, which nothing can reclaim.
    pub fingerprint: Option<String>,
    pub plane: Plane,
    pub preset: Option<PresetRef>,
}

/// A claimed, prepped launch.
pub(crate) struct Prepared {
    /// This session's launch record. `None` without a target.
    pub claim: Option<crate::launchreg::Claim>,
    /// The stamp the lease adopts against: fresh for a spawn, the original launch's for an
    /// adoption, or procscan refuses the running game.
    pub stamp: Option<f64>,
    /// Undoes the prep steps in reverse on drop, panic-unwind included.
    pub prep: Option<crate::hooks::PrepGuard>,
    /// This session won't start its title: its files never arrived. The caller drops the
    /// target and tells the player this sentence; empty when the session left first.
    pub declined: Option<String>,
    /// The `launching` row a download wait published. Held for the session; [`games`] hides
    /// it once the session lists the game itself.
    ///
    /// [`games`]: crate::session_status::games
    pub waiting: Option<crate::session_status::WaitingRow>,
}

/// Before the display opens, so a nested gamescope that starts the game with it already
/// knows whether this session spawns: reprieve this client's copy left from a dropped
/// session, claim and name the launch record, wait for a title that isn't installed to
/// download, run the prep steps (HDR toggle, sink switch), then hold `game.launching` when
/// this session will spawn. `gone` reports the session ending during the wait. Blocking:
/// prep and holds run operator code, and a download takes as long as it takes.
pub(crate) fn prepare(
    target: Option<&LaunchTarget>,
    owner: &LaunchOwner,
    prep: &[crate::hooks::PrepCmd],
    prep_env: &[(String, String)],
    gone: &dyn Fn() -> bool,
) -> Prepared {
    // Before prep and spawn: a later stamp would reject the process it is meant to find.
    let fresh_stamp = crate::gamelease::launch_clock();
    let fp = owner.fingerprint.as_deref();
    let claim = target.map(|t| {
        crate::gamelease::readopt(fp, t.game.id.as_deref());
        let claim = crate::launchreg::claim(fp, t.game.id.as_deref(), t.launcher, fresh_stamp);
        claim.describe(&t.game, owner.plane);
        claim
    });
    let stamp = claim.as_ref().map_or(fresh_stamp, |c| c.stamp());
    // Before prep: an HDR toggle must not stay applied through a download.
    let mut waiting = None;
    if let Some(t) = target.filter(|_| claim.as_ref().is_some_and(|c| c.must_spawn())) {
        match await_files(t, owner, gone) {
            Ok(row) => waiting = row,
            Err(sentence) => {
                // A retry must start the title, not adopt a launch that never happened.
                if let Some(c) = &claim {
                    c.abandon();
                }
                return Prepared {
                    claim: None,
                    stamp,
                    prep: None,
                    declined: Some(sentence),
                    waiting: None,
                };
            }
        }
    }
    let prep = (!prep.is_empty()).then(|| crate::hooks::run_prep(prep, prep_env));
    if let Some(t) = target.filter(|_| claim.as_ref().is_some_and(|c| c.must_spawn())) {
        crate::holds::launching(crate::events::GameRefPayload {
            app: t.game.id.clone(),
            title: t.game.title.clone(),
            store: t.game.store.clone(),
            client: owner.client.clone(),
            fingerprint: owner.fingerprint.clone(),
            plane: owner.plane,
            preset: owner.preset.clone(),
        });
    }
    Prepared {
        claim,
        stamp,
        prep,
        declined: None,
        waiting,
    }
}

/// A title that isn't installed downloads now, through the plugin that lists it. `Ok` once
/// its files are there, with the `launching` row clients saw meanwhile; `Ok(None)` when
/// nothing needed fetching. `Err` is the player's sentence, empty when the session left.
fn await_files(
    t: &LaunchTarget,
    owner: &LaunchOwner,
    gone: &dyn Fn() -> bool,
) -> Result<Option<crate::session_status::WaitingRow>, String> {
    use crate::library::downloads::{self, Action, Refusal, Waited};
    let Some(id) = t.game.id.as_deref() else {
        return Ok(None);
    };
    let Some(entry) = crate::library::entry_for_library_id(id) else {
        return Ok(None);
    };
    let missing = entry.install.as_ref().is_some_and(|i| i.missing());
    let (Some(provider), Some(external)) =
        (entry.provider.as_deref(), entry.external_id.as_deref())
    else {
        return Ok(None);
    };
    if !missing && !downloads::pending(id) {
        return Ok(None);
    }
    let title = &t.game.title;
    if let Err(r) = downloads::call(provider, id, external, Action::Start) {
        tracing::warn!(title = %title, refusal = ?r, "the title's download did not start");
        return Err(match r {
            Refusal::Said(why) => format!("Couldn't download {title} — {why}"),
            _ => format!("Couldn't download {title} — the plugin that lists it didn't answer."),
        });
    }
    downloads::begin(id, title, provider, external, Some(owner.client.clone()));
    let row = crate::session_status::waiting_launch(crate::session_status::GameSnapshot {
        session_id: None,
        client: owner.client.clone(),
        app_id: Some(id.to_string()),
        title: title.clone(),
        store: t.game.store.clone(),
        plane: owner.plane,
        state: "launching",
        awaiting_window: false,
        grace_remaining_s: None,
        launched_by: owner.fingerprint.clone(),
    });
    tracing::info!(title = %title, "the launch waits for the title to download");
    match downloads::wait(id, gone) {
        Waited::Done => Ok(Some(row)),
        w => {
            tracing::info!(title = %title, outcome = ?w, "the launch ends without its title");
            Err(w.sentence(title).unwrap_or_default())
        }
    }
}

/// Where a Linux launch lands.
#[cfg(target_os = "linux")]
pub(crate) struct SpawnAt<'a> {
    pub compositor: crate::vdisplay::Compositor,
    /// This acquire spawned gamescope with the launch as its primary child. A keep-alive
    /// reuse spawned nothing, so its launch goes into the live session instead.
    pub nested_spawn: bool,
    /// This session's compositor seat; `None` is the box's own.
    pub seat: Option<&'a str>,
    /// The seat's Steam home, so a forwarded `steam://` reaches the Steam that reuse kept.
    pub steam_home: Option<&'a std::path::Path>,
}

/// What [`spawn`] started.
#[derive(Default)]
pub(crate) struct Spawned {
    /// This session started the title (nested counts); `false` for an adoption or a failure.
    pub now: bool,
    /// Windows pid for the lease. `None` when nothing spawned, or the spawn only forwards.
    pub pid: Option<u32>,
    /// The launch child and whether it leads its process group: the liveness signal and the
    /// termination ladder's handle.
    #[cfg(target_os = "linux")]
    pub child: Option<(std::process::Child, bool)>,
    /// Workspace this launch owns on the streamed head; the lease releases it.
    #[cfg(target_os = "linux")]
    pub workspace: Option<crate::vdisplay::WorkspaceClaim>,
}

/// Once capture is live: end this client's other games, start the title unless the claim
/// adopted a running copy, then settle the claim. Once per launch; a rebuild never re-spawns.
pub(crate) fn spawn(
    target: Option<&LaunchTarget>,
    owner: &LaunchOwner,
    claim: Option<&crate::launchreg::Claim>,
    #[cfg(target_os = "linux")] at: SpawnAt<'_>,
) -> Spawned {
    #[allow(unused_mut)]
    let mut out = Spawned::default();
    let Some(t) = target else {
        return out;
    };
    let adopt = claim.is_some_and(|c| !c.must_spawn());
    if adopt {
        tracing::info!(
            title = %t.game.title,
            plane = owner.plane.as_str(),
            "this client's copy of this title is already running from an earlier session — not \
             starting a second one"
        );
    } else {
        crate::gamelease::end_others_for_new_launch(
            owner.fingerprint.as_deref(),
            t.game.id.as_deref(),
        );
    }
    // Windows has no nest: a library title launches by id, an `apps.json` command as itself.
    #[cfg(target_os = "windows")]
    if !adopt {
        let launched = match (t.game.id.as_deref(), t.command.as_deref()) {
            (Some(id), _) => crate::library::launch_title(id).map(Some),
            (None, Some(cmd)) => crate::library::launch_gamestream_command(cmd).map(Some),
            (None, None) => Ok(None),
        };
        match launched {
            Ok(l) => {
                out.pid = l.and_then(|l| l.tracked_pid());
                out.now = true;
            }
            Err(e) => tracing::warn!(
                title = %t.game.title,
                plane = owner.plane.as_str(),
                error = %format!("{e:#}"),
                "requested title not launched"
            ),
        }
    }
    #[cfg(target_os = "linux")]
    match t.command.as_deref() {
        // The claim belongs to the launch, not to us: go back to the game's workspace rather
        // than opening an empty one beside it.
        Some(_) if adopt => {
            out.workspace = claim
                .and_then(|c| c.workspace())
                .and_then(|ws| crate::library::adopt_launch_workspace(at.compositor, ws));
        }
        Some(cmd) if at.nested_spawn => {
            tracing::info!(command = %cmd, "launch nested into the per-session gamescope");
            out.now = true;
        }
        Some(cmd) => match crate::library::launch_session_command(
            at.compositor,
            cmd,
            at.seat,
            t.own_workspace,
            at.steam_home,
        ) {
            Ok(mut spawned) => {
                out.now = true;
                out.workspace = spawned.workspace.take();
                if spawned.steam_forwarder {
                    // Reaped here, never leased: it may be the Steam client itself.
                    let mut child = spawned.child;
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                } else {
                    out.child = Some((spawned.child, spawned.group_leader));
                }
            }
            Err(e) => tracing::warn!(
                command = %cmd,
                plane = owner.plane.as_str(),
                error = %format!("{e:#}"),
                "requested title not launched into the session"
            ),
        },
        None => {}
    }
    if let Some(c) = claim {
        if out.now {
            c.launched();
            if let Some(id) = c.credits() {
                crate::library::record_launch(id);
            }
        } else if c.must_spawn() {
            c.abandon();
        }
        // On the record, not on the session: the next reconnect focuses it.
        #[cfg(target_os = "linux")]
        if let Some(ws) = out.workspace.as_ref() {
            c.placed(ws.id());
        }
    }
    out
}

/// Lease inputs only a plane with a per-session head and a control channel has.
#[derive(Default)]
pub(crate) struct LeaseExtras {
    /// A bare-spawn gamescope owns the game.
    pub nested: bool,
    pub scope_pid: Option<u32>,
    pub window: Option<crate::gamelease::WindowSource>,
    pub outcome: Option<crate::gamelease::OutcomeTx>,
}

/// Open the game lease for what [`spawn`] started, adopting against [`Prepared::stamp`].
/// `on_exit` fires once the game was seen running and is gone.
pub(crate) fn lease(
    target: &LaunchTarget,
    owner: &LaunchOwner,
    stamp: Option<f64>,
    claim: Option<&crate::launchreg::Claim>,
    spawned: Spawned,
    extras: LeaseExtras,
    on_exit: crate::gamelease::OnExit,
) -> crate::gamelease::GameLease {
    #[cfg(target_os = "linux")]
    let child = spawned.child;
    #[cfg(not(target_os = "linux"))]
    let child = None;
    crate::gamelease::open(
        crate::gamelease::LeaseRequest {
            game: target.game.clone(),
            client: owner.client.clone(),
            fingerprint: owner.fingerprint.clone(),
            preset: owner.preset.clone(),
            plane: owner.plane,
            spec: target.detect.clone(),
            nested: extras.nested,
            scope_pid: extras.scope_pid,
            launcher: target.launcher,
            child,
            spawned: spawned.pid,
            launch_stamp: stamp,
            // An adoption keeps the original launch's slot across the handover.
            procs: claim.and_then(|c| c.procs()),
            #[cfg(target_os = "linux")]
            workspace: spawned.workspace,
            window: extras.window,
            outcome: extras.outcome,
        },
        on_exit,
    )
}

/// `end`, unless the operator turned off ending the session when the game exits. Read at
/// fire time so a mid-session flip takes effect; the lease keeps running either way.
pub(crate) fn end_on_game_exit<F>(end: F) -> impl Fn() + Clone + Send + Sync + 'static
where
    F: Fn() + Clone + Send + Sync + 'static,
{
    move || {
        if !crate::session_settings::get().session_on_game_exit {
            tracing::info!(
                "the launched game exited, but ending the session on game exit is off — \
                 leaving the stream up"
            );
            return;
        }
        end();
    }
}
