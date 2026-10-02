//! Windows `SendInput` injection (Win32 KeyboardAndMouse) — analogue of [`super::wlr`]:
//! absolute mouse over the streamed output ([`crate::stream_target`]), relative mouse
//! for games, scancode keyboard, scroll, buttons. Survives UAC/lock by Sunshine's
//! retry-on-failure: the thread stays bound and only reattaches
//! (`OpenInputDesktop`/`SetThreadDesktop`) when `SendInput` reports a short write.
//!
//! Keyboard (`crate::KEY_FLAG_SEMANTIC_VK`): first-party clients send US-positional
//! VKs, resolved through [`positional_vk_to_scan`]. GameStream/Moonlight send
//! layout-semantic VKs, resolved under the foreground app's layout. Never resolve a
//! positional VK through a layout: this thread is the SYSTEM service, whose layout
//! is not the user's — a German host would y↔z / ü-on-ö scramble.

use anyhow::Result;
use punktfunk_core::input::{InputEvent, InputKind};

use crate::scroll::{ScrollBackend, ScrollMapper, ScrollOp};
use std::mem::size_of;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
    HDESK,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, MapVirtualKeyExW, SendInput, HKL, INPUT, INPUT_0, INPUT_KEYBOARD,
    INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::InputInjector;

const GENERIC_ALL: u32 = 0x1000_0000;
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

pub struct SendInputInjector {
    desktop: Option<HDESK>,
    /// PT_TOUCH device, created on the first wire-touch. `None` after a failed
    /// create (pre-1809) — touch then stays a no-op.
    touch: Option<crate::pen::SyntheticTouch>,
    touch_failed: bool,
    /// Scroll lowering, legacy and normalized, onto WHEEL/HWHEEL clicks.
    scroll: ScrollMapper,
}

// SAFETY: the only non-`Send` field is `Option<HDESK>` (`SyntheticTouch` is `Send`
// on its own). The host drives this from one dedicated thread; the type is not
// `Sync`. An `HDESK` is not thread-affine for ownership (`CloseDesktop` works from
// any thread; `SetThreadDesktop` rebinds the current thread).
unsafe impl Send for SendInputInjector {}

impl SendInputInjector {
    pub fn open() -> Result<Self> {
        let mut me = Self {
            desktop: None,
            touch: None,
            touch_failed: false,
            scroll: ScrollMapper::new(ScrollBackend::Windows),
        };
        me.reattach_input_desktop(); // best-effort
        tracing::info!("SendInput injector ready (Win32 KeyboardAndMouse)");
        Ok(me)
    }

