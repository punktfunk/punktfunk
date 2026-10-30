//! The Linux pool driven through a [`FakeDisplay`].
//!
//! The pool, its linger timer and the policy store are process globals, and the linger
//! comes from the environment. Every test therefore runs its body in a child of this test
//! binary ([`in_child`]): an unconfigured host with an empty scratch home.
//!
//! A test asserts the backend calls in order, read off the fake's log, and the pooled
//! display's `lifecycle::State`.

use super::*;
use crate::fake::{Call, FakeDisplay};
use crate::topology_journal::{Head, Undo};
use crate::{registry, DisplayOwnership};

const MODE: Mode = Mode {
    width: 1920,
    height: 1080,
    refresh_hz: 60,
};
const SMALL: Mode = Mode {
    width: 1280,
    height: 720,
    refresh_hz: 60,
};
const QHD: Mode = Mode {
    width: 2560,
    height: 1440,
    refresh_hz: 60,
};
const UHD: Mode = Mode {
    width: 3840,
    height: 2160,
    refresh_hz: 60,
};

/// For a child whose released displays must stay kept for the length of the test.
const KEPT: &[(&str, &str)] = &[("PUNKTFUNK_MONITOR_LINGER_MS", "60000")];

/// [`crate::fake::in_child`] for this module.
fn in_child(test: &str, env: &[(&str, &str)], body: fn()) {
    crate::fake::in_child(module_path!(), test, env, body)
}

fn acquire(fake: &FakeDisplay, mode: Mode, quit_now: bool) -> VirtualOutput {
    let quit = Arc::new(AtomicBool::new(quit_now));
    registry::acquire(&mut fake.boxed(), mode, quit, None).expect("acquire")
}

/// Backend and lifecycle of every pooled display, in pool order.
fn pool() -> Vec<(&'static str, lifecycle::State)> {
    let es = reg().entries.lock().unwrap();
    es.iter().map(|e| (e.backend, e.life)).collect()
}

/// Mode of every pooled display, in pool order.
fn modes() -> Vec<Mode> {
    reg()
        .entries
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.mode)
        .collect()
}

/// A `kwin` fake serving device `n`, reporting the identity slot that device resolves to.
fn owned_by(n: u8) -> FakeDisplay {
    let fake = FakeDisplay::new("kwin");
    let slot = crate::identity::resolve_slot(
        Some([n; 32]),
        (MODE.width, MODE.height),
        crate::policy::Identity::PerClient,
    );
    assert!(slot.is_some(), "device {n} has a slot");
    fake.script(|s| {
        s.identity_slot = slot;
        s.client_fp = Some([n; 32]);
    });
    fake
}

/// Put the registry's display events in `fake`'s log, in order with its calls.
fn log_events(fake: &FakeDisplay) {
    let fake = fake.clone();
    let sink = move |ev| match ev {
        crate::DisplayEvent::Created { .. } => fake.log(note("created")),
        crate::DisplayEvent::Released { count } => fake.log(note(&format!("released {count}"))),
    };
    let set = crate::DISPLAY_EVENT_SINK.set(Box::new(sink));
    assert!(set.is_ok(), "one sink per child");
}

fn note(what: &str) -> Call {
    Call::Note(what.into())
}

/// A gamescope bare spawn: a compositor of its own.
fn spawning(name: &'static str) -> FakeDisplay {
    let fake = FakeDisplay::new(name);
    fake.script(|s| s.ownership = DisplayOwnership::Spawned);
    fake
}

/// A backend whose every create prepares a topology restore.
fn restoring(name: &'static str) -> FakeDisplay {
    let fake = FakeDisplay::new(name);
    fake.script(|s| s.restore = true);
    fake
}

/// What the registry asks of a backend around a create it then pools.
fn created(first_in_group: bool, mode: Mode) -> Vec<Call> {
    vec![
        Call::SetFirstInGroup(first_in_group),
        Call::SetSessionCastHandoff(true),
        Call::Create(mode),
        Call::SetSessionCastHandoff(false),
        Call::TakeTopologyRestore,
    ]
}

