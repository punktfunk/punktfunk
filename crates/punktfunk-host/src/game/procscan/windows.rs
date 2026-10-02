//! Windows matcher: Toolhelp snapshot plus each process's full image path.
//!
//! Contract: [`super`]. Two Windows facts load the two rules:
//!
//! * The host is SYSTEM and can open almost any process, so rule 1 (never adopt
//!   a process that predates the launch) is what keeps a pre-existing game from
//!   being killed when the session ends.
//! * There is no launch reaper and no readable environment. Do not query another
//!   process's environment via `NtQueryInformationProcess` — the layout is
//!   undocumented. Match on image path: the game's executable, or any executable
//!   under its install directory. Every Windows store reports one or both
//!   ([`crate::library::DetectSpec`]).

use super::{ProcRef, START_SLACK_SECS};
use crate::library::DetectSpec;
use std::path::{Path, PathBuf};
use windows::core::Owned;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 100-nanosecond `FILETIME` ticks per second — the unit every Win32 time API here reports in.
const FILETIME_TICKS_PER_SEC: f64 = 10_000_000.0;

/// Image-path buffer, UTF-16 units. Past `MAX_PATH` (260): a truncated path would
/// silently fail to match. Longer than this, `QueryFullProcessImageNameW` fails
/// and the process is skipped.
const IMAGE_PATH_MAX: usize = 4096;

/// Live-system scanner. Unit: Win32 has no fake process table. Matching is
/// tested via [`under_dir`] / [`same_path`] plus a live-process test.
pub struct Scanner;

impl Scanner {
    pub fn system() -> Self {
        Self
    }

    /// Seconds on the `FILETIME` epoch (`GetProcessTimes` creation time). `None` if unread.
    ///
    /// From `SystemTime`: same UTC epoch plus a constant. A clock step between this
    /// and the later creation-time read must exceed [`START_SLACK_SECS`] to matter.
    pub fn now_stamp(&self) -> Option<f64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        /// Seconds between the `FILETIME` epoch (1601-01-01) and the Unix epoch.
        const FILETIME_EPOCH_OFFSET_SECS: f64 = 11_644_473_600.0;
        Some(now.as_secs_f64() + FILETIME_EPOCH_OFFSET_SECS)
    }

    /// `min_start` is seconds on the [`Self::now_stamp`] timeline. `None` disables the filter.
    pub fn find(&self, spec: &DetectSpec, min_start: Option<f64>) -> Vec<ProcRef> {
        // Windows has only image-based signals. A spec with none must match nothing;
        // falling through would scan on an empty predicate.
        if spec.exe.is_none() && spec.install_dir.is_none() && spec.process_name.is_none() {
            return Vec::new();
        }
        let exe = spec
            .exe
            .as_deref()
            .map(|e| e.canonicalize().unwrap_or_else(|_| e.to_path_buf()));
        let dir = spec
            .install_dir
            .as_deref()
            .map(|d| d.canonicalize().unwrap_or_else(|_| d.to_path_buf()));

        let mut out = Vec::new();
        for (pid, ..) in processes() {
            // pid 0 (System Idle) and 4 (System) are neither openable nor ever a game.
            if pid <= 4 {
                continue;
            }
            let Some((start, image)) = process_start_and_image(pid) else {
                continue;
            };
            if let Some(min) = min_start {
                if start as f64 / FILETIME_TICKS_PER_SEC + START_SLACK_SECS < min {
                    continue; // predates this launch (rule 1)
                }
            }
            let hit = exe.as_deref().is_some_and(|w| same_path(&image, w))
                || dir.as_deref().is_some_and(|d| under_dir(&image, d))
                // Operator fallback: image file name, case-insensitive like the rest of Windows.
                || spec
                    .process_name
                    .as_deref()
                    .is_some_and(|w| same_name(&image, w));
            // The hints come from a plugin and the ladder kills as SYSTEM: only a process in
            // our session counts, the rule a plugin-reported pid already meets.
            if hit && crate::game_term::in_our_session(pid) {
                out.push(ProcRef { pid, start });
            }
        }
        out
    }

    /// Pin a pid the host just spawned by reading its creation time. Call immediately
    /// after spawn so the pid cannot have recycled (rule 2). Downstream re-verifies
    /// via [`Self::alive`]. `CreateProcessAsUserW` returns a bare pid, not a `Child`.
    pub fn resolve(&self, pid: u32) -> Option<ProcRef> {
        let (start, _image) = process_start_and_image(pid)?;
        Some(ProcRef { pid, start })
    }

    /// Image file name. Diagnostics only ([`super::names`]).
    pub fn name_of(&self, p: ProcRef) -> String {
        process_start_and_image(p.pid)
            .and_then(|(_, image)| image.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into())
    }

    /// `(pid, parent)` for every process. A parent pid can outlive its process and be reused, so
    /// only roots already known to be live are followed.
    pub fn parents(&self) -> Vec<(u32, u32)> {
        processes()
            .into_iter()
            .map(|(pid, parent, _)| (pid, parent))
            .collect()
    }

    /// Pid still present **and** creation time unchanged (rule 2). Windows reuses
    /// pids quickly; without this, signalling a remembered pid is unsafe.
    pub fn alive(&self, procs: &[ProcRef]) -> Vec<ProcRef> {
        procs
            .iter()
            .copied()
            .filter(|p| process_start_and_image(p.pid).is_some_and(|(start, _)| start == p.start))
            .collect()
    }
}

