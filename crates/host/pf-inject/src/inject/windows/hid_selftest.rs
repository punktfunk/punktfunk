//! `vmouse-spike --selftest`: does input from the virtual HID device arrive as hardware?
//!
//! Drives each report kind while a hidden window records raw input and low-level hooks record
//! the injected flag — the two checks anti-cheat runs. A `SendInput` move is the control and
//! must read as injected. Needs the interactive desktop and the host service stopped (it owns
//! the device's mailbox). It moves the pointer, clicks at the screen centre and types `a`.

use crate::mouse_windows::open_for_spike;
use anyhow::{bail, Context, Result};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDI_DEVICENAME, RID_INPUT, RIM_TYPEKEYBOARD,
    RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW,
    GetSystemMetrics, PostThreadMessageW, RegisterClassW, SetWindowsHookExW, UnhookWindowsHookEx,
    KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WM_QUIT, WNDCLASSW,
};

/// One thing the recorder saw, in arrival order.
#[derive(Debug)]
enum Seen {
    Step(&'static str),
    RawMouse {
        ours: bool,
        x: i32,
        y: i32,
        absolute: bool,
        buttons: u16,
        data: i16,
    },
    RawKey {
        ours: bool,
        vkey: u16,
        make: u16,
        up: bool,
    },
    Hook {
        injected: bool,
    },
}

static SEEN: Mutex<Vec<(Instant, Seen)>> = Mutex::new(Vec::new());
/// Every raw-input device path seen — what a game reads as the device's name.
static DEVICES: Mutex<std::collections::BTreeSet<String>> =
    Mutex::new(std::collections::BTreeSet::new());

fn note(s: Seen) {
    let at = Instant::now();
    SEEN.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((at, s));
}

/// A report reaches raw input this fast through the doorbell; the driver's timer alone takes
/// up to a tick (8–16 ms).
const PROMPT: Duration = Duration::from_millis(4);

const RI_MOUSE_LEFT_BUTTON_DOWN: u16 = 0x0001;
const RI_MOUSE_LEFT_BUTTON_UP: u16 = 0x0002;
const RI_MOUSE_WHEEL: u16 = 0x0400;
const RI_KEY_BREAK: u16 = 0x0001;

pub fn run() -> Result<()> {
    let mut m = open_for_spike()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let recorder = std::thread::spawn(move || recorder(tx));
    let thread_id = rx
        .recv_timeout(Duration::from_secs(5))
        .context("the recorder never started")??;
    let pause = || std::thread::sleep(Duration::from_millis(200));
    pause();
    // SAFETY: by-value metric indices, no pointers.
    let centre = unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN) / 2,
            GetSystemMetrics(SM_CYSCREEN) / 2,
        )
    };

    note(Seen::Step("absolute to the primary centre"));
    let (ax, ay) = crate::stream_target::primary_hid_abs(centre).context("primary monitor size")?;
    m.move_to(ax, ay);
    pause();
    let mut at = POINT::default();
    // SAFETY: `at` is a valid out-param.
    unsafe { GetCursorPos(&mut at) }.context("GetCursorPos")?;
    note(Seen::Step("relative +40,+20"));
    m.move_by(40, 20);
    pause();
    note(Seen::Step("primary click"));
    m.button(0, true);
    m.button(0, false);
    pause();
    note(Seen::Step("wheel one notch"));
    m.scroll(false, 120);
    pause();
    note(Seen::Step("wheel quarter notch"));
    m.scroll(false, 30);
    pause();
    note(Seen::Step("key A"));
    m.key(0x04, true);
    m.key(0x04, false);
    pause();
    note(Seen::Step("SendInput control +5,0"));
    send_input_move(5, 0);
    pause();
    let doorbell = m.doorbell_open();
    // SAFETY: plain message post to the recorder thread's queue.
    unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }
        .context("stop the recorder")?;
    let _ = recorder.join();
    let seen = std::mem::take(&mut *SEEN.lock().unwrap_or_else(PoisonError::into_inner));
    report(&seen, centre, (at.x, at.y), doorbell)
}

