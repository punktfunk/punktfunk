//! The legs no process can do: SCM stop-and-wait, `.lnk` writing, the env-change broadcast,
//! the Appx presence check, real randomness, the parent-console attach. `cfg(windows)`
//! implementations; everywhere else an honest error so a cross-OS test that reaches one
//! sees a warning path, never a lie.

/// The parent console's output end. Writes go to the console as UTF-16: the bytes of a plain
/// file write are read in the console's code page (932, 949, 936), which garbles any
/// non-ASCII text, an install path under `C:\Users\田中` above all.
#[cfg(windows)]
pub struct ConsoleOut {
    file: std::fs::File,
    /// `GetConsoleMode` answers only for a console. Anything else keeps the raw byte write.
    console: bool,
    /// The head of a UTF-8 character a write stopped inside.
    pending: Vec<u8>,
}

/// Off Windows [`attach_parent_console`] is always `None`.
#[cfg(not(windows))]
pub type ConsoleOut = std::fs::File;

#[cfg(windows)]
impl std::io::Write for ConsoleOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use ::windows::Win32::Foundation::HANDLE;
        use ::windows::Win32::System::Console::WriteConsoleW;
        use std::os::windows::io::AsRawHandle;
        if !self.console {
            return std::io::Write::write(&mut self.file, buf);
        }
        let wide = utf16_of(&mut self.pending, buf);
        if !wide.is_empty() {
            let mut written = 0u32;
            // SAFETY: the handle is the live `CONOUT$` file this struct owns; `wide` and
            // `written` are live for the call.
            unsafe {
                WriteConsoleW(
                    HANDLE(self.file.as_raw_handle()),
                    &wide,
                    Some(&mut written),
                    None,
                )?
            };
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.file)
    }
}

/// `buf` as UTF-16, after what the last call held back. A UTF-8 sequence cut at the end of
/// `buf` waits in `pending` for the next call; bytes that are not UTF-8 become U+FFFD.
#[cfg(any(windows, test))]
fn utf16_of(pending: &mut Vec<u8>, buf: &[u8]) -> Vec<u16> {
    pending.extend_from_slice(buf);
    let whole = match std::str::from_utf8(pending) {
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        _ => pending.len(),
    };
    let tail = pending.split_off(whole);
    let wide = String::from_utf8_lossy(pending).encode_utf16().collect();
    *pending = tail;
    wide
}

/// A GUI-subsystem exe launched from a terminal: bind the parent's console and open its
/// output end. `None` when no parent console exists (a double-click), which simply means
/// no console output. The updater's spawn does attach: the service host owns a hidden
/// console, and that one dies when the upgrade stops the service. `CONOUT$` rather than
/// `stdout()`: the std handles were fixed at startup, before the attach.
#[cfg(windows)]
pub fn attach_parent_console() -> Option<ConsoleOut> {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::System::Console::{
        AttachConsole, GetConsoleMode, ATTACH_PARENT_PROCESS, CONSOLE_MODE,
    };
    use std::os::windows::io::AsRawHandle;
    // SAFETY: a plain call with a process-id constant; it binds an already-existing console
    // or fails, and either way touches no memory of ours.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS).ok()? };
    // Read access too: `GetConsoleMode` refuses a handle without it.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")
        .ok()?;
    let mut mode = CONSOLE_MODE(0);
    // SAFETY: the handle is the live file just opened; `mode` is a live local.
    let console = unsafe { GetConsoleMode(HANDLE(file.as_raw_handle()), &mut mode) }.is_ok();
    Some(ConsoleOut {
        file,
        console,
        pending: Vec::new(),
    })
}

#[cfg(not(windows))]
pub fn attach_parent_console() -> Option<ConsoleOut> {
    None
}

/// Queue `path` for deletion at the next boot (`PendingFileRenameOperations`): the way out
/// for a file the uninstaller cannot delete now — its own running exe above all.
#[cfg(windows)]
pub fn delete_on_reboot(path: &std::path::Path) -> bool {
    use ::windows::core::{HSTRING, PCWSTR};
    use ::windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: a null destination with DELAY_UNTIL_REBOOT is the documented delete form;
    // the source string outlives the call.
    unsafe { MoveFileExW(&wide, PCWSTR::null(), MOVEFILE_DELAY_UNTIL_REBOOT).is_ok() }
}

#[cfg(not(windows))]
pub fn delete_on_reboot(_path: &std::path::Path) -> bool {
    false
}

