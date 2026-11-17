//! The GameInput row of the matrix, and the probe's only path that drives TRIGGER rumble.
//!
//! WHY THIS IS HAND-WRITTEN COM. GameInput has no binding in the `windows` crate, so the vtables
//! below are declared by hand, each slot index taken from the SDK header
//! `Windows Kits\10\Include\10.0.26100.0\um\GameInput.h`. A COM vtable is positional: a wrong slot
//! calls a different method with the wrong signature and corrupts the stack. Only the slots up to
//! the last one called are declared; nothing past them is ever read.
//!
//! WHY IT MATTERS BEYOND ENUMERATION. `XINPUT_VIBRATION` has exactly two members, so classic
//! XInput can never exercise an Xbox pad's two IMPULSE-TRIGGER motors. `GameInputRumbleParams` has
//! four — `lowFrequency`, `highFrequency`, `leftTrigger`, `rightTrigger`. `parse_xbox_output`
//! (pf-inject `xbox_proto.rs`) reads the `enable` mask of HID output report `0x03` as bit 0 = weak,
//! bit 1 = strong, bit 2 = right trigger, bit 3 = left trigger, as hid-microsoft and xpadneo do.
//!
//! `--gi-rumble` drives four DISTINCT magnitudes, then the pad's decoded output identifies the
//! mapping by itself: a channel that comes back zero had the wrong enable bit, and a channel
//! holding another's value is a swap.
//!
//! The runtime is loaded by name rather than linked, so this builds with no import library and
//! degrades to a clean "GameInput not present" on a box without it.

#![allow(non_snake_case)]

use std::ffi::c_void;

use windows::core::{HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// `GameInputKind` values we use (GameInput.h).
const GAME_INPUT_KIND_GAMEPAD: u32 = 0x0004_0000;
const GAME_INPUT_KIND_CONTROLLER: u32 = 0x0000_000E;

/// The four rumble channels, 0.0..=1.0 each. Layout verbatim from GameInput.h.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct GameInputRumbleParams {
    pub lowFrequency: f32,
    pub highFrequency: f32,
    pub leftTrigger: f32,
    pub rightTrigger: f32,
}

/// `IGameInput`, declared only as far as `GetCurrentReading` (slot 4).
#[repr(C)]
struct IGameInputVtbl {
    QueryInterface: unsafe extern "system" fn(*mut c_void, *const u8, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    Release: unsafe extern "system" fn(*mut c_void) -> u32,
    GetCurrentTimestamp: unsafe extern "system" fn(*mut c_void) -> u64,
    GetCurrentReading:
        unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut *mut c_void) -> HRESULT,
}

/// `IGameInputReading`, declared only as far as `GetDevice` (slot 6).
#[repr(C)]
struct IGameInputReadingVtbl {
    QueryInterface: unsafe extern "system" fn(*mut c_void, *const u8, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    Release: unsafe extern "system" fn(*mut c_void) -> u32,
    GetInputKind: unsafe extern "system" fn(*mut c_void) -> u32,
    GetSequenceNumber: unsafe extern "system" fn(*mut c_void, u32) -> u64,
    GetTimestamp: unsafe extern "system" fn(*mut c_void) -> u64,
    GetDevice: unsafe extern "system" fn(*mut c_void, *mut *mut c_void),
}

/// `IGameInputDevice`, declared only as far as `SetRumbleState` (slot 10).
#[repr(C)]
struct IGameInputDeviceVtbl {
    QueryInterface: unsafe extern "system" fn(*mut c_void, *const u8, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut c_void) -> u32,
    Release: unsafe extern "system" fn(*mut c_void) -> u32,
    GetDeviceInfo: unsafe extern "system" fn(*mut c_void) -> *const c_void,
    GetDeviceStatus: unsafe extern "system" fn(*mut c_void) -> u32,
    GetBatteryState: unsafe extern "system" fn(*mut c_void, *mut c_void),
    CreateForceFeedbackEffect:
        unsafe extern "system" fn(*mut c_void, u32, *const c_void, *mut *mut c_void) -> HRESULT,
    IsForceFeedbackMotorPoweredOn: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    SetForceFeedbackMotorGain: unsafe extern "system" fn(*mut c_void, u32, f32),
    SetHapticMotorState: unsafe extern "system" fn(*mut c_void, u32, *const c_void),
    SetRumbleState: unsafe extern "system" fn(*mut c_void, *const GameInputRumbleParams),
}

/// A loaded GameInput runtime plus the root object. Dropping it releases both.
pub struct GameInput {
    module: HMODULE,
    root: *mut c_void,
}

impl Drop for GameInput {
    fn drop(&mut self) {
        // SAFETY: `root` came from GameInputCreate and is released exactly once; `module` came from
        // LoadLibraryW. Freeing the module after the object is the required order.
        unsafe {
            if !self.root.is_null() {
                let vtbl = *(self.root as *mut *const IGameInputVtbl);
                ((*vtbl).Release)(self.root);
            }
            let _ = FreeLibrary(self.module);
        }
    }
}

impl GameInput {
    /// Load `gameinput.dll` and create the root object. `Err` carries a human reason — a box
    /// without the runtime is a legitimate outcome, not a crash.
    pub fn create() -> Result<Self, String> {
        let name: Vec<u16> = "gameinput.dll\0".encode_utf16().collect();
        // SAFETY: `name` is a NUL-terminated wide string that outlives the call.
        let module = unsafe { LoadLibraryW(PCWSTR(name.as_ptr())) }
            .map_err(|e| format!("gameinput.dll not loadable: {e}"))?;
        // SAFETY: `module` is live; the name is a NUL-terminated byte string.
        let proc =
            unsafe { GetProcAddress(module, PCSTR(c"GameInputCreate".as_ptr() as *const u8)) }
                .ok_or_else(|| "gameinput.dll has no GameInputCreate export".to_string())?;
        // SAFETY: the export's documented signature is
        // `HRESULT GameInputCreate(IGameInput**)` — GameInput.h, `STDAPI GameInputCreate`.
        let create: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT =
            unsafe { std::mem::transmute(proc) };
        let mut root: *mut c_void = std::ptr::null_mut();
        // SAFETY: `root` is a valid out-param slot.
        let hr = unsafe { create(&mut root) };
        if hr.is_err() || root.is_null() {
            // SAFETY: nothing was created; drop the module by hand since we have no object yet.
            unsafe {
                let _ = FreeLibrary(module);
            }
            return Err(format!("GameInputCreate failed: {hr:?}"));
        }
        Ok(Self { module, root })
    }

