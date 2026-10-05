//! The JSON a native host and the console exchange across a language boundary: what the host
//! hands at create, the entry, pad and preset pushes, and the events it reads back. Android (JNI)
//! and Apple (C ABI) both speak it, so Kotlin and Swift encode one contract.

use crate::store::PresetEntry;
use crate::{
    Console, ConsoleEntry, ConsoleOptions, DeviceScreen, HostRow, Key, Platform, SnapshotStore,
    Stale,
};
use pf_client_core::console::{OverlayAction, PointerButton, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse, PadBattery, PadInfo};
use pf_client_core::trust::{KnownHosts, Settings};
use punktfunk_core::config::GamepadPref;
use std::sync::Arc;

/// What the host hands the console at create.
#[derive(serde::Deserialize)]
pub struct CreateOptions {
    pub device_name: String,
    /// Skia's resource budget, bytes (the host sizes it from the device's memory class).
    pub gpu_cache_bytes: usize,
    /// Whether the touch shell exists as a fallback (phones/tablets; false on a TV) —
    /// gates the console-off settings row. Absent means don't offer it.
    #[serde(default)]
    pub fallback_ui: bool,
    /// A TV. Absent means a handheld or a desktop.
    #[serde(default)]
    pub tv: bool,
    /// The browser host runs as a Samsung TV app, and fronts the shell as [`Platform::Tizen`]
    /// rather than [`Platform::Web`]. Only that host sets it; the native hosts name their
    /// platform in code.
    #[serde(default)]
    pub tizen: bool,
    /// The host's own keyboard types into every field (`edit_text` tells it which), so the
    /// console never draws its tray: an Apple TV's, where iPhone typing works.
    #[serde(default)]
    pub system_keyboard: bool,
    /// Whether a real AV1 decoder exists, as the host's codec list answers it. Absent means
    /// don't claim the device lacks it, so the codec row stays unmarked.
    #[serde(default = "yes")]
    pub av1_ok: bool,
    /// Whether this device decodes PyroWave. A host that probes it in Rust overwrites it.
    #[serde(default)]
    pub pyrowave_ok: bool,
    /// The host answers `FetchProfiles`. Absent means it doesn't, and a connect sends the
    /// card's saved pick unchecked.
    #[serde(default)]
    pub profiles: bool,
    /// The settings snapshot the shell starts from.
    pub settings: Settings,
    /// The preset catalog as `[{id, name, overrides}, …]`.
    #[serde(default)]
    pub presets: Vec<PresetJson>,
    /// The known-hosts records, for building `punktfunk://` links.
    #[serde(default)]
    pub known_hosts: KnownHosts,
    /// Where to start: `{}` is Home, `{"library": <HostRow>}` a shelf.
    #[serde(default)]
    pub entry: EntryJson,
    /// This device's screen and its safe area as landscape `[w, h]`, for the Aspect row.
    /// Absent on a TV.
    #[serde(default)]
    pub screen: Option<(u32, u32)>,
    #[serde(default)]
    pub safe_area: Option<(u32, u32)>,
}

/// `#[serde(default)]` for a bool an older caller may omit and that must read `true`.
fn yes() -> bool {
    true
}

impl CreateOptions {
    /// The console's options, where it starts, and the settings store host and shell share.
    pub fn into_console(
        self,
        platform: Platform,
    ) -> (ConsoleOptions, ConsoleEntry, Arc<SnapshotStore>) {
        let store = Arc::new(SnapshotStore::new(
            self.settings,
            self.presets.into_iter().map(Into::into).collect(),
        ));
        store.set_known_hosts(self.known_hosts);
        let opts = ConsoleOptions {
            device_name: self.device_name,
            version: None,
            deck: self.system_keyboard,
            tv: self.tv,
            fallback_ui: self.fallback_ui,
            pyrowave_ok: self.pyrowave_ok,
            av1_ok: self.av1_ok,
            store: Some(store.clone()),
            platform,
            gpu_cache_bytes: self.gpu_cache_bytes.max(16 << 20),
            screen: self.screen.map(|full| DeviceScreen {
                full,
                safe: self.safe_area.unwrap_or(full),
            }),
            profiles: self.profiles,
        };
        (opts, self.entry.into_entry(), store)
    }
}

