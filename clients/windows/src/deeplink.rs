//! `punktfunk://` activation for the Windows shell (design/client-deep-links.md §4.2).
//!
//! Protocol activation of a full-trust packaged app delivers the URI as the command line, so
//! a browser prompt, `start punktfunk://…` and a written `.lnk` all arrive the same way: as a
//! positional argument. What Windows does NOT give us is single-instancing — unlike
//! GApplication on Linux, a second activation is simply a second process. So this module adds
//! it: the first instance claims a named mutex, and any later one hands its URL to the winner
//! over `WM_COPYDATA` and exits.
//!
//! A URL must never be silently dropped, which is why the hand-off retries while the primary's
//! receiver window is still coming up, waits for the receiver to accept the link, and falls back
//! to running this instance normally rather than exiting quietly.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use windows::Win32::libloaderapi::GetModuleHandleW;
use windows::Win32::minwindef::{LPARAM, LRESULT, WPARAM};
use windows::Win32::synchapi::{CreateMutexW, ReleaseMutex};
use windows::Win32::windef::HWND;
use windows::Win32::winnt::HANDLE;
use windows::Win32::winuser::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowExW, GetMessageW, RegisterClassW,
    SendMessageW, COPYDATASTRUCT, HWND_MESSAGE, MSG, WM_COPYDATA, WNDCLASSW,
};

/// The single-instance mutex. Named per the design; deliberately not `Global\` — one shell per
/// user session is the rule, and a second desktop user gets their own.
const MUTEX_NAME: windows::core::PCWSTR = windows::core::w!("unom.punktfunk.client");

/// Tags our `WM_COPYDATA` so a stray message from anything else is ignored rather than parsed.
const COPYDATA_URL: usize = 0x7066_0001; // 'pf' + 1

/// Window class of the primary's message-only receiver; later instances find it by this name.
const RECEIVER_CLASS: windows::core::PCWSTR = windows::core::w!("unom.punktfunk.deeplink");

/// URLs delivered by another instance, waiting for the app's poll to pick them up. A queue
/// rather than a single slot: two shortcuts double-clicked in quick succession are two links,
/// and dropping either would be exactly the silent loss this design forbids.
static INBOX: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The claimed mutex handle, held for the process lifetime. Stored so it is released on exit
/// (Windows would release it anyway when the process dies; being explicit costs nothing and
/// documents the intent).
static MUTEX: AtomicUsize = AtomicUsize::new(0);

/// Close one live, process-owned Win32 handle.
///
/// # Safety
/// `handle` must be valid and must not be used or closed after this call.
unsafe fn close_handle(handle: HANDLE) {
    windows::core::link!("kernel32.dll" "system" fn CloseHandle(hobject: HANDLE) -> windows::core::BOOL);
    // SAFETY: the caller's contract makes `handle` a live, process-owned handle that no one
    // uses or closes again.
    let _ = unsafe { CloseHandle(handle) };
}

/// A positional `punktfunk://` (or the `pf://` input alias) anywhere in argv — how protocol
/// activation, `start`, and a `.lnk` shortcut all deliver a link. Validation happens later in
/// the shared parser; this only decides whether argv carries something addressed to us.
pub(crate) fn positional_url(args: &[String]) -> Option<String> {
    args.iter()
        .skip(1)
        .find(|a| pf_client_core::deeplink::is_link_arg(a))
        .cloned()
}

/// Try to claim the user-session mutex and become the primary shell.
///
/// `CreateMutexW` returns a valid handle even when the mutex already exists. That secondary handle
/// is closed immediately; a newly created handle is retained until [`release_primary`]. If creation
/// fails, this process continues as a primary so activation is not silently dropped.
pub(crate) fn claim_primary() -> bool {
    // SAFETY: the static name stays live through `CreateMutexW`. Every valid returned handle is
    // either closed on the secondary path or stored for `release_primary` to take exactly once.
    unsafe {
        let handle = CreateMutexW(None, true, MUTEX_NAME);
        // A second window is preferable to dropping the activation when ownership is unknowable.
        if handle.0.is_null() {
            let e = std::io::Error::last_os_error();
            tracing::warn!(error = %e, "single instance mutex; continuing as primary");
            return true;
        }
        let already = std::io::Error::last_os_error().raw_os_error()
            == Some(windows::Win32::winerror::ERROR_ALREADY_EXISTS);
        if already {
            close_handle(handle);
            return false;
        }
        MUTEX.store(handle.0 as usize, Ordering::Relaxed);
        true
    }
}

