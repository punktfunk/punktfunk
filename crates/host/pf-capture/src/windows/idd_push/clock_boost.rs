//! NVIDIA's low-latency boost, held from the host while an NVENC session is live: a request
//! that the card keep its clocks up. A stream alone is a light load, so the card sits in its
//! idle clocks and every frame's conversion and encode run two to three times slower.
//!
//! The request is `NvAPI_D3D_SetSleepMode` with `bLowLatencyBoost` alone: no frame limiter, no
//! sleep calls. It reaches the whole GPU and takes effect at once in both directions. It is
//! made here because NVIDIA's own profile for `WUDFHost.exe` refuses the call in the driver.
//! The host keeps one idle device per adapter for the life of the process, since the NVIDIA
//! runtime never gives back what creating one cost, and sets the flag only while a session
//! holds a [`ClockBoost`]. `nvapi64.dll` resolves at runtime.
//!
//! Off unless `PUNKTFUNK_NVENC_CLOCK_BOOST=1`: full clocks draw power for as long as someone
//! is connected, and that is the host owner's choice to make.

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use windows::core::{s, w, Interface};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// `NV_SET_SLEEP_MODE_PARAMS_V1` (nvapi.h): 44 bytes, `NvBool` is one byte.
#[repr(C)]
#[derive(Default)]
struct SleepMode {
    version: u32,
    low_latency: u8,
    boost: u8,
    min_interval_us: u32,
    use_markers: u8,
    use_min_queue: u8,
    rsvd: [u8; 30],
}

const _: () = assert!(size_of::<SleepMode>() == 44);

/// `MAKE_NVAPI_VERSION(NV_SET_SLEEP_MODE_PARAMS_V1, 1)`.
const SLEEP_MODE_VER1: u32 = size_of::<SleepMode>() as u32 | (1 << 16);
/// `nvapi_interface.h` ids for `NvAPI_Initialize` and `NvAPI_D3D_SetSleepMode`.
const ID_INITIALIZE: u32 = 0x0150_E828;
const ID_SET_SLEEP_MODE: u32 = 0xAC1C_A9E0;

type SetSleepMode = unsafe extern "C" fn(*mut c_void, *mut SleepMode) -> i32;

/// `NvAPI_D3D_SetSleepMode`, resolved and initialised once per process. `None` = the driver
/// has no NVAPI or one that predates the call; said once, since NVENC opened without it.
fn set_sleep_mode() -> Option<SetSleepMode> {
    static API: OnceLock<Option<SetSleepMode>> = OnceLock::new();
    *API.get_or_init(|| {
        let api = load();
        if let Err(why) = &api {
            tracing::warn!(why, "NVIDIA clock boost unavailable");
        }
        api.ok()
    })
}

fn load() -> Result<SetSleepMode, String> {
    // SAFETY: `nvapi_QueryInterface` is the DLL's one export and hands out its entry points
    // by id, null for one it lacks; both are checked before the transmutes, whose signatures
    // are the header's. The library is never unloaded.
    unsafe {
        let dll = LoadLibraryW(w!("nvapi64.dll")).map_err(|e| format!("nvapi64.dll: {e}"))?;
        let query = GetProcAddress(dll, s!("nvapi_QueryInterface"))
            .ok_or("nvapi64.dll exports no nvapi_QueryInterface")?;
        let query: unsafe extern "C" fn(u32) -> *mut c_void = std::mem::transmute(query);
        let (init, set) = (query(ID_INITIALIZE), query(ID_SET_SLEEP_MODE));
        if init.is_null() || set.is_null() {
            return Err("this NVAPI has no sleep-mode call".into());
        }
        let init: unsafe extern "C" fn() -> i32 = std::mem::transmute(init);
        match init() {
            0 => Ok(std::mem::transmute::<*mut c_void, SetSleepMode>(set)),
            code => Err(format!("NvAPI_Initialize: {code}")),
        }
    }
}

/// One adapter's idle device and the sessions holding its boost.
struct Held {
    luid: (u32, i32),
    device: ID3D11Device,
    sessions: u32,
}

// SAFETY: a D3D11 device is free-threaded, and this one is only ever passed to NVAPI under
// the `HELD` lock.
unsafe impl Send for Held {}

static HELD: Mutex<Vec<Held>> = Mutex::new(Vec::new());

/// The NVAPI status of the request; `0` took.
fn apply(set: SetSleepMode, device: &ID3D11Device, on: bool) -> i32 {
    let mut mode = SleepMode {
        version: SLEEP_MODE_VER1,
        boost: u8::from(on),
        ..Default::default()
    };
    // SAFETY: `device` is a live D3D11 device for the call and `mode` a version-set local.
    unsafe { set(device.as_raw(), &mut mode) }
}

/// Full clocks on one adapter until dropped.
pub(super) struct ClockBoost((u32, i32));

impl ClockBoost {
    /// Hold the boost on the adapter with `luid`. `None` unless `PUNKTFUNK_NVENC_CLOCK_BOOST`
    /// asks for it, and when the box has no NVAPI or the driver declines.
    pub(super) fn hold(luid: LUID) -> Option<Self> {
        if pf_host_config::env_on("PUNKTFUNK_NVENC_CLOCK_BOOST") != Some(true) {
            return None;
        }
        let set = set_sleep_mode()?;
        let key = (luid.LowPart, luid.HighPart);
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let pos = match held.iter().position(|h| h.luid == key) {
            Some(pos) => pos,
            None => {
                let device = pf_frame::dxgi::probe_device(Some(luid), Default::default())?;
                held.push(Held {
                    luid: key,
                    device,
                    sessions: 0,
                });
                held.len() - 1
            }
        };
        let h = &mut held[pos];
        if h.sessions == 0 {
            let code = apply(set, &h.device, true);
            if code != 0 {
                tracing::warn!(code, "NVIDIA clock boost declined by the driver");
                return None;
            }
            tracing::info!("NVIDIA clock boost on while this session encodes");
        }
        h.sessions += 1;
        Some(Self(key))
    }
}

impl Drop for ClockBoost {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        let Some(h) = held.iter_mut().find(|h| h.luid == self.0) else {
            return;
        };
        h.sessions = h.sessions.saturating_sub(1);
        if h.sessions == 0
            && let Some(set) = set_sleep_mode()
        {
            apply(set, &h.device, false);
            tracing::info!("NVIDIA clock boost off");
        }
    }
}
