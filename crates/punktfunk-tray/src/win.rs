//! Windows tray: a hidden top-level window + `Shell_NotifyIconW`, fed by the status poller.
//!
//! A separate process from the host: `PunktfunkHost` (LocalSystem, session 0) and its `serve`
//! child (SYSTEM) cannot own a per-user icon. The installer starts this exe from the HKLM
//! `Run` key; a `Local\` mutex keeps one instance per interactive session.
//!
//! Start/Stop/Restart and "Stop host and exit tray" each open one UAC prompt
//! (`ShellExecuteW "runas"` on `punktfunk-host.exe service …`). Do not DACL-open the
//! service to every local user — that skips the consent prompt.
//!
//! Only the menu's exit entry stops the host. `WM_ENDSESSION` (sign-out) and the
//! uninstaller's `--quit` (`WM_CLOSE`) leave a headless host running. The host
//! supervises this process in `punktfunk-host`'s `windows/tray.rs`.

use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows::Win32::UI::Shell::{
    SetCurrentProcessExplicitAppUserModelID, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_INFO,
    NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON, NIIF_RESPECT_QUIET_TIME, NIIF_USER,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW,
    NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, LoadImageW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuDefaultItem, TrackPopupMenuEx, TranslateMessage, HICON, IMAGE_ICON, LR_SHARED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, SM_CXICON, SM_CXSMICON, SW_HIDE, SW_SHOWNORMAL,
    TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_NULL, WM_SETTINGCHANGE, WNDCLASSW, WS_OVERLAPPED,
};

use crate::status::{Poller, TrayStatus};
use crate::win_theme;

/// Enter/Space on the icon: `NIN_SELECT | NINF_KEY`. The windows crate exports only `NIN_SELECT`.
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;

/// Poller thread posts this when status changes. Do not touch UI TLS from that thread.
const WMAPP_STATUS: u32 = WM_APP + 2;
/// Notify-icon callback; `NOTIFYICON_VERSION_4` packing of the event in `lParam`.
const WMAPP_NOTIFYCALLBACK: u32 = WM_APP + 1;

// WM_COMMAND LOWORD(wParam).
const IDM_HEADER: usize = 0x0100; // disabled status line
const IDM_OPEN_WEB: usize = 0x0101;
const IDM_START: usize = 0x0102;
const IDM_STOP: usize = 0x0103;
const IDM_RESTART: usize = 0x0104;
const IDM_LOGS: usize = 0x0105;
const IDM_EXIT: usize = 0x0106;
const IDM_PAIRING: usize = 0x0107;
const IDM_DISPLAYS: usize = 0x0108;

/// Resource ordinals from `build.rs`.
fn icon_ordinal(status: &TrayStatus) -> u16 {
    match status {
        TrayStatus::Running(_) if status.is_streaming() => 5,
        TrayStatus::Running(_) => 2,
        TrayStatus::Stopped | TrayStatus::NotInstalled => 3,
        TrayStatus::Error(_) => 4,
        TrayStatus::Starting | TrayStatus::Degraded => 6,
    }
}

/// Process-wide tray state. `wndproc` cannot carry a closure; set before window creation.
struct App {
    hwnd: AtomicIsize,
    status: Mutex<TrayStatus>,
    poller: OnceLock<Poller>,
    /// `TaskbarCreated` id. Explorer restart drops the icon; re-add it.
    taskbar_created: u32,
    /// `punktfunk-host.exe` beside this exe (`{app}`).
    host_exe: Option<std::path::PathBuf>,
    /// Loopback probe succeeded. Left-click opens the console; otherwise the menu.
    web_console: AtomicBool,
    web_port: u16,
    /// Connect-toast edge: 0 = no status yet, 1 = idle, 2 = streaming. 0 skips a mid-session toast.
    streaming_seen: AtomicU8,
}

impl App {
    /// Recovers a poisoned lock. A panic from `wndproc` (`extern "system"`) aborts the
    /// process; this guard is a display enum, so reading through poison is fine.
    fn status(&self) -> std::sync::MutexGuard<'_, TrayStatus> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

static APP: OnceLock<App> = OnceLock::new();

fn app() -> &'static App {
    APP.get().expect("APP initialized before window creation")
}

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
}