fn report(
    seen: &[(Instant, Seen)],
    centre: (i32, i32),
    at: (i32, i32),
    doorbell: bool,
) -> Result<()> {
    let mut failed = 0;
    let mut check = |ok: bool, what: String| {
        println!("    {} {what}", if ok { "PASS" } else { "FAIL" });
        failed += usize::from(!ok);
    };
    for name in DEVICES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        println!("raw input device: {name:?}");
    }
    check(doorbell, "doorbell collection open".into());
    for (i, (started, s)) in seen.iter().enumerate() {
        let Seen::Step(name) = s else { continue };
        let timed: Vec<&(Instant, Seen)> = seen[i + 1..]
            .iter()
            .take_while(|(_, e)| !matches!(e, Seen::Step(_)))
            .collect();
        let events: Vec<&Seen> = timed.iter().map(|(_, e)| e).collect();
        println!("{name}:");
        for e in &events {
            println!("    {e:?}");
        }
        let first_raw = timed
            .iter()
            .find(|(_, e)| matches!(e, Seen::RawMouse { .. } | Seen::RawKey { .. }))
            .map(|(t, _)| t.duration_since(*started));
        let hooks: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                Seen::Hook { injected } => Some(*injected),
                _ => None,
            })
            .collect();
        let raw_ours: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                Seen::RawMouse { ours, .. } | Seen::RawKey { ours, .. } => Some(*ours),
                _ => None,
            })
            .collect();
        let mouse = || {
            events.iter().filter_map(|e| match e {
                Seen::RawMouse {
                    x,
                    y,
                    buttons,
                    data,
                    ..
                } => Some((*x, *y, *buttons, *data)),
                _ => None,
            })
        };
        if *name == "SendInput control +5,0" {
            check(
                !hooks.is_empty() && hooks.iter().all(|i| *i),
                "SendInput reads as injected".into(),
            );
            check(
                raw_ours.iter().all(|o| !o),
                "SendInput names none of our devices".into(),
            );
            continue;
        }
        check(
            !hooks.is_empty() && hooks.iter().all(|i| !i),
            "low-level hooks see no injected flag".into(),
        );
        check(
            !raw_ours.is_empty() && raw_ours.iter().all(|o| *o),
            "raw input names the PF:MO device".into(),
        );
        check(
            first_raw.is_some_and(|d| d < PROMPT),
            format!("first raw input after {first_raw:?}"),
        );
        match *name {
            "absolute to the primary centre" => {
                check(at == centre, format!("cursor at {at:?}, want {centre:?}"));
            }
            "relative +40,+20" => {
                let sum = mouse().fold((0, 0), |a, (x, y, ..)| (a.0 + x, a.1 + y));
                check(sum == (40, 20), format!("raw motion {sum:?}"));
            }
            "primary click" => {
                let flags = mouse().fold(0, |a, (_, _, b, _)| a | b);
                let both = RI_MOUSE_LEFT_BUTTON_DOWN | RI_MOUSE_LEFT_BUTTON_UP;
                check(flags & both == both, format!("button flags {flags:#06x}"));
            }
            "wheel one notch" | "wheel quarter notch" => {
                let want = if name.contains("quarter") { 30 } else { 120 };
                let sum: i32 = mouse()
                    .filter(|(_, _, b, _)| b & RI_MOUSE_WHEEL != 0)
                    .map(|(.., d)| i32::from(d))
                    .sum();
                check(sum == want, format!("wheel delta {sum}, want {want}"));
            }
            "key A" => {
                let keys: Vec<(u16, u16, bool)> = events
                    .iter()
                    .filter_map(|e| match e {
                        Seen::RawKey { vkey, make, up, .. } => Some((*vkey, *make, *up)),
                        _ => None,
                    })
                    .collect();
                check(
                    keys == [(0x41, 0x1E, false), (0x41, 0x1E, true)],
                    format!("keys {keys:x?}"),
                );
            }
            _ => {}
        }
    }
    if failed > 0 {
        bail!("{failed} check(s) failed");
    }
    println!("All checks passed: the device's input reads as hardware.");
    Ok(())
}

fn send_input_move(dx: i32, dy: i32) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: one fully initialised `INPUT`, borrowed for this synchronous call.
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
}

