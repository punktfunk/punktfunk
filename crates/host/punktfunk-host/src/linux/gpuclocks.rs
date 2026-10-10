//! Session-scoped GPU clock floors for Linux encode latency.
//!
//! Adaptive P-states drop clocks between bursty encode frames, so the next
//! frame re-pays spin-up. [`session_pin`] arms the vendor pin on the first live
//! client (native and GameStream share one refcount) and restores idle PM when
//! the last handle drops. Off unless `PUNKTFUNK_PIN_CLOCKS=1`: the pin is
//! box-wide and wrong on battery.
//!
//! **AMD** (root via sysfs): write `high` into each amdgpu
//! `power_dpm_force_performance_level`, restore the prior value on drop.
//!
//! **NVIDIA** — two independent halves, both no-ops off NVIDIA:
//! 1. `CudaNoStablePerfLimit` profile in `~/.nv/nvidia-application-profiles-rc.d/`
//!    lifts the P2 memory-clock cap (`PUNKTFUNK_NV_PROFILE=0` opts out). Do not
//!    set `CUDA_DISABLE_PERF_BOOST`: that blocks the boost *to* P2; the profile
//!    lifts the cap *at* P2 so the process can reach P0.
//! 2. `nvmlDeviceSetGpuLockedClocks(TDP, UNLIMITED)` floors the core at TDP
//!    and leaves boost. Reset-before-pin heals a stale lock from a crash.

use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::sync::{Mutex, OnceLock};

type NvmlDevice = *mut c_void;

const NVML_SUCCESS: c_int = 0;
const NVML_ERROR_NO_PERMISSION: c_int = 4;
/// `(TDP, UNLIMITED)`: floor at base, leave boost. A max pin only burns idle watts.
const NVML_CLOCK_LIMIT_ID_TDP: c_uint = 0xffff_ff01;
const NVML_CLOCK_LIMIT_ID_UNLIMITED: c_uint = 0xffff_ff02;

