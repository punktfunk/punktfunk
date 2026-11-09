use super::*;
use punktfunk_core::input::key_vk;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use Press::{Hold, Keys};

const VECTORS: &str = include_str!("../../../../../clients/shared/shortcut-vectors.json");

fn section(name: &str) -> Vec<Value> {
    let file: Value = serde_json::from_str(VECTORS).expect("shortcut-vectors.json parses");
    file[name]["cases"]
        .as_array()
        .expect("a cases list")
        .clone()
}

fn blob(bindings: &Value) -> String {
    json!({"v": 1, "bindings": bindings}).to_string()
}

fn client(name: &str) -> Client {
    match name {
        "desktop" => Client::Desktop,
        "android" => Client::Android,
        "apple" => Client::Apple,
        "mac" => Client::Mac,
        "webos" => Client::WebOS,
        "web" => Client::Web,
        "tizen" => Client::Tizen,
        "ps5" => Client::Ps5,
        "switch" => Client::Switch,
        other => panic!("a client the vectors name: {other}"),
    }
}

const ALL: [Client; 9] = [
    Client::Desktop,
    Client::Android,
    Client::Apple,
    Client::Mac,
    Client::WebOS,
    Client::Web,
    Client::Tizen,
    Client::Ps5,
    Client::Switch,
];

#[test]
fn parse_vectors() {
    for case in section("parse") {
        let name = case["name"].as_str().unwrap();
        let raw = case["blob"].as_str().unwrap();
        let got: Value = serde_json::from_str(&Shortcuts::parse(raw).to_json()).unwrap();
        assert_eq!(got, case["expect"], "{name}");
        let marker = Shortcuts {
            bindings: vec![Binding {
                trigger: Trigger::Pad {
                    buttons: vec![button_bit("y").unwrap()],
                    hold: true,
                },
                action: Action::Ring,
            }],
        };
        let resolved = Shortcuts::read(raw).unwrap_or_else(|| marker.clone());
        assert_eq!(
            resolved == marker,
            case["defaults"] == true,
            "{name}: defaults"
        );
    }
}

#[test]
fn defaults_vectors() {
    for case in section("defaults") {
        let c = client(case["client"].as_str().unwrap());
        let game_mode = case["game_mode"].as_bool().unwrap();
        let legacy = case["guide_gesture"].as_str().unwrap();
        let got: Value = serde_json::from_str(&defaults(c, game_mode, legacy).to_json()).unwrap();
        assert_eq!(got["bindings"], case["expect"], "{case}");
        assert_eq!(
            resolve("", c, game_mode, legacy),
            defaults(c, game_mode, legacy)
        );
    }
}

#[test]
fn validate_vectors() {
    for case in section("validate") {
        let name = case["name"].as_str().unwrap();
        let have = Shortcuts::parse(&blob(&case["bindings"]));
        assert_eq!(
            have.bindings.len(),
            case["bindings"].as_array().unwrap().len()
        );
        let candidate = Shortcuts::parse(&blob(&json!([case["candidate"]])))
            .bindings
            .pop()
            .expect("the candidate parses");
        let index = || case["index"].as_u64().unwrap() as usize;
        let want = match case["expect"].as_str().unwrap() {
            "ok" => Ok(()),
            "identical" => Err(Conflict::Identical(index())),
            "press_subset" => Err(Conflict::PressSubset(index())),
            "single_press" => Err(Conflict::SinglePress),
            "no_modifier" => Err(Conflict::NoModifier),
            "no_key" => Err(Conflict::NoKey),
            other => panic!("{name}: an expect the vectors use: {other}"),
        };
        assert_eq!(have.validate(&candidate), want, "{name}");
    }
}

