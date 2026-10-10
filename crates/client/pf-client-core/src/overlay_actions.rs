//! In-stream quick-action ring: the `overlay_actions` JSON blob (schema `v: 2`).
//! Six slots clockwise from 12 o'clock, custom shortcuts, and the virtual pad
//! preset. Design: `design/touch-client-overlay.md`.
//!
//! [`OverlayConfig::parse`] never fails. Short rings pad empty, long ones
//! truncate; unknown ids and dangling `shortcut:` refs become empty slots;
//! absent fields take defaults; unparseable blobs take the platform default.
//! Presets sync across client versions, so a newer ring must degrade quietly.
//!
//! Swift (`OverlayActions.swift`) and Kotlin (`OverlayActions.kt`) mirror this
//! file. All three replay `clients/shared/overlay-actions-vectors.json`; a new
//! slot id or parse rule lands there first.

use punktfunk_core::config::GamepadPref;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Clockwise from 12 o'clock.
pub const RING_SLOTS: usize = 6;

/// `Host` is a host-advertised id (`power.sleep`); `Shortcut` is an id in
/// [`OverlayConfig::shortcuts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotId {
    EndStream,
    /// End the game this device launched, then the stream.
    EndGame,
    DisconnectLinger,
    TouchMode,
    Keyboard,
    Stats,
    Mic,
    Pad,
    SendText,
    /// The host's guide button (Xbox / PS / Steam), as a synthetic pad tap.
    Guide,
    /// The host's quick-access button — `BTN_MISC1`, the Deck's `…`.
    Qam,
    /// Controller mouse: the pad drives the host pointer instead of its virtual pad.
    PadMouse,
    /// Step the controller type the host emulates, live ([`next_pad_type`]).
    PadType,
    /// Silence this client's speakers. Local: the host keeps playing for anyone joined to it.
    StreamMute,
    /// A dual-screen handheld's two screens trade the picture and the companion panel.
    SwapScreens,
    Host(String),
    Shortcut(String),
}

impl SlotId {
    /// Inverse of [`SlotId::parse`].
    pub fn id(&self) -> String {
        match self {
            SlotId::EndStream => "end_stream".into(),
            SlotId::EndGame => "end_game".into(),
            SlotId::DisconnectLinger => "disconnect_linger".into(),
            SlotId::TouchMode => "touch_mode".into(),
            SlotId::Keyboard => "keyboard".into(),
            SlotId::Stats => "stats".into(),
            SlotId::Mic => "mic".into(),
            SlotId::Pad => "pad".into(),
            SlotId::SendText => "send_text".into(),
            SlotId::Guide => "guide".into(),
            SlotId::Qam => "qam".into(),
            SlotId::PadMouse => "pad_mouse".into(),
            SlotId::PadType => "pad_type".into(),
            SlotId::StreamMute => "stream_mute".into(),
            SlotId::SwapScreens => "swap_screens".into(),
            SlotId::Host(id) => format!("host:{id}"),
            SlotId::Shortcut(id) => format!("shortcut:{id}"),
        }
    }

    /// `None` is an empty slot: unknown to this build.
    pub fn parse(s: &str) -> Option<SlotId> {
        Some(match s {
            "end_stream" => SlotId::EndStream,
            "end_game" => SlotId::EndGame,
            "disconnect_linger" => SlotId::DisconnectLinger,
            "touch_mode" => SlotId::TouchMode,
            "keyboard" => SlotId::Keyboard,
            "stats" => SlotId::Stats,
            "mic" => SlotId::Mic,
            "pad" => SlotId::Pad,
            "send_text" => SlotId::SendText,
            "guide" => SlotId::Guide,
            "qam" => SlotId::Qam,
            "pad_mouse" => SlotId::PadMouse,
            "pad_type" => SlotId::PadType,
            "stream_mute" => SlotId::StreamMute,
            "swap_screens" => SlotId::SwapScreens,
            _ => {
                if let Some(id) = s.strip_prefix("host:").filter(|id| !id.is_empty()) {
                    SlotId::Host(id.into())
                } else if let Some(id) = s.strip_prefix("shortcut:").filter(|id| !id.is_empty()) {
                    SlotId::Shortcut(id.into())
                } else {
                    return None;
                }
            }
        })
    }
}

