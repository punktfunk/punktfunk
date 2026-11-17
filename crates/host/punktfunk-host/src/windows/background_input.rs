//! A game keeps its controller while another session switches apps, at the desk and on a seat.
//!
//! GameInput's redist gives input to the one focused process on the PC, and a foreground change
//! in any session moves that focus: a window appearing on a seat takes a desk game's pad, and
//! one appearing at the desk takes a seat game's. An exe with the `BackgroundInput` quirk gets
//! input unfocused. With `seat_background_input` on, a seat host gives that quirk to each exe its
//! session starts outside the Windows directory, and the box does the same for the console while
//! it has seats; the session fence keeps each session's pads its own. A game reads the quirk as
//! it sets GameInput up, so an app already running has it from its next launch. The quirk is per
//! exe name, for the whole PC: the box removes what was added once the row is off.

use pf_paths::seat::HostRole;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_MULTI_SZ};
use winreg::types::FromRegValue;
use winreg::RegKey;

/// Per-exe form: a subkey per exe name holding `BackgroundInput` = 1.
const PER_EXE: &str = r"SOFTWARE\Microsoft\GameInputRedist\Exe";
/// List form: `BackgroundInput` lists exe names. The 4.x redist references both forms.
const LIST: &str = r"SOFTWARE\Microsoft\GameInput";
const QUIRK: &str = "BackgroundInput";
/// A value per exe name a seat added, holding which forms it added: removal touches only those.
const RECORD: &str = r"SOFTWARE\Punktfunk\GameInputBackground";
const ADDED_PER_EXE: u32 = 1;
const ADDED_TO_LIST: u32 = 2;
/// Names the seats may add: far past any library, and a bound on how far a guest's renamed
/// exes grow HKLM.
const MAX_RECORDED: u32 = 4096;
/// A game sets GameInput up a second or more after it starts.
const TICK: Duration = Duration::from_millis(250);

/// Once per host, as it serves: a seat host watches its session, the box watches the console
/// and first clears what was added while the row is off.
pub(crate) fn start() {
    if !pf_paths::seat::role().follows_contract() {
        apply();
    }
    let spawned = std::thread::Builder::new()
        .name("pf-background-input".into())
        .spawn(watch);
    if let Err(error) = spawned {
        tracing::warn!(%error, "background input watcher did not start");
    }
}

/// On the box, after the row changed: with it off, removes every quirk a seat added. Only the
/// box reads the row it hands seats; a door or seat host would read its own store as off.
pub(crate) fn apply() {
    let the_box = matches!(pf_paths::seat::role(), HostRole::Plain);
    if !the_box || pf_host_config::config().seat_background_input {
        return;
    }
    if let Err(error) = remove_all() {
        tracing::warn!(%error, "background input quirks not removed");
    }
}

/// How often the box re-asks whether it has seats. The supervisor answers over a pipe.
const SEATS_CHECK: Duration = Duration::from_secs(3);

fn watch() {
    let windows_dir = std::env::var_os("SystemRoot").map(PathBuf::from);
    let mut seen = HashSet::new();
    let mut seats_checked: Option<(std::time::Instant, bool)> = None;
    loop {
        std::thread::sleep(TICK);
        let on = if pf_paths::seat::role().follows_contract() {
            pf_host_config::seat_defaults::current().is_some_and(|d| d.background_input)
        } else {
            // The box grants only while it has seats: without one there is no second session to
            // take a desk game's pad, so a game's GameInput behaviour is left as Windows has it.
            let has_seats = match seats_checked {
                Some((at, has)) if at.elapsed() < SEATS_CHECK => has,
                _ => {
                    let has = !crate::seats::ledger().seats.is_empty();
                    seats_checked = Some((std::time::Instant::now(), has));
                    has
                }
            };
            has_seats && pf_host_config::config().seat_background_input
        };
        if !on {
            seen.clear();
            continue;
        }
        let now: HashSet<u32> = crate::procscan::processes()
            .into_iter()
            .map(|(pid, ..)| pid)
            .collect();
        for &pid in now.difference(&seen) {
            let Some(name) = candidate(pid, windows_dir.as_deref()) else {
                continue;
            };
            if let Err(error) = grant(&name) {
                tracing::warn!(%error, exe = %name, "background input quirk not written");
            }
        }
        seen = now;
    }
}

/// The exe name of a process this session runs, unless it is part of Windows.
fn candidate(pid: u32, windows_dir: Option<&Path>) -> Option<String> {
    // pid 0 and 4 are the kernel's. Without the Windows directory, explorer.exe can't be told
    // from a game, so nothing is named.
    if pid <= 4 || !crate::game_term::in_our_session(pid) {
        return None;
    }
    exe_name(&crate::procscan::process_image(pid)?, windows_dir?)
}

fn exe_name(image: &Path, windows_dir: &Path) -> Option<String> {
    if crate::procscan::under_dir(image, windows_dir) {
        return None;
    }
    image.file_name()?.to_str().map(str::to_string)
}