#[test]
fn engine_vectors() {
    for case in section("engine") {
        let name = case["name"].as_str().unwrap();
        let device = match case["device"].as_str().unwrap() {
            "pad" => Device::Pad,
            "key" => Device::Keyboard,
            other => panic!("{name}: a device: {other}"),
        };
        let id = |v: &Value| -> u32 {
            let n = v.as_str().unwrap();
            match device {
                Device::Pad => button_bit(n),
                Device::Keyboard => key_vk(n).map(u32::from),
            }
            .unwrap_or_else(|| panic!("{name}: a name: {n}"))
        };
        let bindings = Shortcuts::parse(&blob(&case["bindings"])).bindings;
        assert_eq!(
            bindings.len(),
            case["bindings"].as_array().unwrap().len(),
            "{name}"
        );
        let mut engine = Engine::new(&bindings, device);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);

        // (t, order, step): events before the tick at one t, flush last.
        let mut timeline: Vec<(u64, u8, Option<Event>)> = Vec::new();
        for e in case["events"].as_array().unwrap() {
            let bit = id(&e["bit"]);
            let ev = if e["down"].as_bool().unwrap() {
                Event::Press(bit)
            } else {
                Event::Release(bit)
            };
            timeline.push((e["t"].as_u64().unwrap(), 0, Some(ev)));
        }
        for t in case["ticks"].as_array().unwrap() {
            timeline.push((t.as_u64().unwrap(), 1, None));
        }
        if let Some(t) = case["flush"].as_u64() {
            timeline.push((t, 2, None));
        }
        timeline.sort_by_key(|&(t, order, _)| (t, order));

        let mut got = Vec::new();
        for (t, order, ev) in timeline {
            let outs = match (order, ev) {
                (0, Some(ev)) => engine.step(ev, at(t)),
                (1, _) => engine.tick(at(t)),
                _ => engine.flush(),
            };
            got.extend(outs.into_iter().map(|o| (t, o)));
        }
        let want: Vec<(u64, Out)> = case["expect"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let out = &e["out"];
                let o = if let Some(f) = out.get("forward") {
                    Out::Forward(id(&f[0]), f[1].as_bool().unwrap())
                } else if let Some(f) = out.get("fire") {
                    let phase = match f[1].as_str().unwrap() {
                        "once" => Phase::Once,
                        "down" => Phase::Down,
                        "up" => Phase::Up,
                        "tap" => Phase::Tap,
                        other => panic!("{name}: a phase: {other}"),
                    };
                    Out::Fire(Action::parse(f[0].as_str().unwrap()), phase)
                } else {
                    Out::Lift(out["lift"].as_array().unwrap().iter().map(id).collect())
                };
                (e["t"].as_u64().unwrap(), o)
            })
            .collect();
        assert_eq!(got, want, "{name}");
    }
}

/// Every client lists its dial chord and its exit chord; the PS5's use the touchpad for Select.
#[test]
fn every_client_lists_the_controller_chords() {
    for c in ALL {
        let pad = groups(c, true).pop().unwrap();
        let (dial, exit) = match c {
            Client::Ps5 => ("Touchpad + Cross", "L1 + R1 + Options + Touchpad"),
            Client::Switch => ("Minus + A", "L + R + Plus + Minus"),
            _ => ("Select + A", "L1 + R1 + Start + Select"),
        };
        assert!(
            pad.items.iter().any(|i| i.keys == Keys(dial)),
            "{c:?} lacks {dial}"
        );
        assert!(
            pad.items.iter().any(|i| i.keys == Hold(exit)),
            "{c:?} lacks {exit}"
        );
        let pressed = pad.items.iter().any(|i| i.keys == Keys(exit));
        assert_eq!(
            pressed,
            matches!(c, Client::Desktop | Client::Web | Client::Tizen),
            "{c:?}: the exit chord's press stage"
        );
    }
}

/// Only the native pad paths bind Select + X; webOS opens stats from the dial or Green,
/// the browser from the keyboard.
#[test]
fn the_stats_chord_is_native_only() {
    let has = |c| {
        groups(c, true)
            .iter()
            .flat_map(|g| &g.items)
            .any(|i| i.keys == Keys("Select + X"))
    };
    assert!(has(Client::Desktop) && has(Client::Android));
    assert!(has(Client::Apple) && has(Client::Mac));
    assert!(!has(Client::WebOS) && !has(Client::Web) && !has(Client::Tizen));
}