/// Wait until `last` ends `fake`'s log: the linger timer tears down on its own thread.
fn wait_for(fake: &FakeDisplay, last: &Call) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.calls().last() != Some(last) {
        assert!(
            Instant::now() < deadline,
            "no {last:?} in {:?}",
            fake.calls()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_first_acquire_creates_then_pools_the_display() {
    let name = "a_first_acquire_creates_then_pools_the_display";
    in_child(name, &[], || {
        let fake = FakeDisplay::new("kwin");
        log_events(&fake);
        let out = acquire(&fake, MODE, false);
        let mut want = created(true, MODE);
        want.push(note("created"));
        assert_eq!(fake.take_calls(), want);
        assert_eq!((out.node_id, out.reused_gen), (1, None));
        assert!(out.pool_gen.is_some() && out.ownership == DisplayOwnership::Owned);
        assert_eq!(pool(), [("kwin", lifecycle::State::Active { refs: 1 })]);
    });
}

/// keep_alive is off on an unconfigured host, so the last lease tears the display
/// down. The restore re-lights the desk before the output goes.
#[test]
fn the_last_release_restores_the_desk_before_the_output_drops() {
    let name = "the_last_release_restores_the_desk_before_the_output_drops";
    in_child(name, &[], || {
        let fake = restoring("kwin");
        log_events(&fake);
        let out = acquire(&fake, MODE, false);
        fake.take_calls();
        drop(out);
        assert_eq!(
            fake.take_calls(),
            [
                Call::RestoreRan(1),
                Call::KeepaliveDropped(1),
                note("released 1")
            ]
        );
        assert!(pool().is_empty());
    });
}

/// Desktop displays of one backend are one group. A later member is told it is not
/// first and is placed beside the first. The group's restore moves to whichever
/// member outlives the other, and runs when that one leaves.
#[test]
fn a_group_hands_its_restore_to_the_member_that_outlives_it() {
    let name = "a_group_hands_its_restore_to_the_member_that_outlives_it";
    in_child(name, &[], || {
        let fake = restoring("kwin");
        let first = acquire(&fake, MODE, false);
        fake.take_calls();
        // A later exclusive member finds the heads already off.
        fake.script(|s| s.restore = false);
        let second = acquire(&fake, MODE, false);
        let mut want = created(false, MODE);
        want.push(Call::ApplyPosition(1920, 0));
        assert_eq!(fake.take_calls(), want);

        drop(first);
        assert_eq!(fake.take_calls(), [Call::KeepaliveDropped(1)]);
        assert_eq!(pool(), [("kwin", lifecycle::State::Active { refs: 1 })]);
        drop(second);
        assert_eq!(
            fake.take_calls(),
            [Call::RestoreRan(1), Call::KeepaliveDropped(2)]
        );
    });
}

/// A create whose session cast then fails is still a display the pool took and tore down:
/// its `Created` pairs with the `Released` the teardown reports.
#[test]
fn a_failed_first_cast_pairs_created_with_released() {
    let name = "a_failed_first_cast_pairs_created_with_released";
    in_child(name, &[], || {
        let fake = FakeDisplay::new("hyprland");
        fake.script(|s| {
            s.output_name = Some("PF-1");
            s.cast_fails = true;
        });
        log_events(&fake);
        let quit = Arc::new(AtomicBool::new(false));
        assert!(registry::acquire(&mut fake.boxed(), MODE, quit, None).is_err());
        let notes: Vec<Call> = fake
            .take_calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Note(_)))
            .collect();
        assert_eq!(notes, [note("created"), note("released 1")]);
        assert!(pool().is_empty());
    });
}