/// Append-only log for a windows-subsystem process (no stderr): `%LOCALAPPDATA%\punktfunk\tray.log`.
fn log(msg: &str) {
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let dir = std::path::PathBuf::from(base).join("punktfunk");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tray.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

/// One tray per logon session. `Local\` resolves per session, so fast-user-switch and RDP each
/// keep their own icon; the same name twice in one session is a duplicate.
///
/// The name being taken is the answer, however it is taken. `CreateMutexW` asks for full access,
/// which a mutex created at a higher integrity level (or by another user in this session) refuses
/// with `ERROR_ACCESS_DENIED` — reading that as "no tray" is what put two icons in one session.
/// `SYNCHRONIZE` is allowed up, so the open below still sees it. The handle is deliberately
/// leaked: it holds the name for this process's life. `punktfunk-host`'s `windows/tray.rs`
/// opens the same name — keep the two literals in step.
fn another_instance_holds_the_session() -> bool {
    // SYNCHRONIZE only: existence is all that is asked.
    const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
    // SAFETY: both calls take a static nul-terminated name and no security attributes. The
    // CreateMutexW handle is never closed (process-lifetime guard); the OpenMutexW one, which
    // only answers a question, is closed here.
    unsafe {
        match CreateMutexW(None, false, w!("Local\\PunktfunkTray")) {
            Ok(_) => GetLastError() == ERROR_ALREADY_EXISTS,
            Err(_) => match OpenMutexW(SYNCHRONIZE, false, w!("Local\\PunktfunkTray")) {
                Ok(h) => {
                    let _ = CloseHandle(h);
                    true
                }
                // Neither call reached the name: no evidence of a tray, so show an icon
                // rather than leave the user without one.
                Err(_) => false,
            },
        }
    }
}

pub fn run(args: crate::Args) -> anyhow::Result<()> {
    let _ = args.autostart; // Linux-only; accepted so the command line matches
    if args.quit {
        return quit_existing();
    }

    if another_instance_holds_the_session() {
        return Ok(());
    }

    // AUMID must match the key the installer registers (punktfunk-setup's `registry_steps`).
    // Call before
    // the notify icon exists. Unregistered (dev) degrades to default attribution, not an error.
    // SAFETY: static nul-terminated literal.
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(w!("unom.punktfunk.tray"));
    }

    // Before the first popup: opt menus into the system dark mode.
    win_theme::init_dark_mode();

    let host_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("punktfunk-host.exe")))
        .filter(|p| p.exists());

    // SAFETY: RegisterWindowMessageW with a static nul-terminated literal.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    APP.set(App {
        hwnd: AtomicIsize::new(0),
        status: Mutex::new(TrayStatus::Stopped),
        poller: OnceLock::new(),
        taskbar_created,
        host_exe,
        web_console: AtomicBool::new(false),
        web_port: args.web_port,
        streaming_seen: AtomicU8::new(0),
    })
    .ok()
    .expect("run() is called once");

    // Hidden top-level, not message-only: HWND_MESSAGE never gets `TaskbarCreated`.
    // SAFETY: standard window-class registration + creation; the class name literal outlives the
    // call, wndproc is a valid extern "system" fn, and the window is created on this thread which
    // then runs the message loop.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: w!("PunktfunkTrayWindow"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            anyhow::bail!("RegisterClassW failed: {:?}", GetLastError());
        }
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("PunktfunkTrayWindow"),
            w!("punktfunk tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    app().hwnd.store(hwnd.0 as isize, Ordering::SeqCst);

    // NIM_ADD retry: the taskbar may not exist yet at sign-in.
    let mut added = false;
    for _ in 0..10 {
        if update_icon(hwnd, true) {
            added = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if !added {
        log("Shell_NotifyIconW(NIM_ADD) kept failing — no taskbar?");
    }

    // Poller owns network/SCM I/O; it only posts a message here.
    let poller = Poller::spawn(
        args.mgmt_addr.clone(),
        args.mgmt_port,
        args.web_port,
        Box::new(move |st, console_up| {
            *app().status() = st;
            app().web_console.store(console_up, Ordering::SeqCst);
            let hwnd = HWND(app().hwnd.load(Ordering::SeqCst) as *mut _);
            // SAFETY: PostMessageW is documented thread-safe; a stale/destroyed hwnd fails
            // harmlessly with an error we ignore.
            unsafe {
                let _ = PostMessageW(Some(hwnd), WMAPP_STATUS, WPARAM(0), LPARAM(0));
            }
        }),
    );
    let _ = app().poller.set(poller);

    // SAFETY: classic message pump on the window's owning thread.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// `--quit`: ask this session's instance to exit (uninstaller, before file deletion).
/// High-IL may message a medium-IL window; UIPI blocks only low→high.
fn quit_existing() -> anyhow::Result<()> {
    // SAFETY: FindWindowW/PostMessageW on a class-name literal; both fail harmlessly when no
    // instance is running.
    unsafe {
        if let Ok(hwnd) = FindWindowW(w!("PunktfunkTrayWindow"), PCWSTR::null()) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    Ok(())
}

/// Refresh the notify icon from current status. `false` = shell rejected (no taskbar yet).
fn update_icon(hwnd: HWND, add: bool) -> bool {
    let status = app().status().clone();
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WMAPP_NOTIFYCALLBACK,
        ..Default::default()
    };
    // DPI small-icon size so LoadImageW picks the matching .ico frame (not 32 px downscaled).
    // SAFETY: plain metric query; 0 (failure) falls back to the classic 16 px.
    let sm = match unsafe { GetSystemMetricsForDpi(SM_CXSMICON, win_theme::window_dpi(hwnd)) } {
        0 => 16,
        n => n,
    };
    // SAFETY: LoadImageW by ordinal from this exe's embedded resources (build.rs); the ordinal is
    // one of the ids compiled in, LR_SHARED handles are system-cached (never destroyed by us),
    // and a failure falls back to a null icon rather than UB.
    nid.hIcon = unsafe {
        LoadImageW(
            Some(GetModuleHandleW(None).unwrap_or_default().into()),
            PCWSTR(icon_ordinal(&status) as usize as *const u16),
            IMAGE_ICON,
            sm,
            sm,
            LR_SHARED,
        )
    }
    .map(|h| HICON(h.0))
    .unwrap_or(HICON(std::ptr::null_mut()));
    // szTip holds 127 UTF-16 units + nul.
    let tip = to_wide(&status.headline());
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);

    // SAFETY: nid is fully initialized with a correct cbSize; NIM_* calls only read it.
    unsafe {
        if add {
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                return false;
            }
            let mut v = nid;
            v.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &v);
            true
        } else {
            if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                // Icon gone (missed Explorer crash): re-add.
                return update_icon(hwnd, true);
            }
            true
        }
    }
}

