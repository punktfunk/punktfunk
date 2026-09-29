//! The shell's copy of the three files it shares with the session, the console and the CLI:
//! known hosts, settings and presets. Pages render from here and never read the disk while
//! drawing. A change goes through `update_*`, which re-reads the file, applies the edit and
//! saves, so another process's write in between survives. A monitor on the config dir
//! reloads what anyone else writes; each real change reaches the listeners once.

use crate::trust::{self, KnownHosts, Settings};
use gtk::gio;
use gtk::prelude::*;
use pf_client_core::presets::PresetsFile;
use std::cell::{Ref, RefCell};
use std::rc::Rc;

/// Which file moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Changed {
    Hosts,
    Settings,
    Presets,
}

impl Changed {
    /// The file a monitor event names, if it is one of ours. `client-profiles.json` is the
    /// presets' legacy mirror, written just before the real file, so it is not a change.
    fn of_file(name: &str) -> Option<Changed> {
        match name {
            "client-known-hosts.json" => Some(Changed::Hosts),
            "client-gtk-settings.json" => Some(Changed::Settings),
            "client-presets.json" => Some(Changed::Presets),
            _ => None,
        }
    }
}

type Listener = Rc<dyn Fn(Changed)>;

pub struct Store {
    hosts: RefCell<KnownHosts>,
    settings: RefCell<Settings>,
    presets: RefCell<PresetsFile>,
    listeners: RefCell<Vec<Listener>>,
    monitor: RefCell<Option<gio::FileMonitor>>,
}

impl Store {
    /// Load all three and start watching the config dir. `KnownHosts::load` may write once,
    /// to mint record ids; every later read is a plain read.
    pub fn open() -> Rc<Store> {
        let store = Rc::new(Store {
            hosts: RefCell::new(KnownHosts::load()),
            settings: RefCell::new(Settings::load()),
            presets: RefCell::new(PresetsFile::load()),
            listeners: RefCell::default(),
            monitor: RefCell::default(),
        });
        store.watch();
        store
    }

    pub fn hosts(&self) -> Ref<'_, KnownHosts> {
        self.hosts.borrow()
    }

    pub fn settings(&self) -> Ref<'_, Settings> {
        self.settings.borrow()
    }

    pub fn presets(&self) -> Ref<'_, PresetsFile> {
        self.presets.borrow()
    }

    /// Re-read the hosts file, apply `f`, save, and tell the listeners.
    pub fn update_hosts<R>(&self, f: impl FnOnce(&mut KnownHosts) -> R) -> anyhow::Result<R> {
        let mut known = KnownHosts::read();
        let r = f(&mut known);
        known.save()?;
        self.set(Changed::Hosts, known, &self.hosts);
        Ok(r)
    }

    /// Re-read the settings file, apply `f`, save, and tell the listeners. A failed write is
    /// logged by nobody, like every settings save: it must never take a stream down.
    pub fn update_settings<R>(&self, f: impl FnOnce(&mut Settings) -> R) -> R {
        let mut s = Settings::load();
        let r = f(&mut s);
        s.save();
        self.set(Changed::Settings, s, &self.settings);
        r
    }

    /// Re-read the preset catalog, apply `f`, save, and tell the listeners.
    pub fn update_presets<R>(&self, f: impl FnOnce(&mut PresetsFile) -> R) -> anyhow::Result<R> {
        let mut catalog = PresetsFile::load();
        let r = f(&mut catalog);
        catalog.save()?;
        self.set(Changed::Presets, catalog, &self.presets);
        Ok(r)
    }

    /// Re-read one file after a write that went around the store (a core helper that saves
    /// itself, another process). Silent when nothing changed.
    pub fn reload(&self, what: Changed) {
        match what {
            Changed::Hosts => self.set(what, KnownHosts::read(), &self.hosts),
            Changed::Settings => self.set(what, Settings::load(), &self.settings),
            Changed::Presets => self.set(what, PresetsFile::load(), &self.presets),
        }
    }

    /// Called on every real change, after the store holds it.
    pub fn subscribe(&self, f: impl Fn(Changed) + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    fn set<T: serde::Serialize>(&self, what: Changed, value: T, cell: &RefCell<T>) {
        if same(&*cell.borrow(), &value) {
            return;
        }
        *cell.borrow_mut() = value;
        // Cloned out first, so a listener may subscribe or read while the loop runs.
        let listeners: Vec<Listener> = self.listeners.borrow().clone();
        for f in &listeners {
            f(what);
        }
    }

    /// `write_atomic` lands a file by renaming a temp file over it, which a directory monitor
    /// reports as a move into the dir or a rename, not as a change.
    fn watch(self: &Rc<Self>) {
        let Ok(dir) = trust::config_dir() else {
            return;
        };
        let monitor = match gio::File::for_path(&dir)
            .monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "config dir monitor did not start");
                return;
            }
        };
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, file, other, event| {
            use gio::FileMonitorEvent as E;
            let target = match event {
                E::Renamed => other,
                E::MovedIn | E::ChangesDoneHint | E::Created | E::Deleted => Some(file),
                _ => None,
            };
            let what = target
                .and_then(|f| f.basename())
                .and_then(|n| n.to_str().and_then(Changed::of_file));
            if let (Some(what), Some(store)) = (what, weak.upgrade()) {
                store.reload(what);
            }
        });
        *self.monitor.borrow_mut() = Some(monitor);
    }
}

/// Equal as stored: none of the three types compares directly, and the file is what counts.
fn same<T: serde::Serialize>(a: &T, b: &T) -> bool {
    matches!(
        (serde_json::to_value(a), serde_json::to_value(b)),
        (Ok(a), Ok(b)) if a == b
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_three_files_are_changes() {
        assert_eq!(
            Changed::of_file("client-known-hosts.json"),
            Some(Changed::Hosts)
        );
        assert_eq!(
            Changed::of_file("client-gtk-settings.json"),
            Some(Changed::Settings)
        );
        assert_eq!(
            Changed::of_file("client-presets.json"),
            Some(Changed::Presets)
        );
        assert_eq!(Changed::of_file("client-profiles.json"), None);
        assert_eq!(Changed::of_file(".client-known-hosts.json.tmp"), None);
    }

    #[test]
    fn an_equal_value_is_no_change() {
        let a = Settings::default();
        let mut b = Settings::default();
        assert!(same(&a, &b));
        b.bitrate_kbps = 1;
        assert!(!same(&a, &b));
    }
}