/// Chord stored as keymap names (`ctrl`, `f4`, `a`), never VKs, so one
/// preset fires on every client.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub keys: Vec<String>,
}

pub use punktfunk_core::input::key_vk;

pub fn chord_chip(keys: &[String]) -> String {
    keys.iter()
        .map(|k| key_legend(k))
        .collect::<Vec<_>>()
        .join("+")
}

/// Keycap word (`Ctrl`, `Esc`, `PgUp`); arrows as arrows. No ❖/⇧ — they read
/// as nothing to most people.
pub fn key_legend(k: &str) -> String {
    match k.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => "Ctrl".to_string(),
        "shift" => "Shift".into(),
        "alt" | "option" => "Alt".into(),
        "win" | "cmd" | "super" | "meta" => "Win".into(),
        "escape" | "esc" => "Esc".into(),
        "enter" | "return" => "Enter".into(),
        "backspace" => "Backspace".into(),
        "delete" | "del" => "Del".into(),
        "insert" => "Ins".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "printscreen" => "PrtSc".into(),
        "capslock" => "Caps".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The shortcut editors' modifier toggles, in host send order.
pub const CHORD_MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "win"];

/// Every name [`key_vk`] knows that is not a modifier, row by row as a keyboard lays them
/// out. The console, GTK and WinUI shortcut editors all draw these rows.
pub const KEY_GRID: [&[&str]; 6] = [
    &[
        "escape", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
    ],
    &[
        "1",
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "8",
        "9",
        "0",
        "backspace",
    ],
    &[
        "tab", "q", "w", "e", "r", "t", "y", "u", "i", "o", "p", "insert", "delete",
    ],
    &[
        "capslock", "a", "s", "d", "f", "g", "h", "j", "k", "l", "enter",
    ],
    &[
        "z", "x", "c", "v", "b", "n", "m", "home", "end", "pageup", "pagedown",
    ],
    &[
        "space",
        "left",
        "up",
        "down",
        "right",
        "printscreen",
        "pause",
    ],
];

/// A shortcut's chord as an editor holds it: one toggle per [`CHORD_MODIFIERS`] entry and
/// one [`KEY_GRID`] key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chord {
    pub mods: [bool; 4],
    pub key: Option<String>,
}

impl Chord {
    /// Read stored keys: modifier aliases (`control`, `option`, `cmd`, `super`, `meta`) fold
    /// onto their toggle, and the last grid key wins.
    pub fn parse(keys: &[String]) -> Chord {
        let has = |names: &[&str]| keys.iter().any(|k| names.contains(&k.as_str()));
        Chord {
            mods: [
                has(&["ctrl", "control"]),
                has(&["alt", "option"]),
                has(&["shift"]),
                has(&["win", "cmd", "super", "meta"]),
            ],
            key: keys
                .iter()
                .rev()
                .find(|k| KEY_GRID.iter().any(|row| row.contains(&k.as_str())))
                .cloned(),
        }
    }

    /// Host send order: marked modifiers, then the key.
    pub fn keys(&self) -> Vec<String> {
        let mut v: Vec<String> = CHORD_MODIFIERS
            .iter()
            .zip(self.mods)
            .filter(|(_, on)| *on)
            .map(|(m, _)| m.to_string())
            .collect();
        v.extend(self.key.clone());
        v
    }
}

/// Scale a blob may claim for one pad control; ports clamp to this range.
pub const PAD_TWEAK_SCALE_MIN: f32 = 0.5;
pub const PAD_TWEAK_SCALE_MAX: f32 = 2.0;

