//! The seat listener: a copy of Remote Desktop's `RDP-Tcp` on loopback port 3390 whose
//! sessions load `pf_seat_audio.dll`, the enumerator that gives each seat its pads' audio
//! endpoints (`pf_driver_proto::seat_audio`). The keeper connects here when it is
//! [`configured`], and to 3389 otherwise.
//!
//! TermService opens and closes a listener as its key appears and goes, so nothing restarts.
//! The DLL loads into every process of a seat session at that process's bitness: the x64 build
//! goes to `System32\punktfunk` and the x86 one to `SysWOW64\punktfunk` under one name, which
//! WOW64 redirects. A new build gets a new name, since a loaded one cannot be replaced.

use super::enable::run;
use pf_driver_proto::seat_audio::{LISTENER, LISTENER_PORT};
use pf_paths::restart_deletes;
use std::path::{Path, PathBuf};
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_ALL_ACCESS, KEY_READ, KEY_WOW64_64KEY, REG_MULTI_SZ};
use winreg::types::ToRegValue;
use winreg::{RegKey, RegValue};

const WINSTATIONS: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server\WinStations";
const STOCK: &str = "RDP-Tcp";
/// Set on the listener this module writes; [`remove`] deletes no other.
const MARKER: &str = "PunktfunkSeatListener";
/// Values the seat listener sets itself rather than copying.
const OWN_VALUES: [&str; 4] = [
    "fEnableWinStation",
    "PortNumber",
    "AudioEnumeratorDll",
    MARKER,
];
const DLL: &str = "pf_seat_audio";
const RULES: [(&str, &str); 2] = [
    ("punktfunk seat listener (TCP)", "TCP"),
    ("punktfunk seat listener (UDP)", "UDP"),
];
/// Every address but loopback. The `Remote Desktop` rules name port 3389 only.
const NOT_LOOPBACK: &str = "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,\
    ::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";

/// Bring the DLLs, the listener and its firewall rules up to date with `host_path`'s install.
/// Writes nothing that already matches, so live seat sessions see no change.
pub(super) fn ensure(host_path: &Path) -> Result<(), String> {
    if !cfg!(target_arch = "x86_64") {
        // ARM64 would need an ARM64X build for its emulated x64 processes.
        return Ok(());
    }
    let dll = deploy(host_path)?;
    write_listener(&dll).map_err(|e| format!("write the {LISTENER} listener: {e}"))?;
    sweep(&native_dir(), Some(&dll));
    sweep(&wow_dir(), Some(&dll));
    block_network()
}

/// The listener [`ensure`] wrote is in place and enabled on [`LISTENER_PORT`]. Never on ARM64,
/// where `ensure` writes nothing.
pub(super) fn configured() -> bool {
    cfg!(target_arch = "x86_64")
        && RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(
                format!(r"{WINSTATIONS}\{LISTENER}"),
                KEY_READ | KEY_WOW64_64KEY,
            )
            .is_ok_and(|ours| {
                [
                    (MARKER, 1),
                    ("fEnableWinStation", 1),
                    ("PortNumber", u32::from(LISTENER_PORT)),
                ]
                .iter()
                .all(|&(name, want)| ours.get_value::<u32, _>(name).ok() == Some(want))
            })
}

/// Remove the listener, its rules and the DLLs. A DLL still loaded goes at the next restart.
pub(super) fn remove() {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let ours = hklm
        .open_subkey_with_flags(
            format!(r"{WINSTATIONS}\{LISTENER}"),
            KEY_READ | KEY_WOW64_64KEY,
        )
        .and_then(|key| key.get_value::<u32, _>(MARKER))
        .is_ok_and(|marker| marker == 1);
    if ours
        && let Ok(stations) =
            hklm.open_subkey_with_flags(WINSTATIONS, KEY_ALL_ACCESS | KEY_WOW64_64KEY)
        && let Err(e) = stations.delete_subkey_all(LISTENER)
    {
        tracing::warn!(error = %e, "seat listener key did not go");
    }
    for (name, _) in RULES {
        let _ = run(
            "netsh.exe",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={name}"),
            ],
        );
    }
    sweep(&native_dir(), None);
    sweep(&wow_dir(), None);
}