/// Steam `Running` flag, used only to **veto** declaring the game gone
/// ([`super::running_hint`]). Steam also sets it during updates and DLC installs,
/// and can leave it stale after a crash, so it is never a primary signal.
///
/// Reads every loaded hive under `HKEY_USERS`. Do not resolve the interactive SID
/// via `WTSQueryUserToken`: loaded hives are the logged-in set.
pub fn steam_running_hint(appid: u32) -> Option<bool> {
    use winreg::enums::{HKEY_USERS, KEY_READ};
    use winreg::RegKey;

    let users = RegKey::predef(HKEY_USERS);
    let mut saw_key = false;
    for sid in users.enum_keys().flatten() {
        // The `…_Classes` companion hives hold no Steam state.
        if sid.ends_with("_Classes") {
            continue;
        }
        let path = format!("{sid}\\Software\\Valve\\Steam\\Apps\\{appid}");
        let Ok(app) = users.open_subkey_with_flags(&path, KEY_READ) else {
            continue;
        };
        saw_key = true;
        if app.get_value::<u32, _>("Running").unwrap_or(0) != 0 {
            return Some(true);
        }
    }
    // Key exists and is 0 → not running. No key → no opinion (Steam never ran this app here).
    saw_key.then_some(false)
}

/// `(pid, parent pid, image base name)` for every process in one Toolhelp snapshot.
/// `szExeFile` is the module base name, not a path. Empty when the snapshot fails.
pub(crate) fn processes() -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    // SAFETY: `Owned` closes the snapshot on every path. `entry` carries `dwSize` before the
    // first read, and `szExeFile` is read up to its first NUL.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let snap = Owned::new(snap);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(*snap, &mut entry).is_ok();
        while more {
            let name = &entry.szExeFile;
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            out.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                String::from_utf16_lossy(&name[..len]),
            ));
            more = Process32NextW(*snap, &mut entry).is_ok();
        }
    }
    out
}

/// Full image path of a live process; `None` if unopenable or exited.
pub(crate) fn process_image(pid: u32) -> Option<PathBuf> {
    process_start_and_image(pid).map(|(_, image)| image)
}

/// Creation time (`FILETIME` ticks) and full image path.
/// `PROCESS_QUERY_LIMITED_INFORMATION` is the least privilege that answers both
/// and works on elevated processes without VM read. `None` if unopenable or exited.
fn process_start_and_image(pid: u32) -> Option<(u64, PathBuf)> {
    // SAFETY: `OpenProcess` yields an owned handle only on `Ok`; it is closed exactly once below on
    // every path. `GetProcessTimes` writes four `FILETIME`s we fully own; `QueryFullProcessImageNameW`
    // writes into `buf` and updates `len` in place, bounded by the `len` we pass in (buf.len()).
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created = Default::default();
        let (mut exit, mut kernel, mut user) =
            (Default::default(), Default::default(), Default::default());
        let times =
            GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user).is_ok();

        let mut buf = [0u16; IMAGE_PATH_MAX];
        let mut len = buf.len() as u32;
        // `PROCESS_NAME_FORMAT(0)` is `PROCESS_NAME_WIN32` (drive-letter path).
        // `PROCESS_NAME_NATIVE` is `\Device\HarddiskVolume…` and never equals a store path.
        let named = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(handle);

        if !times || !named {
            return None;
        }
        let start = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        let image = PathBuf::from(String::from_utf16_lossy(&buf[..len as usize]));
        Some((start, image))
    }
}

/// Case-insensitive equality: canonicalized store path vs live image path.
fn same_path(image: &Path, want: &Path) -> bool {
    eq_ignore_case(image, want)
}