#[test]
fn an_apple_tv_lists_no_keys() {
    let keys = |c, touchscreen| {
        groups(c, touchscreen)
            .iter()
            .any(|g| g.kind == Kind::Keyboard)
    };
    assert!(keys(Client::Apple, true) && keys(Client::Mac, true));
    assert!(!keys(Client::Apple, false));
}

#[test]
fn a_tv_has_no_touch_group() {
    let touch = |touchscreen| {
        groups(Client::Android, touchscreen)
            .iter()
            .any(|g| g.kind == Kind::Touch)
    };
    assert!(!touch(false) && touch(true));
}

#[test]
fn only_android_lists_the_pad_mute() {
    let has = |c| {
        groups(c, true)
            .iter()
            .flat_map(|g| &g.items)
            .any(|i| i.keys == Keys("Select + Y"))
    };
    assert!(has(Client::Android));
    assert!(ALL.iter().filter(|c| has(**c)).count() == 1);
}

#[test]
fn no_label_is_empty_or_ends_in_a_period() {
    for c in ALL {
        let d = defaults(c, true, "on");
        for i in d.rows(c, true).iter().flat_map(|g| &g.items) {
            let text = i.text();
            assert!(!i.keys.to_string().is_empty() && !text.is_empty());
            assert!(!text.ends_with('.'), "{text}");
            // A default action always has the catalogue's word, never its id.
            if let Some(b) = i.binding {
                assert_ne!(text, d.bindings[b].action.id(), "{c:?}");
            }
        }
    }
}

/// A binding row edits its binding; a fixed row edits nothing. Two presses of one action on
/// one device share a row.
#[test]
fn rows_point_at_their_bindings() {
    let desktop = defaults(Client::Desktop, true, "auto");
    let rows = desktop.rows(Client::Desktop, true);
    let keys = rows.iter().find(|g| g.kind == Kind::Keyboard).unwrap();
    let fullscreen = keys
        .items
        .iter()
        .find(|i| i.keys == Press::Either("F11", "Alt+Enter"))
        .expect("one fullscreen row");
    let b = &desktop.bindings[fullscreen.binding.unwrap()];
    assert_eq!(
        b.action,
        Action::Slot(crate::overlay_actions::SlotId::Fullscreen)
    );
    let guide = rows
        .last()
        .unwrap()
        .items
        .iter()
        .find(|i| i.keys == Hold("Select"));
    assert_eq!(guide.and_then(|i| i.binding), Some(2));
    let touch = rows.iter().find(|g| g.kind == Kind::Touch).unwrap();
    assert!(touch.items.iter().all(|i| i.binding.is_none()));
    let shown: usize = rows
        .iter()
        .flat_map(|g| &g.items)
        .filter(|i| i.binding.is_some())
        .count();
    assert_eq!(
        shown,
        desktop.bindings.len() - 1,
        "F11 and Alt+Enter share a row"
    );
}

/// A client that writes chords and headings as text gets whole English lines.
#[test]
fn a_chord_and_a_group_read_as_one_line() {
    assert_eq!(Hold("Yellow").to_string(), "Hold Yellow");
    assert_eq!(
        Press::Either("F11", "Alt+Enter").to_string(),
        "F11 or Alt+Enter"
    );
    let desktop = groups(Client::Desktop, true);
    let headings: Vec<_> = desktop.iter().map(Group::heading).collect();
    assert_eq!(
        headings,
        [
            "Keyboard",
            "Touchscreen (Trackpad and Direct pointer modes)",
            "Controller (Select is Back or View)"
        ]
    );
    let pad = desktop.last().unwrap();
    assert_eq!(pad.note().as_deref(), Some("Select is Back or View"));
    let ps5 = groups(Client::Ps5, true).pop().unwrap();
    assert_eq!((ps5.title(), ps5.note()), ("DualSense", None));
    let switch = groups(Client::Switch, false);
    assert_eq!(switch.len(), 1, "a Switch lists its pad alone");
    assert_eq!((switch[0].title(), switch[0].note()), ("Controller", None));
    let mac = groups(Client::Mac, true);
    let fullscreen = mac[0]
        .items
        .iter()
        .find(|i| i.keys == Keys("Control+Command+F"))
        .unwrap()
        .text();
    assert!(fullscreen.contains("(after Command+Esc,"), "{fullscreen}");
    assert!(mac[0]
        .items
        .iter()
        .any(|i| i.keys == Keys("Control+Option+Shift+Q")));
}

