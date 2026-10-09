//! Every NVENC session on the box, ours marked, out of NVML.
//!
//! NVENC has no priority and no preemption: a second encoder (NVIDIA Instant
//! Replay, OBS, Discord) shares the engine frame by frame, and on a one-engine
//! card that is the stutter. NVML lists each session with its owning pid, so a
//! log line can name the neighbour instead of guessing. Best-effort: no NVIDIA,
//! no NVML, or no permission all read as no sessions. NVML is loaded once and
//! kept; a query after that is sub-millisecond.

use std::os::raw::{c_int, c_uint, c_void};
use std::sync::OnceLock;

type NvmlDevice = *mut c_void;

const NVML_SUCCESS: c_int = 0;
const NVML_ERROR_INSUFFICIENT_SIZE: c_int = 7;

/// Absolute paths only on Windows: this runs as SYSTEM, and a bare `nvml.dll` on a box
/// without NVIDIA falls through the search order to `%PATH%`, where a user may plant one.
#[cfg(windows)]
fn lib_names() -> Vec<String> {
    vec![
        crate::install::sys32("nvml.dll"),
        r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll".to_string(),
    ]
}
#[cfg(not(windows))]
fn lib_names() -> Vec<String> {
    vec!["libnvidia-ml.so.1".into(), "libnvidia-ml.so".into()]
}

/// `nvmlEncoderSessionInfo_t`: eight `unsigned int`s, header order.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawSession {
    session_id: c_uint,
    pid: c_uint,
    vgpu_instance: c_uint,
    codec_type: c_uint,
    h_resolution: c_uint,
    v_resolution: c_uint,
    average_fps: c_uint,
    average_latency: c_uint,
}

/// Runtime NVML symbols; no link-time NVIDIA dep.
struct Nvml {
    _lib: libloading::Library,
    device_count: unsafe extern "C" fn(*mut c_uint) -> c_int,
    device_by_index: unsafe extern "C" fn(c_uint, *mut NvmlDevice) -> c_int,
    encoder_sessions: unsafe extern "C" fn(NvmlDevice, *mut c_uint, *mut RawSession) -> c_int,
}

// SAFETY: NVML is documented thread-safe and its fn pointers carry no thread affinity.
unsafe impl Send for Nvml {}
// SAFETY: a shared `&Nvml` hands out only those same fn pointers.
unsafe impl Sync for Nvml {}

impl Nvml {
    /// Load and init once. `nvmlInit` is refcounted in the driver, so the Linux clock pin's
    /// own init/shutdown pair stays independent of this one.
    fn load() -> Option<Nvml> {
        // SAFETY: each `lib.get` is a documented NVML symbol with the nvml.h signature
        // (by-value ints/pointers, no callbacks); `_lib` outlives every fn pointer.
        unsafe {
            let lib = lib_names()
                .iter()
                .find_map(|name| libloading::Library::new(name).ok())?;
            let init: unsafe extern "C" fn() -> c_int = *lib.get(b"nvmlInit_v2\0").ok()?;
            let device_count = *lib.get(b"nvmlDeviceGetCount_v2\0").ok()?;
            let device_by_index = *lib.get(b"nvmlDeviceGetHandleByIndex_v2\0").ok()?;
            let encoder_sessions = *lib.get(b"nvmlDeviceGetEncoderSessions\0").ok()?;
            if init() != NVML_SUCCESS {
                return None;
            }
            Some(Nvml {
                _lib: lib,
                device_count,
                device_by_index,
                encoder_sessions,
            })
        }
    }

    fn devices(&self) -> Vec<NvmlDevice> {
        let mut n: c_uint = 0;
        // SAFETY: `n` is a live out-param; each handle comes from the live NVML session.
        unsafe {
            if (self.device_count)(&mut n) != NVML_SUCCESS {
                return Vec::new();
            }
            (0..n)
                .filter_map(|i| {
                    let mut dev: NvmlDevice = std::ptr::null_mut();
                    ((self.device_by_index)(i, &mut dev) == NVML_SUCCESS).then_some(dev)
                })
                .collect()
        }
    }