/// Relinquish and close the primary mutex handle at process exit.
///
/// Taking the handle out of the atomic first makes concurrent or repeated calls no-ops, so the one
/// stored owner receives exactly one `ReleaseMutex` and one `CloseHandle`.
pub(crate) fn release_primary() {
    let raw = MUTEX.swap(0, Ordering::Relaxed);
    if raw != 0 {
        let handle = HANDLE(raw as *mut _);
        // SAFETY: `handle` is the live owner taken from `MUTEX`; no other caller can take it again.
        unsafe {
            let _ = ReleaseMutex(handle);
            close_handle(handle);
        }
    }
}

/// Hand `url` to the running shell's receiver. Retries briefly: the primary may still be starting
/// when a second launch lands (a double-clicked shortcut while the app is starting is the
/// ordinary case), and giving up then would drop the link.
///
/// Only a reply of 1 counts: that is `wnd_proc` confirming the link is queued.
/// `false` = the primary never accepted it, and the caller should just run normally.
pub(crate) fn forward_to_primary(url: &str) -> bool {
    let wide: Vec<u16> = url.encode_utf16().collect();
    let data = COPYDATASTRUCT {
        dwData: COPYDATA_URL,
        cbData: (wide.len() * 2) as u32,
        lpData: wide.as_ptr() as *mut _,
    };
    for attempt in 0..20 {
        // SAFETY: `FindWindowExW` takes static literals. `data` points at `wide`; both outlive
        // `SendMessageW`, which returns only after the receiver has copied the payload. That is
        // why this is a send, not a post.
        let accepted = unsafe {
            let hwnd = FindWindowExW(Some(HWND_MESSAGE), None, RECEIVER_CLASS, None);
            !hwnd.0.is_null()
                && SendMessageW(
                    hwnd,
                    WM_COPYDATA as u32,
                    WPARAM(0),
                    LPARAM(&data as *const _ as isize),
                )
                .0 == 1
        };
        if accepted {
            tracing::info!(attempt, "handed the link to the running shell");
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    tracing::warn!("no running shell accepted the link; opening it here instead");
    false
}

/// Start listening for links from later instances on a thread of their own.
pub(crate) fn install_receiver() {
    std::thread::Builder::new()
        .name("pf-deeplink-receiver".into())
        .spawn(run_receiver)
        .ok();
}

/// Register the receiver class, create its message-only window and pump its messages.
///
/// The window belongs to the calling thread, so `wnd_proc` runs here and the WinUI window and UI
/// thread take no part. Runs once per process: a second class registration fails.
fn run_receiver() {
    // SAFETY: `class` and its static name are live across `RegisterClassW`, and `wnd_proc` has
    // the `WNDPROC` signature. The window is created, pumped and dispatched on this one thread;
    // `msg` is a local that `GetMessageW` fills before `DispatchMessageW` reads it.
    unsafe {
        let instance = GetModuleHandleW(None);
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: RECEIVER_CLASS,
            ..Default::default()
        };
        let hwnd = if RegisterClassW(&class).0 == 0 {
            HWND::default()
        } else {
            CreateWindowExW(
                0,
                RECEIVER_CLASS,
                None,
                0,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance),
                None,
            )
        };
        if hwnd.0.is_null() {
            let e = std::io::Error::last_os_error();
            tracing::warn!(
                error = %e,
                "deep link receiver did not start; links from other instances won't arrive"
            );
            return;
        }
        let mut msg = MSG::default();
        // 0 is WM_QUIT and -1 an error; both end the loop.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            DispatchMessageW(&msg);
        }
    }
}

/// Decode an OS-marshalled UTF-16 payload without assuming its byte pointer is u16-aligned.
///
/// An odd byte count is truncated in the middle of a code unit and is rejected. Complete native-
/// endian units retain the existing lossy handling of malformed surrogate pairs.
fn decode_utf16_payload(payload: &[u8]) -> Option<String> {
    if !payload.len().is_multiple_of(size_of::<u16>()) {
        return None;
    }
    let wide = payload
        .chunks_exact(size_of::<u16>())
        .map(|chunk| u16::from_ne_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Some(String::from_utf16_lossy(&wide))
}

/// The receiver's window procedure: queue tagged `WM_COPYDATA` links, default everything else.
///
/// Returns 1 only for a queued link. Null metadata or data pointers are ignored. Payloads are
/// bounded as bytes, then decoded without imposing u16 alignment on the marshalled buffer.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_COPYDATA as u32 && lparam.0 != 0 {
        // SAFETY: Windows keeps the marshalled `COPYDATASTRUCT` live for this synchronous handler.
        // The null check above and unaligned value read avoid manufacturing a borrowed reference.
        let cds = unsafe { (lparam.0 as *const COPYDATASTRUCT).read_unaligned() };
        if cds.dwData == COPYDATA_URL && !cds.lpData.is_null() {
            // SAFETY: Windows keeps the marshalled payload valid for its declared `cbData` bytes
            // during this handler. A byte slice imposes no u16-alignment requirement.
            let payload =
                unsafe { std::slice::from_raw_parts(cds.lpData.cast::<u8>(), cds.cbData as usize) };
            if let Some(url) = decode_utf16_payload(payload) {
                tracing::debug!(%url, "link from another instance");
                // Poison recovery avoids unwinding from a window procedure; the Vec remains valid.
                INBOX
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(url);
                return LRESULT(1);
            }
        }
    }
    // SAFETY: forwarding the OS parameters unchanged is required for messages we do not consume.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Take everything delivered since the last call — the app polls this and routes each one.
