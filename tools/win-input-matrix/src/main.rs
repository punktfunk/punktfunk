//! Which Windows input APIs can see this gamepad?
//!
//! The Xbox-pad-on-Windows work (`punktfunk-planning/design/xbox-pad-windows-handoff.md`) is a
//! five-row matrix — classic XInput, WGI `Gamepad`, WGI `RawGameController`, GameInput, and the
//! HID/DirectInput/Steam family. This makes the matrix a command you can run twice and diff.
//!
//! ⚠️⚠️ **THE FALSE-POSITIVE TRAP, and why `--watch` exists.** A test box usually has REAL pads on
//! it. A real Xbox pad owns XInput slot 0 and appears in WGI, so "I can see a pad" proves nothing:
//! when slot 0 is the real pad, `XInputGetState(0)` answers `rc=0` with the virtual pad live or
//! killed.
//! ⇒ **ALWAYS take a baseline with your pad STOPPED and diff it**, and identify entries by name and
//! vendor/product id, never by slot index alone.
//! `--watch N` is the second half of that discipline: it samples repeatedly and reports whether a
//! device's timestamps ADVANCE. An entry that enumerates but never moves is the exact failure mode
//! this programme is chasing — WGI listing a gamepad that reports nothing is arguably worse than
//! not listing it, because a title that binds the first gamepad latches a dead one.
//!
//! GameInput has no binding in the `windows` crate: `gameinput.rs` declares its COM vtables by
//! hand, and `--gameinput`, `--gi-rumble` and `--gi-focus` drive it.

#[cfg(not(windows))]
fn main() {
    eprintln!("win-input-matrix is Windows-only.");
    std::process::exit(2);
}

#[cfg(windows)]
mod gameinput;

#[cfg(windows)]
mod imp {
    use std::time::Duration;