fn native_dir() -> PathBuf {
    PathBuf::from(pf_paths::system32("punktfunk"))
}

fn wow_dir() -> PathBuf {
    PathBuf::from(pf_paths::syswow64("punktfunk"))
}

/// Copy both builds from `{app}\seat-audio` under a name that changes with their content, and
/// return the path the listener names. A build in use again leaves the restart's delete queue,
/// where [`sweep`] put it while seats were off.
fn deploy(host_path: &Path) -> Result<PathBuf, String> {
    let source = host_path.with_file_name("seat-audio");
    let read = |arch: &str| {
        let path = source.join(arch).join(format!("{DLL}.dll"));
        std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))
    };
    let (x64, x86) = (read("x64")?, read("x86")?);
    let name = format!("{DLL}-{:016x}.dll", fnv1a(&[&x64, &x86]));
    for (dir, bytes) in [(native_dir(), &x64), (wow_dir(), &x86)] {
        let target = dir.join(&name);
        if !std::fs::read(&target).is_ok_and(|have| have == *bytes) {
            std::fs::create_dir_all(&dir)
                .and_then(|()| std::fs::write(&target, bytes))
                .map_err(|e| format!("write {}: {e}", target.display()))?;
        }
        keep_at_restart(&target);
    }
    Ok(native_dir().join(name))
}

/// Delete every seat audio DLL in `dir` but `keep`, scheduling the loaded ones for restart.
fn sweep(dir: &Path, keep: Option<&Path>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let keep = keep.and_then(Path::file_name);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if !text.starts_with(DLL) || !text.ends_with(".dll") || Some(name.as_os_str()) == keep {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_err() {
            delete_at_restart(&entry.path());
        }
    }
    if keep.is_none() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Queue a loaded DLL for the next restart, once: every seat start sweeps the same stale ones.
fn delete_at_restart(path: &Path) {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    if restart_queue(KEY_READ)
        .is_some_and(|(_, ops)| restart_deletes::deletes(&ops, &path.to_string_lossy()))
    {
        return;
    }
    let path = HSTRING::from(path.as_os_str());
    // SAFETY: schedules a file this module wrote for deletion at the next restart.
    let _ = unsafe { MoveFileExW(&path, None, MOVEFILE_DELAY_UNTIL_REBOOT) };
}

/// The restart's file queue and its key, when the key opens and holds one.
fn restart_queue(access: u32) -> Option<(RegKey, Vec<String>)> {
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(restart_deletes::KEY, access | KEY_WOW64_64KEY)
        .ok()?;
    let raw = key.get_raw_value(restart_deletes::VALUE).ok()?;
    let units: Vec<u16> = raw
        .bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some((key, restart_deletes::decode(&units)))
}

/// Take `path` out of the restart's delete queue.
fn keep_at_restart(path: &Path) {
    let Some((key, ops)) = restart_queue(KEY_ALL_ACCESS) else {
        return;
    };
    let Some(kept) = restart_deletes::without_deletes_under(&ops, &path.to_string_lossy()) else {
        return;
    };
    let written = if kept.is_empty() {
        key.delete_value(restart_deletes::VALUE)
    } else {
        let bytes: Vec<u8> = restart_deletes::encode(&kept)
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        key.set_raw_value(
            restart_deletes::VALUE,
            &RegValue {
                bytes: bytes.into(),
                vtype: REG_MULTI_SZ,
            },
        )
    };
    if let Err(error) = written {
        tracing::warn!(%error, path = %path.display(), "seat audio DLL stays queued for deletion");
    }
}

/// `RDP-Tcp`'s settings, our port and enumerator, then the enable flag last.
fn write_listener(dll: &Path) -> std::io::Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let stock = hklm.open_subkey_with_flags(
        format!(r"{WINSTATIONS}\{STOCK}"),
        KEY_READ | KEY_WOW64_64KEY,
    )?;
    let (ours, _) = hklm.create_subkey_with_flags(
        format!(r"{WINSTATIONS}\{LISTENER}"),
        KEY_ALL_ACCESS | KEY_WOW64_64KEY,
    )?;
    let marker = ours.get_value::<u32, _>(MARKER).ok();
    if marker != Some(1) && ours.enum_values().next().is_some() {
        return Err(std::io::Error::other(format!(
            "a listener named {LISTENER} exists and is not punktfunk's"
        )));
    }
    set(&ours, MARKER, &1_u32.to_reg_value())?;
    copy_values(&stock, &ours)?;
    for name in stock.enum_keys().flatten() {
        let (sub, _) = ours.create_subkey(&name)?;
        copy_tree(&stock.open_subkey(&name)?, &sub)?;
    }
    set(
        &ours,
        "PortNumber",
        &u32::from(LISTENER_PORT).to_reg_value(),
    )?;
    set(
        &ours,
        "AudioEnumeratorDll",
        &dll.to_string_lossy().as_ref().to_reg_value(),
    )?;
    set(&ours, "fEnableWinStation", &1_u32.to_reg_value())
}