/// A manual pin at the origin on a later member is a move the compositor would not make on
/// its own; only the first member at (0, 0) is the compositor's default.
#[test]
fn a_later_member_pinned_at_the_origin_is_positioned() {
    let name = "a_later_member_pinned_at_the_origin_is_positioned";
    in_child(name, &[], || {
        std::fs::write(
            pf_paths::config_dir().join("display-settings.json"),
            br#"{"layout":{"mode":"manual","positions":{"1":{"x":1920,"y":0},"2":{"x":0,"y":0}}}}"#,
        )
        .unwrap();
        let fake = FakeDisplay::new("kwin");
        fake.script(|s| s.identity_slot = Some(1));
        let _first = acquire(&fake, MODE, false);
        let mut want = created(true, MODE);
        want.push(Call::ApplyPosition(1920, 0));
        assert_eq!(fake.take_calls(), want);
        fake.script(|s| s.identity_slot = Some(2));
        let _second = acquire(&fake, MODE, false);
        let mut want = created(false, MODE);
        want.push(Call::ApplyPosition(0, 0));
        assert_eq!(fake.take_calls(), want);
    });
}

/// Two members that each disabled heads: both undos run, newest first, once the group is
/// empty, and before the last output goes.
#[test]
fn a_group_runs_every_undo_its_members_journaled() {
    let name = "a_group_runs_every_undo_its_members_journaled";
    in_child(name, &[], || {
        let heads = |name: &str| Undo::EnableHeads {
            heads: vec![Head {
                name: name.into(),
                mode: None,
            }],
        };
        let fake = FakeDisplay::new("kwin");
        fake.serve_by_name();
        fake.script(|s| s.undo = Some(heads("DP-1")));
        let first = acquire(&fake, MODE, false);
        // A head was lit again in between, so this create disables one too.
        fake.script(|s| s.undo = Some(heads("HDMI-A-1")));
        let second = acquire(&fake, MODE, false);
        fake.take_calls();

        drop(second);
        assert_eq!(fake.take_calls(), [Call::KeepaliveDropped(2)]);
        drop(first);
        assert_eq!(
            fake.take_calls(),
            [
                Call::Undo(heads("HDMI-A-1")),
                Call::Undo(heads("DP-1")),
                Call::KeepaliveDropped(1)
            ]
        );
    });
}

/// A failed create takes the cast hand-off back, takes no restore and files nothing:
/// the live member still carries the group's restore.
#[test]
fn a_create_that_fails_mid_group_leaves_the_group_as_it_was() {
    let name = "a_create_that_fails_mid_group_leaves_the_group_as_it_was";
    in_child(name, &[], || {
        let fake = restoring("kwin");
        log_events(&fake);
        let first = acquire(&fake, MODE, false);
        fake.take_calls();

        fake.script(|s| s.fail_create = Some("the compositor refused the output"));
        let quit = Arc::new(AtomicBool::new(false));
        let err = registry::acquire(&mut fake.boxed(), MODE, quit, None)
            .err()
            .expect("the create fails");
        assert_eq!(err.to_string(), "the compositor refused the output");
        let mut want = created(false, MODE);
        want.pop();
        assert_eq!(fake.take_calls(), want);
        assert_eq!(pool(), [("kwin", lifecycle::State::Active { refs: 1 })]);

        drop(first);
        assert_eq!(
            fake.take_calls(),
            [
                Call::RestoreRan(1),
                Call::KeepaliveDropped(1),
                note("released 1")
            ]
        );
    });
}

/// A reconnect inside the linger window gets the kept node back under a new lease:
/// no create, and no `Created`.
#[test]
fn a_kept_display_is_reused_without_a_create() {
    in_child("a_kept_display_is_reused_without_a_create", KEPT, || {
        let fake = FakeDisplay::new("kwin");
        log_events(&fake);
        let first = acquire(&fake, MODE, false);
        let first_gen = first.pool_gen;
        drop(first);
        assert!(matches!(
            pool().as_slice(),
            [("kwin", lifecycle::State::Lingering { .. })]
        ));
        fake.take_calls();

        let again = acquire(&fake, MODE, false);
        assert_eq!(fake.take_calls(), [Call::KeptDisplayAlive(1)]);
        assert_eq!(again.node_id, 1);
        assert!(again.reused_gen.is_some() && again.reused_gen == again.pool_gen);
        assert_ne!(again.pool_gen, first_gen, "a reuse re-stamps the lease");
        assert_eq!(pool(), [("kwin", lifecycle::State::Active { refs: 1 })]);
    });
}