/// Take `dir` and everything under it out of the next boot's delete queue, and say how many
/// deletes went. An uninstall queues the files it can't delete, its own exe above all; the
/// install that writes them again must unqueue them, or the next restart deletes the new ones.
#[cfg(windows)]
pub fn cancel_restart_deletes(dir: &std::path::Path) -> usize {
    use ::windows::core::HSTRING;
    use ::windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_LOCAL_MACHINE, REG_MULTI_SZ,
        RRF_RT_REG_MULTI_SZ,
    };
    use pf_paths::restart_deletes as queue;
    let (key, value) = (HSTRING::from(queue::KEY), HSTRING::from(queue::VALUE));
    let mut bytes = 0u32;
    // SAFETY: `key` and `value` are live NUL-terminated strings; a null data pointer asks only
    // for the size, written to the live local `bytes`.
    let sized = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &key,
            &value,
            RRF_RT_REG_MULTI_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if sized.is_err() {
        return 0;
    }
    let mut data = vec![0u16; (bytes as usize).div_ceil(2)];
    // SAFETY: as above; `data` is writable for `bytes` bytes and outlives the call.
    let read = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &key,
            &value,
            RRF_RT_REG_MULTI_SZ,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if read.is_err() {
        return 0;
    }
    data.truncate(bytes as usize / 2);
    let ops = queue::decode(&data);
    let Some(kept) = queue::without_deletes_under(&ops, &dir.to_string_lossy()) else {
        return 0;
    };
    let cancelled = ops.len().div_ceil(2) - kept.len() / 2;
    let written = if kept.is_empty() {
        // SAFETY: `key` and `value` are live NUL-terminated strings.
        unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, &key, &value) }
    } else {
        let data = queue::encode(&kept);
        // SAFETY: as above; `data` is live for the call and its byte length is passed.
        unsafe {
            RegSetKeyValueW(
                HKEY_LOCAL_MACHINE,
                &key,
                &value,
                REG_MULTI_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        }
    };
    if written.is_ok() {
        cancelled
    } else {
        0
    }
}

#[cfg(not(windows))]
pub fn cancel_restart_deletes(_dir: &std::path::Path) -> usize {
    0
}

/// Queue `new` to replace `dest` at the next boot: Inno's `restartreplace` for an image
/// still mapped by a process the pre-copy stop could not reach.
#[cfg(windows)]
pub fn replace_on_reboot(new: &std::path::Path, dest: &std::path::Path) -> bool {
    use ::windows::core::HSTRING;
    use ::windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT, MOVEFILE_REPLACE_EXISTING,
    };
    let (from, to) = (
        HSTRING::from(new.as_os_str()),
        HSTRING::from(dest.as_os_str()),
    );
    // SAFETY: both strings outlive the call; the flag pair is the documented replace form.
    unsafe {
        MoveFileExW(
            &from,
            &to,
            MOVEFILE_DELAY_UNTIL_REBOOT | MOVEFILE_REPLACE_EXISTING,
        )
        .is_ok()
    }
}

#[cfg(not(windows))]
pub fn replace_on_reboot(_new: &std::path::Path, _dest: &std::path::Path) -> bool {
    false
}

#[cfg(windows)]
pub fn stop_service_wait(name: &str) -> Result<(), String> {
    use ::windows::core::{Owned, HSTRING};
    use ::windows::Win32::System::Services::{
        ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
        SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED,
    };

    // SAFETY: each SCM handle is adopted by an `Owned` right after its open succeeds, which
    // closes it on every path. The status buffer is a stack local owned by this frame.
    unsafe {
        let scm = Owned::new(
            OpenSCManagerW(None, None, SC_MANAGER_CONNECT).map_err(|e| format!("SCM: {e}"))?,
        );
        let Ok(service) = OpenServiceW(
            *scm,
            &HSTRING::from(name),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        ) else {
            // Not installed — nothing to stop, which is the goal state.
            return Ok(());
        };
        let service = Owned::new(service);
        let mut status = SERVICE_STATUS::default();
        let _ = ControlService(*service, SERVICE_CONTROL_STOP, &mut status);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if QueryServiceStatus(*service, &mut status).is_err()
                || status.dwCurrentState == SERVICE_STOPPED
            {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(format!("'{name}' did not stop within 30s"));
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
}

#[cfg(not(windows))]
pub fn stop_service_wait(_name: &str) -> Result<(), String> {
    Err("SCM is Windows-only".into())
}

#[cfg(windows)]
pub fn create_shortcut(link: &str, target: &str, args: &str) -> Result<(), String> {
    use ::windows::core::{Interface, HSTRING};
    use ::windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use ::windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink,
        KF_FLAG_DEFAULT,
    };

    // SAFETY: COM lifecycle as in `nlm_networks` — init tolerating an initialized thread,
    // smart-pointer interfaces, uninit only when this call did the init. The known-folder
    // path is CoTaskMem the shell hands us: copied out, then freed once.
    unsafe {
        let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| -> Result<(), String> {
            let resolve = |id, tail: &str| -> Result<String, String> {
                let base = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
                    .map_err(|e| format!("known folder: {e}"))?;
                let text = base.to_string();
                CoTaskMemFree(Some(base.0 as *const _));
                Ok(format!("{}{tail}", text.map_err(|e| e.to_string())?))
            };
            let path = if let Some(rest) = link.strip_prefix("<start menu>") {
                resolve(&FOLDERID_Programs, rest)?
            } else if let Some(rest) = link.strip_prefix("<desktop>") {
                resolve(&FOLDERID_Desktop, rest)?
            } else {
                link.to_string()
            };
            let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("ShellLink: {e}"))?;
            sl.SetPath(&HSTRING::from(target))
                .map_err(|e| format!("SetPath: {e}"))?;
            if !args.is_empty() {
                sl.SetArguments(&HSTRING::from(args))
                    .map_err(|e| format!("SetArguments: {e}"))?;
            }
            let pf: IPersistFile = sl.cast().map_err(|e| format!("IPersistFile: {e}"))?;
            pf.Save(&HSTRING::from(path.as_str()), true)
                .map_err(|e| format!("Save: {e}"))
        })();
        if inited {
            CoUninitialize();
        }
        result
    }
}