/// Toast on the idle → streaming edge. Windows 11 renders `NIF_INFO` as a native toast
/// under the AUMID; a plain exe does not need WinRT. UI thread, on `WMAPP_STATUS`.
fn notify_on_connect(hwnd: HWND) {
    let status = app().status().clone();
    let now: u8 = if status.is_streaming() { 2 } else { 1 };
    let was = app().streaming_seen.swap(now, Ordering::SeqCst);
    if !(was == 1 && now == 2) {
        return;
    }
    let (title, body) = match &status {
        TrayStatus::Running(s) => (
            // Trust-store name, else the client's Hello name; missing → generic title.
            match &s.client_name {
                Some(name) => format!("{name} connected"),
                None => "Client connected".to_string(),
            },
            match &s.session {
                Some(sess) => format!(
                    "Streaming {}×{} @ {} fps",
                    sess.width, sess.height, sess.fps
                ),
                None => "A client is streaming from this host".to_string(),
            },
        ),
        _ => return, // is_streaming() implies Running
    };
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_INFO, // NIM_MODIFY with NIF_INFO leaves icon/tip alone
        dwInfoFlags: NIIF_USER | NIIF_LARGE_ICON | NIIF_RESPECT_QUIET_TIME,
        ..Default::default()
    };
    let title = to_wide(&title);
    let n = title.len().min(nid.szInfoTitle.len() - 1);
    nid.szInfoTitle[..n].copy_from_slice(&title[..n]);
    let body = to_wide(&body);
    let n = body.len().min(nid.szInfo.len() - 1);
    nid.szInfo[..n].copy_from_slice(&body[..n]);
    // SAFETY: plain metric query; 0 (failure) falls back to the classic 32 px.
    let sm = match unsafe { GetSystemMetricsForDpi(SM_CXICON, win_theme::window_dpi(hwnd)) } {
        0 => 32,
        n => n,
    };
    // Brand logo (ordinal 1), not a status glyph — the toast is attributed to the app.
    // SAFETY: LoadImageW by ordinal from this exe's embedded resources; LR_SHARED handles
    // are system-cached (never destroyed by us), and on failure the toast shows no image.
    nid.hBalloonIcon = unsafe {
        LoadImageW(
            Some(GetModuleHandleW(None).unwrap_or_default().into()),
            // MAKEINTRESOURCE(1): the address *is* the ordinal. `dangling()` would be 2.
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            sm,
            sm,
            LR_SHARED,
        )
    }
    .map(|h| HICON(h.0))
    .unwrap_or(HICON(std::ptr::null_mut()));
    // SAFETY: nid fully initialized with a correct cbSize; NIM_MODIFY only reads it.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn show_menu(hwnd: HWND) {
    let status = app().status().clone();
    let running = matches!(
        status,
        TrayStatus::Running(_) | TrayStatus::Starting | TrayStatus::Degraded
    );
    let startable = matches!(status, TrayStatus::Stopped | TrayStatus::Error(_));
    let can_control = app().host_exe.is_some();

    // SAFETY: menu handle created and destroyed here; AppendMenuW copies the item strings, whose
    // wide buffers outlive each call. TrackPopupMenuEx requires the foreground quirk handled
    // below (SetForegroundWindow before, WM_NULL after) per the Shell_NotifyIcon docs.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        // Menu references the bitmaps; the guard deletes them after DestroyMenu.
        let mut glyphs = win_theme::MenuGlyphs::new(hwnd);
        let mut add = |id: usize, text: &str, grayed: bool, glyph: Option<u16>| {
            let wide = to_wide(text);
            let flags = if grayed {
                MF_STRING | MF_GRAYED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr()));
            if let Some(g) = glyph {
                glyphs.set(menu, id, g);
            }
        };
        add(IDM_HEADER, &status.headline(), true, None);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        // Keep the console entry even when the probe fails; hide-on-down is not discoverable.
        if app().web_console.load(Ordering::SeqCst) {
            add(
                IDM_OPEN_WEB,
                "Open web console",
                false,
                Some(win_theme::GLYPH_GLOBE),
            );
        } else {
            add(
                IDM_OPEN_WEB,
                "Open web console (not responding)",
                false,
                Some(win_theme::GLYPH_GLOBE),
            );
        }
        let _ = SetMenuDefaultItem(menu, IDM_OPEN_WEB as u32, 0);
        if status.pairing_attention() {
            add(
                IDM_PAIRING,
                "Approve pairing request…",
                false,
                Some(win_theme::GLYPH_APPROVE),
            );
        }
        match status.kept_displays() {
            0 => {}
            1 => add(
                IDM_DISPLAYS,
                "Release kept display…",
                false,
                Some(win_theme::GLYPH_DISPLAY),
            ),
            n => add(
                IDM_DISPLAYS,
                &format!("Release {n} kept displays…"),
                false,
                Some(win_theme::GLYPH_DISPLAY),
            ),
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        // Shield = this item opens a UAC prompt (`punktfunk-host.exe service …`).
        if can_control {
            if startable {
                add(
                    IDM_START,
                    "Start host",
                    false,
                    Some(win_theme::GLYPH_SHIELD),
                );
            }
            if running {
                add(IDM_STOP, "Stop host", false, Some(win_theme::GLYPH_SHIELD));
                // "Restart Punktfunk" restarts the service. Clients use "Restart host" for the
                // machine (`design/host-actions.md`). Same phrase must not mean both.
                add(
                    IDM_RESTART,
                    "Restart Punktfunk",
                    false,
                    Some(win_theme::GLYPH_SHIELD),
                );
            } else if matches!(status, TrayStatus::Error(_)) {
                add(
                    IDM_RESTART,
                    "Restart Punktfunk",
                    false,
                    Some(win_theme::GLYPH_SHIELD),
                );
            }
        }
        add(
            IDM_LOGS,
            "Open logs folder",
            false,
            Some(win_theme::GLYPH_FOLDER),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        // Exit stops the host (IDM_EXIT); the label and shield must match that.
        if can_control && running {
            add(
                IDM_EXIT,
                "Stop host and exit tray",
                false,
                Some(win_theme::GLYPH_SHIELD),
            );
        } else {
            add(IDM_EXIT, "Exit tray", false, Some(win_theme::GLYPH_POWER));
        }

        let mut pt = Default::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenuEx(
            menu,
            (TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

fn shell_open(hwnd: HWND, target: &str) {
    let wide = to_wide(target);
    // SAFETY: all strings nul-terminated and live across the call.
    unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// One UAC prompt: relaunch the host exe elevated with `service <verb>`.
///
/// `false` is a declined prompt (`ERROR_CANCELLED`) or any `ShellExecuteW` ≤ 32.
/// Harmless for start/stop/restart. `IDM_EXIT` must not close the icon on `false`.
fn elevate_service(hwnd: HWND, verb: &str) -> bool {
    let Some(exe) = app().host_exe.as_ref() else {
        return false;
    };
    let exe_w = to_wide(&exe.to_string_lossy());
    let params = to_wide(&format!("service {verb}"));
    // SAFETY: nul-terminated strings live across the call; "runas" spawns the elevated child
    // (hidden console — the tray re-polls for the outcome instead of scraping its output).
    let rc = unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };
    if let Some(p) = app().poller.get() {
        p.poke();
    }
    rc.0 as isize > 32
}

/// Open the web console at `path` (`""` = dashboard).
fn open_web_console(hwnd: HWND, path: &str) {
    // `127.0.0.1`, not `localhost`: the console binds IPv4 only (`PUNKTFUNK_UI_BIND`) and Windows
    // resolves `localhost` to ::1 first. Same literal as the poller probe.
    shell_open(
        hwnd,
        &format!("https://127.0.0.1:{}/{path}", app().web_port),
    );
}

fn open_logs(hwnd: HWND) {
    let Some(base) = std::env::var_os("ProgramData") else {
        return;
    };
    let dir = std::path::PathBuf::from(base)
        .join("punktfunk")
        .join("logs");
    shell_open(hwnd, &dir.to_string_lossy());
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = APP.get() else {
        // SAFETY: pass-through for messages arriving before APP is set (CreateWindowExW sends
        // WM_NCCREATE/WM_CREATE synchronously — APP is set before that, but stay defensive).
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WMAPP_STATUS => {
            update_icon(hwnd, false);
            notify_on_connect(hwnd);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // Color scheme flipped: drop the cached menu theme.
            if win_theme::is_color_scheme_change(lparam) {
                win_theme::on_color_scheme_changed();
            }
            // SAFETY: setting broadcasts still get default processing.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WMAPP_NOTIFYCALLBACK => {
            // NOTIFYICON_VERSION_4: LOWORD(lParam) is the event.
            match (lparam.0 as u32) & 0xffff {
                WM_CONTEXTMENU => show_menu(hwnd),
                x if x == NIN_SELECT || x == NIN_KEYSELECT => {
                    if app.web_console.load(Ordering::SeqCst) {
                        open_web_console(hwnd, "");
                    } else {
                        show_menu(hwnd);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wparam.0) & 0xffff {
                IDM_OPEN_WEB => open_web_console(hwnd, ""),
                IDM_PAIRING => open_web_console(hwnd, "pairing"),
                IDM_DISPLAYS => open_web_console(hwnd, "displays"),
                IDM_START => {
                    let _ = elevate_service(hwnd, "start");
                }
                IDM_STOP => {
                    let _ = elevate_service(hwnd, "stop");
                }
                IDM_RESTART => {
                    let _ = elevate_service(hwnd, "restart");
                }
                IDM_LOGS => open_logs(hwnd),
                IDM_EXIT => {
                    // Only this entry stops the host. `WM_CLOSE` (`--quit`) and `WM_ENDSESSION`
                    // leave a headless host. A declined UAC cancels the exit too.
                    // Match `show_menu`: no host exe means no stop, and the icon must still close.
                    let stop_first = app.host_exe.is_some()
                        && matches!(
                            *app.status(),
                            TrayStatus::Running(_) | TrayStatus::Starting | TrayStatus::Degraded
                        );
                    if stop_first && !elevate_service(hwnd, "stop") {
                        return LRESULT(0);
                    }
                    // SAFETY: DestroyWindow on the wndproc's own window/thread.
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE | WM_ENDSESSION => {
            // SAFETY: as above — triggers WM_DESTROY below.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            // SAFETY: minimal, correctly sized nid; NIM_DELETE only reads hWnd/uID.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        m if m == app.taskbar_created => {
            // Explorer restarted; the icon is gone.
            update_icon(hwnd, true);
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