/// A kept display whose compositor is gone is torn down, restore first, and the
/// acquire creates.
#[test]
fn a_dead_kept_display_is_torn_down_before_the_create() {
    let name = "a_dead_kept_display_is_torn_down_before_the_create";
    in_child(name, KEPT, || {
        let fake = restoring("kwin");
        log_events(&fake);
        drop(acquire(&fake, MODE, false));
        fake.take_calls();

        fake.script(|s| s.alive = false);
        let out = acquire(&fake, MODE, false);
        let mut want = vec![
            Call::KeptDisplayAlive(1),
            Call::RestoreRan(1),
            Call::KeepaliveDropped(1),
            note("released 1"),
        ];
        want.extend(created(true, MODE));
        want.push(note("created"));
        assert_eq!(fake.take_calls(), want);
        assert_eq!((out.node_id, out.reused_gen), (2, None));
    });
}

/// The linger timer ends a kept display nobody came back for, restore first, and
/// never inside its window.
#[test]
fn a_linger_window_ends_in_a_teardown() {
    let window = &[("PUNKTFUNK_MONITOR_LINGER_MS", "20")];
    in_child("a_linger_window_ends_in_a_teardown", window, || {
        let fake = restoring("kwin");
        log_events(&fake);
        let out = acquire(&fake, MODE, false);
        fake.take_calls();
        let released = Instant::now();
        drop(out);
        wait_for(&fake, &note("released 1"));
        assert!(released.elapsed() >= Duration::from_millis(20));
        assert_eq!(
            fake.take_calls(),
            [
                Call::RestoreRan(1),
                Call::KeepaliveDropped(1),
                note("released 1")
            ]
        );
        assert!(pool().is_empty());
    });
}

/// A suspend past a kept display's window leaves it the grace, not a teardown at the wake.
#[test]
fn a_suspend_runs_a_kept_display_down_to_the_grace() {
    let name = "a_suspend_runs_a_kept_display_down_to_the_grace";
    in_child(name, KEPT, || {
        let fake = restoring("kwin");
        drop(acquire(&fake, MODE, false));
        fake.take_calls();
        reap(crate::session_epoch(), Duration::from_secs(3600));
        let until = match pool().as_slice() {
            [(_, lifecycle::State::Lingering { until })] => *until,
            other => panic!("the grace holds the display past the wake: {other:?}"),
        };
        assert!(until.saturating_duration_since(Instant::now()) <= Duration::from_secs(5));
        assert_eq!(fake.take_calls(), []);
    });
}

/// A pre-warmed display outlives a keep_alive that is off. A second pre-warm adopts
/// it and leaves it parked. Once a session claims it the policy applies again.
#[test]
fn a_session_claims_its_parked_display() {
    in_child("a_session_claims_its_parked_display", &[], || {
        let fake = spawning("gamescope");
        fake.script(|s| s.isolation = Some("cafe0123".into()));
        log_events(&fake);
        assert!(registry::park(&mut fake.boxed(), MODE).expect("park"));
        let mut want = created(true, MODE);
        want.push(note("created"));
        assert_eq!(fake.take_calls(), want);
        assert_eq!(pool(), [("gamescope", lifecycle::State::Pinned)]);
        assert_eq!(registry::parked_isolations(), ["cafe0123"]);

        assert!(registry::park(&mut fake.boxed(), MODE).expect("park again"));
        assert_eq!(fake.take_calls(), [Call::KeptDisplayAlive(1)]);
        assert_eq!(registry::parked_isolations(), ["cafe0123"]);

        let out = acquire(&fake, MODE, false);
        assert_eq!(fake.take_calls(), [Call::KeptDisplayAlive(1)]);
        assert!(out.reused_gen.is_some());
        assert!(registry::parked_isolations().is_empty());
        drop(out);
        assert_eq!(
            fake.take_calls(),
            [Call::KeepaliveDropped(1), note("released 1")]
        );
    });
}