/// Per-control override. `x`/`y` are centre as fractions of the layer;
/// `hidden` drops it from the stream (the editor still ghosts it). Absent
/// fields keep the preset.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PadTweak {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// Virtual controller (Android and Apple). `layout` is `full`, `sticks` or
/// `dpad`. `controls` / `controls_narrow` are keyed by id (`ls`, `rs`,
/// `dpad`, `face`, `lb`/`rb`, `lt`/`rt`, `select`, `guide`, `start`);
/// unknown ids ride through a rewrite, same as unknown ring slots.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PadConfig {
    pub layout: String,
    pub opacity: f32,
    pub scale: f32,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub controls: BTreeMap<String, PadTweak>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub controls_narrow: BTreeMap<String, PadTweak>,
}

impl Default for PadConfig {
    fn default() -> Self {
        PadConfig {
            layout: "full".into(),
            opacity: 0.45,
            scale: 1.0,
            controls: BTreeMap::new(),
            controls_narrow: BTreeMap::new(),
        }
    }
}

/// Touch rings include keyboard and pad; desktop rings include linger and
/// send-text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingPlatform {
    Touch,
    Desktop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlayConfig {
    pub ring: [Option<SlotId>; RING_SLOTS],
    pub shortcuts: Vec<Shortcut>,
    pub pad: PadConfig,
}

/// On-disk blob. Every field defaults so parse stays lenient.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Raw {
    v: u32,
    ring: Vec<Option<String>>,
    shortcuts: Vec<Shortcut>,
    pad: PadConfig,
}

impl OverlayConfig {
    pub const SCHEMA_VERSION: u32 = 2;

    pub fn platform_default(platform: RingPlatform) -> Self {
        let ring = match platform {
            RingPlatform::Touch => [
                Some(SlotId::EndStream),
                Some(SlotId::Keyboard),
                Some(SlotId::TouchMode),
                Some(SlotId::Stats),
                Some(SlotId::Mic),
                Some(SlotId::Pad),
            ],
            RingPlatform::Desktop => [
                Some(SlotId::EndStream),
                Some(SlotId::DisconnectLinger),
                Some(SlotId::TouchMode),
                Some(SlotId::Stats),
                Some(SlotId::Mic),
                Some(SlotId::SendText),
            ],
        };
        OverlayConfig {
            ring,
            shortcuts: Vec::new(),
            pad: PadConfig::default(),
        }
    }

    /// Empty or unparseable → platform default; else slot-by-slot (module docs).
    pub fn parse(json: &str, platform: RingPlatform) -> Self {
        if json.trim().is_empty() {
            return Self::platform_default(platform);
        }
        let raw: Raw = match serde_json::from_str(json) {
            Ok(r) => r,
            Err(_) => return Self::platform_default(platform),
        };
        let shortcuts: Vec<Shortcut> = raw
            .shortcuts
            .into_iter()
            .filter(|s| !s.id.is_empty())
            .collect();
        let mut ring: [Option<SlotId>; RING_SLOTS] = Default::default();
        for (slot, id) in ring.iter_mut().zip(raw.ring) {
            *slot = id.as_deref().and_then(SlotId::parse).filter(|s| match s {
                SlotId::Shortcut(id) => shortcuts.iter().any(|sc| &sc.id == id),
                _ => true,
            });
        }
        OverlayConfig {
            ring,
            shortcuts,
            pad: raw.pad,
        }
    }

    pub fn to_json(&self) -> String {
        let raw = Raw {
            v: Self::SCHEMA_VERSION,
            ring: self
                .ring
                .iter()
                .map(|s| s.as_ref().map(SlotId::id))
                .collect(),
            shortcuts: self.shortcuts.clone(),
            pad: self.pad.clone(),
        };
        serde_json::to_string(&raw).expect("plain data serializes")
    }

    pub fn shortcut(&self, id: &str) -> Option<&Shortcut> {
        self.shortcuts.iter().find(|s| s.id == id)
    }