#[cfg(not(windows))]
pub fn create_shortcut(_link: &str, _target: &str, _args: &str) -> Result<(), String> {
    Err("shortcuts are Windows-only".into())
}

/// The visible desktop, this user's and every user's Start menu Programs folder, from the
/// shell: OneDrive can move the first two away from `%USERPROFILE%`. `None` where the shell
/// has no answer.
#[cfg(windows)]
pub fn shell_folders() -> (Option<String>, Option<String>, Option<String>) {
    use ::windows::core::GUID;
    use ::windows::Win32::System::Com::CoTaskMemFree;
    use ::windows::Win32::UI::Shell::{
        FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs, SHGetKnownFolderPath,
        KF_FLAG_DEFAULT,
    };
    let get = |id: &GUID| {
        // SAFETY: `id` is a live known-folder GUID. The path is CoTaskMem the shell hands us:
        // copied out, then freed once.
        unsafe {
            let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
            let text = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            text
        }
    };
    (
        get(&FOLDERID_Desktop),
        get(&FOLDERID_Programs),
        get(&FOLDERID_CommonPrograms),
    )
}

#[cfg(not(windows))]
pub fn shell_folders() -> (Option<String>, Option<String>, Option<String>) {
    (None, None, None)
}

/// A `REG_SZ` / `REG_EXPAND_SZ` value under `HKLM\…` or `HKCU\…`, unexpanded, through the
/// registry API: no code-page round trip. `None` when absent or unreadable.
#[cfg(windows)]
pub fn reg_string(key: &str, name: &str) -> Option<String> {
    use ::windows::core::HSTRING;
    use ::windows::Win32::Foundation::ERROR_MORE_DATA;
    use ::windows::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ,
        RRF_RT_REG_SZ,
    };
    let (hive, sub) = key.split_once('\\')?;
    let hive = match hive {
        "HKLM" | "HKEY_LOCAL_MACHINE" => HKEY_LOCAL_MACHINE,
        "HKCU" | "HKEY_CURRENT_USER" => HKEY_CURRENT_USER,
        _ => return None,
    };
    let (sub, name) = (HSTRING::from(sub), HSTRING::from(name));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut bytes = 0u32;
    // SAFETY: `sub` and `name` are live NUL-terminated strings; a null data pointer asks only
    // for the size, written to the live local `bytes`.
    unsafe { RegGetValueW(hive, &sub, &name, flags, None, None, Some(&mut bytes)) }
        .ok()
        .ok()?;
    // Three tries: the value can grow between the size query and the read.
    for _ in 0..3 {
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
        // SAFETY: as above; `buf` is writable for `bytes` bytes and outlives the call.
        let got = unsafe {
            RegGetValueW(
                hive,
                &sub,
                &name,
                flags,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if got == ERROR_MORE_DATA {
            continue;
        }
        got.ok().ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        return Some(String::from_utf16_lossy(&buf[..len]));
    }
    None
}

#[cfg(windows)]
pub fn broadcast_env_change() -> Result<(), String> {
    use ::windows::core::w;
    use ::windows::Win32::Foundation::{LPARAM, WPARAM};
    use ::windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };

    // SAFETY: HWND_BROADCAST with a static wide literal and a bounded timeout; no memory is
    // handed to the receivers beyond the constant string.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2000,
            None,
        );
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn broadcast_env_change() -> Result<(), String> {
    Err("WM_SETTINGCHANGE is Windows-only".into())
}