    /// The current reading for `kind`, if any device is producing one.
    fn reading(&self, kind: u32) -> Option<*mut c_void> {
        let mut reading: *mut c_void = std::ptr::null_mut();
        // SAFETY: `self.root` is a live IGameInput; slot 4 is GetCurrentReading with this exact
        // signature (GameInput.h). A null `device` means "any device", which is what we want.
        let hr = unsafe {
            let vtbl = *(self.root as *mut *const IGameInputVtbl);
            ((*vtbl).GetCurrentReading)(self.root, kind, std::ptr::null_mut(), &mut reading)
        };
        (hr.is_ok() && !reading.is_null()).then_some(reading)
    }

    /// Poll for a reading, because GameInput's device enumeration is ASYNCHRONOUS.
    ///
    /// A freshly created `IGameInput` has not finished enumerating yet, so the first
    /// `GetCurrentReading` can return nothing even with a pad actively reporting. This is the
    /// GameInput analogue of `wake_wgi`: the API looks like a query and is really a cache someone
    /// else fills.
    ///
    /// ⚠️ Focus is NOT the cause and was ruled out from the header: `GameInputDefaultFocusPolicy`
    /// is 0 and every `GameInputFocusPolicy` flag is a RESTRICTION
    /// (`GameInputDisableBackgroundInput`, `GameInputExclusiveForegroundInput`, …), so the default
    /// already admits background input. Do not "fix" this by calling `SetFocusPolicy`.
    fn reading_wait(&self, kind: u32, timeout: std::time::Duration) -> Option<*mut c_void> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(r) = self.reading(kind) {
                return Some(r);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// `(vendorId, productId)` for a device, read straight off `GameInputDeviceInfo`.
    ///
    /// The struct begins `uint32_t infoSize; uint16_t vendorId; uint16_t productId; …`
    /// (GameInput.h), so the two ids sit at byte offsets 4 and 6. Only those two are read — the
    /// rest of the struct carries variable-size members whose layout we would have to mirror
    /// exactly, and nothing here needs them.
    fn device_ids(dev: *mut c_void) -> (u16, u16) {
        // SAFETY: `dev` is a live IGameInputDevice; slot 3 is GetDeviceInfo, which returns a
        // pointer to a struct owned by the runtime and valid for the device's lifetime.
        unsafe {
            let vtbl = *(dev as *mut *const IGameInputDeviceVtbl);
            let info = ((*vtbl).GetDeviceInfo)(dev) as *const u8;
            if info.is_null() {
                return (0, 0);
            }
            (
                u16::from_le_bytes([*info.add(4), *info.add(5)]),
                u16::from_le_bytes([*info.add(6), *info.add(7)]),
            )
        }
    }

    /// Take the device off a reading (AddRef'd), releasing the reading.
    fn device_of(r: *mut c_void) -> *mut c_void {
        let mut dev: *mut c_void = std::ptr::null_mut();
        // SAFETY: `r` is a live IGameInputReading; slot 6 is GetDevice (returns void, hands back
        // an AddRef'd device), slot 2 is Release.
        unsafe {
            let vtbl = *(r as *mut *const IGameInputReadingVtbl);
            ((*vtbl).GetDevice)(r, &mut dev);
            ((*vtbl).Release)(r);
        }
        dev
    }

    /// Hunt for a device with `pid`, polling because several devices take turns reporting and
    /// `GetCurrentReading(kind, null, …)` hands back whichever one spoke most recently. A box with
    /// a chatty pad on it (a DualSense streams continuously) will otherwise never yield ours.
    fn find_device(&self, pid: u16, timeout: std::time::Duration) -> Option<*mut c_void> {
        let deadline = std::time::Instant::now() + timeout;
        let mut seen: Vec<(u16, u16)> = Vec::new();
        loop {
            for kind in [GAME_INPUT_KIND_GAMEPAD, GAME_INPUT_KIND_CONTROLLER] {
                if let Some(r) = self.reading(kind) {
                    let dev = Self::device_of(r);
                    if !dev.is_null() {
                        let ids = Self::device_ids(dev);
                        if !seen.contains(&ids) {
                            seen.push(ids);
                            println!("    saw device {:04X}:{:04X}", ids.0, ids.1);
                        }
                        if ids.1 == pid {
                            return Some(dev);
                        }
                        // SAFETY: not our target; drop our reference.
                        unsafe {
                            let v = *(dev as *mut *const IGameInputDeviceVtbl);
                            ((*v).Release)(dev);
                        }
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }

    /// `(timestamp, sequence)` of the current gamepad reading. `None` when GameInput hands this
    /// process nothing: no pad, or its one focus per box sits on another process.
    pub fn gamepad_stamp(&self) -> Option<(u64, u64)> {
        let (kind, r) = [GAME_INPUT_KIND_GAMEPAD, GAME_INPUT_KIND_CONTROLLER]
            .into_iter()
            .find_map(|kind| self.reading(kind).map(|r| (kind, r)))?;
        // SAFETY: `r` is a live IGameInputReading we own; slot 4 is GetSequenceNumber(kind),
        // slot 5 GetTimestamp, slot 2 Release (GameInput.h).
        unsafe {
            let vtbl = *(r as *mut *const IGameInputReadingVtbl);
            let seq = ((*vtbl).GetSequenceNumber)(r, kind);
            let ts = ((*vtbl).GetTimestamp)(r);
            ((*vtbl).Release)(r);
            Some((ts, seq))
        }
    }

    /// Does GameInput see a gamepad at all? This is the matrix row.
    pub fn report(&self) {
        for (kind, label) in [
            (GAME_INPUT_KIND_GAMEPAD, "Gamepad"),
            (GAME_INPUT_KIND_CONTROLLER, "Controller"),
        ] {
            match self.reading_wait(kind, std::time::Duration::from_secs(3)) {
                Some(r) => {
                    println!("  {label:<11}: reading available (a device is producing input)");
                    // SAFETY: `r` is a live IGameInputReading we own a reference to.
                    unsafe {
                        let vtbl = *(r as *mut *const IGameInputReadingVtbl);
                        ((*vtbl).Release)(r);
                    }
                }
                None => println!("  {label:<11}: no reading"),
            }
        }
    }

    /// Drive `SetRumbleState` on whatever device is currently reporting.
    ///
    /// Returns whether a device was found and driven. The values are deliberately the caller's to
    /// choose: the whole point of the probe is sending four DISTINCT magnitudes so the pad's decoded
    /// output identifies the channel mapping by itself.
    pub fn rumble(
        &self,
        params: GameInputRumbleParams,
        hold: std::time::Duration,
        target_pid: Option<u16>,
    ) -> bool {
        // A gamepad reading is the right one to hang this off: it is the kind an Xbox pad produces,
        // and the device it names is the one a game would rumble.
        let wait = std::time::Duration::from_secs(6);
        let dev = match target_pid {
            Some(pid) => {
                println!("  hunting for PID {pid:04X} …");
                match self.find_device(pid, wait) {
                    Some(d) => d,
                    None => {
                        println!("  never saw PID {pid:04X} — nothing to rumble");
                        return false;
                    }
                }
            }
            None => {
                let Some(r) = self
                    .reading_wait(GAME_INPUT_KIND_GAMEPAD, wait)
                    .or_else(|| self.reading_wait(GAME_INPUT_KIND_CONTROLLER, wait))
                else {
                    println!("  no GameInput reading — nothing to rumble");
                    return false;
                };
                Self::device_of(r)
            }
        };
        if dev.is_null() {
            println!("  reading had no device");
            return false;
        }
        let ids = Self::device_ids(dev);
        println!("  driving {:04X}:{:04X}", ids.0, ids.1);
        println!(
            "  SetRumbleState(low={:.2} high={:.2} lt={:.2} rt={:.2}) for {:?}",
            params.lowFrequency,
            params.highFrequency,
            params.leftTrigger,
            params.rightTrigger,
            hold
        );
        // SAFETY: `dev` is a live IGameInputDevice; slot 10 is SetRumbleState, which returns void
        // and takes a const pointer to the four-float struct above.
        unsafe {
            let vtbl = *(dev as *mut *const IGameInputDeviceVtbl);
            ((*vtbl).SetRumbleState)(dev, &params);
        }
        std::thread::sleep(hold);
        let off = GameInputRumbleParams::default();
        // SAFETY: as above; stopping is the same call with zeroes.
        unsafe {
            let vtbl = *(dev as *mut *const IGameInputDeviceVtbl);
            ((*vtbl).SetRumbleState)(dev, &off);
            ((*vtbl).Release)(dev);
        }
        println!("  cleared.");
        true
    }
}