#[derive(serde::Deserialize, Default)]
pub struct EntryJson {
    #[serde(default)]
    library: Option<HostRow>,
    /// The same row, plus a connect to its desktop before the first frame. `library`
    /// wins if a caller sends both — a shelf is the safe half of the pair.
    #[serde(default)]
    stream: Option<HostRow>,
    /// The row's Pair screen over Home. Either shelf key above wins over it.
    #[serde(default)]
    pair: Option<HostRow>,
}

impl EntryJson {
    pub fn into_entry(self) -> ConsoleEntry {
        match (self.library, self.stream, self.pair) {
            (Some(h), _, _) => ConsoleEntry::Library(Box::new(h)),
            (None, Some(h), _) => ConsoleEntry::Stream(Box::new(h)),
            (None, None, Some(h)) => ConsoleEntry::Pair(Box::new(h)),
            (None, None, None) => ConsoleEntry::Home,
        }
    }
}

/// The connected controllers: `{"label": "DualSense", "pref": 1, "pads": [{name, key, pref,
/// steam_virtual, battery: {percent, charging} | null, detail, forwarded, rumble}], "others":
/// [{name, kind, detail}]}`.
#[derive(serde::Deserialize)]
pub struct PadsJson {
    #[serde(default)]
    label: Option<String>,
    /// The glyph style's pref as its wire byte; absent = keyboard glyphs.
    #[serde(default)]
    pref: Option<u8>,
    #[serde(default)]
    pads: Vec<PadJson>,
    /// Keyboards, mice and the like, for [`crate::ConsoleShared::set_other_devices`].
    #[serde(default)]
    others: Vec<crate::OtherDevice>,
}

#[derive(serde::Deserialize)]
struct PadJson {
    name: String,
    key: String,
    pref: u8,
    #[serde(default)]
    steam_virtual: bool,
    #[serde(default)]
    battery: Option<BatteryJson>,
    /// `VID:PID · gamepad · dpad` — what the controllers screen prints under the name.
    #[serde(default)]
    detail: String,
    #[serde(default)]
    forwarded: bool,
    #[serde(default)]
    rumble: bool,
}

#[derive(serde::Deserialize)]
struct BatteryJson {
    percent: u8,
    charging: bool,
}

/// The legend label, the glyph pref and the pads, as `Console::frame` takes them.
pub type Pads = (Option<String>, Option<GamepadPref>, Vec<PadInfo>);

impl PadsJson {
    pub fn take_others(&mut self) -> Vec<crate::OtherDevice> {
        std::mem::take(&mut self.others)
    }

    pub fn into_pads(self) -> Pads {
        let pads = self
            .pads
            .into_iter()
            .map(|j| PadInfo {
                name: j.name,
                key: j.key,
                pref: GamepadPref::from_u8(j.pref),
                steam_virtual: j.steam_virtual,
                battery: j.battery.map(|b| PadBattery {
                    percent: b.percent.min(100),
                    charging: b.charging,
                }),
                detail: j.detail,
                forwarded: j.forwarded,
                rumble: j.rumble,
            })
            .collect();
        (self.label, self.pref.map(GamepadPref::from_u8), pads)
    }
}

/// One preset as the host sends it. `overrides` is the console's settings encoding; an
/// overlay the console cannot read leaves that one preset unmarked, not the catalog empty.
#[derive(serde::Deserialize)]
pub struct PresetJson {
    id: String,
    name: String,
    #[serde(default)]
    overrides: serde_json::Value,
}

impl From<PresetJson> for PresetEntry {
    fn from(p: PresetJson) -> Self {
        PresetEntry {
            id: p.id,
            name: p.name,
            overrides: serde_json::from_value(p.overrides).unwrap_or_default(),
        }
    }
}

