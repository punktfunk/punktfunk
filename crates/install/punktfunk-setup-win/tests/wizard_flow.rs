#![cfg(windows)]
//! WP2.1's acceptance, headless: the wizard drives a fake executor end-to-end. Reactor's
//! `ComponentHost` over its `RecordingAdapter` renders the real component and dispatches
//! events into the real callbacks, so these run on the Windows CI lane with no window.
//!
//! The model below replays the recorded mutation batches. Text and automation names are kept
//! as everything ever shown (the install log stays asserted after Done replaced it); controls
//! are looked up among the live objects of the retained graph.

use std::collections::HashMap;

use pf_fluent::{Palette, Shade};
use pf_l10n::tools;
use punktfunk_setup::platform::windows::demo::win_preset;
use punktfunk_setup_win::real::Seams;
use punktfunk_setup_win::wizard::{Wizard, WizardInput};
use windows_reactor::test::{
    ComponentHost, EventDispatch, EventId, EventPayload, EventValue, Mutation, ObjectId,
    ObjectType, PropertyId, PropertyValue, RecordingAdapter, RelationId,
};
use windows_reactor::{component, Brush, ThemeBrush};

#[derive(Default)]
struct Model {
    batches_seen: usize,
    order: Vec<ObjectId>,
    kinds: HashMap<ObjectId, ObjectType>,
    props: HashMap<ObjectId, HashMap<PropertyId, PropertyValue>>,
    events: HashMap<ObjectId, HashMap<EventId, EventValue>>,
    texts: Vec<String>,
    names: Vec<String>,
}

struct Wiz {
    host: ComponentHost<RecordingAdapter>,
    model: Model,
}

impl Wiz {
    fn open(preset: &str) -> Wiz {
        let mut adapter = RecordingAdapter::new();
        adapter.record_batches(true);
        // A small per-step latency, not zero: each settle renders the CURRENT state, and an
        // instant install would finish between two polls without painting a log line.
        let input = WizardInput {
            preset: win_preset(preset).expect(preset),
            seams: Seams::Demo { latency_ms: 25 },
        };
        let host = ComponentHost::mount(adapter, [component::<Wizard>("root", input)]);
        let mut wiz = Wiz {
            host,
            model: Model::default(),
        };
        wiz.settle();
        wiz
    }