/// Runtime `libnvidia-ml` symbols; no link-time NVIDIA dep. Missing library is a no-op.
struct Nvml {
    _lib: libloading::Library,
    init: unsafe extern "C" fn() -> c_int,
    shutdown: unsafe extern "C" fn() -> c_int,
    device_count: unsafe extern "C" fn(*mut c_uint) -> c_int,
    device_by_index: unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> c_int,
    set_locked_clocks: unsafe extern "C" fn(NvmlDevice, c_uint, c_uint) -> c_int,
    reset_locked_clocks: unsafe extern "C" fn(NvmlDevice) -> c_int,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl Nvml {
    fn load() -> Option<Nvml> {
        // SAFETY: `Library::new` loads the NVIDIA driver (`libnvidia-ml.so.1`).
        // Each `lib.get` is a documented NVML symbol with the nvml.h signature
        // (by-value ints/pointers, no callbacks). `_lib` is stored in `Nvml`,
        // so every fn pointer outlives its uses.
        unsafe {
            let lib = libloading::Library::new("libnvidia-ml.so.1")
                .or_else(|_| libloading::Library::new("libnvidia-ml.so"))
                .ok()?;
            let init = *lib.get(b"nvmlInit_v2\0").ok()?;
            let shutdown = *lib.get(b"nvmlShutdown\0").ok()?;
            let device_count = *lib.get(b"nvmlDeviceGetCount_v2\0").ok()?;
            let device_by_index = *lib.get(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
            let set_locked_clocks = *lib.get(b"nvmlDeviceSetGpuLockedClocks\0").ok()?;
            let reset_locked_clocks = *lib.get(b"nvmlDeviceResetGpuLockedClocks\0").ok()?;
            let error_string = *lib.get(b"nvmlErrorString\0").ok()?;
            Some(Nvml {
                _lib: lib,
                init,
                shutdown,
                device_count,
                device_by_index,
                set_locked_clocks,
                reset_locked_clocks,
                error_string,
            })
        }
    }

    fn err_str(&self, r: c_int) -> String {
        // SAFETY: `nvmlErrorString` returns a pointer into NVML's static error-string table for
        // ANY input value (documented total function), valid for the process lifetime; we only
        // read it via `CStr` while the library is loaded (`self` borrows `_lib`).
        unsafe {
            let p = (self.error_string)(r);
            if p.is_null() {
                format!("NVML error {r}")
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

/// Device nodes only — no CUDA/NVML init on the probe.
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

struct NvmlPin {
    nvml: Nvml,
    pinned: Vec<NvmlDevice>,
}

struct AmdPin {
    path: std::path::PathBuf,
    restore: String,
}

/// Owned by the [`session_pin`] refcount; Drop restores vendor clocks.
pub struct ClockGuard {
    nvml: Option<NvmlPin>,
    amd: Vec<AmdPin>,
}

// SAFETY: NVML handles and fn pointers have no thread affinity (NVML is
// documented thread-safe). The guard is only moved into/out of `pin_refcount`
// and used under exclusive ownership of that mutex — never shared.
unsafe impl Send for ClockGuard {}

impl Drop for ClockGuard {
    fn drop(&mut self) {
        if let Some(pin) = &self.nvml {
            // SAFETY: each handle in `pinned` came from `nvmlDeviceGetHandleByIndex_v2` on this
            // live NVML session (init'd in `pin_nvidia`, shut down only here, after the resets).
            // The calls take the handle by value and return an int status — no Rust memory is
            // borrowed.
            unsafe {
                for &dev in &pin.pinned {
                    let _ = (pin.nvml.reset_locked_clocks)(dev);
                }
                let _ = (pin.nvml.shutdown)();
            }
            if !pin.pinned.is_empty() {
                tracing::info!("NVIDIA clock floor released (locked clocks reset)");
            }
        }
        for pin in &self.amd {
            match std::fs::write(&pin.path, &pin.restore) {
                Ok(()) => tracing::info!(
                    card = %pin.path.display(),
                    restored = %pin.restore,
                    "amdgpu performance level restored"
                ),
                Err(e) => tracing::warn!(
                    card = %pin.path.display(),
                    error = %e,
                    "amdgpu performance level not restored"
                ),
            }
        }
    }
}

/// Install the NVIDIA P2-cap application profile. Does not arm the clock pin;
/// that is refcounted per live client via [`session_pin`].
pub fn on_host_start() {
    if nvidia_present() {
        ensure_cuda_perf_profile();
    }
}

/// One box-wide pin shared by native and GameStream: N sessions, one GPU setting.
struct PinRefcount {
    live: usize,
    /// Present iff `live > 0` and something was actually pinnable.
    guard: Option<ClockGuard>,
}

fn pin_refcount() -> &'static Mutex<PinRefcount> {
    static STATE: OnceLock<Mutex<PinRefcount>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(PinRefcount {
            live: 0,
            guard: None,
        })
    })
}

/// Holds the box-wide pin armed. Last drop restores idle downclocking.
/// No-op unless `PUNKTFUNK_PIN_CLOCKS` is on; blank or junk stays off.
pub struct SessionClockPin {
    /// False: opt-in gate off, this handle did not tick the refcount.
    counted: bool,
}

pub fn session_pin() -> SessionClockPin {
    let asked = pf_host_config::knob("PUNKTFUNK_PIN_CLOCKS")
        .and_then(|v| pf_host_config::registry::parse_bool(&v.trim().to_ascii_lowercase()));
    if asked != Some(true) {
        return SessionClockPin { counted: false };
    }
    let mut state = pin_refcount().lock().unwrap();
    state.live += 1;
    if state.live == 1 {
        // 0→1: arm. `pin_nvidia` resets first so a crash leftover is not compounded.
        let nvml = if nvidia_present() { pin_nvidia() } else { None };
        let amd = pin_amdgpu();
        state.guard = if nvml.is_none() && amd.is_empty() {
            None
        } else {
            Some(ClockGuard { nvml, amd })
        };
    }
    SessionClockPin { counted: true }
}

impl Drop for SessionClockPin {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        // Drop the guard outside the mutex: release does NVML + sysfs I/O.
        let release = {
            let mut state = pin_refcount().lock().unwrap();
            state.live = state.live.saturating_sub(1);
            if state.live == 0 {
                state.guard.take()
            } else {
                None
            }
        };
        drop(release);
    }
}

/// Write `high` to each amdgpu `power_dpm_force_performance_level`; remember the prior
/// value for Drop. Root-gated by sysfs; non-root warns once and streams unpinned.
fn pin_amdgpu() -> Vec<AmdPin> {
    let mut pins = Vec::new();
    let mut denied = false;
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return pins;
    };
    for entry in cards.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        // `cardN` only — skip connectors (`card0-DP-1`) and render nodes.
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let dev = entry.path().join("device");
        let is_amdgpu = std::fs::read_link(dev.join("driver"))
            .map(|t| t.to_string_lossy().ends_with("amdgpu"))
            .unwrap_or(false);
        if !is_amdgpu {
            continue;
        }
        let path = dev.join("power_dpm_force_performance_level");
        let Ok(prev) = std::fs::read_to_string(&path) else {
            continue;
        };
        let prev = prev.trim().to_string();
        if prev == "high" {
            continue; // already `high`; do not take restore ownership
        }
        match std::fs::write(&path, "high") {
            Ok(()) => {
                tracing::info!(
                    card = %name,
                    was = %prev,
                    "amdgpu performance level pinned to high (encode clock sag removed) — \
                     restored when the last client disconnects"
                );
                pins.push(AmdPin {
                    path,
                    restore: prev,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
            Err(e) => tracing::debug!(card = %name, error = %e, "amdgpu perf-level write failed"),
        }
    }
    if denied {
        tracing::warn!(
            "PUNKTFUNK_PIN_CLOCKS: writing power_dpm_force_performance_level requires root — \
             grant it via a boot oneshot / udev rule chowning the attribute, or run the host as \
             a system service. The host keeps running unpinned"
        );
    }
    pins
}

/// Floor core at TDP/base. Reset first so a crash leftover is replaced, not stacked.
fn pin_nvidia() -> Option<NvmlPin> {
    let nvml = match Nvml::load() {
        Some(n) => n,
        None => {
            tracing::warn!("PUNKTFUNK_PIN_CLOCKS: libnvidia-ml not loadable — clocks not pinned");
            return None;
        }
    };
    // SAFETY: all calls follow the documented NVML lifecycle on the successfully-loaded library:
    // `nvmlInit_v2` first (status-checked; on failure we return without touching anything else),
    // then count/handle queries writing through valid `&mut` out-pointers of the exact C types,
    // then set/reset taking those returned handles by value. `shutdown` is called on every path
    // that does not hand the session to a `ClockGuard` (whose Drop shuts it down).
    unsafe {
        let r = (nvml.init)();
        if r != NVML_SUCCESS {
            tracing::warn!(
                error = nvml.err_str(r),
                "PUNKTFUNK_PIN_CLOCKS: NVML init failed — clocks not pinned"
            );
            return None;
        }
        let mut count: c_uint = 0;
        if (nvml.device_count)(&mut count) != NVML_SUCCESS || count == 0 {
            let _ = (nvml.shutdown)();
            return None;
        }
        let mut pinned = Vec::new();
        let mut denied = false;
        for i in 0..count {
            let mut dev: NvmlDevice = std::ptr::null_mut();
            if (nvml.device_by_index)(i, &mut dev) != NVML_SUCCESS {
                continue;
            }
            let _ = (nvml.reset_locked_clocks)(dev);
            let r = (nvml.set_locked_clocks)(
                dev,
                NVML_CLOCK_LIMIT_ID_TDP,
                NVML_CLOCK_LIMIT_ID_UNLIMITED,
            );
            match r {
                NVML_SUCCESS => pinned.push(dev),
                NVML_ERROR_NO_PERMISSION => denied = true,
                _ => tracing::debug!(
                    device = i,
                    error = nvml.err_str(r),
                    "SetGpuLockedClocks failed"
                ),
            }
        }
        if denied {
            tracing::warn!(
                "PUNKTFUNK_PIN_CLOCKS: the driver requires root for locked clocks \
                 (NVML_ERROR_NO_PERMISSION). Grant it via a boot oneshot (`nvidia-smi -lgc \
                 tdp,unlimited`) or sudoers (`<user> ALL=(ALL) NOPASSWD: /usr/bin/nvidia-smi`) — \
                 the host keeps running unpinned"
            );
        }
        if pinned.is_empty() {
            let _ = (nvml.shutdown)();
            return None;
        }
        tracing::info!(
            devices = pinned.len(),
            "NVIDIA core-clock floor armed (min=TDP/base, max=boost) — released when the last \
             client disconnects"
        );
        Some(NvmlPin { nvml, pinned })
    }
}

/// Drop a `punktfunk-host` `CudaNoStablePerfLimit` rule into
/// `~/.nv/nvidia-application-profiles-rc.d/`. Never overwrite: the file is the
/// operator's once it exists.
fn ensure_cuda_perf_profile() {
    if pf_host_config::env_on("PUNKTFUNK_NV_PROFILE") == Some(false) {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home)
        .join(".nv")
        .join("nvidia-application-profiles-rc.d");
    let path = dir.join("50-punktfunk");
    if path.exists() {
        return;
    }
    // Inline profile (not a named-profile reference) so pre-R595 drivers load it too.
    let profile = r#"{
    "profiles": [ { "name": "CudaNoStablePerfLimit", "settings": [ "0x166c5e", 0 ] } ],
    "rules": [
        { "pattern": { "feature": "procname", "matches": "punktfunk-host" }, "profile": "CudaNoStablePerfLimit" }
    ]
}
"#;
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, profile)
    };
    match write() {
        Ok(()) => tracing::info!(
            path = %path.display(),
            "installed the CudaNoStablePerfLimit driver profile (lifts the P2 memory-clock cap \
             for NVENC/CUDA; read when the driver next initializes — PUNKTFUNK_NV_PROFILE=0 opts \
             out)"
        ),
        Err(e) => tracing::debug!(error = %e, "NVIDIA application profile not installed"),
    }
}