    /// Size probe with a zero count, then the read. A session opening between the two calls
    /// returns `INSUFFICIENT_SIZE`; one retry covers it.
    fn sessions(&self, dev: NvmlDevice) -> Vec<RawSession> {
        let mut count: c_uint = 0;
        // SAFETY: `count` is a live out-param; a null buffer with count 0 is the documented
        // size probe. The buffer below is `count` elements of the exact `#[repr(C)]` struct.
        unsafe {
            let rc = (self.encoder_sessions)(dev, &mut count, std::ptr::null_mut());
            if rc != NVML_SUCCESS && rc != NVML_ERROR_INSUFFICIENT_SIZE {
                return Vec::new();
            }
            for _ in 0..2 {
                if count == 0 {
                    return Vec::new();
                }
                let mut buf = vec![RawSession::default(); count as usize];
                let rc = (self.encoder_sessions)(dev, &mut count, buf.as_mut_ptr());
                if rc == NVML_SUCCESS {
                    buf.truncate(count as usize);
                    return buf;
                }
                if rc != NVML_ERROR_INSUFFICIENT_SIZE {
                    return Vec::new();
                }
            }
            Vec::new()
        }
    }
}

fn nvml() -> Option<&'static Nvml> {
    static NVML: OnceLock<Option<Nvml>> = OnceLock::new();
    NVML.get_or_init(Nvml::load).as_ref()
}

/// One live encoder session on an NVIDIA GPU.
#[derive(Clone, Debug)]
pub struct Session {
    /// Image name (`NVIDIA Overlay.exe`, `obs64`), or `pid <n>` when unreadable.
    pub process: String,
    pub codec: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// This host's own encode: our pid, or the virtual display driver's host on Windows.
    pub ours: bool,
}

impl std::fmt::Display for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}x{}@{} {}",
            self.process, self.width, self.height, self.fps, self.codec
        )
    }
}

fn codec_name(t: c_uint) -> &'static str {
    match t {
        0 => "H.264",
        1 => "HEVC",
        2 => "AV1",
        _ => "?",
    }
}

#[cfg(windows)]
fn process_name(pid: u32) -> Option<String> {
    crate::procscan::process_image(pid)
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
}

#[cfg(target_os = "linux")]
fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_owned())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_name(_pid: u32) -> Option<String> {
    None
}

/// The virtual display driver's host encodes for us on Windows.
fn is_ours(pid: u32, name: Option<&str>) -> bool {
    pid == std::process::id()
        || name.is_some_and(|n| {
            n.eq_ignore_ascii_case("WUDFHost.exe") || n.eq_ignore_ascii_case("punktfunk-host.exe")
        })
}

/// Whether this box can answer at all: NVML loaded and at least one NVIDIA GPU.
pub fn available() -> bool {
    nvml().is_some_and(|n| !n.devices().is_empty())
}

/// Every encoder session on every NVIDIA GPU here.
pub fn list() -> Vec<Session> {
    let Some(n) = nvml() else {
        return Vec::new();
    };
    n.devices()
        .into_iter()
        .flat_map(|dev| n.sessions(dev))
        .map(|raw| {
            let name = process_name(raw.pid);
            Session {
                ours: is_ours(raw.pid, name.as_deref()),
                process: name.unwrap_or_else(|| format!("pid {}", raw.pid)),
                codec: codec_name(raw.codec_type),
                width: raw.h_resolution,
                height: raw.v_resolution,
                fps: raw.average_fps,
            }
        })
        .collect()
}

/// Sessions that are not ours.
pub fn foreign() -> Vec<Session> {
    list().into_iter().filter(|s| !s.ours).collect()
}

pub fn describe(sessions: &[Session]) -> String {
    sessions
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A stream fell behind cadence: name the neighbours on the engine, off the stream thread.
/// Silent when only our own sessions exist. Windows pauses Instant Replay from here.
pub fn on_behind_cadence() {
    let _ = std::thread::Builder::new()
        .name("punktfunk-nvenc-neighbours".into())
        .spawn(|| {
            let others = foreign();
            if others.is_empty() {
                return;
            }
            tracing::info!(
                sessions = %describe(&others),
                "NVENC engine shared with another app while behind cadence"
            );
            #[cfg(windows)]
            crate::windows::instant_replay::on_encoder_shared();
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_session_matches_the_nvml_layout() {
        assert_eq!(
            std::mem::size_of::<RawSession>(),
            8 * std::mem::size_of::<c_uint>()
        );
    }

    #[test]
    fn ours_is_our_pid_or_the_driver_host() {
        assert!(is_ours(std::process::id(), None));
        assert!(is_ours(1, Some("wudfhost.exe")));
        assert!(!is_ours(1, Some("NVIDIA Overlay.exe")));
        assert!(!is_ours(1, None));
    }

    #[test]
    fn describe_names_each_session() {
        let s = Session {
            process: "NVIDIA Overlay.exe".into(),
            codec: "HEVC",
            width: 2560,
            height: 1440,
            fps: 60,
            ours: false,
        };
        assert_eq!(describe(&[s]), "NVIDIA Overlay.exe 2560x1440@60 HEVC");
    }
}