    /// Insert or replace. New ids are `s<n>` into the first empty ring slot.
    /// One implementation for every editor so a shortcut lands the same everywhere.
    pub fn upsert_shortcut(&mut self, id: Option<&str>, label: &str, keys: Vec<String>) -> String {
        let label = label.trim().to_string();
        if let Some(sc) = id.and_then(|id| self.shortcuts.iter_mut().find(|s| s.id == id)) {
            sc.label = label;
            sc.keys = keys;
            return sc.id.clone();
        }
        let next = self
            .shortcuts
            .iter()
            .filter_map(|s| s.id.trim_start_matches('s').parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        let id = format!("s{next}");
        if let Some(slot) = self.ring.iter_mut().find(|s| s.is_none()) {
            *slot = Some(SlotId::Shortcut(id.clone()));
        }
        self.shortcuts.push(Shortcut {
            id: id.clone(),
            label,
            keys,
        });
        id
    }

    /// Drop the shortcut and empty the ring slot that pointed at it (`parse`
    /// would on the next read; doing it here shows it at once).
    pub fn remove_shortcut(&mut self, id: &str) {
        self.shortcuts.retain(|s| s.id != id);
        for slot in self.ring.iter_mut() {
            if matches!(slot, Some(SlotId::Shortcut(s)) if s == id) {
                *slot = None;
            }
        }
    }
}

/// One catalogue row. Empty `id` is the empty slot; empty `note` means
/// available on this platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogueEntry {
    pub id: String,
    pub label: String,
    pub note: String,
}

/// Editor group, in display order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogueGroup {
    pub title: &'static str,
    pub entries: Vec<CatalogueEntry>,
}

/// One table every editor renders. Notes are the platform's: desktop has no
/// virtual pad and no typed-text path; a phone has both. Empty last.
pub fn catalogue(cfg: &OverlayConfig, platform: RingPlatform) -> Vec<CatalogueGroup> {
    let e = |id: &str, label: &str, note: &str| CatalogueEntry {
        id: id.into(),
        label: label.into(),
        note: note.into(),
    };
    let desktop = platform == RingPlatform::Desktop;
    let host = "Only where the host offers it";
    let mut g = vec![
        CatalogueGroup {
            title: "Session",
            entries: vec![
                e("end_stream", "End stream", ""),
                e("end_game", "End game", "Only a game this device launched"),
                e("disconnect_linger", "Disconnect, keep the game running", ""),
            ],
        },
        CatalogueGroup {
            title: "Input",
            entries: vec![
                e("touch_mode", "Touch mode", ""),
                e("keyboard", "Keyboard", ""),
                e(
                    "pad",
                    "Virtual controller",
                    if desktop {
                        "Phones and tablets only"
                    } else {
                        ""
                    },
                ),
                e(
                    "send_text",
                    "Send text",
                    if desktop {
                        "Not on this client yet"
                    } else {
                        ""
                    },
                ),
                e("guide", "Guide button", ""),
                e(
                    "qam",
                    "Quick access menu",
                    "Only where the host's pad is Steam-shaped",
                ),
                e("pad_mouse", "Controller mouse", ""),
                e("pad_type", "Controller type", "For this stream"),
            ],
        },
        CatalogueGroup {
            title: "View",
            entries: vec![
                e("stats", "Statistics", ""),
                e("swap_screens", "Swap screens", "Dual-screen handhelds only"),
            ],
        },
        CatalogueGroup {
            title: "Audio",
            entries: vec![
                e("mic", "Microphone", ""),
                e("stream_mute", "Mute this stream", "This device only"),
            ],
        },
        CatalogueGroup {
            title: "Host",
            entries: vec![
                e("host:power.sleep", "Sleep host", host),
                e("host:power.reboot", "Restart host", host),
                e("host:power.shutdown", "Shut down host", host),
            ],
        },
    ];
    if !cfg.shortcuts.is_empty() {
        g.push(CatalogueGroup {
            title: "Shortcuts",
            entries: cfg
                .shortcuts
                .iter()
                .map(|sc| {
                    let chip = chord_chip(&sc.keys);
                    if sc.label.is_empty() {
                        e(&format!("shortcut:{}", sc.id), &chip, "")
                    } else {
                        e(&format!("shortcut:{}", sc.id), &sc.label, &chip)
                    }
                })
                .collect(),
        });
    }
    g.push(CatalogueGroup {
        title: "Empty",
        entries: vec![e("", "Empty slot", "")],
    });
    g
}