pub(crate) fn drain() -> Vec<String> {
    std::mem::take(&mut *INBOX.lock().unwrap())
}

/// Queue a link this process was launched with, so the cold start and the hand-off path feed
/// the router through one door.
pub(crate) fn queue(url: String) {
    INBOX.lock().unwrap().push(url);
}

/// Is a link waiting? Read WITHOUT draining, so the start screen can stand down for one while
/// leaving it for the router. The cold-start URL is queued before the app is built, so this is
/// already true by the first render — the poll below only picks up later arrivals.
pub(crate) fn pending() -> bool {
    !INBOX.lock().unwrap().is_empty()
}

/// Whether this process runs with MSIX package identity. Decides how a shortcut must target us
/// (`write_shortcut` below) and whether the process may stamp its own AppUserModelID
/// (`set_app_user_model_id` in main.rs).
pub(crate) fn has_package_identity() -> bool {
    use windows::Win32::appmodel::GetCurrentPackageFullName;
    use windows::Win32::winerror::APPMODEL_ERROR_NO_PACKAGE;
    // SAFETY: `GetCurrentPackageFullName` with `len = 0` and no buffer is the documented identity
    // PROBE — it writes nothing and only reports whether this process is packaged.
    unsafe {
        let mut len: u32 = 0;
        GetCurrentPackageFullName(&mut len, None) != APPMODEL_ERROR_NO_PACKAGE
    }
}

/// Write a `.lnk` on the Desktop that launches this URL, and return its path.
///
/// The shortcut targets the client exe with the URL as an ARGUMENT, rather than being a `.url`
/// internet shortcut. Both would work while the scheme is registered; only this one still works
/// if it isn't, because it invokes the client directly — which is the whole point of a shortcut
/// being a container for a URL rather than a second launch mechanism
/// (design/client-deep-links.md §5). Which exe reference is durable depends on how we were
/// installed: under MSIX the install path changes on every update but the app execution alias
/// doesn't, so packaged runs target the alias; the installer and portable builds have no alias
/// but a stable install dir, so unpackaged runs target the absolute exe path.
pub(crate) fn write_shortcut(label: &str, url: &str) -> Result<std::path::PathBuf, String> {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::combaseapi::{CoCreateInstance, CoInitializeEx};
    use windows::Win32::objbase::COINIT_APARTMENTTHREADED;
    use windows::Win32::objidl::IPersistFile;
    use windows::Win32::shobjidl_core::{IShellLinkW, ShellLink};
    use windows::Win32::wtypesbase::CLSCTX_INPROC_SERVER;

    let desktop = desktop_dir()?;
    let path = desktop.join(format!("{}.lnk", file_name(label)));
    // Alias when packaged, absolute path when not — see the doc comment above.
    let target = if has_package_identity() {
        "punktfunk-client.exe".to_string()
    } else {
        std::env::current_exe()
            .map_err(|e| format!("current exe: {e}"))?
            .to_string_lossy()
            .into_owned()
    };
    // SAFETY: COM calls on this thread's apartment. `CoCreateInstance` returns an owned interface
    // checked by `?`, and every setter below takes a borrowed `HSTRING`/`PCWSTR` that outlives its
    // synchronous call; nothing here dereferences a pointer the caller supplied.
    unsafe {
        // The UI thread is already apartment-threaded; this is belt and braces for the case
        // where a caller ever moves this off it. An already-initialised apartment returns
        // S_FALSE, which is not an error.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED as u32);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("shell link: {e}"))?;
        link.SetPath(&HSTRING::from(target.as_str()))
            .ok()
            .map_err(|e| format!("shortcut target: {e}"))?;
        link.SetArguments(&HSTRING::from(url))
            .ok()
            .map_err(|e| format!("shortcut argument: {e}"))?;
        link.SetDescription(&HSTRING::from(format!("Stream from {label}")))
            .ok()
            .map_err(|e| format!("shortcut description: {e}"))?;
        let persist: IPersistFile = link.cast().map_err(|e| format!("shortcut save: {e}"))?;
        // `to_string_lossy` rather than the OsStr: HSTRING is UTF-16 and the path came from an
        // env var plus our own sanitised name, so there is nothing lossy left to lose.
        persist
            .Save(&HSTRING::from(path.to_string_lossy().as_ref()), true)
            .ok()
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(path)
}

/// The user's Desktop as the shell resolves it. OneDrive Known Folder Move and policy redirect
/// it away from `%USERPROFILE%\Desktop`, which may then be stale or missing.
fn desktop_dir() -> Result<std::path::PathBuf, String> {
    use windows::Win32::combaseapi::CoTaskMemFree;
    use windows::Win32::shlobj_core::SHGetKnownFolderPath;
    // FOLDERID_Desktop: this windows-rs rev ships no knownfolders constants.
    const FOLDERID_DESKTOP: windows::core::GUID =
        windows::core::GUID::from_u128(0xb4bfcc3a_db2c_424c_b029_7fe99a87c641);
    // SAFETY: the GUID is a live constant; on success the returned string is the shell's
    // CoTaskMem allocation, copied out before it is freed exactly once.
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_DESKTOP, 0, None)
            .map_err(|e| format!("Desktop folder: {e}"))?;
        let path = p.to_string();
        CoTaskMemFree(p.0.cast());
        path.map(std::path::PathBuf::from)
            .map_err(|e| format!("Desktop folder: {e}"))
    }
}

