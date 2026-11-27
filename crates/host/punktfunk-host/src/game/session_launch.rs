//! The launch sequence every plane runs (`design/session-game-lifetime.md`): [`prepare`]
//! claims and preps before a session's display opens, [`spawn`] starts the title once capture
//! is live, [`lease`] ties the game to its owner. Each plane supplies its own prep steps and
//! env, and native adds the lease's window, scope and outcome. The desk runs the same three
//! with no session ([`crate::desk`]).

use crate::events::{Plane, PresetRef};
use crate::library::LaunchTarget;

/// Who launches: the lease, the launch hold and the game events name this client.
pub(crate) struct LaunchOwner {
    /// Stats and lease label: native's fingerprint prefix or peer IP, GameStream's peer IP,
    /// the desk's key.
    pub client: String,
    /// The launch record's key: a hex fingerprint, or the desk's key. `None` for an anonymous
    /// client, which nothing can reclaim.
    pub fingerprint: Option<String>,
    pub plane: Plane,
    pub preset: Option<PresetRef>,
    /// The session's profile, credited in play stats beside the title and named on the game
    /// events. `None` on GameStream.
    pub profile: Option<crate::events::ProfileRef>,
    /// The pads an emulator's players are bound to.
    pub pads: Pads,
    /// The stream's picture, which an emulator's render scale fills. `None` at the desk, whose
    /// screen the player set the emulator up for.
    pub screen: Option<hermir::Size>,
}

impl LaunchOwner {
    /// The device on the game events and the lease: `None` for the desk, whose key names none.
    fn device(&self) -> Option<String> {
        self.fingerprint
            .clone()
            .filter(|_| self.plane != Plane::Local)
    }
}