/// The oldest Windows App Runtime 2 the client's windows-reactor starts on.
pub const APP_RUNTIME_MIN: (u32, u32, u32) = (2, 6, 4);

/// Package-family query for this user: is a runtime at or above [`APP_RUNTIME_MIN`]
/// installed? `false` means "run the installer"; any query error reads as absent.
#[cfg(windows)]
pub fn app_runtime_present() -> bool {
    use ::windows::core::{HSTRING, PWSTR};
    use ::windows::Win32::Foundation::ERROR_SUCCESS;
    use ::windows::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;

    let family = HSTRING::from("Microsoft.WindowsAppRuntime.2_8wekyb3d8bbwe");
    let (mut count, mut len) = (0u32, 0u32);
    // SAFETY: the count-query form — null buffers, out-params only.
    unsafe {
        let _ = GetPackagesByPackageFamily(&family, &mut count, None, &mut len, None);
    }
    if count == 0 || len == 0 {
        return false;
    }
    let mut names = vec![PWSTR::null(); count as usize];
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `names` holds `count` pointers and `buf` holds `len` UTF-16 units, the sizes the
    // count query asked for; the call writes no further than either.
    let err = unsafe {
        GetPackagesByPackageFamily(
            &family,
            &mut count,
            Some(names.as_mut_ptr()),
            &mut len,
            Some(PWSTR(buf.as_mut_ptr())),
        )
    };
    if err != ERROR_SUCCESS {
        return false;
    }
    names.iter().take(count as usize).any(|name| {
        // SAFETY: each pointer the call wrote points at a NUL-terminated name inside `buf`.
        let name = unsafe { name.to_string() }.unwrap_or_default();
        runtime_meets_floor(&name)
    })
}

#[cfg(not(windows))]
pub fn app_runtime_present() -> bool {
    false
}

/// `Microsoft.WindowsAppRuntime.2_2.6.4.0_x64__8wekyb3d8bbwe`: the second field is the version.
#[cfg(any(windows, test))]
fn runtime_meets_floor(full_name: &str) -> bool {
    let mut parts = full_name.split('_').nth(1).unwrap_or("").split('.');
    let mut next = || {
        parts
            .next()
            .and_then(|p| p.parse::<u32>().ok())
            .unwrap_or(0)
    };
    (next(), next(), next()) >= APP_RUNTIME_MIN
}

/// `n` random bytes as lowercase hex. Real RNG — this is a credential, not a timestamp.
#[cfg(windows)]
pub fn random_hex(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness source: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(not(windows))]
pub fn random_hex(_n: usize) -> Result<String, String> {
    Err("password generation runs on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::{runtime_meets_floor, utf16_of};

    fn utf16(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn the_console_gets_non_ascii_text_as_utf16() {
        let mut pending = Vec::new();
        let line = "install to C:\\Users\\田中\\Punktfunk 😀\n";
        assert_eq!(utf16_of(&mut pending, line.as_bytes()), utf16(line));
        assert!(pending.is_empty());
    }

    #[test]
    fn a_character_split_across_writes_is_held_until_whole() {
        let bytes = "田".as_bytes();
        let mut pending = Vec::new();
        assert!(utf16_of(&mut pending, &bytes[..2]).is_empty());
        assert_eq!(utf16_of(&mut pending, &bytes[2..]), utf16("田"));
        assert!(pending.is_empty());
    }

    #[test]
    fn bytes_that_are_not_utf8_do_not_panic() {
        let mut pending = Vec::new();
        assert_eq!(utf16_of(&mut pending, b"a\xffb"), utf16("a\u{fffd}b"));
    }

    #[test]
    fn the_runtime_floor_reads_the_package_version() {
        assert!(runtime_meets_floor(
            "Microsoft.WindowsAppRuntime.2_2.6.4.0_x64__8wekyb3d8bbwe"
        ));
        assert!(runtime_meets_floor(
            "Microsoft.WindowsAppRuntime.2_2.10.0.0_arm64__8wekyb3d8bbwe"
        ));
        assert!(!runtime_meets_floor(
            "Microsoft.WindowsAppRuntime.2_2.6.3.0_x64__8wekyb3d8bbwe"
        ));
        assert!(!runtime_meets_floor(
            "Microsoft.WindowsAppRuntime.2_2.2.7.0_x64__8wekyb3d8bbwe"
        ));
        assert!(!runtime_meets_floor("garbage"));
    }
}