/// What the console raised for the host.
pub enum Event {
    Action(OverlayAction),
    Pulse(MenuPulse),
    Editing(bool),
    /// The field an `Editing(true)` opens, raised just before it.
    EditField(crate::screens::EditField),
    /// What the console's focus now reads as. Raised only when it changes; the host hands it
    /// to the screen reader.
    Announce(String),
    /// The shell saved settings: here is the whole snapshot to persist.
    Settings(Box<Settings>),
}

impl Event {
    /// `{"action": <OverlayAction>}`, `{"pulse": "move"|"confirm"|"boundary"}`,
    /// `{"editing": bool}`, `{"announce": "…"}` or `{"settings": <Settings>}`.
    pub fn to_json(&self) -> String {
        match self {
            Event::Action(a) => format!(
                "{{\"action\":{}}}",
                serde_json::to_string(a).unwrap_or_else(|_| "null".into())
            ),
            Event::Pulse(p) => format!(
                "{{\"pulse\":\"{}\"}}",
                match p {
                    MenuPulse::Move => "move",
                    MenuPulse::Confirm => "confirm",
                    MenuPulse::Boundary => "boundary",
                }
            ),
            Event::Editing(e) => format!("{{\"editing\":{e}}}"),
            Event::EditField(f) => format!(
                "{{\"edit_text\":{}}}",
                serde_json::to_string(f).unwrap_or_else(|_| "null".into())
            ),
            Event::Announce(text) => format!(
                "{{\"announce\":{}}}",
                serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
            ),
            Event::Settings(s) => format!(
                "{{\"settings\":{}}}",
                serde_json::to_string(s).unwrap_or_else(|_| "null".into())
            ),
        }
    }
}

/// The last of each edge-triggered event the host was handed, so a repeat is silence.
pub struct Published {
    was_editing: bool,
    /// Last string handed to the screen reader. Repeating one is worse than silence.
    spoken: Option<String>,
    saved_gen: u64,
}

impl Published {
    pub fn new(console: &Console, store: &SnapshotStore) -> Published {
        Published {
            was_editing: console.editing(),
            spoken: None,
            saved_gen: store.saved_gen(),
        }
    }

    /// Hand `emit` what the console raised since the last call.
    pub fn publish(
        &mut self,
        console: &mut Console,
        store: &SnapshotStore,
        mut emit: impl FnMut(Event),
    ) {
        while let Some(a) = console.take_action() {
            emit(Event::Action(a));
        }
        let editing = console.editing();
        if editing != self.was_editing {
            self.was_editing = editing;
            if let Some(field) = editing.then(|| console.edit_field()).flatten() {
                emit(Event::EditField(field));
            }
            emit(Event::Editing(editing));
        }
        let announce = console.focus_announcement();
        if announce != self.spoken {
            self.spoken = announce;
            if let Some(text) = &self.spoken {
                emit(Event::Announce(text.clone()));
            }
        }
        if store.saved_gen() != self.saved_gen {
            let (settings, current_gen) = store.snapshot();
            self.saved_gen = current_gen;
            emit(Event::Settings(Box::new(settings)));
        }
    }
}

/// A menu code: an event, or a remote's OK edge (`true` = down), which acts on release and,
/// held, opens the card's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCode {
    Menu(MenuEvent),
    Ok(bool),
}

/// 0..3 move up/down/left/right, 4 confirm, 5 back, 6 secondary (Y), 7 tertiary (X),
/// 8 jump back (L1), 9 jump forward (R1), 10/11 a remote's OK down/up.
pub fn menu_code(code: u8) -> Option<MenuCode> {
    Some(MenuCode::Menu(match code {
        0 => MenuEvent::Move(MenuDir::Up),
        1 => MenuEvent::Move(MenuDir::Down),
        2 => MenuEvent::Move(MenuDir::Left),
        3 => MenuEvent::Move(MenuDir::Right),
        4 => MenuEvent::Confirm,
        5 => MenuEvent::Back,
        6 => MenuEvent::Secondary,
        7 => MenuEvent::Tertiary,
        8 => MenuEvent::JumpBack,
        9 => MenuEvent::JumpForward,
        10 | 11 => return Some(MenuCode::Ok(code == 10)),
        _ => return None,
    }))
}