/// Case-insensitive prefix, with a separator after the directory so
/// `…\Games\X` does not match `…\Games\XY\game.exe`.
fn under_dir(image: &Path, dir: &Path) -> bool {
    let (i, d) = (wide_lower(image), wide_lower(dir));
    let Some(rest) = i.strip_prefix(d.as_str()) else {
        return false;
    };
    rest.starts_with('\\') || rest.starts_with('/')
}

/// Image file name equals `want` ([`DetectSpec::process_name`]). Last component
/// only, so `Hades.exe` does not match a path that merely contains it.
fn same_name(image: &Path, want: &str) -> bool {
    image
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(want.trim()))
}

fn eq_ignore_case(a: &Path, b: &Path) -> bool {
    wide_lower(a) == wide_lower(b)
}

/// Lowercase for comparison. Strip the `\\?\` prefix `canonicalize` prepends;
/// a live image path never has it.
fn wide_lower(p: &Path) -> String {
    let s = p.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_dir_match_is_case_insensitive_and_separator_aware() {
        let dir = Path::new(r"C:\Games\Hades");
        assert!(under_dir(Path::new(r"c:\games\hades\Hades.exe"), dir));
        assert!(under_dir(Path::new(r"C:\Games\Hades\bin\crash.exe"), dir));
        assert!(!under_dir(Path::new(r"C:\Games\HadesII\game.exe"), dir));
        assert!(!under_dir(dir, dir));
        assert!(!under_dir(Path::new(r"D:\Games\Hades\Hades.exe"), dir));
    }

    #[test]
    fn exe_match_is_case_insensitive_and_ignores_the_verbatim_prefix() {
        assert!(same_path(
            Path::new(r"C:\Games\Hades\Hades.exe"),
            Path::new(r"\\?\c:\games\hades\hades.exe")
        ));
        assert!(!same_path(
            Path::new(r"C:\Games\Hades\Hades.exe"),
            Path::new(r"C:\Games\Hades\Other.exe")
        ));
    }

    #[test]
    fn process_name_matches_the_image_name_only() {
        assert!(same_name(
            Path::new(r"C:\Games\Hades\Hades.exe"),
            "hades.exe"
        ));
        assert!(same_name(
            Path::new(r"C:\Games\Hades\Hades.exe"),
            " Hades.exe "
        ));
        assert!(!same_name(
            Path::new(r"C:\Games\Hades\HadesLauncher.exe"),
            "Hades.exe"
        ));
        assert!(!same_name(
            Path::new(r"C:\Games\Hades.exe\other.exe"),
            "Hades.exe"
        ));

        // Reaches the live scan: `find` would otherwise skip a process_name-only spec.
        let me = std::env::current_exe().expect("current exe");
        let name = me.file_name().and_then(|n| n.to_str()).expect("exe name");
        let spec = DetectSpec {
            process_name: Some(name.to_ascii_uppercase()),
            ..Default::default()
        };
        assert!(Scanner::system()
            .find(&spec, None)
            .iter()
            .any(|p| p.pid == std::process::id()));
    }

    #[test]
    fn a_spec_with_no_path_signal_matches_nothing() {
        let s = Scanner::system();
        assert!(s.find(&DetectSpec::steam(570), None).is_empty());
        assert!(s
            .find(
                &DetectSpec::default().with_env("HEROIC_APP_NAME", Some("Quail".into())),
                None
            )
            .is_empty());
        assert!(s.find(&DetectSpec::default(), None).is_empty());
    }

    /// Live process table: the only way to catch a wrong `PROCESSENTRY32W` size, a
    /// bad `OpenProcess` mask, or creation times from the wrong `FILETIME` half.
    #[test]
    fn finds_this_process_by_its_own_image_path() {
        let me = std::env::current_exe().expect("current exe");
        let s = Scanner::system();
        let pid = std::process::id();
        let found = s.find(&DetectSpec::exe(&me), None);
        assert!(
            found.iter().any(|p| p.pid == pid),
            "scanning the real process table did not find this test process ({pid}) as {}",
            me.display()
        );
        let dir = me.parent().expect("exe has a parent");
        assert!(s
            .find(&DetectSpec::dir(dir), None)
            .iter()
            .any(|p| p.pid == pid));
        let mine: Vec<ProcRef> = found.into_iter().filter(|p| p.pid == pid).collect();
        assert_eq!(s.alive(&mine), mine);
        let recycled = vec![ProcRef {
            pid,
            start: mine[0].start ^ 0xFFFF,
        }];
        assert!(s.alive(&recycled).is_empty());
        // Future launch stamp excludes it (rule 1 against a real creation time).
        let future = s.now_stamp().unwrap() + 3_600.0;
        assert!(s.find(&DetectSpec::exe(&me), Some(future)).is_empty());
    }
}