    use windows::Foundation::EventHandler;
    use windows::Gaming::Input::{Gamepad, IGameController, RawGameController};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    };
    use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;
    use windows::Win32::System::Com::CoIncrementMTAUsage;
    use windows::Win32::UI::Input::XboxController::{
        XInputGetState, XInputSetState, XINPUT_STATE, XINPUT_VIBRATION,
    };
    // `Interface` brings `cast()` into scope, which is how a WinRT `Gamepad` is correlated to the
    // `RawGameController` that knows its name.
    use windows::core::{Interface, GUID};

    /// `GUID_DEVINTERFACE_XUSB` — the interface class `xinput1_4` enumerates. This is the one that
    /// matters: XInput does not read HID at all, it walks this class.
    const GUID_DEVINTERFACE_XUSB: GUID = GUID::from_u128(0xec87f1e3_c13b_4100_b5f7_8b84d54260cb);
    /// `GUID_DEVINTERFACE_HID` — what Steam, SDL/hidapi, RawInput, DirectInput and joy.cpl walk.
    const GUID_DEVINTERFACE_HID: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);

    /// Enumerate the device paths for every present interface in `class`.
    ///
    /// SetupAPI returns each variable-length detail record as an exact byte count. The backing
    /// allocation stays aligned for both its fixed header and trailing WCHAR path, while path
    /// decoding remains bounded by that reported count. Registry-only, long-dead devnodes are
    /// excluded because this probe asks what input APIs can see now.
    fn interfaces(class: GUID) -> Vec<String> {
        let mut out = Vec::new();
        // SAFETY: `class` is a valid GUID; we pass no enumerator and no owner window. The returned
        // handle is destroyed unconditionally below.
        let set: HDEVINFO = match unsafe {
            SetupDiGetClassDevsW(
                Some(&class),
                None,
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        } {
            Ok(h) => h,
            Err(_) => return out,
        };

        let mut index = 0u32;
        loop {
            let mut ifdata = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            // SAFETY: `set` is a live device-info set; `ifdata.cbSize` is initialised as the API
            // requires. A failure here means "no more items", which ends the loop.
            let ok = unsafe { SetupDiEnumDeviceInterfaces(set, None, &class, index, &mut ifdata) }
                .is_ok();
            if !ok {
                break;
            }
            index += 1;

            // Two-call dance: ask for the required byte count, then fetch into a buffer of that
            // size. The detail struct is variable-length (a trailing WCHAR path), so it cannot be
            // stack-allocated by type alone.
            let mut needed = 0u32;
            // SAFETY: passing a null detail pointer with a null size is the documented way to
            // query the required length; it always "fails" with ERROR_INSUFFICIENT_BUFFER.
            let _ = unsafe {
                SetupDiGetDeviceInterfaceDetailW(set, &ifdata, None, 0, Some(&mut needed), None)
            };
            if needed == 0 {
                continue;
            }
            let byte_len = needed as usize;
            if byte_len < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
                continue;
            }
            let word_len = byte_len.div_ceil(size_of::<usize>());
            let mut storage = vec![0usize; word_len];
            let detail = storage.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            // SAFETY: `storage` is aligned for the detail header and holds at least `needed` bytes.
            // `cbSize` is the target ABI's fixed struct size, not the variable buffer length.
            unsafe {
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            }
            // SAFETY: `detail` points into aligned storage that lives through the call. The exact
            // byte count passed to SetupAPI is the capacity it reported, excluding rounded padding.
            if unsafe {
                SetupDiGetDeviceInterfaceDetailW(set, &ifdata, Some(detail), needed, None, None)
            }
            .is_err()
            {
                continue;
            }
            let path_offset = std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
            let Some(path_bytes) = byte_len.checked_sub(path_offset) else {
                continue;
            };
            let path_units = path_bytes / size_of::<u16>();
            // SAFETY: `DevicePath` is u16-aligned within the aligned detail record. `path_units`
            // covers only complete WCHARs inside SetupAPI's exact returned byte count, and
            // `addr_of!` keeps the pointer's reach over all of `storage`, not the `[u16; 1]`.
            let path = unsafe {
                let first = std::ptr::addr_of!((*detail).DevicePath).cast::<u16>();
                let wide = std::slice::from_raw_parts(first, path_units);
                let len = wide
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(wide.len());
                String::from_utf16_lossy(&wide[..len])
            };
            out.push(path);
        }

        // SAFETY: `set` came from SetupDiGetClassDevsW and is not used again.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(set) };
        let _ = ERROR_NO_MORE_ITEMS;
        out
    }

    /// 🛑 **DO NOT DELETE THIS — without it the whole WGI half of the matrix reads zero.**
    ///
    /// `Gamepad::Gamepads()` and `RawGameController::RawGameControllers()` are not queries; they
    /// return a cache that WGI's device-watcher fills in. In a GUI app something else has already
    /// started that watcher, so the cache looks like a query and everyone writes code as if it
    /// were one. In a bare console process nothing has, and both collections come back **EMPTY
    /// even with real controllers attached**.
    ///
    /// Subscribing to the Added events is what starts the watcher. The handlers deliberately do
    /// nothing; registering them is the entire point. The sleep gives the watcher a beat to
    /// enumerate before the first read.
    ///
    /// ⚠️ A probe without this reports "WGI cannot see the pad" when WGI could not see ANYTHING,
    /// which is a very different conclusion.
    fn wake_wgi() {
        let gp_tok = Gamepad::GamepadAdded(&EventHandler::<Gamepad>::new(|_, _| Ok(())));
        let raw_tok = RawGameController::RawGameControllerAdded(
            &EventHandler::<RawGameController>::new(|_, _| Ok(())),
        );
        if gp_tok.is_err() || raw_tok.is_err() {
            eprintln!("warning: WGI Added subscription failed — counts may read zero");
        }
        std::thread::sleep(Duration::from_millis(1500));
    }

    /// Drive rumble into an XInput slot and hold it, so the other end of the pipe can be watched.
    ///
    /// This is the WP0 probe from `design/trigger-rumble-plane.md`: does anything Windows-side ever
    /// write an output report back to a synthesized `045E:0B13`? For the HID backend the chain
    /// under test is `XInputSetState` → `xinputhid` → a HID output report on our collection →
    /// `on_output_report` → the shm out-ring → `parse_xbox_output`, and the observable is the
    /// devtest printing `rumble from game`. Run this with the devtest live and watch its stdout.
    ///
    /// ⚠️ `XINPUT_VIBRATION` has exactly TWO members, so this can only ever drive the two handle
    /// motors — it can never source TRIGGER rumble. That is a property of the API, not of our
    /// plumbing, and it is why the trigger plane needs its own transport.
    fn rumble(slot: u32, seconds: u64) {
        println!("== RUMBLE PROBE: XInputSetState(slot {slot}) for {seconds}s ==");
        let v = XINPUT_VIBRATION {
            wLeftMotorSpeed: 0xFFFF,
            wRightMotorSpeed: 0x8000,
        };
        // SAFETY: `v` is a valid, fully-initialised XINPUT_VIBRATION.
        let rc = unsafe { XInputSetState(slot, &v) };
        println!(
            "  set  low=0xFFFF high=0x8000 -> rc={rc}{}",
            if rc == 0 {
                " (accepted)"
            } else {
                " (REJECTED)"
            }
        );
        if rc != 0 {
            println!("  (slot not connected — nothing downstream can be concluded)");
            return;
        }
        std::thread::sleep(Duration::from_secs(seconds));
        let off = XINPUT_VIBRATION::default();
        // SAFETY: as above.
        let rc2 = unsafe { XInputSetState(slot, &off) };
        println!("  clear low=0 high=0 -> rc={rc2}");
        println!("  ⇒ now check the devtest stdout for `rumble from game`.");
    }

    fn xinput() {
        println!("== classic XInput (xinput1_4 walks GUID_DEVINTERFACE_XUSB) ==");
        for slot in 0..4u32 {
            let mut st = XINPUT_STATE::default();
            // SAFETY: `st` is a valid, fully-initialised XINPUT_STATE for the call to fill in.
            let rc = unsafe { XInputGetState(slot, &mut st) };
            if rc == 0 {
                let g = st.Gamepad;
                println!(
                    "  slot {slot}: rc=0 packet={} buttons=0x{:04X} LT={} RT={} LX={} LY={} RX={} RY={}",
                    st.dwPacketNumber,
                    g.wButtons.0,
                    g.bLeftTrigger,
                    g.bRightTrigger,
                    g.sThumbLX,
                    g.sThumbLY,
                    g.sThumbRX,
                    g.sThumbRY
                );
            } else {
                println!(
                    "  slot {slot}: rc={rc}{}",
                    if rc == 1167 {
                        " (ERROR_DEVICE_NOT_CONNECTED)"
                    } else {
                        ""
                    }
                );
            }
        }
    }

    /// One WGI sample, for the mute detector.
    struct Sample {
        label: String,
        ts: u64,
        axes: Vec<f64>,
    }

    fn wgi_gamepads() -> Vec<Sample> {
        let mut out = Vec::new();
        let Ok(list) = Gamepad::Gamepads() else {
            return out;
        };
        let n = list.Size().unwrap_or(0);
        for i in 0..n {
            let Ok(gp) = list.GetAt(i) else { continue };
            // Correlate to a RawGameController purely to get a human-readable name — a bare
            // `Gamepad` has none, and identifying entries by index is how false positives happen.
            let label = gp
                .cast::<IGameController>()
                .ok()
                .and_then(|c| RawGameController::FromGameController(&c).ok())
                .and_then(|r| r.DisplayName().ok())
                .map(|h| h.to_string())
                .unwrap_or_else(|| format!("<gamepad {i}>"));
            let (ts, axes) = match gp.GetCurrentReading() {
                Ok(r) => (
                    r.Timestamp,
                    vec![
                        r.LeftThumbstickX,
                        r.LeftThumbstickY,
                        r.RightThumbstickX,
                        r.RightThumbstickY,
                        r.LeftTrigger,
                        r.RightTrigger,
                    ],
                ),
                Err(_) => (0, Vec::new()),
            };
            out.push(Sample { label, ts, axes });
        }
        out
    }

    fn wgi_raw() -> Vec<Sample> {
        let mut out = Vec::new();
        let Ok(list) = RawGameController::RawGameControllers() else {
            return out;
        };
        let n = list.Size().unwrap_or(0);
        for i in 0..n {
            let Ok(rc) = list.GetAt(i) else { continue };
            let name = rc
                .DisplayName()
                .map(|h| h.to_string())
                .unwrap_or_else(|_| "<unnamed>".into());
            let vid = rc.HardwareVendorId().unwrap_or(0);
            let pid = rc.HardwareProductId().unwrap_or(0);
            let nb = rc.ButtonCount().unwrap_or(0).max(0) as usize;
            let ns = rc.SwitchCount().unwrap_or(0).max(0) as usize;
            let na = rc.AxisCount().unwrap_or(0).max(0) as usize;
            let mut buttons = vec![false; nb];
            let mut switches = vec![Default::default(); ns];
            let mut axes = vec![0f64; na];
            let ts = rc
                .GetCurrentReading(&mut buttons, &mut switches, &mut axes)
                .unwrap_or(0);
            out.push(Sample {
                label: format!("{name} [{vid:04X}:{pid:04X}] buttons={nb} switches={ns} axes={na}"),
                ts,
                axes,
            });
        }
        out
    }

    fn print_samples(title: &str, s: &[Sample]) {
        println!("== {title} == count={}", s.len());
        for (i, e) in s.iter().enumerate() {
            let axes = e
                .axes
                .iter()
                .map(|v| format!("{v:.4}"))
                .collect::<Vec<_>>()
                .join(",");
            println!("  [{i}] ts={} {} axes=[{axes}]", e.ts, e.label);
        }
        if s.is_empty() {
            println!("  (none)");
        }
    }

    /// What `xinput_watch` tracks per slot: the packet range, the buttons ever seen and the RANGE
    /// each axis covered.
    ///
    /// A single `XInputGetState` call cannot tell "translated correctly" from "stuck at zero" —
    /// a sweeping stick reads 0 every time it crosses centre. `dwPacketNumber` advancing proves
    /// the state is changing at all; the min/max spread proves the AXES specifically are, which is
    /// the half that can fail on its own while buttons work.
    struct XiTrack {
        first_packet: u32,
        last_packet: u32,
        lx: (i16, i16),
        ly: (i16, i16),
        rx: (i16, i16),
        ry: (i16, i16),
        buttons: u16,
        lt: (u8, u8),
        rt: (u8, u8),
    }

    fn xinput_watch(rounds: usize) {
        println!("\n== XINPUT WATCH ({rounds} samples) — do PACKETS advance and AXES move? ==");
        for slot in 0..4u32 {
            let mut t: Option<XiTrack> = None;
            for _ in 0..rounds {
                let mut st = XINPUT_STATE::default();
                // SAFETY: `st` is a valid, fully-initialised XINPUT_STATE.
                if unsafe { XInputGetState(slot, &mut st) } != 0 {
                    break;
                }
                let g = st.Gamepad;
                match &mut t {
                    None => {
                        t = Some(XiTrack {
                            first_packet: st.dwPacketNumber,
                            last_packet: st.dwPacketNumber,
                            lx: (g.sThumbLX, g.sThumbLX),
                            ly: (g.sThumbLY, g.sThumbLY),
                            rx: (g.sThumbRX, g.sThumbRX),
                            ry: (g.sThumbRY, g.sThumbRY),
                            buttons: g.wButtons.0,
                            lt: (g.bLeftTrigger, g.bLeftTrigger),
                            rt: (g.bRightTrigger, g.bRightTrigger),
                        });
                    }
                    Some(t) => {
                        t.last_packet = st.dwPacketNumber;
                        t.lx = (t.lx.0.min(g.sThumbLX), t.lx.1.max(g.sThumbLX));
                        t.ly = (t.ly.0.min(g.sThumbLY), t.ly.1.max(g.sThumbLY));
                        t.rx = (t.rx.0.min(g.sThumbRX), t.rx.1.max(g.sThumbRX));
                        t.ry = (t.ry.0.min(g.sThumbRY), t.ry.1.max(g.sThumbRY));
                        t.buttons |= g.wButtons.0;
                        t.lt = (t.lt.0.min(g.bLeftTrigger), t.lt.1.max(g.bLeftTrigger));
                        t.rt = (t.rt.0.min(g.bRightTrigger), t.rt.1.max(g.bRightTrigger));
                    }
                }
                std::thread::sleep(Duration::from_millis(120));
            }
            match t {
                None => println!("  slot {slot}: not connected"),
                Some(t) => {
                    let moved = t.last_packet != t.first_packet;
                    let axes_moved = t.lx.0 != t.lx.1
                        || t.ly.0 != t.ly.1
                        || t.rx.0 != t.rx.1
                        || t.ry.0 != t.ry.1
                        || t.lt.0 != t.lt.1
                        || t.rt.0 != t.rt.1;
                    println!(
                        "  slot {slot}: packets {}..{} ({}), buttons seen 0x{:04X}",
                        t.first_packet,
                        t.last_packet,
                        if moved { "ADVANCING" } else { "FROZEN" },
                        t.buttons
                    );
                    println!(
                        "    LX [{}..{}]  LY [{}..{}]  RX [{}..{}]  RY [{}..{}]  LT [{}..{}]  RT [{}..{}]  -> axes {}",
                        t.lx.0,
                        t.lx.1,
                        t.ly.0,
                        t.ly.1,
                        t.rx.0,
                        t.rx.1,
                        t.ry.0,
                        t.ry.1,
                        t.lt.0,
                        t.lt.1,
                        t.rt.0,
                        t.rt.1,
                        if axes_moved { "MOVING" } else { "STUCK" }
                    );
                }
            }
        }
    }

    /// Flag every device whose current reading differs from the baseline one. Once a device has
    /// moved it stays flagged — a pad that twitches once in twenty samples is still LIVE.
    fn mark_moved(base: &[Sample], now: &[Sample], moved: &mut [bool]) {
        for (i, e) in now.iter().enumerate() {
            if let Some(b) = base.get(i)
                && (b.ts != e.ts || b.axes != e.axes)
                && let Some(m) = moved.get_mut(i)
            {
                *m = true;
            }
        }
    }

    /// Sample repeatedly and report, per device, whether anything ever MOVED. This is the
    /// enumerated-but-mute detector: `ts` frozen across every sample means the API lists a pad
    /// that is not reporting.
    fn watch(rounds: usize) {
        println!("\n== WATCH ({rounds} rounds, 200 ms apart) — does anything actually MOVE? ==");
        let mut first_gp: Option<Vec<Sample>> = None;
        let mut first_raw: Option<Vec<Sample>> = None;
        let mut moved_gp: Vec<bool> = Vec::new();
        let mut moved_raw: Vec<bool> = Vec::new();

        for _ in 0..rounds {
            let gp = wgi_gamepads();
            let raw = wgi_raw();
            match &first_gp {
                None => {
                    moved_gp = vec![false; gp.len()];
                    first_gp = Some(gp);
                }
                Some(base) => mark_moved(base, &gp, &mut moved_gp),
            }
            match &first_raw {
                None => {
                    moved_raw = vec![false; raw.len()];
                    first_raw = Some(raw);
                }
                Some(base) => mark_moved(base, &raw, &mut moved_raw),
            }
            std::thread::sleep(Duration::from_millis(200));
        }

        for (label, base, moved) in [
            ("WGI Gamepad", first_gp, moved_gp),
            ("WGI RawGameController", first_raw, moved_raw),
        ] {
            println!("  {label}:");
            let Some(base) = base else { continue };
            if base.is_empty() {
                println!("    (none)");
            }
            for (i, e) in base.iter().enumerate() {
                println!(
                    "    [{i}] {} — {}",
                    e.label,
                    if *moved.get(i).unwrap_or(&false) {
                        "LIVE (readings changed)"
                    } else {
                        "MUTE (ts and axes frozen for every sample)"
                    }
                );
            }
        }
    }

    /// A visible window that asks GameInput for the gamepad reading once a second and logs
    /// whether it still advances. Run it in two sessions and change the foreground in one: the
    /// other's log shows whether GameInput's one focus per box took its pad away.
    fn focus_window(seconds: u64, log: &std::path::Path) {
        use std::io::Write as _;
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, PeekMessageW,
            PostQuitMessage, RegisterClassW, SetForegroundWindow, ShowWindow, TranslateMessage,
            MSG, PM_REMOVE, SW_SHOW, WINDOW_EX_STYLE, WM_DESTROY, WM_QUIT, WNDCLASSW,
            WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        };
        unsafe extern "system" fn wnd_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if m == WM_DESTROY {
                // SAFETY: plain Win32 call with no pointer arguments.
                unsafe { PostQuitMessage(0) };
                return LRESULT(0);
            }
            // SAFETY: forwarding the message the system handed us, unchanged.
            unsafe { DefWindowProcW(h, m, w, l) }
        }
        let mut file = std::fs::File::create(log).expect("open the focus log");
        let mut line = |s: String| {
            let _ = writeln!(file, "{s}");
            let _ = file.flush();
            println!("{s}");
        };
        let class = windows::core::w!("pf-gi-focus");
        // SAFETY: Win32 window bring-up on this thread; `class` is a static literal and `wc` is a
        // fully initialised local that RegisterClassW reads synchronously.
        let hwnd = unsafe {
            let hinst = GetModuleHandleW(None).expect("module handle");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinst.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let h = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                class,
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                420,
                160,
                None,
                None,
                Some(hinst.into()),
                None,
            )
            .expect("create the focus window");
            let _ = ShowWindow(h, SW_SHOW);
            let _ = SetForegroundWindow(h);
            h
        };
        let gi = match crate::gameinput::GameInput::create() {
            Ok(gi) => gi,
            Err(e) => {
                line(format!("gameinput unavailable: {e}"));
                return;
            }
        };
        let start = std::time::Instant::now();
        let mut next = start;
        let mut last: Option<(u64, u64)> = None;
        while start.elapsed().as_secs() < seconds {
            // SAFETY: the standard message pump on the window's own thread.
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if std::time::Instant::now() >= next {
                next += Duration::from_secs(1);
                let now = gi.gamepad_stamp();
                // SAFETY: plain Win32 query with no arguments.
                let fg = unsafe { GetForegroundWindow() } == hwnd;
                let advanced = matches!((last, now), (Some(a), Some(b)) if a != b);
                let epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                line(format!(
                    "t={:>5.1}s epoch_ms={epoch} pid={} fg={fg} reading={} ts={} seq={} advanced={advanced}",
                    start.elapsed().as_secs_f32(),
                    std::process::id(),
                    now.is_some(),
                    now.map_or(0, |x| x.0),
                    now.map_or(0, |x| x.1),
                ));
                last = now;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut rounds = 0usize;
        let mut rumble_slot: Option<u32> = None;
        let mut gameinput_report = false;
        let mut gi_rumble: Option<String> = None;
        let mut gi_pid: Option<u16> = None;
        let mut gi_focus: Option<(u64, String)> = None;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--watch" => {
                    rounds = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20);
                    i += 1;
                }
                "--gameinput" => gameinput_report = true,
                "--gi-focus" => {
                    let secs = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(60);
                    let log = args
                        .get(i + 2)
                        .cloned()
                        .unwrap_or_else(|| "gi-focus.log".into());
                    gi_focus = Some((secs, log));
                    i += 2;
                }
                "--gi-pid" => {
                    gi_pid = args
                        .get(i + 1)
                        .and_then(|v| u16::from_str_radix(v.trim_start_matches("0x"), 16).ok());
                    i += 1;
                }
                "--gi-rumble" => {
                    gi_rumble = args.get(i + 1).cloned();
                    i += 1;
                }
                "--rumble" => {
                    rumble_slot = Some(args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(0));
                    i += 1;
                }
                "--help" | "-h" => {
                    println!(
                        "win-input-matrix [--watch N] [--rumble SLOT] [--gameinput] [--gi-focus SECS LOG]\n\n  \
                         --watch N     sample WGI N times and report LIVE vs MUTE per device\n  \
                         --rumble SLOT drive XInputSetState into that slot for 3 s (WP0 probe:\n                \
                         does anything write an output report back to our pad?)\n  \
                         --gi-focus SECS LOG  open a window and log once a second whether GameInput\n                \
                         still hands this process gamepad readings (its focus is one per box)\n\n\
                         ALWAYS take a baseline with your virtual pad STOPPED and diff it: a real\n\
                         pad on the box owns XInput slot 0 and shows up in WGI."
                    );
                    return;
                }
                other => eprintln!("(ignoring unknown argument {other})"),
            }
            i += 1;
        }
        if let Some((secs, log)) = gi_focus {
            focus_window(secs, std::path::Path::new(&log));
            return;
        }

        // WinRT needs an initialised apartment. CoIncrementMTAUsage keeps an MTA alive for the
        // life of the process without committing this thread to a specific apartment.
        // SAFETY: no arguments to get wrong. The cookie is a plain handle value and is dropped on
        // purpose — decrementing would tear the MTA down again, and we want it up for the whole
        // process.
        match unsafe { CoIncrementMTAUsage() } {
            Ok(_cookie) => {}
            Err(e) => eprintln!("warning: MTA start failed — WGI calls may fail: {e}"),
        }
        wake_wgi();

        xinput();
        println!();
        print_samples("WGI Gamepad", &wgi_gamepads());
        println!();
        print_samples("WGI RawGameController", &wgi_raw());

        println!("\n== XUSB device interfaces (GUID_DEVINTERFACE_XUSB, present only) ==");
        let xusb = interfaces(GUID_DEVINTERFACE_XUSB);
        if xusb.is_empty() {
            println!("  (none)");
        }
        for p in &xusb {
            println!("  {p}");
        }

        println!("\n== HID device interfaces (what Steam/SDL/DirectInput/joy.cpl walk) ==");
        let hid = interfaces(GUID_DEVINTERFACE_HID);
        println!("  {} present; those matching a gamepad vendor:", hid.len());
        for p in &hid {
            let lower = p.to_ascii_lowercase();
            if lower.contains("vid_045e")
                || lower.contains("vid_054c")
                || lower.contains("punktfunk")
            {
                println!("  {p}");
            }
        }

        if rounds > 0 {
            watch(rounds);
            xinput_watch(rounds);
        }
        if let Some(slot) = rumble_slot {
            println!();
            rumble(slot, 3);
        }
        if gameinput_report || gi_rumble.is_some() {
            println!("\n== GameInput ==");
            match crate::gameinput::GameInput::create() {
                Err(e) => println!("  unavailable: {e}"),
                Ok(gi) => {
                    gi.report();
                    if let Some(spec) = &gi_rumble {
                        let v: Vec<f32> = spec
                            .split(',')
                            .map(|p| p.trim().parse().unwrap_or(0.0))
                            .collect();
                        let p = crate::gameinput::GameInputRumbleParams {
                            lowFrequency: v.first().copied().unwrap_or(0.0),
                            highFrequency: v.get(1).copied().unwrap_or(0.0),
                            leftTrigger: v.get(2).copied().unwrap_or(0.0),
                            rightTrigger: v.get(3).copied().unwrap_or(0.0),
                        };
                        gi.rumble(p, std::time::Duration::from_secs(3), gi_pid);
                    }
                }
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    imp::run();
}