/// A kept display whose only mismatch is the mode moves to it. One that refuses is
/// retired before the create: a sole-instance backend never runs two.
#[test]
fn a_kept_display_takes_the_new_mode_or_is_retired_for_a_spawn() {
    let name = "a_kept_display_takes_the_new_mode_or_is_retired_for_a_spawn";
    in_child(name, &[], || {
        let fake = spawning("gamescope");
        fake.script(|s| {
            s.sole_instance = true;
            s.can_resize = true;
            s.resizes = true;
        });
        log_events(&fake);
        assert!(registry::park(&mut fake.boxed(), MODE).expect("park"));
        fake.take_calls();
        let out = acquire(&fake, SMALL, false);
        assert_eq!(
            fake.take_calls(),
            [Call::KeptDisplayAlive(1), Call::ResizeKept(None, SMALL)]
        );
        assert_eq!(
            (out.node_id, out.preferred_mode),
            (1, Some((1280, 720, 60)))
        );
        drop(out);

        assert!(registry::park(&mut fake.boxed(), MODE).expect("park again"));
        fake.take_calls();
        fake.script(|s| s.resizes = false);
        let out = acquire(&fake, SMALL, false);
        let mut want = vec![
            Call::KeptDisplayAlive(2),
            Call::ResizeKept(None, SMALL),
            Call::KeepaliveDropped(2),
            note("released 1"),
        ];
        want.extend(created(true, SMALL));
        want.push(note("created"));
        assert_eq!(fake.take_calls(), want);
        assert_eq!(out.node_id, 3);
    });
}

/// One client cycling modes under keep-alive holds one display on a backend that cannot resize
/// a kept one: each create retires that client's kept display first. Another client's stays.
#[test]
fn a_client_cycling_modes_keeps_one_display() {
    in_child("a_client_cycling_modes_keeps_one_display", KEPT, || {
        let other = owned_by(2);
        drop(acquire(&other, SMALL, false));
        let fake = owned_by(1);
        for mode in [MODE, QHD, UHD] {
            drop(acquire(&fake, mode, false));
            assert_eq!(modes(), [SMALL, mode]);
        }
        assert_eq!(other.take_calls().last(), Some(&Call::TakeTopologyRestore));
        let calls = fake.take_calls();
        let dropped: Vec<&Call> = (calls.iter())
            .filter(|c| matches!(c, Call::KeepaliveDropped(_)))
            .collect();
        assert_eq!(
            dropped,
            [&Call::KeepaliveDropped(1), &Call::KeepaliveDropped(2)]
        );
    });
}

/// A pinned display holds its place against the cap with nobody streaming: a client alone on
/// the host is refused rather than created past `max_displays`. The owner is admitted.
#[test]
fn the_cap_holds_with_one_client() {
    in_child("the_cap_holds_with_one_client", &[], || {
        std::fs::write(
            pf_paths::config_dir().join("display-settings.json"),
            br#"{"keep_alive":{"mode":"forever"},"max_displays":1}"#,
        )
        .unwrap();
        drop(acquire(&owned_by(1), MODE, false));
        assert_eq!(pool(), [("kwin", lifecycle::State::Pinned)]);
        let (alone, _alone) = crate::admission::admit(Some([2; 32]), (1920, 1080, 60), stop());
        assert!(matches!(alone, crate::admission::Admission::Reject(_)));
        let (owner, _owner) = crate::admission::admit(Some([1; 32]), (2560, 1440, 60), stop());
        assert!(matches!(owner, crate::admission::Admission::Separate));
    });
}

fn stop() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// A kept display goes back to the device that owns its slot, wherever it sits in the pool.
/// Another device at the same mode creates its own beside it.
#[test]
fn a_kept_display_goes_back_to_its_owner() {
    in_child("a_kept_display_goes_back_to_its_owner", KEPT, || {
        let (owner, other) = (owned_by(1), owned_by(2));
        drop(acquire(&other, MODE, false));
        drop(acquire(&owner, MODE, false));
        assert!(owner.take_calls().contains(&Call::Create(MODE)));
        let again = acquire(&owner, MODE, false);
        assert_eq!(owner.take_calls(), [Call::KeptDisplayAlive(1)]);
        assert!(again.reused_gen.is_some());
        assert!(matches!(
            pool().as_slice(),
            [
                ("kwin", lifecycle::State::Lingering { .. }),
                ("kwin", lifecycle::State::Active { refs: 1 }),
            ]
        ));
    });
}

