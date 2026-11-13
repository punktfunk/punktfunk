use super::*;
use crate::library::LibraryShared;
use crate::screens::Device as This;
use crate::store::{SettingsStore, SnapshotStore};

fn bits(names: &[&str]) -> Vec<u32> {
    names.iter().map(|n| button_bit(n).unwrap()).collect()
}

fn desktop() -> This {
    This::test()
}

/// An Android TV: a pad and a remote, no screen to touch.
fn tv() -> This {
    This {
        platform: Platform::Android,
        tv: true,
        ..This::test()
    }
}

/// `f` over `store`'s settings, on `device`. Each test keeps its own store.
fn with<R>(device: &This, store: &SnapshotStore, f: impl FnOnce(&mut Ctx) -> R) -> R {
    let mut settings = store.load();
    let library = LibraryShared::default();
    let mut ctx = Ctx {
        store,
        device,
        ..Ctx::test(&mut settings, &library)
    };
    f(&mut ctx)
}

fn store() -> SnapshotStore {
    SnapshotStore::new(Settings::default(), Vec::new())
}

fn pad(names: &[&str], hold: bool) -> Trigger {
    Trigger::Pad {
        buttons: bits(names),
        hold,
    }
}

fn saved(store: &SnapshotStore) -> Shortcuts {
    Shortcuts::parse(&store.load().shortcuts)
}

/// Every default binding has a row of its own, under its device, with Add rows for keys and
/// the pad and Reset last. A desktop lists touch rows fixed; a TV has no screen to touch.
#[test]
fn the_page_lists_the_defaults_rows_for_a_desktop_and_a_tv() {
    for device in [desktop(), tv()] {
        let store = store();
        let page = with(&device, &store, |ctx| {
            ShortcutsScreen::new(ctx, Scope::Global)
        });
        let lines = with(&device, &store, |ctx| page.lines(ctx));
        let want = defaults(page.client, false, "auto").bindings.len();
        let mut bound: Vec<usize> = (lines.iter())
            .filter_map(|(l, _)| match l {
                Line::Binding(i) => Some(*i),
                _ => None,
            })
            .collect();
        bound.sort_unstable();
        assert_eq!(
            bound,
            (0..want).collect::<Vec<_>>(),
            "{:?}",
            device.platform
        );
        let adds: Vec<_> = (lines.iter())
            .filter_map(|(l, _)| match l {
                Line::Add(d) => Some(*d),
                _ => None,
            })
            .collect();
        assert_eq!(adds, [Device::Keyboard, Device::Pad]);
        assert_eq!(lines.last().map(|(l, _)| *l), Some(Line::Reset));
        let fixed = lines.iter().filter(|(l, _)| *l == Line::Fixed);
        assert!(fixed.clone().all(|(_, r)| !r.enabled && r.note.is_some()));
        assert_eq!(fixed.count() > 0, !device.tv, "{:?}", device.platform);
        let headers: Vec<&str> = lines.iter().filter_map(|(_, r)| r.header).collect();
        assert_eq!(headers.first(), Some(&c::controls_group_keyboard()));
        assert!(headers.last().unwrap().starts_with(c::controls_group_pad()));
    }
}

/// With every key removed the keyboard has no group, but its Add row keeps a heading.
#[test]
fn a_keyboard_with_nothing_bound_still_offers_add() {
    let store = SnapshotStore::new(
        Settings {
            shortcuts: r#"{"v":1,"bindings":[{"on":"pad","keys":["select","a"],"do":"ring"}]}"#
                .into(),
            ..Settings::default()
        },
        Vec::new(),
    );
    let device = desktop();
    let page = with(&device, &store, |ctx| {
        ShortcutsScreen::new(ctx, Scope::Global)
    });
    let lines = with(&device, &store, |ctx| page.lines(ctx));
    let (line, row) = &lines[0];
    assert_eq!(*line, Line::Add(Device::Keyboard));
    assert_eq!(row.header, Some(c::controls_group_keyboard()));
    assert!(lines.iter().any(|(l, _)| *l == Line::Add(Device::Pad)));
}

/// The dial's own entry leads Input in the picker; the empty slot is the dial's alone.
#[test]
fn the_picker_offers_quick_actions_and_no_empty_slot() {
    let groups = actions(&Settings::default(), Platform::Desktop);
    let input = groups.iter().find(|g| g.id == GroupId::Input).unwrap();
    assert_eq!(input.entries[0].id, "ring");
    assert_eq!(input.entries[0].label, c::ring_open_ring());
    assert!(groups.iter().all(|g| g.id != GroupId::Empty));
    assert_eq!(
        action_label(&groups, &Action::Unknown("later".into())),
        None,
        "an action from a newer client"
    );
}