/// Touch or mouse at `x`, `y`: 0 move, 1 primary down (a mouse, acts at once), 2 primary up,
/// 3 secondary down (= Back), 4 wheel (`dy` steps, + = up), 5 cancel, 6 primary down from a
/// finger, which the shell defers so a swipe scrolls instead.
pub fn pointer_code(kind: u8, x: f32, y: f32, dy: f32) -> Option<PointerInput> {
    let down = |button, touch| PointerInput::Down {
        x,
        y,
        button,
        touch,
    };
    Some(match kind {
        0 => PointerInput::Move { x, y },
        1 => down(PointerButton::Primary, false),
        2 => PointerInput::Up {
            x,
            y,
            button: PointerButton::Primary,
        },
        3 => down(PointerButton::Secondary, false),
        4 => PointerInput::Wheel { x, y, dy },
        5 => PointerInput::Cancel,
        6 => down(PointerButton::Primary, true),
        _ => return None,
    })
}

/// 0..3 left/right/up/down, 4 return, 5 space, 6 escape, 7 backspace, 8 page up, 9 page down,
/// 10 tab, 11 Y, 12 X. Any other key is the host's to keep.
pub fn key_code(code: u8) -> Option<Key> {
    Some(match code {
        0 => Key::Left,
        1 => Key::Right,
        2 => Key::Up,
        3 => Key::Down,
        4 => Key::Return,
        5 => Key::Space,
        6 => Key::Escape,
        7 => Key::Backspace,
        8 => Key::PageUp,
        9 => Key::PageDown,
        10 => Key::Tab,
        11 => Key::Y,
        12 => Key::X,
        _ => return None,
    })
}

/// 0 connecting, 1 streaming, 2 failed, 3 ended (an empty `message` is a clean end),
/// 4 reconnecting.
pub fn phase_code(code: u8, message: &str) -> Option<SessionPhase<'_>> {
    Some(match code {
        0 => SessionPhase::Connecting,
        1 => SessionPhase::Streaming,
        2 => SessionPhase::Failed(message),
        3 => SessionPhase::Ended((!message.is_empty()).then_some(message)),
        4 => SessionPhase::Reconnecting(message),
        _ => return None,
    })
}