/// A kept display with no owner goes to the first device that asks, and is that device's from
/// then on: `/display/state` shows its slot, and a shared device no longer takes it.
#[test]
fn a_kept_display_with_no_owner_is_first_come() {
    in_child("a_kept_display_with_no_owner_is_first_come", KEPT, || {
        let shared = FakeDisplay::new("kwin");
        shared.script(|s| s.client_fp = Some([3; 32]));
        drop(acquire(&shared, MODE, false));
        let claimant = owned_by(2);
        let held = acquire(&claimant, MODE, false);
        assert_eq!(claimant.take_calls(), [Call::KeptDisplayAlive(1)]);
        let slots: Vec<Option<u32>> = (registry::snapshot().displays.iter())
            .map(|d| d.identity_slot)
            .collect();
        assert_eq!(slots, [claimant.last_identity_slot()]);
        drop(held);
        shared.take_calls();
        drop(acquire(&shared, MODE, false));
        assert!(shared.take_calls().contains(&Call::Create(MODE)));
    });
}

/// Every pooled desktop backend gives a kept display only to the device that owns its slot. One
/// with no slot goes to anyone.
#[test]
fn every_pooled_backend_reuses_by_owner() {
    in_child("every_pooled_backend_reuses_by_owner", &[], || {
        let slot = |n: u8| {
            let mode = (MODE.width, MODE.height);
            crate::identity::resolve_slot(Some([n; 32]), mode, crate::policy::Identity::PerClient)
        };
        let (own, theirs) = (slot(1), slot(2));
        let backends: [Box<dyn VirtualDisplay>; 5] = [
            Box::new(crate::kwin::KwinDisplay::new().unwrap()),
            Box::new(crate::mutter::MutterDisplay::new().unwrap()),
            Box::new(crate::wlroots::WlrootsDisplay::new().unwrap()),
            Box::new(crate::niri::NiriDisplay::new().unwrap()),
            Box::new(crate::hyprland::HyprlandDisplay::new().unwrap()),
        ];
        for mut vd in backends {
            vd.set_client_identity(Some([1; 32]));
            let answers = [own, theirs, None].map(|kept| vd.accepts_kept(kept, Some("PF-1-1")));
            assert_eq!(answers, [true, false, true], "{}", vd.name());
        }
    });
}

/// KWin and Mutter report a session's slot before its first create resolves it: a reuse
/// stamps it and the same-client retire keys on it. Their own default is shared.
#[test]
fn kwin_and_mutter_report_the_slot_before_the_create() {
    in_child(
        "kwin_and_mutter_report_the_slot_before_the_create",
        &[],
        || {
            let mode = (MODE.width, MODE.height);
            let own = crate::identity::resolve_slot(
                Some([1; 32]),
                mode,
                crate::policy::Identity::PerClient,
            );
            assert!(own.is_some());
            let pending = |fp: Option<[u8; 32]>| {
                let backends: [Box<dyn VirtualDisplay>; 2] = [
                    Box::new(crate::kwin::KwinDisplay::new().unwrap()),
                    Box::new(crate::mutter::MutterDisplay::new().unwrap()),
                ];
                backends.map(|mut vd| {
                    vd.set_client_identity(fp);
                    vd.last_identity_slot()
                })
            };
            assert_eq!(
                pending(Some([1; 32])),
                [None, None],
                "an unconfigured host is shared"
            );
            let per_client = |p: &mut crate::policy::DisplayPolicy| {
                p.identity = crate::policy::Identity::PerClient;
                true
            };
            crate::policy::prefs().update(per_client).unwrap();
            assert_eq!(pending(Some([1; 32])), [own, own]);
            assert_eq!(
                pending(Some([2; 32])),
                [None, None],
                "no slot until a create takes one"
            );
            assert_eq!(pending(None), [None, None]);
        },
    );
}