/// Gives `name` the quirk in each form the installed GameInput reads, where it has none yet. A
/// new name past [`MAX_RECORDED`] gets nothing, and the first such says so once.
fn grant(name: &str) -> io::Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let per_exe = match open_rw(&hklm, PER_EXE)? {
        Some(per_exe) if !has_quirk(&per_exe, name)? => Some(per_exe),
        _ => None,
    };
    let list = match open_rw(&hklm, LIST)? {
        Some(key) => read_list(&key)?
            .and_then(|list| with_name(list, name))
            .map(|list| (key, list)),
        None => None,
    };
    let added =
        per_exe.as_ref().map_or(0, |_| ADDED_PER_EXE) | list.as_ref().map_or(0, |_| ADDED_TO_LIST);
    if added == 0 {
        return Ok(());
    }
    // Recorded first, so a quirk is never left behind unrecorded.
    let (record, _) = hklm.create_subkey(RECORD)?;
    let before: u32 = record.get_value(name).unwrap_or(0);
    if !may_record(before, record.query_info()?.values) {
        static FULL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !FULL.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(cap = MAX_RECORDED, exe = %name,
                "background input record full; turning the row off and on clears it");
        }
        return Ok(());
    }
    record.set_value(name, &(before | added))?;
    if let Some(per_exe) = &per_exe {
        per_exe.create_subkey(name)?.0.set_value(QUIRK, &1u32)?;
    }
    // Two seats writing at once can drop a name; the exe's next start writes it again.
    if let Some((key, list)) = &list {
        key.set_value(QUIRK, list)?;
    }
    tracing::info!(exe = %name, "background input quirk written");
    Ok(())
}

/// Whether a name holding `before` may be recorded beside `recorded` others.
fn may_record(before: u32, recorded: u32) -> bool {
    before != 0 || recorded < MAX_RECORDED
}

fn remove_all() -> io::Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Some(record) = open_rw(&hklm, RECORD)? else {
        return Ok(());
    };
    let names: Vec<String> = record
        .enum_values()
        .filter_map(Result::ok)
        .map(|(name, _)| name)
        .collect();
    for name in names {
        revoke(&hklm, &name, record.get_value(&name).unwrap_or(0))?;
        record.delete_value(&name)?;
        tracing::info!(exe = %name, "background input quirk removed");
    }
    if record.query_info()?.values == 0 {
        drop(record);
        hklm.delete_subkey(RECORD)?;
    }
    Ok(())
}

/// Takes back what `added` says a seat wrote for `name`. A value set to anything but 1 since
/// then is someone else's and stays.
fn revoke(hklm: &RegKey, name: &str, added: u32) -> io::Result<()> {
    if added & ADDED_PER_EXE != 0 {
        if let Some(per_exe) = open_rw(hklm, PER_EXE)? {
            if let Some(key) = open_rw(&per_exe, name)? {
                if key.get_value::<u32, _>(QUIRK).ok() == Some(1) {
                    key.delete_value(QUIRK)?;
                }
                let left = key.query_info()?;
                drop(key);
                if left.values == 0 && left.sub_keys == 0 {
                    per_exe.delete_subkey(name)?;
                }
            }
        }
    }
    if added & ADDED_TO_LIST != 0 {
        if let Some(key) = open_rw(hklm, LIST)? {
            if let Some(list) = read_list(&key)? {
                let rest = without_name(list, name);
                if rest.is_empty() {
                    key.delete_value(QUIRK)?;
                } else {
                    key.set_value(QUIRK, &rest)?;
                }
            }
        }
    }
    Ok(())
}

/// `None` when the key is missing: that GameInput form is not installed.
fn open_rw(parent: &RegKey, path: &str) -> io::Result<Option<RegKey>> {
    match parent.open_subkey_with_flags(path, KEY_READ | KEY_WRITE) {
        Ok(key) => Ok(Some(key)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether `name` has a 3.x quirk value, whatever it holds: Microsoft's 0 is a choice too.
fn has_quirk(per_exe: &RegKey, name: &str) -> io::Result<bool> {
    let key = match per_exe.open_subkey_with_flags(name, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match key.get_raw_value(QUIRK) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// The 4.x list, empty when unset. `None` when the value has another type: not ours to rewrite.
fn read_list(key: &RegKey) -> io::Result<Option<Vec<String>>> {
    match key.get_raw_value(QUIRK) {
        Ok(value) if value.vtype == REG_MULTI_SZ => Vec::<String>::from_reg_value(&value).map(Some),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Some(Vec::new())),
        Err(error) => Err(error),
    }
}

/// `list` with `name` added, or `None` when it already holds it in any case.
fn with_name(mut list: Vec<String>, name: &str) -> Option<Vec<String>> {
    let lower = name.to_lowercase();
    if list.iter().any(|n| n.to_lowercase() == lower) {
        return None;
    }
    list.push(name.to_string());
    Some(list)
}

fn without_name(mut list: Vec<String>, name: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    list.retain(|n| n.to_lowercase() != lower);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_record_takes_no_new_name_but_keeps_its_own() {
        assert!(may_record(0, 0));
        assert!(may_record(0, MAX_RECORDED - 1));
        assert!(!may_record(0, MAX_RECORDED));
        assert!(may_record(ADDED_PER_EXE, MAX_RECORDED));
    }

    #[test]
    fn only_apps_outside_the_windows_directory_are_named() {
        let windows = Path::new(r"C:\Windows");
        assert_eq!(
            exe_name(
                Path::new(r"D:\Games\The Witcher 3\bin\x64_dx12\witcher3.exe"),
                windows
            )
            .as_deref(),
            Some("witcher3.exe")
        );
        assert_eq!(
            exe_name(Path::new(r"c:\windows\explorer.exe"), windows),
            None
        );
        assert_eq!(
            exe_name(Path::new(r"C:\Windows\System32\svchost.exe"), windows),
            None
        );
        assert_eq!(
            exe_name(Path::new(r"C:\WindowsApps\game.exe"), windows).as_deref(),
            Some("game.exe")
        );
    }

    #[test]
    fn the_list_takes_a_name_once_and_gives_it_back_in_any_case() {
        let steam = vec!["steam.exe".to_string()];
        let both = with_name(steam.clone(), "Witcher3.exe").expect("a new name is added");
        assert_eq!(both, ["steam.exe", "Witcher3.exe"]);
        assert_eq!(with_name(both.clone(), "witcher3.EXE"), None);
        assert_eq!(without_name(both, "WITCHER3.exe"), steam);
    }
}