/// Whose pads an emulator's players are.
pub(crate) enum Pads {
    /// A stream's: the kind an undeclared pad starts as, the pads its input thread knows
    /// ([`crate::pad_feed::PadFeed::roster`]), and the device's fingerprint, which keys the pads
    /// it brought last time. `roster` is `None` on GameStream, which declares no pads.
    Session {
        kind: punktfunk_core::config::GamepadPref,
        roster: Option<std::sync::Arc<crate::pad_feed::PadFeed>>,
        device: Option<String>,
    },
    /// The pads at the desk, in seat order, each named as SDL names it. Empty binds no players.
    Desk(Vec<hermir::PadRef>),
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
    /// target and sends the player this refusal; its message is empty when the session left
    /// first.
    pub declined: Option<punktfunk_core::quic::LaunchOutcome>,
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
/// this session will spawn, a seat's saves crossing to the owner's plugins and back around it,
/// and prepare an emulator for `owner`'s pads. `gone` reports the session ending during the
/// wait. Blocking: prep and holds run operator code, and a download takes as long as it takes.
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
    let mut claim = target.map(|t| {
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
            Err(refusal) => {
                // A retry must start the title, not adopt a launch that never happened.
                if let Some(c) = &claim {
                    c.abandon();
                }
                return Prepared {
                    claim: None,
                    stamp,
                    prep: None,
                    declined: Some(refusal),
                    waiting: None,
                };
            }
        }
    }
    let prep = (!prep.is_empty()).then(|| crate::hooks::run_prep(prep, prep_env));
    if let Some(t) = target.filter(|_| claim.as_ref().is_some_and(|c| c.must_spawn())) {
        crate::emulators::exchange::before_hold(t.game.id.as_deref());
        crate::holds::launching(crate::events::GameRefPayload {
            app: t.game.id.clone(),
            title: t.game.title.clone(),
            store: t.game.store.clone(),
            client: owner.client.clone(),
            fingerprint: owner.device(),
            plane: owner.plane,
            preset: owner.preset.clone(),
            profile: owner.profile.clone(),
        });
        crate::emulators::exchange::after_hold(t.game.id.as_deref());
        // After the holds: they stage the firmware this places.
        if let Some(id) = t.game.id.as_deref() {
            let held = crate::emulators::prepare_launch(id, &owner.pads, owner.screen);
            if let Some(c) = claim.as_mut() {
                c.bind(held);
            }
        }
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
/// nothing needed fetching. `Err` is the player's refusal, without words when the session
/// left.
fn await_files(
    t: &LaunchTarget,
    owner: &LaunchOwner,
    gone: &dyn Fn() -> bool,
) -> Result<Option<crate::session_status::WaitingRow>, punktfunk_core::quic::LaunchOutcome> {
    use crate::library::downloads::{self, Action, Refusal, Waited};
    use punktfunk_core::quic::{LaunchOutcome, LaunchOutcomeKind};
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
        let refused = |text: String| LaunchOutcome::new(LaunchOutcomeKind::Refused, &text);
        return Err(match r {
            Refusal::Said(why) => refused(format!("Couldn't download {title} — {why}")).worded(
                "launch_download_refused",
                &[("title", title), ("reason", &why)],
            ),
            _ => refused(format!(
                "Couldn't download {title} — the plugin that lists it didn't answer."
            ))
            .worded("launch_download_no_answer", &[("title", title)]),
        });
    }
    downloads::begin(id, title, Some(owner.client.clone()));
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
        outcome: None,
    });
    tracing::info!(title = %title, "the launch waits for the title to download");
    match downloads::wait(id, gone) {
        Waited::Done => Ok(Some(row)),
        w => {
            tracing::info!(title = %title, outcome = ?w, "the launch ends without its title");
            Err(w
                .outcome(title)
                .unwrap_or_else(|| LaunchOutcome::new(LaunchOutcomeKind::Refused, "")))
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
    pub place: crate::library::Place,
    /// The compositor process this session spawned. The launch goes down with it. `None`
    /// where the session spawned none.
    pub compositor_pid: Option<u32>,
    /// This session shows another device's head, which stays on what that device chose.
    pub joined: bool,
}

/// What [`spawn`] started.
#[derive(Default)]
pub(crate) struct Spawned {
    /// This session started the title (nested counts); `false` for an adoption or a failure.
    pub now: bool,
    /// When the title started: stamped as the start returns, before the launch's synced
    /// write. `None` when `now` is `false`.
    pub started_at: Option<std::time::Instant>,
    /// Windows pid for the lease. `None` when nothing spawned, or the spawn only forwards.
    pub pid: Option<u32>,
    /// The launch child and whether it leads its process group: the liveness signal and the
    /// termination ladder's handle.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub child: Option<(std::process::Child, bool)>,
    /// Workspace this launch owns on the streamed head; the lease releases it.
    #[cfg(target_os = "linux")]
    pub workspace: Option<crate::vdisplay::WorkspaceClaim>,
}

/// Once capture is live: end this client's other games, start the title unless the claim
/// adopted a running copy, then settle the claim with the compositor the launch runs in. Once
/// per launch; a rebuild never re-spawns. With no title, a client whose launch still runs gets
/// that launch's workspace back, as an adoption does. Windows looks a library title up again in
/// `view`. The start's instant is taken as the start returns, ahead of the launch's synced
/// write, so the lease counts the launcher window from the start and not from its own open.
pub(crate) fn spawn(
    target: Option<&LaunchTarget>,
    owner: &LaunchOwner,
    claim: Option<&crate::launchreg::Claim>,
    #[cfg(target_os = "linux")] at: SpawnAt<'_>,
    #[cfg(windows)] view: &crate::library::View,
) -> Spawned {
    #[allow(unused_mut)]
    let mut out = Spawned::default();
    let Some(t) = target else {
        // No lease holds this claim, so nothing puts the head back at the end: the workspace
        // stays the launch's, and a per-session head goes with the session.
        #[cfg(target_os = "linux")]
        if at.place == crate::library::Place::Stream && !at.joined {
            if let Some(ws) = crate::launchreg::running_workspace(owner.fingerprint.as_deref()) {
                crate::library::adopt_launch_workspace(at.compositor, ws);
            }
        }
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
            (Some(id), _) => crate::library::launch_title(view, id).map(Some),
            (None, Some(cmd)) => crate::library::launch_gamestream_command(cmd).map(Some),
            (None, None) => Ok(None),
        };
        match launched {
            Ok(l) => {
                out.started_at = Some(std::time::Instant::now());
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
    // macOS starts the command on the logged-in desktop; `open` hands a link or a bundle over.
    #[cfg(target_os = "macos")]
    if let Some(cmd) = t.command.as_deref().filter(|_| !adopt) {
        match crate::library::launch_mac_command(cmd) {
            Ok(child) => {
                out.started_at = Some(std::time::Instant::now());
                out.now = true;
                out.child = Some((child, true));
            }
            Err(e) => tracing::warn!(
                command = %cmd,
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
            // Gamescope started the title before this call, so this instant is later than that.
            out.started_at = Some(std::time::Instant::now());
            out.now = true;
        }
        Some(cmd) => match crate::library::launch_session_command(
            at.compositor,
            cmd,
            at.seat,
            t.own_workspace,
            at.place,
        ) {
            Ok(mut spawned) => {
                out.started_at = Some(std::time::Instant::now());
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
            #[cfg(target_os = "linux")]
            if let Some(p) = at.compositor_pid.and_then(crate::procscan::resolve) {
                c.nested_in(p);
            }
            c.launched();
            if let Some(id) = c.credits() {
                crate::library::record_launch(id, owner.profile.as_ref().map(|p| p.id.as_str()));
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

/// Open the game lease for what [`spawn`] started, adopting against [`Prepared::stamp`], under
/// `owner`'s device. The lease counts the launch from [`Spawned::started_at`]. `on_exit` fires
/// once the game was seen running and is gone.
pub(crate) fn lease(
    target: &LaunchTarget,
    owner: &LaunchOwner,
    stamp: Option<f64>,
    claim: Option<&crate::launchreg::Claim>,
    spawned: Spawned,
    extras: LeaseExtras,
    on_exit: crate::gamelease::OnExit,
) -> crate::gamelease::GameLease {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let child = spawned.child;
    #[cfg(windows)]
    let child = None;
    crate::gamelease::open(
        crate::gamelease::LeaseRequest {
            game: target.game.clone(),
            client: owner.client.clone(),
            fingerprint: owner.device(),
            preset: owner.preset.clone(),
            plane: owner.plane,
            profile: owner.profile.clone(),
            spec: target.detect.clone(),
            nested: extras.nested,
            scope_pid: extras.scope_pid,
            launcher: target.launcher,
            child,
            spawned: spawned.pid,
            started_at: spawned.started_at,
            launch_stamp: stamp,
            // An adoption keeps the original launch's slot across the handover.
            procs: claim.and_then(|c| c.procs()),
            bindings: claim.and_then(|c| c.bindings()),
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
/// `box_session` is the box session the game was launched into, `None` for a game in its own
/// gamescope: a game that goes down with that session, on a Game Mode ↔ desktop switch, is not
/// the player quitting, and the stream follows the switch.
pub(crate) fn end_on_game_exit<F>(
    box_session: Option<crate::vdisplay::SessionMark>,
    end: F,
) -> impl Fn() + Clone + Send + Sync + 'static
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
        if box_session.is_some_and(crate::vdisplay::session_left) {
            tracing::info!(
                "the launched game went down with the session it ran in — the stream follows \
                 the switch"
            );
            return;
        }
        end();
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::gamelease::{GameRef, GameState, LeaseKind, EXIT_CONFIRM, SHIM_WINDOW, SLOW_DISK};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A title with nothing to recognize it by: the pid the host starts is the only signal.
    fn target(id: &str) -> LaunchTarget {
        LaunchTarget {
            game: GameRef {
                id: Some(id.into()),
                store: Some("custom".into()),
                title: format!("Test Title {id}"),
            },
            launcher: false,
            detect: crate::library::DetectSpec::default(),
            command: None,
            own_workspace: false,
            on_window: crate::library::OnWindow::default(),
        }
    }

    fn owner(fingerprint: &str) -> LaunchOwner {
        LaunchOwner {
            client: "test".into(),
            fingerprint: Some(fingerprint.into()),
            plane: Plane::Native,
            preset: None,
            profile: None,
            pads: Pads::Session {
                kind: punktfunk_core::config::GamepadPref::default(),
                roster: None,
                device: None,
            },
            screen: None,
        }
    }

    /// A reconnect adopts the running launch and spawns nothing, so its lease gets no pid
    /// of its own. It still has to watch the process the first session started: quitting the
    /// game must end the session that came back. The Windows pid-only shape, driven here.
    #[test]
    fn a_reconnect_still_ends_when_the_first_sessions_game_exits() {
        // Ending the game credits play time to the config dir: no test that snapshots a scratch
        // config dir may run meanwhile.
        let _config = crate::identity::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (fp, id) = ("fp-adopt-exit", "custom:adopt-exit");
        let (target, owner) = (target(id), owner(fp));
        let mut game = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn the fake game");

        let claim =
            crate::launchreg::claim(Some(fp), Some(id), false, crate::gamelease::launch_clock());
        assert!(claim.must_spawn(), "the first session starts the title");
        claim.launched();
        let spawned = Spawned {
            now: true,
            pid: Some(game.id()),
            ..Spawned::default()
        };
        let first = lease(
            &target,
            &owner,
            claim.stamp(),
            Some(&claim),
            spawned,
            LeaseExtras::default(),
            Box::new(|| {}),
        );
        assert!(matches!(first.shared().kind(), LeaseKind::Child));
        // Past the shim window the pid is the game, and the record learns it.
        std::thread::sleep(SHIM_WINDOW + Duration::from_millis(1_500));
        assert_eq!(first.shared().state(), GameState::Running);
        drop(first);
        drop(claim);

        let claim =
            crate::launchreg::claim(Some(fp), Some(id), false, crate::gamelease::launch_clock());
        assert!(
            !claim.must_spawn(),
            "the reconnect adopts the running launch"
        );
        assert_eq!(claim.adopted(), Some(crate::launchreg::Liveness::Running));
        let exits = Arc::new(AtomicUsize::new(0));
        let seen = exits.clone();
        let second = lease(
            &target,
            &owner,
            claim.stamp(),
            Some(&claim),
            Spawned::default(),
            LeaseExtras::default(),
            Box::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert!(
            matches!(second.shared().kind(), LeaseKind::Adopted),
            "the lease of a reconnect must watch what the first session started"
        );

        // The player quits. Reaped here: a zombie keeps `/proc/<pid>` and looks alive.
        let _ = game.kill();
        let _ = game.wait();
        let deadline = Instant::now() + EXIT_CONFIRM + Duration::from_secs(5) + SLOW_DISK;
        while Instant::now() < deadline && exits.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            exits.load(Ordering::SeqCst),
            1,
            "quitting the game after a reconnect must end the session"
        );
        assert_eq!(second.shared().state(), GameState::Exited);
        drop(second);
        // The registry is process-wide: leave it as it was.
        claim.abandon();
    }
}