    /// Bind this thread to the desktop currently receiving input. UAC / lock / Ctrl-Alt-Del
    /// swap the input desktop; `SendInput` silently no-ops unless our thread is on it.
    fn reattach_input_desktop(&mut self) {
        // SAFETY: `OpenInputDesktop`/`SetThreadDesktop`/`CloseDesktop` are FFI calls passed only
        // by-value args (constant desktop flags, a `bool`, an access mask). `OpenInputDesktop`
        // yields an owned `HDESK` only on `Ok`; we then either install it with `SetThreadDesktop`
        // (closing the previously-owned handle exactly once) or close the fresh handle on failure —
        // so every handle is closed exactly once and none is used after close. `SetThreadDesktop`
        // only rebinds this calling thread, which is where the injector runs.
        unsafe {
            match OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            ) {
                Ok(h) => {
                    if SetThreadDesktop(h).is_ok() {
                        if let Some(old) = self.desktop.replace(h) {
                            let _ = CloseDesktop(old);
                        }
                    } else {
                        let _ = CloseDesktop(h);
                    }
                }
                Err(_) => { /* not privileged enough for the secure desktop; stay put */ }
            }
        }
    }

    /// Scroll lowers through the shared mapper: v120 stays v120, DIP re-prices
    /// at the nominal detent, both axes keep the wire sign. The OS applies the
    /// user's wheel-lines setting itself. Stops are no-ops — Win32 has no
    /// scroll-stop primitive.
    fn inject_scroll(&mut self, event: &InputEvent) -> Result<()> {
        let mut inputs = Vec::new();
        for op in self.scroll.plan(event) {
            if let ScrollOp::Discrete120 { horizontal, value } = op {
                inputs.push(mouse(MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: value as u32, // signed wheel delta reinterpreted as DWORD
                    dwFlags: if horizontal {
                        MOUSEEVENTF_HWHEEL
                    } else {
                        MOUSEEVENTF_WHEEL
                    },
                    time: 0,
                    dwExtraInfo: 0,
                }));
            }
        }
        if inputs.is_empty() {
            return Ok(());
        }
        self.send(&inputs)
    }

    /// Inject with Sunshine's retry-on-failure: stay bound to the last desktop, and only
    /// when `SendInput` reports a short write (0 = input desktop switched) reattach and
    /// retry once. No per-event `OpenInputDesktop`/`SetThreadDesktop`.
    fn send(&mut self, inputs: &[INPUT]) -> Result<()> {
        // SAFETY: `inputs` is a live `&[INPUT]` slice that outlives this synchronous `SendInput`
        // call; `size_of::<INPUT>()` is the exact per-element stride Win32 requires as `cbSize`. The
        // call only reads the array (one event per element) and returns the count injected.
        let n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        if n as usize == inputs.len() {
            return Ok(());
        }
        // Short write → the input desktop likely changed. Reattach + retry the TAIL: the first
        // `n` events were injected, and re-sending them duplicates real input — a repeated
        // character on the text path, a second click on the pointer one.
        self.reattach_input_desktop();
        let rest = &inputs[n as usize..];
        // SAFETY: same as the first `SendInput` — `rest` borrows the identical live slice, which
        // outlives the call, and `cbSize == size_of::<INPUT>()`.
        let n2 = unsafe { SendInput(rest, size_of::<INPUT>() as i32) };
        if n2 as usize != rest.len() {
            anyhow::bail!(
                "SendInput injected {}/{} events (blocked desktop?)",
                n as usize + n2 as usize,
                inputs.len()
            );
        }
        Ok(())
    }
}

impl Drop for SendInputInjector {
    fn drop(&mut self) {
        if let Some(h) = self.desktop.take() {
            // SAFETY: `h` is the `HDESK` this injector owned (moved out of `self.desktop`);
            // `CloseDesktop` runs once here in `Drop` on that still-valid handle, with no later use —
            // no double close.
            unsafe {
                let _ = CloseDesktop(h);
            }
        }
    }
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        let down = matches!(event.kind, InputKind::MouseButtonDown | InputKind::KeyDown);
        match event.kind {
            InputKind::MouseMove => self.send(&[mouse(MOUSEINPUT {
                dx: event.x,
                dy: event.y,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            })]),
            InputKind::MouseMoveAbs => {
                abs_move_input(event).map_or(Ok(()), |mi| self.send(&[mouse(mi)]))
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                mouse_button_input(event.code, down).map_or(Ok(()), |mi| self.send(&[mouse(mi)]))
            }
            InputKind::MouseScroll | InputKind::Scroll => self.inject_scroll(event),
            InputKind::KeyDown | InputKind::KeyUp => {
                let vk = (event.code & 0xff) as u16;
                let semantic = (event.flags & crate::KEY_FLAG_SEMANTIC_VK) != 0;
                key_input(vk, semantic, down, |vk| scan_ex(vk, semantic))
                    .map_or(Ok(()), |ki| self.send(&[key(ki)]))
            }
            InputKind::TextInput => {
                let inputs: Vec<INPUT> = text_units(event.code).into_iter().map(key).collect();
                if inputs.is_empty() {
                    return Ok(());
                }
                self.send(&inputs)
            }
            // Gamepad goes through the XUSB backend.
            InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadState
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => Ok(()),
            // Wire touch → PT_TOUCH (design/pen-tablet-input.md).
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => {
                if let Some(t) = self.ensure_touch() {
                    t.apply(event);
                }
                Ok(())
            }
        }
    }
}