    /// Run queued messages and native events, then fold the new batches into the model.
    fn settle(&mut self) {
        self.host.drain(100_000);
        let m = &mut self.model;
        let batches = self.host.adapter().batches();
        for batch in &batches[m.batches_seen..] {
            for mutation in batch {
                match mutation {
                    Mutation::Create { object, kind } | Mutation::Replace { object, kind } => {
                        m.order.push(*object);
                        m.kinds.insert(*object, *kind);
                        m.props.remove(object);
                        m.events.remove(object);
                    }
                    Mutation::SetProperties { object, set, clear } => {
                        let props = m.props.entry(*object).or_default();
                        for id in clear.iter() {
                            props.remove(id);
                        }
                        for p in set.iter() {
                            if let PropertyValue::String(s) = &p.value {
                                if p.id == PropertyId::AutomationName {
                                    m.names.push(s.to_string());
                                } else {
                                    m.texts.push(s.to_string());
                                }
                            }
                            props.insert(p.id, p.value.clone());
                        }
                    }
                    Mutation::SetEvents { object, set, clear } => {
                        let events = m.events.entry(*object).or_default();
                        for id in clear.iter() {
                            events.remove(id);
                        }
                        for e in set.iter() {
                            events.insert(e.id, e.value.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        m.batches_seen = batches.len();
    }

    fn has_text(&self, needle: &str) -> bool {
        self.model.texts.iter().any(|t| t.contains(needle))
    }

    fn has_name(&self, name: &str) -> bool {
        self.model.names.iter().any(|n| n == name)
    }

    /// The live objects of `kind`, newest first.
    fn live(&self, kind: ObjectType) -> Vec<ObjectId> {
        let graph = self.host.runtime().graph();
        self.model
            .order
            .iter()
            .rev()
            .copied()
            .filter(|id| graph.kind(*id) == Some(kind))
            .collect()
    }

    fn prop(&self, id: ObjectId, prop: PropertyId) -> Option<&PropertyValue> {
        self.model.props.get(&id)?.get(&prop)
    }

    fn text_of(&self, id: ObjectId) -> Option<&str> {
        match self.prop(id, PropertyId::Text)? {
            PropertyValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// The live control of `kind` whose content text is `label` — of that kind, so a stepper
    /// label reading "Uninstall" never shadows the button.
    fn control_with_text(&self, kind: ObjectType, label: &str) -> ObjectId {
        let graph = self.host.runtime().graph();
        self.live(kind)
            .into_iter()
            .find(|id| {
                graph
                    .child(*id, RelationId::Content)
                    .and_then(|c| self.text_of(c))
                    == Some(label)
            })
            .unwrap_or_else(|| panic!("no {kind:?} carries the text '{label}'"))
    }

    fn fire(&mut self, id: ObjectId, event: EventId, payload: EventPayload) {
        let value = self
            .model
            .events
            .get(&id)
            .and_then(|e| e.get(&event))
            .cloned()
            .unwrap_or_else(|| panic!("the control has no {event:?} handler"));
        self.host
            .queue_event(EventDispatch::new(id, event, value, payload));
        self.settle();
    }

    fn click(&mut self, label: &str) {
        let id = self.control_with_text(ObjectType::Button, label);
        self.fire(id, EventId::Click, EventPayload::Unit);
    }

    /// An icon-only button carries no text: find it by its automation name.
    fn click_named(&mut self, name: &str) {
        let id = self
            .live(ObjectType::Button)
            .into_iter()
            .find(|id| {
                matches!(self.prop(*id, PropertyId::AutomationName),
                    Some(PropertyValue::String(s)) if &**s == name)
            })
            .unwrap_or_else(|| panic!("no button is named '{name}'"));
        self.fire(id, EventId::Click, EventPayload::Unit);
    }

    /// Check a radio button by its label (Welcome's Recommended / Custom).
    fn choose(&mut self, label: &str) {
        let id = self.control_with_text(ObjectType::RadioButton, label);
        self.fire(id, EventId::Checked, EventPayload::OptionalBool(Some(true)));
    }

    /// The stepper's dots (WP2.1b): every step of this run's path is one 10 px ellipse —
    /// the lockup's two circles are ellipses too, so the size is the tell.
    fn dots(&self) -> usize {
        self.live(ObjectType::Ellipse)
            .into_iter()
            .filter(|id| {
                matches!(self.prop(*id, PropertyId::Width), Some(PropertyValue::F64(w)) if *w == 10.0)
            })
            .count()
    }

    /// The toggle of the Configure row labelled `label`: the first ToggleSwitch created after
    /// that label — row order inside a section, whatever the columns do.
    fn toggle_of(&self, label: &str) -> ObjectId {
        let graph = self.host.runtime().graph();
        let order = &self.model.order;
        let at = order
            .iter()
            .rposition(|id| graph.kind(*id).is_some() && self.text_of(*id) == Some(label))
            .unwrap_or_else(|| panic!("no row labelled '{label}'"));
        order[at..]
            .iter()
            .copied()
            .find(|id| graph.kind(*id) == Some(ObjectType::ToggleSwitch))
            .unwrap_or_else(|| panic!("no toggle after '{label}'"))
    }

    /// Drive until the Done page's Finish button exists (the install thread finished).
    fn wait_for_done(&mut self) {
        for _ in 0..500 {
            self.settle();
            if self.has_text("Finish") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "the install never reached Done; texts: {:?}",
            self.model.texts
        );
    }
}

#[test]
fn the_fresh_host_demo_walks_welcome_to_done_through_the_fake_executor() {
    let mut wiz = Wiz::open("win11-fresh");
    assert!(
        wiz.has_text("Punktfunk"),
        "the Welcome sentence names the product"
    );
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
        wiz.has_text(tools::wizard_row_gamestream()),
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

    let gamestream = wiz.toggle_of(tools::wizard_row_gamestream());
    wiz.fire(gamestream, EventId::Toggled, EventPayload::Bool(true));

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

/// Nothing on Welcome keeps a colour of its own: the lit step and the selection ring are the
/// accent brush the buttons use, and the mark's circles are shades of the system accent.
#[test]
fn welcome_paints_in_the_system_accent() {
    let wiz = Wiz::open("win11-fresh");
    let accent = PropertyValue::Brush(Brush::Theme(ThemeBrush::Accent));
    let ellipses = wiz.live(ObjectType::Ellipse);
    let is_dot = |id: &ObjectId| matches!(wiz.prop(*id, PropertyId::Width), Some(PropertyValue::F64(w)) if *w == 10.0);
    assert!(
        ellipses
            .iter()
            .any(|id| is_dot(id) && wiz.prop(*id, PropertyId::Fill) == Some(&accent)),
        "the current step's dot"
    );
    assert!(
        wiz.live(ObjectType::Border)
            .iter()
            .any(|id| wiz.prop(*id, PropertyId::BorderBrush) == Some(&accent)),
        "the selection ring"
    );
    let shades = [
        Shade::Dark3,
        Shade::Dark2,
        Shade::Dark1,
        Shade::Accent,
        Shade::Light1,
        Shade::Light2,
        Shade::Light3,
    ];
    let ramp: Vec<_> = shades
        .iter()
        .filter_map(|s| Palette::system().accent(*s))
        .collect();
    if ramp.len() < shades.len() {
        return; // Windows named no accent here: the brand tones stand in.
    }
    let circles: Vec<ObjectId> = ellipses.iter().copied().filter(|id| !is_dot(id)).collect();
    assert_eq!(circles.len(), 2, "the mark's two circles");
    for id in circles {
        match wiz.prop(id, PropertyId::Fill) {
            Some(PropertyValue::Brush(Brush::Solid(c))) => {
                assert!(ramp.contains(c), "{c:?} is no accent shade of {ramp:?}")
            }
            other => panic!("a circle fill of {other:?}"),
        }
    }
}

#[test]
fn the_public_network_demo_walks_the_d12_step_and_answer_b_opens_the_firewall() {
    let mut wiz = Wiz::open("win11-public");
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
    let radio = wiz.live(ObjectType::RadioButtons)[0];
    wiz.fire(
        radio,
        EventId::SelectionChanged,
        EventPayload::SelectionIndex(Some(1)),
    );

    wiz.click("Install");
    wiz.wait_for_done();
    assert!(
        wiz.has_text("--allow-public-network=on"),
        "answer (b) provably flips the allowpublicfw plan step"
    );
}

#[test]
fn the_uninstaller_demo_offers_only_the_teardown_and_runs_it_in_the_sandbox() {
    let mut wiz = Wiz::open("win11-uninstall");
    assert!(wiz.has_text("0.34.0 · host installed"));
    assert!(wiz.has_text(tools::wizard_welcome_uninstall()));
    assert!(!wiz.has_text("Reconfigure"), "no payload, no reconfigure");
    assert_eq!(wiz.dots(), 3, "Welcome · Uninstall · Done");
    wiz.click("Uninstall");
    wiz.wait_for_done();
    assert!(wiz.has_text("service uninstall"), "the teardown ran");
    assert!(wiz.has_text(tools::wizard_done_removed()));
}

#[test]
fn the_upgrade_demo_opens_in_manage_mode_and_reconfigure_walks_the_upgrade() {
    let mut wiz = Wiz::open("win11-upgrade");
    assert!(wiz.has_text("0.34.0 · host installed"));
    wiz.click("Reconfigure");
    assert!(
        wiz.has_text(tools::wizard_row_gamestream()),
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
    let mut wiz = Wiz::open("win11-upgrade");
    wiz.click("Uninstall");
    wiz.wait_for_done();
    assert!(wiz.has_text("service uninstall"), "the teardown ran");
    assert!(wiz.has_text(tools::wizard_done_removed()));
}

#[test]
fn the_sunshine_demo_shows_the_coexistence_row_and_moves_the_mgmt_port() {
    let mut wiz = Wiz::open("win11-sunshine");
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
    let mut wiz = Wiz::open("win11-fresh");
    wiz.click("Install");
    wiz.wait_for_done();
    assert!(wiz.has_text("--gamestream=off"), "the defaults ran");
    assert!(wiz.has_text("Your web console password"));
    // Masked until Show; Copy is there either way and reports back.
    assert!(wiz.has_text(punktfunk_setup_win::wizard::PASSWORD_MASK));
    assert!(!wiz.has_name("Hide"));
    wiz.click_named("Show");
    assert!(wiz.has_name("Hide"));
    wiz.click_named("Copy");
    assert!(wiz.has_name("Copied"));
    assert!(wiz.has_text("Open the web console") && wiz.has_text("Install a client"));
}