/// Hidden window + raw input sink + both low-level hooks, pumping messages until `WM_QUIT`.
fn recorder(ready: std::sync::mpsc::Sender<Result<u32>>) {
    // SAFETY: Win32 setup on this thread; every pointer passed lives for its call, and the
    // window procedure and hooks are `extern "system"` fns that live for the process.
    let hooks = unsafe {
        (|| -> Result<_> {
            let module = GetModuleHandleW(PCWSTR::null()).context("GetModuleHandleW")?;
            let instance = HINSTANCE(module.0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: w!("pf_hid_selftest"),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                bail!("RegisterClassW");
            }
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("pf_hid_selftest"),
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
            .context("CreateWindowExW")?;
            let sink = |usage| RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: usage,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            };
            RegisterRawInputDevices(
                &[sink(0x02), sink(0x06)],
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            )
            .context("RegisterRawInputDevices")?;
            let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(instance), 0)
                .context("mouse hook")?;
            let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), Some(instance), 0)
                .context("keyboard hook")?;
            Ok((mouse, keyboard))
        })()
    };
    let (mouse, keyboard) = match hooks {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: no arguments.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid out-param; the loop ends at WM_QUIT (0) or an error (-1).
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: `msg` was just filled by GetMessageW.
        unsafe { DispatchMessageW(&msg) };
    }
    // SAFETY: both hooks were installed above and are removed once.
    unsafe {
        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(keyboard);
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        record_raw(HRAWINPUT(l.0 as _));
    }
    // SAFETY: the arguments are the ones Windows passed this window procedure.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn record_raw(h: HRAWINPUT) {
    let mut raw = RAWINPUT::default();
    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
    let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    // SAFETY: `raw` is a RAWINPUT-sized buffer and `size` says so.
    let n =
        unsafe { GetRawInputData(h, RID_INPUT, Some((&raw mut raw).cast()), &mut size, header) };
    if n == u32::MAX || n == 0 {
        return;
    }
    let name = device_name(raw.header.hDevice);
    // hidclass names the collections after the SwDevice enumerator: `HID\PUNKTFUNK&COL0n`.
    let ours = name.contains("HID#PUNKTFUNK&COL");
    DEVICES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name);
    // SAFETY: the union member read matches `dwType`.
    unsafe {
        if raw.header.dwType == RIM_TYPEMOUSE.0 {
            let m = raw.data.mouse;
            note(Seen::RawMouse {
                ours,
                x: m.lLastX,
                y: m.lLastY,
                absolute: m.usFlags.0 & 1 != 0,
                buttons: m.Anonymous.Anonymous.usButtonFlags,
                data: m.Anonymous.Anonymous.usButtonData as i16,
            });
        } else if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
            let k = raw.data.keyboard;
            note(Seen::RawKey {
                ours,
                vkey: k.VKey,
                make: k.MakeCode,
                up: k.Flags & RI_KEY_BREAK != 0,
            });
        }
    }
}

/// The raw-input device's interface path; empty for `SendInput` (no device).
fn device_name(device: HANDLE) -> String {
    if device.is_invalid() {
        return String::new();
    }
    let mut len = 0u32;
    // SAFETY: size query — no buffer, `len` receives the length in characters.
    unsafe { GetRawInputDeviceInfoW(Some(device), RIDI_DEVICENAME, None, &mut len) };
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: `buf` holds `len` characters.
    let n = unsafe {
        GetRawInputDeviceInfoW(
            Some(device),
            RIDI_DEVICENAME,
            Some(buf.as_mut_ptr().cast()),
            &mut len,
        )
    };
    if n == u32::MAX {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize]).to_uppercase()
}

unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_MOUSE_LL with code >= 0, `l` points at an MSLLHOOKSTRUCT.
        let info = unsafe { &*(l.0 as *const MSLLHOOKSTRUCT) };
        note(Seen::Hook {
            injected: info.flags & LLMHF_INJECTED != 0,
        });
    }
    // SAFETY: passes Windows' own arguments down the chain.
    unsafe { CallNextHookEx(None, code, w, l) }
}

unsafe extern "system" fn key_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL with code >= 0, `l` points at a KBDLLHOOKSTRUCT.
        let info = unsafe { &*(l.0 as *const KBDLLHOOKSTRUCT) };
        note(Seen::Hook {
            injected: info.flags.0 & LLKHF_INJECTED.0 != 0,
        });
    }
    // SAFETY: passes Windows' own arguments down the chain.
    unsafe { CallNextHookEx(None, code, w, l) }
}