/// Buttons held when recording starts belong to the press that opened it; the chord is
/// what was held when the first button lifted.
#[test]
fn record_takes_the_set_when_held_shrinks() {
    let held = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let mut r = Recorder::default();
    assert_eq!(r.feed(&held(&["A"])), None, "the A that opened it");
    assert_eq!(r.feed(&held(&[])), None);
    assert_eq!(r.feed(&held(&["Back"])), None);
    assert_eq!(r.feed(&held(&["Back", "Start", "LT"])), None);
    let took = r.feed(&held(&["Start"])).expect("Back lifted");
    assert_eq!(took, bits(&["select", "start"]));

    let mut r = Recorder::default();
    assert_eq!(
        r.feed(&held(&["Start"])),
        None,
        "no A: the gesture has begun"
    );
    assert_eq!(r.feed(&held(&[])), Some(vec![button_bit("start").unwrap()]));
}

/// A recorded set lands on the row; one button is a hold.
#[test]
fn a_recorded_button_saves_as_a_hold() {
    let store = store();
    let device = desktop();
    let mut row = with(&device, &store, |ctx| {
        ShortcutRowScreen::edit(ctx, Scope::Global, Client::Desktop, 1)
    });
    row.start_recording();
    let mut fx = Outbox::default();
    for held in [&[][..], &["Y"], &[]] {
        let state = PadTestState {
            held: held.iter().map(|h| h.to_string()).collect(),
            axes: Vec::new(),
        };
        with(&device, &store, |ctx| row.pad_test(&state, ctx, &mut fx));
    }
    assert!(!row.recording());
    assert_eq!(saved(&store).bindings[1].trigger, pad(&["y"], true));
}

/// A trigger another row already has asks first; Replace drops that row.
#[test]
fn replace_drops_the_old_row() {
    let store = store();
    let device = desktop();
    let before = defaults(Client::Desktop, false, "auto");
    let mut row = with(&device, &store, |ctx| {
        ShortcutRowScreen::edit(ctx, Scope::Global, Client::Desktop, 1)
    });
    let stats = row.draft.action.clone();
    row.draft.trigger = before.bindings[0].trigger.clone();
    let mut fx = Outbox::default();
    with(&device, &store, |ctx| row.commit(None, ctx, &mut fx));
    assert_eq!(row.replace, Some(0));
    assert!(
        store.load().shortcuts.is_empty(),
        "nothing saved before the answer"
    );
    let fields = with(&device, &store, |ctx| row.fields(ctx));
    assert_eq!(fields, [Field::Replace, Field::Cancel]);
    let question = with(&device, &store, |ctx| row.question(ctx)).unwrap();
    assert!(question.contains("Select + A"), "{question}");

    with(&device, &store, |ctx| {
        row.activate(Field::Replace, ctx, &mut fx)
    });
    let after = saved(&store);
    assert_eq!(after.bindings.len(), before.bindings.len() - 1);
    assert_eq!(after.bindings[0].trigger, before.bindings[0].trigger);
    assert_eq!(after.bindings[0].action, stats);
    assert_eq!(row.index, Some(0), "the row follows its binding");
}

/// A press inside a longer chord would fire first: refused, with both chords named.
#[test]
fn a_subset_is_refused() {
    let store = store();
    let device = desktop();
    let mut row = with(&device, &store, |ctx| {
        ShortcutRowScreen::edit(ctx, Scope::Global, Client::Desktop, 1)
    });
    row.draft.trigger = pad(&["lb", "rb"], false);
    let mut fx = Outbox::default();
    with(&device, &store, |ctx| row.commit(None, ctx, &mut fx));
    let line = row.line.clone().expect("refused");
    assert!(line.starts_with("L1 + R1 would fire before"), "{line}");
    assert!(store.load().shortcuts.is_empty());
    assert_eq!(row.replace, None);
}

/// Reset asks for a second press, then stores the empty blob: this client's defaults. In a
/// preset's scope it pins the empty blob on the preset instead.
#[test]
fn reset_writes_the_empty_blob() {
    let store = SnapshotStore::new(
        Settings {
            shortcuts: r#"{"v":1,"bindings":[]}"#.into(),
            ..Settings::default()
        },
        Vec::new(),
    );
    let device = desktop();
    let mut page = with(&device, &store, |ctx| {
        ShortcutsScreen::new(ctx, Scope::Global)
    });
    let mut fx = Outbox::default();
    with(&device, &store, |ctx| {
        page.activate(Line::Reset, ctx, &mut fx)
    });
    assert!(!store.load().shortcuts.is_empty(), "the first press arms");
    with(&device, &store, |ctx| {
        page.activate(Line::Reset, ctx, &mut fx)
    });
    assert_eq!(store.load().shortcuts, "");

    let draft = super::super::preset::PresetDraft {
        id: "p1".into(),
        name: "Couch".into(),
        overlay: Default::default(),
    };
    let scope = Scope::Preset(std::sync::Arc::new(Mutex::new(draft)));
    let mut page = with(&device, &store, |ctx| ShortcutsScreen::new(ctx, scope));
    let mut fx = Outbox::default();
    with(&device, &store, |ctx| {
        page.activate(Line::Reset, ctx, &mut fx);
        page.activate(Line::Reset, ctx, &mut fx)
    });
    let Some(crate::model::ConsoleCmd::SavePreset { overrides, .. }) = fx.cmds.last() else {
        panic!("a preset saves through the host");
    };
    assert_eq!(overrides["shortcuts"], "");
}

