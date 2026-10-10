#![cfg(windows)]
//! WP2.1's acceptance, headless: the wizard drives a fake executor end-to-end. Same harness
//! as `clients/windows/tests/reactor_semantics.rs` — reactor's `RecordingBackend` renders
//! the real component tree and replays events into the real handlers, so these run on the
//! Windows CI lane with no window and no glass.
//!
//! Events are fired by replaying the `AttachEvent` op each control registered, never by
//! guessing an `Event` variant — what the backend attached is what a user's click reaches.

use std::cell::RefCell;
use std::rc::Rc;

use punktfunk_setup::platform::windows::demo::win_preset;
use punktfunk_setup_win::real::Seams;
use punktfunk_setup_win::wizard::WizardRoot;
use test_reactor::{Op, RecordingBackend};
use windows_reactor::{
    ChannelDispatcher, Component, ControlId, ControlKind, Dispatcher, DispatcherQueuePriority,
    Event, Prop, PropValue, RenderHost,
};

type Job = Box<dyn FnOnce()>;

/// The upstream tests' single-threaded "UI thread": jobs queue until drained.
#[derive(Clone, Default)]
struct TestDispatcher {
    queue: Rc<RefCell<Vec<Job>>>,
}

impl TestDispatcher {
    fn drain(&self) {
        loop {
            let item = {
                let mut q = self.queue.borrow_mut();
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            };
            match item {
                Some(f) => f(),
                None => break,
            }
        }
    }
}

impl Dispatcher for TestDispatcher {
    fn enqueue(&self, _p: DispatcherQueuePriority, f: Box<dyn FnOnce()>) -> bool {
        self.queue.borrow_mut().push(f);
        true
    }
}

struct Wiz {
    host: RenderHost<RecordingBackend, TestDispatcher>,
    dispatcher: TestDispatcher,
    channel: ChannelDispatcher,
}

impl Wiz {
    fn open(preset: &str) -> Wiz {
        let dispatcher = TestDispatcher::default();
        let channel = ChannelDispatcher::new();
        // A small per-step latency, not zero: `wait_for_done` polls with settles, and each
        // settle renders the CURRENT state — instant installs finish whole between two
        // polls, so the Install page (whose rendered log the tests assert on) would never
        // paint a single line.
        let root = WizardRoot::new(
            win_preset(preset).expect(preset),
            Seams::Demo { latency_ms: 25 },
        );
        let host = RenderHost::new(
            RecordingBackend::new(),
            Box::new(root) as Box<dyn Component>,
            dispatcher.clone(),
        );
        host.set_marshaller(Some(channel.marshaller()));
        host.kick();
        dispatcher.drain();
        Wiz {
            host,
            dispatcher,
            channel,
        }
    }

    /// Marshal cross-thread writes in, run the renders they request.
    fn settle(&self) {
        self.channel.drain();
        self.dispatcher.drain();
        self.host.kick();
        self.dispatcher.drain();
    }

    fn texts(&self) -> Vec<String> {
        self.host.with_reconciler(|r| {
            r.backend
                .ops
                .iter()
                .filter_map(|op| match op {
                    Op::SetProp {
                        value: PropValue::Str(s),
                        ..
                    } => Some(s.clone()),
                    _ => None,
                })
                .collect()
        })
    }

    fn has_text(&self, needle: &str) -> bool {
        self.texts().iter().any(|t| t.contains(needle))
    }