fn copy_values(from: &RegKey, to: &RegKey) -> std::io::Result<()> {
    for (name, value) in from.enum_values().flatten() {
        if !OWN_VALUES.iter().any(|own| own.eq_ignore_ascii_case(&name)) {
            set(to, &name, &value)?;
        }
    }
    Ok(())
}

fn copy_tree(from: &RegKey, to: &RegKey) -> std::io::Result<()> {
    for (name, value) in from.enum_values().flatten() {
        set(to, &name, &value)?;
    }
    for name in from.enum_keys().flatten() {
        let (sub, _) = to.create_subkey(&name)?;
        copy_tree(&from.open_subkey(&name)?, &sub)?;
    }
    Ok(())
}

/// Write `value` unless the key already holds it.
fn set(key: &RegKey, name: &str, value: &RegValue) -> std::io::Result<()> {
    match key.get_raw_value(name) {
        Ok(have) if have.vtype == value.vtype && have.bytes == value.bytes => Ok(()),
        _ => key.set_raw_value(name, value),
    }
}

/// Block the listener's port from everything but loopback, as the `Remote Desktop` group's
/// scope does for 3389.
fn block_network() -> Result<(), String> {
    for (name, protocol) in RULES {
        let name_arg = format!("name={name}");
        if run(
            "netsh.exe",
            &["advfirewall", "firewall", "show", "rule", &name_arg],
        ) == Some(0)
        {
            continue;
        }
        let port = format!("localport={LISTENER_PORT}");
        let added = run(
            "netsh.exe",
            &[
                "advfirewall",
                "firewall",
                "add",
                "rule",
                &name_arg,
                "dir=in",
                "action=block",
                &format!("protocol={protocol}"),
                &port,
                &format!("remoteip={NOT_LOOPBACK}"),
                "profile=any",
            ],
        );
        if added != Some(0) {
            return Err(format!("netsh add rule {name} exited {added:?}"));
        }
    }
    Ok(())
}

/// FNV-1a over the concatenation of `parts`: a stable name for one pair of builds.
fn fnv1a(parts: &[&[u8]]) -> u64 {
    parts
        .iter()
        .flat_map(|part| part.iter())
        .fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_pair_names_its_file() {
        assert_eq!(fnv1a(&[]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(&[b"a"]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(&[b"ab", b"c"]), fnv1a(&[b"a", b"bc"]));
        assert_ne!(fnv1a(&[b"x64", b"x86"]), fnv1a(&[b"x86", b"x64"]));
    }
}