/// Lucide name for a wire id. `mic` swaps to `mic-off` while muted; `more` is
/// the ring centre. `None` for a shortcut (the chord is the face) and any host
/// action beyond the three powers. One table so stream and editor cannot
/// disagree; Rust shells key [`crate::lucide`], Windows keys a baked PNG.
pub fn slot_icon(id: &str, state: &str) -> Option<&'static str> {
    Some(match id {
        "end_stream" => "square",
        "end_game" => "x",
        "disconnect_linger" => "log-out",
        "touch_mode" => "pointer",
        "keyboard" => "keyboard",
        "stats" => "chart-column",
        "mic" if state == "Muted" => "mic-off",
        "mic" => "mic",
        "pad" => "gamepad-2",
        "send_text" => "send",
        "guide" => "house",
        "qam" => "panel-right",
        "pad_mouse" => "mouse",
        "pad_type" => "gamepad-2",
        "stream_mute" => "volume-2",
        "swap_screens" => "arrow-up-down",
        "more" => "ellipsis",
        "host:power.sleep" => "moon",
        "host:power.reboot" => "rotate-cw",
        "host:power.shutdown" => "power",
        _ => return None,
    })
}

/// What the Controller type slot steps through: Automatic, then the pads every host builds.
pub const PAD_TYPE_CYCLE: [GamepadPref; 6] = [
    GamepadPref::Auto,
    GamepadPref::Xbox360,
    GamepadPref::XboxOne,
    GamepadPref::DualSense,
    GamepadPref::DualShock4,
    GamepadPref::SteamDeck,
];

/// The type after `current` in [`PAD_TYPE_CYCLE`]. A type outside it, picked in Settings,
/// steps back to Automatic.
pub fn next_pad_type(current: GamepadPref) -> GamepadPref {
    let at = PAD_TYPE_CYCLE.iter().position(|&p| p == current);
    at.map_or(GamepadPref::Auto, |i| {
        PAD_TYPE_CYCLE[(i + 1) % PAD_TYPE_CYCLE.len()]
    })
}