/// A filename Windows will accept: its reserved characters replaced, length capped, and never
/// empty. Host and preset names are user text and reach this directly.
fn file_name(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if (c as u32) < 0x20 => '-',
            c => c,
        })
        .take(64)
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').to_string();
    if trimmed.is_empty() {
        "Punktfunk".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a test string in the payload's native-endian byte format.
    fn utf16_bytes(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_ne_bytes).collect()
    }

    /// Native-endian units decode correctly even when the byte slice starts at an odd address.
    #[test]
    fn utf16_payload_decodes_without_alignment() {
        let mut storage = vec![0xff];
        storage.extend(utf16_bytes("punktfunk://connect/Café 🎮"));
        assert_eq!(
            decode_utf16_payload(&storage[1..]),
            Some("punktfunk://connect/Café 🎮".to_string())
        );
    }

    /// A final unmatched byte is a truncated code unit, not a shorter valid payload.
    #[test]
    fn utf16_payload_rejects_odd_truncation() {
        let mut payload = utf16_bytes("link");
        payload.pop();
        assert_eq!(decode_utf16_payload(&payload), None);
    }

    /// Complete but malformed UTF-16 retains the receiver's lossy replacement behavior.
    #[test]
    fn utf16_payload_replaces_a_truncated_surrogate_pair() {
        assert_eq!(
            decode_utf16_payload(&0xd83du16.to_ne_bytes()),
            Some("\u{fffd}".to_string())
        );
    }

    /// Links are recognised wherever they sit in argv, and nothing else is.
    #[test]
    fn positional_url_finds_links_only() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            positional_url(&args(&["punktfunk-client.exe", "punktfunk://connect/Desk"])),
            Some("punktfunk://connect/Desk".into())
        );
        // The alias form still parses (it is never emitted, only accepted).
        assert_eq!(
            positional_url(&args(&["exe", "--windowed", "PF://connect/Desk"])),
            Some("PF://connect/Desk".into())
        );
        assert_eq!(positional_url(&args(&["exe", "--console"])), None);
        // argv[0] is never a link, even if someone renames the binary.
        assert_eq!(positional_url(&args(&["punktfunk://connect/Desk"])), None);
    }

    /// Shortcut names survive user text: reserved characters, control characters, a trailing
    /// dot (which Windows silently strips, breaking the path) and an empty result.
    #[test]
    fn shortcut_file_names_are_safe() {
        assert_eq!(file_name("Living Room PC"), "Living Room PC");
        assert_eq!(file_name("Desk: Work/Play"), "Desk- Work-Play");
        assert_eq!(file_name("Desk\u{1}"), "Desk-");
        assert_eq!(file_name("Trailing."), "Trailing");
        assert_eq!(file_name("   "), "Punktfunk");
        assert!(file_name(&"x".repeat(300)).len() <= 64);
    }
}