impl SendInputInjector {
    /// The PT_TOUCH device, created on the first wire touch: a session that never touches
    /// never creates one, and a pre-1809 create failure latches the no-op.
    fn ensure_touch(&mut self) -> Option<&mut crate::pen::SyntheticTouch> {
        if self.touch.is_none() && !self.touch_failed {
            match crate::pen::SyntheticTouch::create() {
                Ok(t) => self.touch = Some(t),
                Err(e) => {
                    self.touch_failed = true;
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "touch: synthetic pointer unavailable — wire touch stays a no-op"
                    );
                }
            }
        }
        self.touch.as_mut()
    }
}

/// Client (0..w,0..h) → STREAMED output rect ([`crate::stream_target`]; whole virtual desktop
/// only as fallback) → 0..65535 over the virtual desktop for MOUSEEVENTF_VIRTUALDESK. Mapping
/// over the desktop alone is the Extend-topology offset bug (design/pen-tablet-input.md).
/// `None` for a zero extent, which the contract drops.
fn abs_move_input(event: &InputEvent) -> Option<MOUSEINPUT> {
    let w = (event.flags >> 16) & 0xffff;
    let h = event.flags & 0xffff;
    if w == 0 || h == 0 {
        return None;
    }
    let cx = (event.x.clamp(0, w as i32)) as f64 / w as f64;
    let cy = (event.y.clamp(0, h as i32)) as f64 / h as f64;
    let px = crate::stream_target::map_normalized(cx, cy);
    let (ax, ay) = crate::stream_target::desktop_px_to_virtualdesk(px);
    Some(MOUSEINPUT {
        dx: ax,
        dy: ay,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        time: 0,
        dwExtraInfo: 0,
    })
}

/// Wire buttons 1..=5 (left, middle, right, X1, X2) as (down, up, `mouseData`).
const BUTTONS: [(MOUSE_EVENT_FLAGS, MOUSE_EVENT_FLAGS, u32); 5] = [
    (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
    (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
    (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
    (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1),
    (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2),
];

/// `None` for a button the wire does not define.
fn mouse_button_input(code: u32, down: bool) -> Option<MOUSEINPUT> {
    let &(press, release, data) = BUTTONS.get(code.checked_sub(1)? as usize)?;
    Some(MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: data,
        dwFlags: if down { press } else { release },
        time: 0,
        dwExtraInfo: 0,
    })
}

/// One key edge as a scancode input. Positional VKs take the US table for the layout-variant
/// typing area; everything else, and every semantic VK, goes through `map` (the extended
/// scancode `MapVirtualKeyExW` returns, 0 = unmappable → `None`). Pause make is E1 1D 45:
/// scan 0x45 is NumLock and KEYEVENTF_EXTENDEDKEY invents E0+45, so Pause goes by `wVk`.
fn key_input(
    vk: u16,
    semantic: bool,
    down: bool,
    map: impl FnOnce(u16) -> u32,
) -> Option<KEYBDINPUT> {
    if vk == crate::keymap::VK_PAUSE {
        return Some(KEYBDINPUT {
            wVk: VIRTUAL_KEY(vk),
            wScan: 0,
            dwFlags: if down {
                KEYBD_EVENT_FLAGS(0)
            } else {
                KEYEVENTF_KEYUP
            },
            time: 0,
            dwExtraInfo: 0,
        });
    }
    let table = if semantic {
        None
    } else {
        positional_vk_to_scan(vk)
    };
    let (scan, extended) = match table {
        // Typing area: never E0-extended. Keypad Enter is.
        Some(scan) => (scan, crate::keymap::vk_forced_extended(vk)),
        None => {
            let sc_ex = map(vk);
            if sc_ex == 0 {
                return None;
            }
            (
                (sc_ex & 0xff) as u16,
                (sc_ex & 0xe000) == 0xe000 || crate::keymap::vk_forced_extended(vk),
            )
        }
    };
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    Some(KEYBDINPUT {
        wVk: VIRTUAL_KEY(0),
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    })
}

/// `MapVirtualKeyExW` VK → extended scancode, under the foreground app's layout for a
/// semantic VK and this thread's otherwise. 0 = unmappable.
fn scan_ex(vk: u16, semantic: bool) -> u32 {
    let hkl = if semantic { foreground_hkl() } else { None };
    // SAFETY: `MapVirtualKeyExW` is a pure value translation (VK → scancode); all three args
    // are by-value (`u32`, the `MAPVK_VK_TO_VSC_EX` map-type constant, an optional `HKL`
    // handle used only as a lookup key). It dereferences no pointer and returns a `u32`.
    unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC_EX, hkl) }
}