/// A key combination row shows the player's name for it, or its keys; a host action this app
/// knows has its words; a dangling id stays an id.
#[test]
fn rows_in_names_key_combinations_and_host_actions() {
    use crate::overlay_actions::{OverlayConfig, RingPlatform};
    let cfg = OverlayConfig::parse(
        r#"{"v":2,"shortcuts":[
            {"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]},
            {"id":"s2","keys":["alt","tab"]}]}"#,
        RingPlatform::Desktop,
    );
    let s = Shortcuts::parse(
        r#"{"v":1,"bindings":[
            {"on":"pad","keys":["select"],"hold":true,"do":"shortcut:s1"},
            {"on":"pad","keys":["start"],"hold":true,"do":"shortcut:s2"},
            {"on":"pad","keys":["lb","rb"],"do":"shortcut:gone"},
            {"on":"pad","keys":["ls","rs"],"do":"host:display.next"},
            {"on":"pad","keys":["up","down"],"do":"host:custom.eject"}]}"#,
    );
    let texts = |groups: Vec<Group>| -> Vec<String> {
        let pad = groups.last().unwrap();
        pad.items.iter().map(|i| i.text().into_owned()).collect()
    };
    assert_eq!(
        texts(s.rows_in(&cfg, Client::Desktop, false)),
        [
            "Task Manager",
            "Alt+Tab",
            "shortcut:gone",
            pf_l10n::host::hostaction_display_next(),
            "host:custom.eject",
        ]
    );
    assert_eq!(texts(s.rows(Client::Desktop, false))[0], "shortcut:s1");
}

/// The editor checks a changed row against the others; every default row passes its own check.
#[test]
fn every_default_row_can_be_edited_in_place() {
    for c in ALL {
        let d = defaults(c, true, "on");
        for (i, b) in d.bindings.iter().enumerate() {
            let mut others = d.clone();
            others.bindings.remove(i);
            assert_eq!(others.validate(b), Ok(()), "{c:?} row {i}: {b:?}");
        }
    }
}

#[test]
fn a_hint_names_the_live_trigger() {
    use crate::overlay_actions::SlotId;
    let end = Action::Slot(SlotId::EndStream);
    let desktop = defaults(Client::Desktop, false, "auto");
    assert_eq!(
        desktop.hint(&end, Device::Pad).as_deref(),
        Some("Hold L1 + R1 + Start + Select")
    );
    assert_eq!(
        desktop.hint(&end, Device::Keyboard).as_deref(),
        Some("Ctrl+Alt+Shift+D")
    );
    let moved = Shortcuts::parse(
        r#"{"v":1,"bindings":[{"on":"pad","keys":["start","select"],"hold":true,"do":"end_stream"}]}"#,
    );
    assert_eq!(
        moved.hint(&end, Device::Pad).as_deref(),
        Some("Hold Start + Select")
    );
    assert_eq!(moved.hint(&end, Device::Keyboard), None);
    let switch = defaults(Client::Switch, false, "auto");
    assert_eq!(
        switch
            .hint_for(&Action::Ring, Device::Pad, Client::Switch)
            .as_deref(),
        Some("Minus + A")
    );
    assert_eq!(
        switch
            .hint_for(&end, Device::Pad, Client::Switch)
            .as_deref(),
        Some("Hold L + R + Plus + Minus")
    );
}

#[test]
fn every_button_name_round_trips() {
    for (name, bit) in BUTTONS {
        assert_eq!(button_bit(name), Some(bit));
        assert_eq!(button_name(bit), Some(name));
    }
    assert_eq!(button_bit("BACK"), button_bit("select"));
}

#[test]
fn a_blob_round_trips_through_the_editor() {
    for c in ALL {
        let d = defaults(c, true, "on");
        assert_eq!(Shortcuts::parse(&d.to_json()), d, "{c:?}");
    }
}
