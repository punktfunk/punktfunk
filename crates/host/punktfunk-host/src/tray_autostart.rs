//! The status tray's autostart entry follows the `tray_autostart` setting.
//!
//! Linux: the host user's `~/.config/autostart` entry ([`pf_paths::tray_autostart`]). Windows:
//! the machine `Run` value, which the SYSTEM host may write. `<config_dir>/tray-autostart` holds
//! the value last applied, so [`decide`] can tell a changed setting from an entry someone changed
//! in the desktop's startup settings or the tray menu. That change becomes the setting and is
//! never undone. A seat or door host has no tray and leaves the entry alone.

use pf_host_config::Source;

const ID: &str = "tray_autostart";
const RECORD: &str = "tray-autostart";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Make the entry match the setting.
    Write(bool),
    /// Save the entry's state as the setting.
    Adopt(bool),
    Keep,
}

/// `record` is the value last applied. `entry` is `None` where a missing entry is no choice yet:
/// Linux before the first write, when only the old global entry existed.
fn decide(want: bool, source: Source, record: Option<bool>, entry: Option<bool>) -> Action {
    let actual = entry.unwrap_or(false);
    match (source, record) {
        (Source::Env | Source::Flag, _) => Action::Write(want),
        (Source::Default, None) if entry.is_some() => Action::Adopt(actual),
        (_, None) => Action::Write(want),
        (_, Some(applied)) if applied != want => Action::Write(want),
        _ if actual != want => Action::Adopt(actual),
        _ => Action::Keep,
    }
}

/// At host start: seed the entry, carry a setting changed while the host was down, or take a
/// change made outside.
pub fn converge() {
    let Some((want, source)) = wanted() else {
        return;
    };
    match decide(want, source, read_record(), platform::read()) {
        Action::Write(on) => write(on),
        Action::Adopt(on) => {
            let patch = serde_json::Map::from_iter([(ID.to_string(), serde_json::Value::Bool(on))]);
            match pf_host_config::save(&patch) {
                Ok(()) => tracing::info!(on, "tray autostart taken from its entry"),
                Err(e) => tracing::warn!(error = %e, "tray autostart setting not saved"),
            }
            record(on);
        }
        Action::Keep => {}
    }
}

/// After the console or the CLI changed the setting.
pub fn apply() {
    if let Some((want, _)) = wanted() {
        write(want);
    }
}

/// The setting and where it came from, or `None` on a host that owns no entry.
fn wanted() -> Option<(bool, Source)> {
    if pf_paths::seat::is_seat_host() || pf_paths::seat::is_door() || !platform::installed() {
        return None;
    }
    let row = pf_host_config::snapshot().get(ID)?;
    Some((row.value.as_bool().unwrap_or(true), row.source))
}

/// A failed write records nothing, so the next start tries again.
fn write(on: bool) {
    match platform::write(on) {
        Ok(()) => record(on),
        Err(e) => tracing::warn!(on, error = %e, "tray autostart entry not written"),
    }
}

fn record_path() -> std::path::PathBuf {
    pf_paths::config_dir().join(RECORD)
}

fn read_record() -> Option<bool> {
    match std::fs::read_to_string(record_path()).ok()?.trim() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn record(on: bool) {
    let text: &[u8] = if on { b"on\n" } else { b"off\n" };
    if let Err(e) = pf_paths::replace_file(&record_path(), text) {
        tracing::warn!(error = %e, "tray autostart record not written");
    }
}

#[cfg(not(windows))]
mod platform {
    /// Linux only, and only where the tray is installed beside the host.
    pub fn installed() -> bool {
        cfg!(target_os = "linux")
            && std::env::current_exe()
                .is_ok_and(|exe| exe.with_file_name("punktfunk-tray").exists())
    }

    pub fn read() -> Option<bool> {
        pf_paths::tray_autostart::read()
    }

    pub fn write(on: bool) -> std::io::Result<()> {
        pf_paths::tray_autostart::write(on, &std::env::current_exe()?)
    }
}

#[cfg(windows)]
mod platform {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    const RUN: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "PunktfunkTray";

    pub fn installed() -> bool {
        crate::tray::tray_exe().is_some()
    }

    /// A missing value is a choice here: the installer wrote it or left it out.
    pub fn read() -> Option<bool> {
        let run = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(RUN);
        Some(run.and_then(|k| k.get_value::<String, _>(VALUE)).is_ok())
    }

    pub fn write(on: bool) -> std::io::Result<()> {
        let (run, _) = RegKey::predef(HKEY_LOCAL_MACHINE).create_subkey(RUN)?;
        if !on {
            return match run.delete_value(VALUE) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let exe = crate::tray::tray_exe().ok_or(std::io::ErrorKind::NotFound)?;
        // Quoted: unquoted, a path with a space makes Windows try `C:\Program.exe` first.
        run.set_value(VALUE, &format!("\"{}\"", exe.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::{Adopt, Keep, Write};
    use Source::{Default, Env, Store};

    #[test]
    fn the_start_rule() {
        let on = Some(true);
        let off = Some(false);
        let none = None;
        // (want, source, record, entry) → action
        let table = [
            // First start: seed the default, keep an old opt-out, honour an installer's value.
            (true, Default, none, none, Write(true)),
            (true, Default, none, off, Adopt(false)),
            (true, Default, none, on, Adopt(true)),
            (false, Store, none, on, Write(false)),
            // Changed while the host was down.
            (false, Store, on, on, Write(false)),
            (true, Store, off, none, Write(true)),
            // Changed outside: the desktop or the tray menu.
            (true, Store, on, none, Adopt(false)),
            (true, Store, on, off, Adopt(false)),
            (false, Store, off, on, Adopt(true)),
            // Nothing changed.
            (true, Store, on, on, Keep),
            (false, Store, off, none, Keep),
            // An env pin wins at every start.
            (false, Env, off, on, Write(false)),
        ];
        for (want, source, record, entry, expected) in table {
            assert_eq!(
                decide(want, source, record, entry),
                expected,
                "want {want} from {source:?}, record {record:?}, entry {entry:?}"
            );
        }
    }
}