/// A key is always a press, and one button always a hold: neither offers the other.
#[test]
fn the_row_offers_no_hold_for_a_key_and_no_press_for_one_button() {
    let store = store();
    let device = desktop();
    let list = defaults(Client::Desktop, false, "auto");
    let key = (list.bindings.iter())
        .position(|b| b.trigger.device() == Device::Keyboard)
        .unwrap();
    let mut row = with(&device, &store, |ctx| {
        ShortcutRowScreen::edit(ctx, Scope::Global, Client::Desktop, key)
    });
    let fires = with(&device, &store, |ctx| row.spec(Field::FiresOn, ctx));
    assert!(!fires.adjustable);
    assert_eq!(fires.value.as_deref(), Some(c::shortcuts_fires_press()));
    let mut fx = Outbox::default();
    let pulse = with(&device, &store, |ctx| {
        row.adjust(Field::FiresOn, 1, ctx, &mut fx)
    });
    assert!(matches!(pulse, MenuPulse::Boundary));
    assert!(!row.draft.trigger.hold());
    let fields = with(&device, &store, |ctx| row.fields(ctx));
    assert!(fields.contains(&Field::Modifier(0)) && !fields.contains(&Field::Choose));

    let mut row = with(&device, &store, |ctx| {
        ShortcutRowScreen::add(ctx, Scope::Global, Client::Desktop, Device::Pad)
    });
    assert!(row.picker.is_some(), "Add picks the action first");
    with(&device, &store, |ctx| {
        row.picked(Picked::Chose("stats".into()), ctx, &mut fx)
    });
    assert_eq!(row.tray, Some(Tray::Buttons), "no recorder here: the grid");
    with(&device, &store, |ctx| {
        row.tray_msg(TrayMsg::Pick("start"), ctx, &mut fx);
        row.tray_msg(TrayMsg::Close, ctx, &mut fx);
    });
    let fires = with(&device, &store, |ctx| row.spec(Field::FiresOn, ctx));
    assert_eq!(fires.value.as_deref(), Some(c::shortcuts_fires_hold()));
    assert!(!fires.enabled && !fires.adjustable);
    assert_eq!(fires.note.as_deref(), Some(c::shortcuts_one_button_hold()));
    let pulse = with(&device, &store, |ctx| {
        row.adjust(Field::FiresOn, -1, ctx, &mut fx)
    });
    assert!(matches!(pulse, MenuPulse::Boundary));
    let last = saved(&store).bindings.pop().unwrap();
    assert_eq!(last.trigger, pad(&["start"], true));
    assert_eq!(last.action, Action::parse("stats"));
}

/// A new key needs Ctrl, Alt or Shift; the row stays open with the line until it has one.
#[test]
fn a_bare_key_stays_unsaved_with_its_line() {
    let store = store();
    let device = desktop();
    let mut row = with(&device, &store, |ctx| {
        ShortcutRowScreen::add(ctx, Scope::Global, Client::Desktop, Device::Keyboard)
    });
    let mut fx = Outbox::default();
    with(&device, &store, |ctx| {
        row.picked(Picked::Chose("mic".into()), ctx, &mut fx);
        row.tray_msg(TrayMsg::Pick("k"), ctx, &mut fx);
    });
    assert_eq!(row.line.as_deref(), Some(c::shortcuts_needs_modifier()));
    assert!(store.load().shortcuts.is_empty());
    with(&device, &store, |ctx| {
        row.activate(Field::Modifier(0), ctx, &mut fx)
    });
    assert_eq!(row.line, None);
    let last = saved(&store).bindings.pop().unwrap();
    assert_eq!(last.trigger.label(Client::Desktop), "Ctrl+K");
}

/// `PF_CONSOLE_DUMP=dir cargo test -p pf-console-ui --lib dump_shortcuts -- --ignored`
/// writes the page and a row for a look.
#[test]
#[ignore]
fn dump_shortcuts() {
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP");
    let fonts = crate::theme::build_fonts().unwrap();
    let store = store();
    let device = desktop();
    let rect = Rect::from_xywh(0.0, 0.0, 1280.0, 800.0);
    let shoot = |name: &str, view: &mut dyn ScreenView| {
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        for _ in 0..30 {
            let canvas = surface.canvas();
            canvas.clear(skia_safe::Color::from_rgb(18, 20, 28));
            with(&device, &store, |ctx| {
                view.render(surface.canvas(), rect, 1.0, 1.0 / 30.0, &fonts, ctx)
            });
        }
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
    };
    let mut page = with(&device, &store, |ctx| {
        ShortcutsScreen::new(ctx, Scope::Global)
    });
    shoot("shortcuts", &mut page);
    let mut row = with(&device, &store, |ctx| {
        ShortcutRowScreen::edit(ctx, Scope::Global, Client::Desktop, 1)
    });
    shoot("shortcut-row", &mut row);
    row.start_recording();
    row.recorder.as_mut().unwrap().chord = bits(&["select", "a"]);
    shoot("shortcut-record", &mut row);
}
