//! The console display's power state (`GUID_CONSOLE_DISPLAY_STATE`) and its documented wake.
//!
//! IddCx powers a virtual monitor's pipeline with the console display: while that is off, a new
//! monitor's path commits inactive and gets no swap-chain, so nothing streams. [`ensure_on`]
//! wakes a dark display before a monitor is created, and says so when Windows keeps it dark.
//!
//! Measured on Windows 11 24H2: a one-shot `ES_DISPLAY_REQUIRED` from the console session turns
//! the display on. The continuous form and `PowerRequestDisplayRequired` only keep a lit display
//! on, and no call from session 0 wakes it.

use std::ffi::c_void;
use std::mem::offset_of;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::GUID;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{
    PowerSettingRegisterNotification, SetThreadExecutionState, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
    ES_DISPLAY_REQUIRED, POWERBROADCAST_SETTING,
};
use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK;

const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_u128(0x6fe6_9556_704a_47a0_8f24_c28d_936f_da47);
const PBT_POWERSETTINGCHANGE: u32 = 0x8013;

/// The console display as Windows reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayPower {
    Off,
    On,
    Dimmed,
}

impl DisplayPower {
    fn from_raw(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Off),
            1 => Some(Self::On),
            2 => Some(Self::Dimmed),
            _ => None,
        }
    }
}

/// What [`ensure_on`] found and did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// No state: the registration failed or never reported. Nothing was attempted.
    Unknown,
    AlreadyOn,
    /// Windows reported the display lit this long after the wake.
    Woken(Duration),
    /// Still off when the wait ran out.
    StillOff,
}

static STATE: Mutex<Option<DisplayPower>> = Mutex::new(None);
static CHANGED: Condvar = Condvar::new();

/// The value a `PBT_POWERSETTINGCHANGE` carries for the console display, if that is what it is.
///
/// # Safety
/// For `PBT_POWERSETTINGCHANGE`, `setting` points to a `POWERBROADCAST_SETTING` whose `Data`
/// holds `DataLength` bytes, readable for the call.
unsafe fn decode(kind: u32, setting: *const c_void) -> Option<DisplayPower> {
    if kind != PBT_POWERSETTINGCHANGE || setting.is_null() {
        return None;
    }
    let s = setting.cast::<POWERBROADCAST_SETTING>();
    // SAFETY: `s` is a live POWERBROADCAST_SETTING (caller contract); both fields are read by value.
    let (guid, len) = unsafe { ((*s).PowerSetting, (*s).DataLength) };
    if guid != GUID_CONSOLE_DISPLAY_STATE || len < 4 {
        return None;
    }
    // SAFETY: `Data` holds `len` >= 4 bytes (caller contract); it is only byte-aligned.
    let raw = unsafe {
        setting
            .cast::<u8>()
            .add(offset_of!(POWERBROADCAST_SETTING, Data))
            .cast::<u32>()
            .read_unaligned()
    };
    DisplayPower::from_raw(raw)
}

unsafe extern "system" fn on_setting(
    _ctx: *const c_void,
    kind: u32,
    setting: *const c_void,
) -> u32 {
    // SAFETY: Windows calls this with the documented PDEVICE_NOTIFY_CALLBACK_ROUTINE arguments.
    if let Some(power) = unsafe { decode(kind, setting) } {
        *STATE.lock().unwrap_or_else(|p| p.into_inner()) = Some(power);
        CHANGED.notify_all();
    }
    0
}

/// Subscribe once for the process. Windows reports the current value at registration.
fn registered() -> bool {
    static DONE: OnceLock<bool> = OnceLock::new();
    *DONE.get_or_init(|| {
        // Leaked: the registration lives as long as the process.
        let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(on_setting),
            Context: std::ptr::null_mut(),
        }));
        let mut handle = std::ptr::null_mut();
        // SAFETY: the GUID is a static, `params` is leaked so it outlives the registration, and
        // `handle` is a live out-param. DEVICE_NOTIFY_CALLBACK makes `recipient` that struct.
        let rc = unsafe {
            PowerSettingRegisterNotification(
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_CALLBACK,
                HANDLE(std::ptr::from_mut(params).cast()),
                &mut handle,
            )
        };
        if rc.is_err() {
            tracing::warn!(rc = rc.0, "console display state subscription refused");
        }
        rc.is_ok()
    })
}

/// Wake a dark console display and wait up to `timeout` for Windows to report it lit. Call from
/// the console session; elsewhere the wake has no effect and this reports [`Wake::StillOff`].
pub fn ensure_on(timeout: Duration) -> Wake {
    if !registered() {
        return Wake::Unknown;
    }
    let lock = || STATE.lock().unwrap_or_else(|p| p.into_inner());
    // The first report can trail the registration by a callback dispatch.
    let (guard, _) = CHANGED
        .wait_timeout_while(lock(), Duration::from_millis(500), |s| s.is_none())
        .unwrap_or_else(|p| p.into_inner());
    match *guard {
        None => return Wake::Unknown,
        Some(DisplayPower::On) => return Wake::AlreadyOn,
        Some(DisplayPower::Off | DisplayPower::Dimmed) => {}
    }
    drop(guard);
    let started = Instant::now();
    // SAFETY: plain flag call. Without ES_CONTINUOUS it resets the display idle timer once.
    let _ = unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) };
    let (guard, _) = CHANGED
        .wait_timeout_while(lock(), timeout, |s| *s == Some(DisplayPower::Off))
        .unwrap_or_else(|p| p.into_inner());
    if *guard == Some(DisplayPower::Off) {
        Wake::StillOff
    } else {
        Wake::Woken(started.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_reads_only_the_console_display_setting() {
        let mut s = POWERBROADCAST_SETTING {
            PowerSetting: GUID_CONSOLE_DISPLAY_STATE,
            DataLength: 4,
            Data: [0],
        };
        // Data is the first byte of a little-endian u32 that spills into the struct's padding.
        let p = std::ptr::from_mut(&mut s).cast::<c_void>();
        // SAFETY: `s` is a live POWERBROADCAST_SETTING of 24 bytes; Data plus padding covers 4.
        let at = |kind, v: u32| unsafe {
            p.cast::<u8>()
                .add(offset_of!(POWERBROADCAST_SETTING, Data))
                .cast::<u32>()
                .write_unaligned(v);
            decode(kind, p)
        };
        assert_eq!(at(PBT_POWERSETTINGCHANGE, 0), Some(DisplayPower::Off));
        assert_eq!(at(PBT_POWERSETTINGCHANGE, 1), Some(DisplayPower::On));
        assert_eq!(at(PBT_POWERSETTINGCHANGE, 2), Some(DisplayPower::Dimmed));
        assert_eq!(at(PBT_POWERSETTINGCHANGE, 7), None);
        assert_eq!(at(0x000A, 1), None);
        // SAFETY: as above; the write goes through the same pointer the reads use.
        let other = unsafe {
            (*p.cast::<POWERBROADCAST_SETTING>()).PowerSetting = GUID::zeroed();
            decode(PBT_POWERSETTINGCHANGE, p)
        };
        assert_eq!(other, None);
    }
}