/// A display the registry does not own goes straight to the session: no pool entry, no
/// display event, its restore left with the backend, and a kept spawn of the same name not
/// offered.
#[test]
fn a_display_the_registry_does_not_own_passes_through() {
    let name = "a_display_the_registry_does_not_own_passes_through";
    in_child(name, &[], || {
        let spawn = spawning("gamescope");
        assert!(registry::park(&mut spawn.boxed(), MODE).expect("park"));

        let managed = FakeDisplay::new("gamescope");
        log_events(&managed);
        managed.script(|s| {
            s.ownership = DisplayOwnership::SessionManaged;
            s.restore = true;
        });
        let out = acquire(&managed, MODE, false);
        let mut want = created(true, MODE);
        want.pop();
        assert_eq!(managed.take_calls(), want);
        assert!(out.pool_gen.is_none() && !out.ownership.pooled());
        assert_eq!(pool(), [("gamescope", lifecycle::State::Pinned)]);
        drop(out);
        assert_eq!(managed.take_calls(), [Call::KeepaliveDropped(1)]);
    });
}

/// Hyprland and sway keep the named head in the pool and give each session its own
/// cast of it, which closes before the lease lets the head go.
#[test]
fn a_session_cast_closes_before_its_lease() {
    in_child("a_session_cast_closes_before_its_lease", &[], || {
        let fake = FakeDisplay::new("hyprland");
        fake.script(|s| s.output_name = Some("PF-1"));
        let out = acquire(&fake, MODE, false);
        let mut want = created(true, MODE);
        want.push(Call::SessionCastFor("PF-1".into()));
        assert_eq!(fake.take_calls(), want);
        assert_eq!(
            (out.node_id, out.output_name.as_deref()),
            (1001, Some("PF-1"))
        );
        drop(out);
        assert_eq!(
            fake.take_calls(),
            [Call::CastClosed(1001), Call::KeepaliveDropped(1)]
        );
    });
}

/// A mode switch creates before it drops. The successor does not count the display
/// it replaces: it is first in the group and sits at the origin, and it inherits
/// the group's restore when the old one is retired.
#[test]
fn a_mode_switch_successor_takes_the_group_from_the_display_it_replaces() {
    let name = "a_mode_switch_successor_takes_the_group_from_the_display_it_replaces";
    in_child(name, KEPT, || {
        let fake = restoring("kwin");
        let old = acquire(&fake, MODE, false);
        let old_gen = old.pool_gen.expect("pooled");
        fake.take_calls();

        fake.script(|s| s.restore = false);
        let quit = Arc::new(AtomicBool::new(false));
        let new = registry::acquire(&mut fake.boxed(), SMALL, quit, Some(old_gen)).expect("create");
        assert_eq!(fake.take_calls(), created(true, SMALL));
        // The old lease drop looks like a disconnect, so the display lingers.
        drop(old);
        registry::retire(old_gen);
        assert_eq!(fake.take_calls(), [Call::KeepaliveDropped(1)]);
        drop(new);
        registry::release(None);
        assert_eq!(
            fake.take_calls(),
            [Call::RestoreRan(1), Call::KeepaliveDropped(2)]
        );
    });
}

/// `Entry::drop`: gamescope is the one virtual output that offers HDR, so only its
/// teardown re-arms an offer that failed.
#[test]
fn only_a_gamescope_teardown_rearms_the_hdr_offer() {
    let name = "only_a_gamescope_teardown_rearms_the_hdr_offer";
    in_child(name, &[], || {
        use pf_frame::hdr::{hdr_capture_failed, note_hdr_capture_failed, HdrSource};
        for (backend, rearmed) in [("kwin", false), ("gamescope", true)] {
            note_hdr_capture_failed(HdrSource::VirtualOutput);
            let fake = if rearmed {
                spawning(backend)
            } else {
                FakeDisplay::new(backend)
            };
            drop(acquire(&fake, MODE, false));
            assert_eq!(fake.calls().last(), Some(&Call::KeepaliveDropped(1)));
            let offered = !hdr_capture_failed(HdrSource::VirtualOutput);
            assert_eq!(offered, rearmed, "{backend}");
        }
    });
}

