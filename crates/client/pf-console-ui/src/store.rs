//! Persistence seam between the console shell and its host.
//!
//! Screens mutate in-memory [`Settings`] and persist through [`SettingsStore`].
//! Call `load` immediately before every write: a whole-file store otherwise
//! reverts another writer's save. Desktop is `FileSettingsStore`
//! (`pf_client_core::trust` JSON). Android is [`SnapshotStore`] — the host
//! pushes a snapshot in and polls `saved_gen` out.
//!
//! Presets are `(id, name)` in display order, with their overrides beside them so a
//! settings row can say when a host's bound preset outranks the global. The console
//! lists and pins; it does not create. Design: `design/client-settings-profiles.md`.

use pf_client_core::presets::SettingsOverlay;
use pf_client_core::trust::{KnownHosts, Settings};
use std::collections::HashMap;

/// One catalog entry as a host pushes it: the overrides are in the console's settings
/// encoding, empty when the host sent none.
#[derive(Clone, Debug, Default)]
pub struct PresetEntry {
    pub id: String,
    pub name: String,
    pub overrides: SettingsOverlay,
}

pub trait SettingsStore: Send + Sync {
    /// Called at construction and immediately before every mutation. Cache rather
    /// than hit disk per call.
    fn load(&self) -> Settings;
    /// Failures are the store's to log. The shell has already applied the change
    /// in memory and shows it as done.
    fn save(&self, settings: &Settings);
    fn presets(&self) -> Vec<(String, String)>;
    /// Each preset's overrides by id. Loaded once per screen, like `presets`.
    fn preset_overrides(&self) -> HashMap<String, SettingsOverlay>;
    /// The store behind the carousel: copy-link reads a record's id from it, and the
    /// start-screen policy needs `paired` on every record, not only the drawn ones.
    fn known_hosts(&self) -> KnownHosts;
}

/// Desktop JSON via `pf_client_core::trust`. Unit struct so the shell can hold
/// a `&'static` to [`FILE_STORE`] when the host provides none.
#[cfg(any(target_os = "linux", windows))]
pub struct FileSettingsStore;

#[cfg(any(target_os = "linux", windows))]
impl SettingsStore for FileSettingsStore {
    fn load(&self) -> Settings {
        Settings::load()
    }

    fn save(&self, settings: &Settings) {
        settings.save();
    }

    fn presets(&self) -> Vec<(String, String)> {
        pf_client_core::presets::PresetsFile::load()
            .presets
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect()
    }

    fn preset_overrides(&self) -> HashMap<String, SettingsOverlay> {
        pf_client_core::presets::PresetsFile::load()
            .presets
            .into_iter()
            .map(|p| (p.id, p.overrides))
            .collect()
    }

    fn known_hosts(&self) -> KnownHosts {
        KnownHosts::load()
    }
}

#[cfg(any(target_os = "linux", windows))]
pub static FILE_STORE: FileSettingsStore = FileSettingsStore;

/// Default store when the host provides none.
#[cfg(all(not(test), any(target_os = "linux", windows)))]
pub fn file_store() -> &'static dyn SettingsStore {
    &FILE_STORE
}

/// The screen tests all build their screen around `file_store()`, so this is what they get:
/// one in-memory store per test thread. Never the file — a whole-file writer loses another
/// test's save between its own load and save, and libtest runs them in parallel.
/// Leaks one store per thread; only the test binary calls this.
#[cfg(test)]
pub fn file_store() -> &'static dyn SettingsStore {
    thread_local! {
        static STORE: &'static SnapshotStore =
            Box::leak(Box::new(SnapshotStore::new(Settings::default(), Vec::new())));
    }
    STORE.with(|s| *s)
}

/// In-memory snapshot the host pushes and polls. `save` replaces it and bumps
/// `saved_gen`; `set` does not. Android JNI; tests that must not touch a file.
pub struct SnapshotStore {
    inner: std::sync::Mutex<SnapshotInner>,
}

struct SnapshotInner {
    settings: Settings,
    presets: Vec<PresetEntry>,
    known_hosts: KnownHosts,
    /// Bumped on every `save`. The host compares against what it last persisted.
    saved_gen: u64,
}

impl SnapshotStore {
    pub fn new(settings: Settings, presets: Vec<PresetEntry>) -> SnapshotStore {
        SnapshotStore {
            inner: std::sync::Mutex::new(SnapshotInner {
                settings,
                presets,
                known_hosts: KnownHosts::default(),
                saved_gen: 0,
            }),
        }
    }

    /// Replace what the shell will load next. Does not bump `saved_gen`.
    pub fn set(&self, settings: Settings) {
        self.inner.lock().unwrap().settings = settings;
    }

    pub fn set_presets(&self, presets: Vec<PresetEntry>) {
        self.inner.lock().unwrap().presets = presets;
    }

    pub fn set_known_hosts(&self, hosts: KnownHosts) {
        self.inner.lock().unwrap().known_hosts = hosts;
    }

    /// Generation only — cheap enough to compare every frame.
    pub fn saved_gen(&self) -> u64 {
        self.inner.lock().unwrap().saved_gen
    }

    /// Persist when the generation moved since the last look.
    pub fn snapshot(&self) -> (Settings, u64) {
        let g = self.inner.lock().unwrap();
        (g.settings.clone(), g.saved_gen)
    }
}

impl SettingsStore for SnapshotStore {
    fn load(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    fn save(&self, settings: &Settings) {
        let mut g = self.inner.lock().unwrap();
        g.settings = settings.clone();
        g.saved_gen += 1;
    }

    fn presets(&self) -> Vec<(String, String)> {
        let g = self.inner.lock().unwrap();
        g.presets
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect()
    }

    fn preset_overrides(&self) -> HashMap<String, SettingsOverlay> {
        let g = self.inner.lock().unwrap();
        g.presets
            .iter()
            .map(|p| (p.id.clone(), p.overrides.clone()))
            .collect()
    }

    fn known_hosts(&self) -> KnownHosts {
        let g = self.inner.lock().unwrap();
        KnownHosts {
            hosts: g.known_hosts.hosts.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_store_round_trips_and_counts_saves() {
        let store = SnapshotStore::new(
            Settings::default(),
            vec![PresetEntry {
                id: "p1".into(),
                name: "Work".into(),
                overrides: Default::default(),
            }],
        );
        assert_eq!(store.snapshot().1, 0);
        let mut s = store.load();
        s.ui_palette = "sky".into();
        store.save(&s);
        let (after, generation) = store.snapshot();
        assert_eq!(after.ui_palette, "sky");
        assert_eq!(generation, 1);
        assert_eq!(
            store.presets(),
            vec![("p1".to_string(), "Work".to_string())]
        );
        store.set(Settings::default());
        assert_eq!(store.load().ui_palette, Settings::default().ui_palette);
        assert_eq!(store.snapshot().1, 1);
    }
}