    /// The most recent control of `kind` whose string prop equals `label` — of that kind,
    /// so a stepper label reading "Uninstall" never shadows the button.
    fn control_with_text(&self, kind: ControlKind, label: &str) -> ControlId {
        self.host.with_reconciler(|r| {
            let ops = &r.backend.ops;
            let of_kind = |id: &ControlId| {
                ops.iter()
                    .any(|op| matches!(op, Op::Create { id: c, kind: k } if c == id && *k == kind))
            };
            ops.iter()
                .rev()
                .find_map(|op| match op {
                    Op::SetProp {
                        id,
                        value: PropValue::Str(s),
                        ..
                    } if s == label && of_kind(id) => Some(*id),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no {kind:?} carries the text '{label}'"))
        })
    }

    /// Replay the event this control attached — a unit event (a button click).
    fn click(&self, label: &str) {
        self.fire_unit(ControlKind::Button, label);
    }

    /// An icon-only button carries no text: find it by its automation name.
    fn click_named(&self, name: &str) {
        let id = self.host.with_reconciler(|r| {
            r.backend
                .ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    Op::SetAccessibility { id, accessibility }
                        if accessibility.automation_name.as_deref() == Some(name) =>
                    {
                        Some(*id)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no control is named '{name}'"))
        });
        let event = self.attached_event(id);
        self.host.with_reconciler(|r| r.backend.fire(id, event));
        self.settle();
    }

    fn has_name(&self, name: &str) -> bool {
        self.host.with_reconciler(|r| {
            r.backend.ops.iter().any(|op| {
                matches!(op, Op::SetAccessibility { accessibility, .. }
                    if accessibility.automation_name.as_deref() == Some(name))
            })
        })
    }

    /// Check a radio button by its label (Welcome's Recommended / Custom).
    fn choose(&self, label: &str) {
        self.fire_unit(ControlKind::RadioButton, label);
    }

    fn fire_unit(&self, kind: ControlKind, label: &str) {
        let id = self.control_with_text(kind, label);
        let event = self.attached_event(id);
        self.host.with_reconciler(|r| r.backend.fire(id, event));
        self.settle();
    }

    fn attached_event(&self, id: ControlId) -> Event {
        self.host.with_reconciler(|r| {
            r.backend
                .ops
                .iter()
                .rev()
                .find_map(|op| match op {
                    Op::AttachEvent { id: c, event, .. } if *c == id => Some(*event),
                    _ => None,
                })
                .expect("the control attached an event")
        })
    }

    /// The stepper's dots (WP2.1b): every step of this run's path is one 10 px ellipse —
    /// the lockup's two circles are ellipses too, so the size is the tell.
    fn dots(&self) -> usize {
        self.host.with_reconciler(|r| {
            let ops = &r.backend.ops;
            ops.iter()
                .filter(|op| {
                    matches!(
                        op,
                        Op::SetProp {
                            id,
                            prop: Prop::Width,
                            value: PropValue::F64(w),
                        } if *w == 10.0 && ops.iter().any(|o| matches!(o, Op::Create { id: c, kind: ControlKind::Ellipse } if c == id))
                    )
                })
                .count()
        })
    }

    /// The toggle of the Configure row labelled `label`: the first ToggleSwitch created
    /// after that label — row order inside a section, whatever the columns do.
    fn toggle_of(&self, label: &str) -> ControlId {
        self.host.with_reconciler(|r| {
            let ops = &r.backend.ops;
            let at = ops
                .iter()
                .position(
                    |op| matches!(op, Op::SetProp { value: PropValue::Str(s), .. } if s == label),
                )
                .unwrap_or_else(|| panic!("no row labelled '{label}'"));
            ops[at..]
                .iter()
                .find_map(|op| match op {
                    Op::Create {
                        id,
                        kind: ControlKind::ToggleSwitch,
                    } => Some(*id),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no toggle after '{label}'"))
        })
    }

    /// The nth control of `kind` ever created (creation order = row order).
    fn nth(&self, kind: ControlKind, n: usize) -> ControlId {
        self.host.with_reconciler(|r| {
            r.backend
                .ops
                .iter()
                .filter_map(|op| match op {
                    Op::Create { id, kind: k } if *k == kind => Some(*id),
                    _ => None,
                })
                .nth(n)
                .unwrap_or_else(|| panic!("no {kind:?} number {n}"))
        })
    }

    /// Drive until the Done page's Finish button exists (the install thread finished).
    fn wait_for_done(&self) {
        for _ in 0..500 {
            self.settle();
            if self.has_text("Finish") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the install never reached Done; texts: {:?}", self.texts());
    }
}

#[test]
fn the_fresh_host_demo_walks_welcome_to_done_through_the_fake_executor() {
    let wiz = Wiz::open("win11-fresh");
    assert!(wiz.has_text("punktfunk"), "the Welcome wordmark");
    assert!(wiz.has_text("Recommended") && wiz.has_text("Custom"));
    assert_eq!(wiz.dots(), 3, "Recommended: Welcome · Install · Done");
    assert!(
        !wiz.has_text("Network"),
        "no ghost Network dot on a private-network box"
    );

    // Custom puts Configure on the path — one more dot, and Continue goes there.
    wiz.choose("Custom");
    assert_eq!(wiz.dots(), 4, "Custom adds the Configure dot");
    wiz.click("Continue");
    assert!(
        wiz.has_text("Moonlight compat"),
        "the Configure rows render"
    );
    assert!(
        wiz.has_text("Drivers") && wiz.has_text("Web console"),
        "grouped"
    );
    assert!(
        wiz.has_text("Web console password"),
        "the fresh-only password row"
    );

    let gamestream = wiz.toggle_of("Moonlight compat");
    let event = wiz.attached_event(gamestream);
    wiz.host
        .with_reconciler(|r| r.backend.fire_bool(gamestream, event, true));
    wiz.settle();

    wiz.click("Install");
    wiz.wait_for_done();

    // The dim command echo proves the executor ran the RE-DERIVED plan (the toggle above).
    assert!(
        wiz.has_text("--gamestream=on"),
        "the install log carries the re-derived service argv"
    );
    assert!(
        wiz.has_text("Web console password"),
        "the Done page shows the password card"
    );
}

#[test]
fn the_public_network_demo_walks_the_d12_step_and_answer_b_opens_the_firewall() {
    let wiz = Wiz::open("win11-public");
    assert_eq!(
        wiz.dots(),
        4,
        "the Network dot materializes for a Public network"
    );
    assert!(wiz.has_text("Network"), "the stepper labels the extra dot");

    // Recommended skips Configure, never the D12 consent step.
    wiz.click("Continue");
    assert!(
        wiz.has_text("'Cafe' is set to Public"),
        "the step names the network"
    );

    // Answer (b): keep it Public, open the firewall.
    let radio = wiz.nth(ControlKind::RadioButtons, 0);
    let event = wiz.attached_event(radio);
    wiz.host
        .with_reconciler(|r| r.backend.fire_i32(radio, event, 1));
    wiz.settle();

    wiz.click("Install");
    wiz.wait_for_done();
    assert!(
        wiz.has_text("--allow-public-network=on"),
        "answer (b) provably flips the allowpublicfw plan step"
    );
}

#[test]
fn the_uninstaller_demo_offers_only_the_teardown_and_runs_it_in_the_sandbox() {
    let wiz = Wiz::open("win11-uninstall");
    assert!(wiz.has_text("0.34.0 · host installed"));
    assert!(wiz.has_text("This removes punktfunk"));
    assert!(!wiz.has_text("Reconfigure"), "no payload, no reconfigure");
    assert_eq!(wiz.dots(), 3, "Welcome · Uninstall · Done");
    wiz.click("Uninstall");
    wiz.wait_for_done();
    assert!(wiz.has_text("service uninstall"), "the teardown ran");
    assert!(wiz.has_text("punktfunk was removed"));
}

#[test]
fn the_upgrade_demo_opens_in_manage_mode_and_reconfigure_walks_the_upgrade() {
    let wiz = Wiz::open("win11-upgrade");
    assert!(wiz.has_text("0.34.0 · host installed"));
    wiz.click("Reconfigure");
    assert!(
        wiz.has_text("Moonlight compat"),
        "the Configure rows render"
    );
    assert!(
        !wiz.has_text("Web console password"),
        "no password row on an installed box"
    );
    wiz.click("Install");
    wiz.wait_for_done();
    assert!(wiz.has_text("service install"), "the upgrade plan ran");
    assert!(
        !wiz.has_text("service uninstall"),
        "Reconfigure never tears down"
    );
}

#[test]
fn uninstall_from_the_manage_welcome_tears_down() {
    let wiz = Wiz::open("win11-upgrade");
    wiz.click("Uninstall");
    wiz.wait_for_done();
    assert!(wiz.has_text("service uninstall"), "the teardown ran");
    assert!(wiz.has_text("punktfunk was removed"));
}

#[test]
fn the_sunshine_demo_shows_the_coexistence_row_and_moves_the_mgmt_port() {
    let wiz = Wiz::open("win11-sunshine");
    wiz.choose("Custom");
    wiz.click("Continue");
    assert!(
        wiz.has_text("Sunshine detected"),
        "the D11 coexistence row is a visible row, not a dialog"
    );
    wiz.click("Install");
    wiz.wait_for_done();
    assert!(
        wiz.has_text("PUNKTFUNK_MGMT_BIND=0.0.0.0:47991"),
        "the move is announced"
    );
}

// Recommended: Welcome's Install runs the defaults and Done is where the generated password
// is seen for the first time.
#[test]
fn recommended_installs_the_defaults_and_done_shows_the_password_and_next_steps() {
    let wiz = Wiz::open("win11-fresh");
    wiz.click("Install");
    wiz.wait_for_done();
    assert!(wiz.has_text("--gamestream=off"), "the defaults ran");
    assert!(wiz.has_text("Your web console password"));
    // Masked until Show (the op log is append-only, so "Hide" appearing is the flip);
    // Copy is there either way and reports back.
    assert!(wiz.has_text(punktfunk_setup_win::wizard::PASSWORD_MASK));
    assert!(!wiz.has_name("Hide"));
    wiz.click_named("Show");
    assert!(wiz.has_name("Hide"));
    wiz.click_named("Copy");
    assert!(wiz.has_name("Copied"));
    assert!(wiz.has_text("Open the web console") && wiz.has_text("Install a client"));
}