/// `(name, short)` for the Controller type slot: the sheet's value and the dial's face.
pub fn pad_type_label(pref: GamepadPref) -> (&'static str, &'static str) {
    match pref {
        GamepadPref::Auto => ("Automatic", "Pad type"),
        GamepadPref::Xbox360 => ("Xbox 360", "Xbox 360"),
        GamepadPref::XboxOne => ("Xbox One", "Xbox One"),
        GamepadPref::DualSense => ("DualSense", "DualSense"),
        GamepadPref::DualShock4 => ("DualShock 4", "DS4"),
        GamepadPref::SteamDeck => ("Steam Deck", "Deck"),
        other => (other.as_str(), other.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_grouped_noted_per_platform_and_ends_with_empty() {
        let blob = r#"{"v":2,"ring":[],"shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]},{"id":"s2","keys":["alt","f4"]}]}"#;
        let cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        let groups = catalogue(&cfg, RingPlatform::Desktop);
        let titles: Vec<&str> = groups.iter().map(|g| g.title).collect();
        assert_eq!(
            titles,
            [
                "Session",
                "Input",
                "View",
                "Audio",
                "Host",
                "Shortcuts",
                "Empty"
            ]
        );
        let pad = &groups[1].entries[2];
        assert_eq!(
            (pad.id.as_str(), pad.note.as_str()),
            ("pad", "Phones and tablets only")
        );
        let phone = catalogue(&cfg, RingPlatform::Touch);
        assert_eq!(phone[1].entries[2].note, "", "a phone has the pad");
        let s = &groups[5].entries;
        assert_eq!(
            (s[0].id.as_str(), s[0].label.as_str(), s[0].note.as_str()),
            ("shortcut:s1", "Task Manager", "Ctrl+Shift+Esc")
        );
        assert_eq!(
            (s[1].id.as_str(), s[1].label.as_str(), s[1].note.as_str()),
            ("shortcut:s2", "Alt+F4", "")
        );
        assert_eq!(groups[6].entries[0].id, "");
        let none = catalogue(
            &OverlayConfig::parse("", RingPlatform::Desktop),
            RingPlatform::Desktop,
        );
        assert!(none.iter().all(|g| g.title != "Shortcuts"));
    }

    #[test]
    fn a_shortcut_is_upserted_by_id_and_takes_the_first_empty_slot() {
        let mut cfg = OverlayConfig::parse(
            r#"{"v":2,"ring":["end_stream",null,null,null,null,null]}"#,
            RingPlatform::Desktop,
        );
        let id = cfg.upsert_shortcut(
            None,
            " Task Manager ",
            vec!["ctrl".into(), "shift".into(), "escape".into()],
        );
        assert_eq!(id, "s1");
        assert_eq!(cfg.ring[1], Some(SlotId::Shortcut("s1".into())));
        assert_eq!(cfg.shortcuts[0].label, "Task Manager");
        let again = cfg.upsert_shortcut(Some("s1"), "Tasks", vec!["ctrl".into(), "escape".into()]);
        assert_eq!(again, "s1");
        assert_eq!(cfg.shortcuts.len(), 1);
        assert_eq!(cfg.shortcuts[0].keys, vec!["ctrl", "escape"]);
        let second = cfg.upsert_shortcut(None, "", vec!["alt".into(), "f4".into()]);
        assert_eq!(second, "s2");
        assert_eq!(cfg.ring[2], Some(SlotId::Shortcut("s2".into())));
        cfg.remove_shortcut("s1");
        assert_eq!(cfg.shortcuts.len(), 1);
        assert_eq!(cfg.ring[1], None);
        assert_eq!(cfg.ring[2], Some(SlotId::Shortcut("s2".into())));
    }

    /// `clients/shared/overlay-actions-vectors.json`, which the Swift and Kotlin twins replay.
    fn vectors() -> serde_json::Value {
        let raw = include_str!("../../../../clients/shared/overlay-actions-vectors.json");
        serde_json::from_str(raw).expect("vector file parses")
    }

    /// JSON equality with every number compared as f32: the blob stores f32s, and each
    /// client prints them its own way.
    fn same_json(a: &serde_json::Value, b: &serde_json::Value) -> bool {
        use serde_json::Value::{Array, Number, Object};
        match (a, b) {
            (Number(x), Number(y)) => x.as_f64().map(|v| v as f32) == y.as_f64().map(|v| v as f32),
            (Array(x), Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_json(x, y))
            }
            (Object(x), Object(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .all(|(k, v)| y.get(k).is_some_and(|w| same_json(v, w)))
            }
            _ => a == b,
        }
    }

    #[test]
    fn shared_vectors_parse_and_round_trip() {
        let file = vectors();
        for id in file["slot_ids"].as_array().expect("slot_ids") {
            let id = id.as_str().unwrap();
            assert_eq!(SlotId::parse(id).map(|s| s.id()).as_deref(), Some(id));
        }
        let cases = file["cases"].as_array().expect("cases");
        assert!(cases.len() >= 10, "the vector file is the contract");
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let platform = match case["platform"].as_str().unwrap() {
                "touch" => RingPlatform::Touch,
                "desktop" => RingPlatform::Desktop,
                other => panic!("{name}: platform {other}"),
            };
            let cfg = OverlayConfig::parse(case["blob"].as_str().unwrap(), platform);
            let ring: Vec<Option<String>> = cfg
                .ring
                .iter()
                .map(|s| s.as_ref().map(SlotId::id))
                .collect();
            let want: Vec<Option<String>> = serde_json::from_value(case["ring"].clone()).unwrap();
            assert_eq!(ring, want, "{name}: ring");
            let stored: serde_json::Value = serde_json::from_str(&cfg.to_json()).unwrap();
            assert!(
                same_json(&stored, &case["round_trip"]),
                "{name}: stored {stored}"
            );
            assert_eq!(
                OverlayConfig::parse(&cfg.to_json(), platform),
                cfg,
                "{name}: reparse"
            );
        }
    }

    #[test]
    fn shared_pad_type_cycle() {
        let file = vectors();
        let pref = |v: &serde_json::Value| GamepadPref::from_name(v.as_str().unwrap()).unwrap();
        let cycle: Vec<GamepadPref> = file["pad_type_cycle"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let p = pref(&row["name"]);
                assert_eq!(pad_type_label(p).0, row["label"].as_str().unwrap());
                p
            })
            .collect();
        assert_eq!(cycle, PAD_TYPE_CYCLE);
        for (i, &p) in cycle.iter().enumerate() {
            assert_eq!(next_pad_type(p), cycle[(i + 1) % cycle.len()]);
        }
        for name in file["pad_type_outside_cycle"].as_array().unwrap() {
            assert_eq!(next_pad_type(pref(name)), GamepadPref::Auto, "{name}");
        }
        assert_eq!(pad_type_label(GamepadPref::DualShock4).1, "DS4");
    }

    #[test]
    fn key_names_map_to_windows_vks() {
        assert_eq!(key_vk("ctrl"), Some(0x11));
        assert_eq!(key_vk("Shift"), Some(0x10));
        assert_eq!(key_vk("escape"), Some(0x1B));
        assert_eq!(key_vk("tab"), Some(0x09));
        assert_eq!(key_vk("a"), Some(0x41));
        assert_eq!(key_vk("z"), Some(0x5A));
        assert_eq!(key_vk("0"), Some(0x30));
        assert_eq!(key_vk("f1"), Some(0x70));
        assert_eq!(key_vk("f12"), Some(0x7B));
        assert_eq!(key_vk("f25"), None);
        assert_eq!(key_vk("hyper"), None);
        assert_eq!(key_vk(""), None);
        let keys: Vec<String> = ["ctrl", "shift", "escape"].map(String::from).into();
        assert_eq!(chord_chip(&keys), "Ctrl+Shift+Esc");
        assert_eq!(key_legend("win"), "Win");
        assert_eq!(key_legend("pageup"), "PgUp");
        assert_eq!(key_legend("f4"), "F4");
        assert_eq!(key_legend("left"), "←");
    }

    #[test]
    fn the_key_grid_holds_every_key_once_and_no_modifier() {
        let mut seen: Vec<&str> = Vec::new();
        for name in KEY_GRID.iter().flat_map(|row| row.iter()) {
            assert!(key_vk(name).is_some(), "{name} is unknown to the wire");
            assert!(!CHORD_MODIFIERS.contains(name), "{name} is a modifier");
            assert!(!seen.contains(name), "{name} twice");
            seen.push(name);
        }
        assert_eq!(seen.len(), 66);
        for m in CHORD_MODIFIERS {
            assert!(key_vk(m).is_some(), "{m} is unknown to the wire");
        }
    }

    #[test]
    fn a_chord_folds_aliases_and_sends_modifiers_first() {
        let keys = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let c = Chord::parse(&keys(&["escape", "control", "meta", "a"]));
        assert_eq!(c.mods, [true, false, false, true]);
        assert_eq!(c.key.as_deref(), Some("a"), "the last grid key wins");
        assert_eq!(c.keys(), keys(&["ctrl", "win", "a"]));
        assert_eq!(Chord::parse(&keys(&["option", "hyper"])).key, None);
    }

    #[test]
    fn every_built_in_slot_names_an_icon_that_ships() {
        let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
        for group in catalogue(&cfg, RingPlatform::Desktop) {
            for entry in group.entries {
                if entry.id.is_empty() {
                    continue; // empty slot draws a plus, not a slot icon
                }
                let name =
                    slot_icon(&entry.id, "").unwrap_or_else(|| panic!("{} has no icon", entry.id));
                assert!(
                    crate::lucide::path(name).is_some(),
                    "{}: the set does not ship '{name}'",
                    entry.id
                );
            }
        }
        assert_eq!(slot_icon("more", ""), Some("ellipsis"), "the ring's centre");
        assert_eq!(slot_icon("mic", "Muted"), Some("mic-off"));
        assert_eq!(slot_icon("host:custom.eject", ""), None);
        assert_eq!(
            slot_icon("shortcut:s1", ""),
            None,
            "a chord IS its own face"
        );
    }
}