/// Committed IME text: one Unicode scalar as `KEYEVENTF_UNICODE` (wScan = UTF-16 unit, no
/// scancode or layout), each unit down then up, so an astral scalar is its surrogate pair.
/// Empty for a lone surrogate, an out-of-range code, or a control character, which rides
/// the VK path (Enter/Backspace/Tab).
fn text_units(code: u32) -> Vec<KEYBDINPUT> {
    let Some(ch) = char::from_u32(code).filter(|c| !c.is_control()) else {
        return Vec::new();
    };
    let mut units = [0u16; 2];
    let mut out = Vec::with_capacity(4);
    for &unit in ch.encode_utf16(&mut units).iter() {
        for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
            out.push(KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            });
        }
    }
    out
}

fn mouse(mi: MOUSEINPUT) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    }
}

fn key(ki: KEYBDINPUT) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }
}

/// US-positional VK → set-1 make scancode for the layout-variant typing area (letters,
/// digit row, OEM punctuation, ISO 102nd key) and the Korean/Japanese IME keys. Mirror
/// of Linux `crate::vk_to_evdev` — for the typing area the evdev code IS the set-1
/// scancode. Other layout-invariant keys are absent (`MapVirtualKeyExW` resolves them
/// under any layout); the IME keys it resolves only under a Korean or Japanese one, so
/// they carry the scancode the physical key sends. Never E0-extended, but for keypad Enter.
fn positional_vk_to_scan(vk: u16) -> Option<u16> {
    Some(match vk {
        // Keypad Enter, which clients send as VK_SEPARATOR and no layout maps: E0 1C.
        0x6C => 0x1C,                    // VK_SEPARATOR
        0x30 => 0x0B,                    // VK_0
        0x31..=0x39 => vk - 0x31 + 0x02, // VK_1..VK_9 → 0x02..0x0A
        0x41 => 0x1E,                    // A
        0x42 => 0x30,                    // B
        0x43 => 0x2E,                    // C
        0x44 => 0x20,                    // D
        0x45 => 0x12,                    // E
        0x46 => 0x21,                    // F
        0x47 => 0x22,                    // G
        0x48 => 0x23,                    // H
        0x49 => 0x17,                    // I
        0x4A => 0x24,                    // J
        0x4B => 0x25,                    // K
        0x4C => 0x26,                    // L
        0x4D => 0x32,                    // M
        0x4E => 0x31,                    // N
        0x4F => 0x18,                    // O
        0x50 => 0x19,                    // P
        0x51 => 0x10,                    // Q
        0x52 => 0x13,                    // R
        0x53 => 0x1F,                    // S
        0x54 => 0x14,                    // T
        0x55 => 0x16,                    // U
        0x56 => 0x2F,                    // V
        0x57 => 0x11,                    // W
        0x58 => 0x2D,                    // X
        0x59 => 0x15,                    // Y (US position — a QWERTZ host renders it as Z)
        0x5A => 0x2C,                    // Z (US position)
        0xBA => 0x27,                    // VK_OEM_1      ;:  (DE: ö)
        0xBB => 0x0D,                    // VK_OEM_PLUS   =+
        0xBC => 0x33,                    // VK_OEM_COMMA  ,<
        0xBD => 0x0C,                    // VK_OEM_MINUS  -_  (DE: ß)
        0xBE => 0x34,                    // VK_OEM_PERIOD .>
        0xBF => 0x35,                    // VK_OEM_2      /?
        0xC0 => 0x29,                    // VK_OEM_3      `~  (DE: ^)
        0xDB => 0x1A,                    // VK_OEM_4      [{  (DE: ü)
        0xDC => 0x2B,                    // VK_OEM_5      \|
        0xDD => 0x1B,                    // VK_OEM_6      ]}
        0xDE => 0x28,                    // VK_OEM_7      '"  (DE: ä)
        0xE2 => 0x56,                    // VK_OEM_102    <>| (ISO key next to left shift)
        0x15 => 0x72,                    // VK_HANGUL     한/영
        0x19 => 0x71,                    // VK_HANJA      한자
        0x1C => 0x79,                    // VK_CONVERT    変換
        0x1D => 0x7B,                    // VK_NONCONVERT 無変換
        0xF2 => 0x70,                    // VK_DBE_HIRAGANA カタカナ/ひらがな
        0xF3 => 0x29,                    // VK_DBE_SBCSCHAR 半角/全角 (the JIS grave position)
        0xC1 => 0x73,                    // VK_ABNT_C1    JIS ろ, ABNT2 /?
        0xC2 => 0x7E,                    // VK_ABNT_C2    ABNT2 keypad .
        0xE1 => 0x7D,                    // VK_OEM_AX     JIS ¥
        _ => return None,
    })
}