/// `group_key`: each gamescope is a desktop of its own. A second one is first in
/// its group, is not placed beside the first, and neither inherits the other's
/// restore. `a_group_hands_its_restore_to_the_member_that_outlives_it` is the
/// desktop arm.
#[test]
fn each_gamescope_display_is_a_group_of_its_own() {
    in_child("each_gamescope_display_is_a_group_of_its_own", &[], || {
        let fake = restoring("gamescope");
        fake.script(|s| s.ownership = DisplayOwnership::Spawned);
        let first = acquire(&fake, MODE, false);
        fake.take_calls();
        let second = acquire(&fake, MODE, false);
        assert_eq!(fake.take_calls(), created(true, MODE));

        drop(first);
        assert_eq!(
            fake.take_calls(),
            [Call::RestoreRan(1), Call::KeepaliveDropped(1)]
        );
        drop(second);
        assert_eq!(
            fake.take_calls(),
            [Call::RestoreRan(2), Call::KeepaliveDropped(2)]
        );
    });
}

/// `epoch_matches`: a new desktop compositor ends every kept desktop display, whose
/// node died with the old one. A gamescope display lives with its own child, so it
/// is kept and reused.
#[test]
fn a_new_session_epoch_ends_a_kept_desktop_display_but_not_gamescope() {
    let name = "a_new_session_epoch_ends_a_kept_desktop_display_but_not_gamescope";
    in_child(name, KEPT, || {
        let desk = FakeDisplay::new("kwin");
        let game = spawning("gamescope");
        drop(acquire(&desk, MODE, false));
        drop(acquire(&game, MODE, false));
        game.take_calls();

        crate::session::bump_session_epoch();
        reap(crate::session_epoch(), Duration::ZERO);
        wait_for(&desk, &Call::KeepaliveDropped(1));
        assert!(matches!(
            pool().as_slice(),
            [("gamescope", lifecycle::State::Lingering { .. })]
        ));
        let out = acquire(&game, MODE, false);
        assert_eq!(game.take_calls(), [Call::KeptDisplayAlive(1)]);
        assert!(out.reused_gen.is_some());
    });
}

/// `is_seat_game_mode` and `release_seat_game_mode`: a seat in Game Mode keeps its
/// own gamescope through a disconnect, until it switches to its desktop. A light
/// seat's isolated gamescope and a desktop display follow keep_alive instead, and a
/// deliberate quit ends even the seat's own.
#[test]
fn a_seat_in_game_mode_keeps_only_its_own_gamescope() {
    let name = "a_seat_in_game_mode_keeps_only_its_own_gamescope";
    in_child(name, KEPT, || {
        registry::set_seat_game_mode(true);
        let own = spawning("gamescope");
        let light = spawning("gamescope");
        light.script(|s| s.isolation = Some("ab12cd34".into()));
        let desk = FakeDisplay::new("kwin");
        for fake in [&own, &light, &desk] {
            drop(acquire(fake, MODE, false));
        }
        assert!(matches!(
            pool().as_slice(),
            [
                ("gamescope", lifecycle::State::Pinned),
                ("gamescope", lifecycle::State::Lingering { .. }),
                ("kwin", lifecycle::State::Lingering { .. }),
            ]
        ));

        own.take_calls();
        assert_eq!(registry::release_seat_game_mode(), 1);
        assert_eq!(own.take_calls(), [Call::KeepaliveDropped(1)]);
        assert_eq!(pool().len(), 2, "the switch ends only the seat's own");

        drop(acquire(&own, MODE, true));
        assert_eq!(own.take_calls().last(), Some(&Call::KeepaliveDropped(2)));

        registry::set_seat_game_mode(false);
        drop(acquire(&own, MODE, false));
        assert!(matches!(
            pool().last(),
            Some(("gamescope", lifecycle::State::Lingering { .. }))
        ));
    });
}
