//! The JSON a native host and the console exchange across a language boundary: what the host
//! hands at create, the entry, pad and preset pushes, and the events it reads back. Android (JNI)
//! and Apple (C ABI) both speak it, so Kotlin and Swift encode one contract.

use crate::store::PresetEntry;
use crate::{
    Console, ConsoleEntry, ConsoleOptions, DeviceScreen, HostRow, Platform, SnapshotStore,
};
use pf_client_core::console::OverlayAction;
use pf_client_core::menu_nav::{MenuPulse, PadBattery, PadInfo};
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
    /// Whether a real AV1 decoder exists, as the host's codec list answers it. Absent means
    /// don't claim the device lacks it, so the codec row stays unmarked.
    #[serde(default = "yes")]
    pub av1_ok: bool,
    /// Whether this device decodes PyroWave. A host that probes it in Rust overwrites it.
    #[serde(default)]
    pub pyrowave_ok: bool,
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
            deck: false,
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
}

impl EntryJson {
    pub fn into_entry(self) -> ConsoleEntry {
        match (self.library, self.stream) {
            (Some(h), _) => ConsoleEntry::Library(Box::new(h)),
            (None, Some(h)) => ConsoleEntry::Stream(Box::new(h)),
            (None, None) => ConsoleEntry::Home,
        }
    }
}

/// The connected controllers: `{"label": "DualSense", "pref": 1, "pads": [{name, key, pref,
/// steam_virtual, battery: {percent, charging} | null, detail, forwarded, rumble}]}`.
#[derive(serde::Deserialize)]
pub struct PadsJson {
    #[serde(default)]
    label: Option<String>,
    /// The glyph style's pref as its wire byte; absent = keyboard glyphs.
    #[serde(default)]
    pref: Option<u8>,
    #[serde(default)]
    pads: Vec<PadJson>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_create_starts_home_with_the_defaults() {
        let o: CreateOptions =
            serde_json::from_str(r#"{"device_name": "TV", "gpu_cache_bytes": 0, "settings": {}}"#)
                .unwrap();
        assert!(o.av1_ok && !o.fallback_ui && !o.pyrowave_ok);
        let (opts, entry, _) = o.into_console(Platform::Android);
        assert!(matches!(entry, ConsoleEntry::Home));
        assert_eq!(opts.gpu_cache_bytes, 16 << 20);
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
}