/// Keyboard layout of the thread owning the foreground window — the layout the
/// receiving app will decode our scancodes under. `None` when there is no
/// foreground window (secure desktop) — caller falls back to this thread's layout.
fn foreground_hkl() -> Option<HKL> {
    // SAFETY: three read-only queries. `GetForegroundWindow` takes nothing and returns a possibly
    // null `HWND` (checked). `GetWindowThreadProcessId` reads the window's owning thread id (the
    // process-id out-param is `None`, allowed). `GetKeyboardLayout` maps a thread id to its input
    // locale by value. No pointer we own is dereferenced; a stale/foreign `tid` yields a null HKL,
    // which is filtered.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let tid = GetWindowThreadProcessId(hwnd, None);
        let hkl = GetKeyboardLayout(tid);
        (!hkl.is_invalid()).then_some(hkl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The positional table must mirror Linux `vk_to_evdev` exactly — for the typing
    /// area the evdev code IS the set-1 scancode, so a divergence lands the same wire
    /// VK on different physical keys on the two hosts. The IME keys are the one place
    /// the two codes differ; both hosts must still know every one of them.
    #[test]
    fn positional_table_mirrors_linux_vk_to_evdev() {
        let ime: &[(u16, u16, u16)] = &[
            (0x15, 0x72, 122),
            (0x19, 0x71, 123),
            (0x1C, 0x79, 92),
            (0x1D, 0x7B, 94),
            (0xF2, 0x70, 93),
            (0xF3, 0x29, 85),
            (0xC1, 0x73, 89),
            (0xC2, 0x7E, 121),
            (0xE1, 0x7D, 124),
        ];
        let mut checked = 0;
        for vk in 0x01..=0xFEu16 {
            if let Some(scan) = positional_vk_to_scan(vk) {
                let evdev = ime
                    .iter()
                    .find(|(k, _, _)| *k == vk)
                    .map(|&(_, s, e)| {
                        assert_eq!(scan, s, "vk 0x{vk:02X}: IME scancode");
                        e
                    })
                    .unwrap_or(scan);
                assert_eq!(
                    Some(evdev),
                    crate::vk_to_evdev(vk as u8),
                    "vk 0x{vk:02X}: sendinput scancode diverges from vk_to_evdev"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 57, "typing-area coverage changed unexpectedly");
    }

    #[test]
    fn wire_buttons_one_to_five_map_and_nothing_else_does() {
        let x2 = mouse_button_input(5, true).unwrap();
        assert_eq!((x2.dwFlags, x2.mouseData), (MOUSEEVENTF_XDOWN, XBUTTON2));
        assert_eq!(
            mouse_button_input(3, false).unwrap().dwFlags,
            MOUSEEVENTF_RIGHTUP
        );
        assert!(mouse_button_input(0, true).is_none());
        assert!(mouse_button_input(6, true).is_none());
    }

    /// A positional VK never asks the layout; a semantic one does, and keeps its E0 bit.
    #[test]
    fn key_input_takes_the_table_for_positional_and_the_layout_for_semantic() {
        let ki = key_input(0x59, false, true, |_| panic!("positional must not map")).unwrap();
        assert_eq!((ki.wScan, ki.dwFlags), (0x15, KEYEVENTF_SCANCODE));
        let ki = key_input(0x59, true, false, |_| 0xe02c).unwrap();
        assert_eq!(ki.wScan, 0x2c);
        assert_eq!(
            ki.dwFlags,
            KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP
        );
        assert!(
            key_input(0x0D, false, true, |_| 0).is_none(),
            "unmappable drops"
        );
        // Pause rides `wVk`: its scancode is NumLock's.
        let pause = key_input(crate::keymap::VK_PAUSE, false, true, |_| panic!()).unwrap();
        assert_eq!(
            (pause.wVk, pause.wScan),
            (VIRTUAL_KEY(crate::keymap::VK_PAUSE), 0)
        );
    }

    #[test]
    fn text_is_utf16_units_down_then_up() {
        assert_eq!(text_units('a' as u32).len(), 2);
        let pair = text_units(0x1F600);
        assert_eq!(pair.len(), 4);
        assert_eq!((pair[0].wScan, pair[2].wScan), (0xD83D, 0xDE00));
        assert_eq!(pair[1].dwFlags, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
        assert!(
            text_units(0x0A).is_empty(),
            "control characters ride the VK path"
        );
        assert!(text_units(0xD800).is_empty(), "a lone surrogate drops");
    }

    /// US Y/Z/ö/ü stay on those positions. Pause and NumLock stay out — they share scan 0x45.
    #[test]
    fn positional_pins_for_the_qwertz_scramble() {
        assert_eq!(positional_vk_to_scan(0x59), Some(0x15)); // VK_Y → US-Y position (QWERTZ: Z key)
        assert_eq!(positional_vk_to_scan(0x5A), Some(0x2C)); // VK_Z → US-Z position (QWERTZ: Y key)
        assert_eq!(positional_vk_to_scan(0xBA), Some(0x27)); // VK_OEM_1 → ;: position (QWERTZ: ö)
        assert_eq!(positional_vk_to_scan(0xDB), Some(0x1A)); // VK_OEM_4 → [{ position (QWERTZ: ü)
                                                             // Layout-invariant keys stay out of the table (resolved via MapVirtualKeyExW).
        assert_eq!(positional_vk_to_scan(0x70), None); // VK_F1
        assert_eq!(positional_vk_to_scan(0x0D), None); // VK_RETURN
        assert_eq!(positional_vk_to_scan(0xA0), None); // VK_LSHIFT

        // Pause/NumLock share scan 0x45; the table must not claim either.
        assert_eq!(positional_vk_to_scan(crate::keymap::VK_PAUSE), None);
        assert_eq!(positional_vk_to_scan(0x90), None); // VK_NUMLOCK
        assert!(!crate::keymap::vk_forced_extended(0x90));
        assert!(!crate::keymap::vk_forced_extended(crate::keymap::VK_PAUSE));
    }

    /// Keypad Enter is E0 1C even though `MapVirtualKeyExW` knows no VK_SEPARATOR.
    #[test]
    fn keypad_enter_is_extended_return() {
        let ki = key_input(0x6C, false, true, |_| 0).expect("keypad Enter maps");
        assert_eq!(ki.wScan, 0x1C);
        assert!(ki.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
    }
}