/// 1 waking, 2 offline; anything else is fresh.
pub fn stale_code(code: u8) -> Stale {
    match code {
        1 => Stale::Waking,
        2 => Stale::Offline,
        _ => Stale::No,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_create_starts_home_with_the_defaults() {
        let o: CreateOptions =
            serde_json::from_str(r#"{"device_name": "TV", "gpu_cache_bytes": 0, "settings": {}}"#)
                .unwrap();
        assert!(o.av1_ok && !o.fallback_ui && !o.pyrowave_ok && !o.profiles);
        let (opts, entry, _) = o.into_console(Platform::Android);
        assert!(matches!(entry, ConsoleEntry::Home));
        assert_eq!(opts.gpu_cache_bytes, 16 << 20);
    }

    #[test]
    fn a_pair_key_enters_on_the_pair_screen() {
        let row = r#"{"key": "aa", "name": "Desk", "addr": "10.0.0.5", "port": 47989,
            "fp_hex": "", "paired": false, "saved": true, "online": true, "mgmt_port": 47990,
            "can_wake": false, "last_used": null, "os": "", "pin": null, "bound_preset": null}"#;
        let pair: EntryJson = serde_json::from_str(&format!(r#"{{"pair": {row}}}"#)).unwrap();
        assert!(matches!(pair.into_entry(), ConsoleEntry::Pair(_)));
        let both: EntryJson =
            serde_json::from_str(&format!(r#"{{"library": {row}, "pair": {row}}}"#)).unwrap();
        assert!(matches!(both.into_entry(), ConsoleEntry::Library(_)));
    }

    /// The field a host's own keyboard types into, as the host reads it.
    #[test]
    fn an_opened_field_reads_with_its_label_and_text() {
        let field = crate::screens::EditField {
            label: "PIN".into(),
            text: "12".into(),
            digits: true,
        };
        assert_eq!(
            Event::EditField(field).to_json(),
            r#"{"edit_text":{"label":"PIN","text":"12","digits":true}}"#
        );
    }

    #[test]
    fn events_read_as_the_host_parses_them() {
        assert_eq!(
            Event::Pulse(MenuPulse::Boundary).to_json(),
            r#"{"pulse":"boundary"}"#
        );
        assert_eq!(Event::Editing(true).to_json(), r#"{"editing":true}"#);
        assert_eq!(
            Event::Announce("Home \"1\"".into()).to_json(),
            r#"{"announce":"Home \"1\""}"#
        );
    }

    /// The codes Kotlin and Swift send. A new code lands here and in both shims' callers.
    #[test]
    fn codes_decode_to_the_documented_inputs() {
        use MenuDir::*;
        let menu = [
            MenuEvent::Move(Up),
            MenuEvent::Move(Down),
            MenuEvent::Move(Left),
            MenuEvent::Move(Right),
            MenuEvent::Confirm,
            MenuEvent::Back,
            MenuEvent::Secondary,
            MenuEvent::Tertiary,
            MenuEvent::JumpBack,
            MenuEvent::JumpForward,
        ];
        for (code, ev) in menu.into_iter().enumerate() {
            assert_eq!(menu_code(code as u8), Some(MenuCode::Menu(ev)));
        }
        assert_eq!(menu_code(10), Some(MenuCode::Ok(true)));
        assert_eq!(menu_code(11), Some(MenuCode::Ok(false)));

        let (x, y, dy) = (1.0, 2.0, 3.0);
        let pointer = [
            PointerInput::Move { x, y },
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Primary,
                touch: false,
            },
            PointerInput::Up {
                x,
                y,
                button: PointerButton::Primary,
            },
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Secondary,
                touch: false,
            },
            PointerInput::Wheel { x, y, dy },
            PointerInput::Cancel,
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Primary,
                touch: true,
            },
        ];
        for (kind, input) in pointer.into_iter().enumerate() {
            assert_eq!(pointer_code(kind as u8, x, y, dy), Some(input));
        }

        let keys = [
            Key::Left,
            Key::Right,
            Key::Up,
            Key::Down,
            Key::Return,
            Key::Space,
            Key::Escape,
            Key::Backspace,
            Key::PageUp,
            Key::PageDown,
            Key::Tab,
            Key::Y,
            Key::X,
        ];
        for (code, key) in keys.into_iter().enumerate() {
            assert_eq!(key_code(code as u8), Some(key));
        }

        for code in 0..=u8::MAX {
            let known = |n: usize| usize::from(code) < n;
            assert_eq!(
                menu_code(code).is_some(),
                known(menu.len() + 2),
                "menu {code}"
            );
            assert_eq!(pointer_code(code, x, y, dy).is_some(), known(pointer.len()));
            assert_eq!(key_code(code).is_some(), known(keys.len()), "key {code}");
            assert_eq!(phase_code(code, "").is_some(), known(5), "phase {code}");
        }
        assert!(matches!(
            phase_code(2, "lost"),
            Some(SessionPhase::Failed("lost"))
        ));
        assert!(matches!(phase_code(3, ""), Some(SessionPhase::Ended(None))));
        assert!(matches!(
            phase_code(3, "gone"),
            Some(SessionPhase::Ended(Some("gone")))
        ));
        assert!(matches!(
            phase_code(4, "retry"),
            Some(SessionPhase::Reconnecting("retry"))
        ));
        assert_eq!(stale_code(1), Stale::Waking);
        assert_eq!(stale_code(2), Stale::Offline);
        assert_eq!(stale_code(0), Stale::No);
        assert_eq!(stale_code(9), Stale::No);
    }
}
