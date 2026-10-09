//! GPU inventory and operator GPU preference for multi-GPU hosts.
//!
//! Three concerns, one crate:
//! - **Enumeration** ([`enumerate`]): hardware GPUs. DXGI adapters on Windows
//!   (WARP/Basic-Render and IddCx/display-only adapters dropped — an IddCx
//!   adapter mirrors its render GPU's DXGI identity and would list every GPU
//!   twice). `/dev/dri/renderD*` + sysfs PCI ids on Linux. Empty elsewhere so
//!   management endpoints stay identical.
//! - **Preference** ([`prefs`]): auto/manual in `<config>/gpu-settings.json`.
//!   Manual is stored by stable identity (PCI vendor:device + occurrence +
//!   name), not LUID (reassigned every boot) or adapter index.
//! - **Selection** ([`selected_gpu`] / [`pick`]): inventory + preference +
//!   `PUNKTFUNK_RENDER_ADAPTER` → render/encode GPU. Precedence: **manual >
//!   env substring > auto (max dedicated VRAM)**. A vanished preferred GPU
//!   logs and auto-selects so the host keeps streaming.
//!
//! A preference change applies to the **next session**. [`session_begin`] /
//! [`active`] record which GPU a live session encodes on.

// Unsafe-proof program: every `unsafe {}` in this leaf carries a `// SAFETY:` proof.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// PCI vendor ids the encode backends know (NVENC / AMF / QSV / VAAPI).
pub const VENDOR_NVIDIA: u32 = 0x10DE;
pub const VENDOR_AMD: u32 = 0x1002;
pub const VENDOR_INTEL: u32 = 0x8086;
/// Adreno, which DXGI reports as the FourCC `"QCOM"` rather than a PCI id; `0x5143` is
/// the PCI-SIG assignment the same driver uses elsewhere. Both mean Media Foundation is
/// the only hardware encoder on the adapter.
pub const VENDOR_QUALCOMM: u32 = 0x4D4F_4351;
pub const VENDOR_QUALCOMM_PCI: u32 = 0x5143;

/// How the pipeline addresses a GPU. Not stable identity: Windows LUIDs are
/// per-boot; a render node can renumber across kernel updates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuHandle {
    /// DXGI `AdapterLuid` (this boot only).
    #[cfg(target_os = "windows")]
    pub luid_low: u32,
    #[cfg(target_os = "windows")]
    pub luid_high: i32,
    /// DRM render node (`/dev/dri/renderD*`).
    #[cfg(target_os = "linux")]
    pub render_node: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct GpuInfo {
    /// `"{vendor:04x}-{device:04x}-{occurrence}"`. Occurrence disambiguates
    /// identical cards by inventory order among twins.
    pub id: String,
    /// DXGI description (Windows) / vendor label + node (Linux).
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Index among GPUs with the same (vendor_id, device_id).
    pub occurrence: u32,
    /// Dedicated VRAM in bytes. 0 where the platform does not expose it
    /// (non-amdgpu Linux sysfs).
    pub vram_bytes: u64,
    pub handle: GpuHandle,
}

pub fn vendor_tag(vendor_id: u32) -> &'static str {
    match vendor_id {
        VENDOR_NVIDIA => "nvidia",
        VENDOR_AMD => "amd",
        VENDOR_INTEL => "intel",
        _ => "other",
    }
}

impl GpuInfo {
    pub fn vendor_tag(&self) -> &'static str {
        vendor_tag(self.vendor_id)
    }

    #[cfg(target_os = "windows")]
    pub fn luid(&self) -> windows::Win32::Foundation::LUID {
        windows::Win32::Foundation::LUID {
            LowPart: self.handle.luid_low,
            HighPart: self.handle.luid_high,
        }
    }
}

/// Fill `id` + `occurrence` (index among same-(vendor,device) twins). Windows
/// sorts by LUID first so twin numbers stay stable for the boot.
// Linux/Windows `enumerate()` only; other targets' stub leaves this unused.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn assign_ids(gpus: &mut [GpuInfo]) {
    for i in 0..gpus.len() {
        let occ = gpus[..i]
            .iter()
            .filter(|g| g.vendor_id == gpus[i].vendor_id && g.device_id == gpus[i].device_id)
            .count() as u32;
        gpus[i].occurrence = occ;
        gpus[i].id = format!(
            "{:04x}-{:04x}-{}",
            gpus[i].vendor_id, gpus[i].device_id, occ
        );
    }
}

// --- Enumeration ---

/// [`D3DKMT_ADAPTERTYPE`] bits that never belong in inventory. Pure bit math
/// on every platform so tests can use captured words; only Windows
/// [`enumerate`] consumes it at runtime.
///
/// [`D3DKMT_ADAPTERTYPE`]: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmdt/ns-d3dkmdt-_d3dkmt_adaptertype
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
mod adapter_type {
    /// `RenderSupported` — clear on display-only adapters (an IDD ghost has no render engine).
    const RENDER_SUPPORTED: u32 = 1 << 0;
    /// `SoftwareDevice` — WARP/Basic Render (normally already dropped via the DXGI flag).
    const SOFTWARE_DEVICE: u32 = 1 << 2;
    /// `IndirectDisplayDevice` — an IddCx virtual-display adapter.
    const INDIRECT_DISPLAY_DEVICE: u32 = 1 << 6;

    pub fn hidden(bits: u32) -> bool {
        bits & INDIRECT_DISPLAY_DEVICE != 0
            || bits & SOFTWARE_DEVICE != 0
            || bits & RENDER_SUPPORTED == 0
    }
}

/// `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERTYPE)` via raw gdi32 FFI — the
/// `windows` crate's `Wdk_*` bindings aren't enabled.
#[cfg(target_os = "windows")]
mod kmt {
    /// `KMTQUERYADAPTERINFOTYPE::KMTQAITYPE_ADAPTERTYPE` (d3dkmdt.h).
    const KMTQAITYPE_ADAPTERTYPE: u32 = 15;

    /// `D3DKMT_OPENADAPTERFROMLUID`: LUID in, kernel adapter handle out.
    #[repr(C)]
    struct OpenAdapterFromLuid {
        luid_low: u32,
        luid_high: i32,
        adapter: u32,
    }
    /// `D3DKMT_QUERYADAPTERINFO`.
    #[repr(C)]
    struct QueryAdapterInfo {
        adapter: u32,
        ty: u32,
        private_data: *mut core::ffi::c_void,
        private_data_size: u32,
    }
    /// `D3DKMT_CLOSEADAPTER`.
    #[repr(C)]
    struct CloseAdapter {
        adapter: u32,
    }

    #[link(name = "gdi32")]
    unsafe extern "system" {
        fn D3DKMTOpenAdapterFromLuid(arg: *mut OpenAdapterFromLuid) -> i32;
        fn D3DKMTQueryAdapterInfo(arg: *mut QueryAdapterInfo) -> i32;
        fn D3DKMTCloseAdapter(arg: *mut CloseAdapter) -> i32;
    }

    /// `D3DKMT_ADAPTERTYPE` bits for this LUID, `None` on query failure. Callers
    /// fail open — a listed twin beats a hidden real GPU.
    pub fn adapter_type_bits(luid_low: u32, luid_high: i32) -> Option<u32> {
        // SAFETY: every pointer handed to the three D3DKMT calls addresses a stack local that
        // outlives the call; NTSTATUS >= 0 is success. The kernel handle is closed on every
        // path that opened it, including a failed query.
        unsafe {
            let mut open = OpenAdapterFromLuid {
                luid_low,
                luid_high,
                adapter: 0,
            };
            if D3DKMTOpenAdapterFromLuid(&mut open) < 0 {
                return None;
            }
            let mut bits: u32 = 0;
            let mut query = QueryAdapterInfo {
                adapter: open.adapter,
                ty: KMTQAITYPE_ADAPTERTYPE,
                private_data: (&mut bits as *mut u32).cast(),
                private_data_size: size_of::<u32>() as u32,
            };
            let status = D3DKMTQueryAdapterInfo(&mut query);
            let mut close = CloseAdapter {
                adapter: open.adapter,
            };
            let _ = D3DKMTCloseAdapter(&mut close);
            (status >= 0).then_some(bits)
        }
    }
}

/// DXGI adapters minus WARP/Basic-Render and minus IddCx/display-only
/// adapters (ghost-twin filter below). Linux and other targets have their
/// own [`enumerate`] arms.
#[cfg(target_os = "windows")]
pub fn enumerate() -> Vec<GpuInfo> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    let mut out = Vec::new();
    // SAFETY: `CreateDXGIFactory1` returns an owned COM factory (Released when the local drops) or
    // errs (→ empty inventory). `EnumAdapters1(i)` yields owned `IDXGIAdapter1`s until it errors
    // past the last adapter; `GetDesc1()` returns the descriptor by value. Only locals are touched,
    // nothing escapes, no raw pointer is dereferenced.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = adapter.GetDesc1() else { continue };
            if (d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&d.Description)
                .trim_end_matches('\u{0}')
                .to_string();
            let lname = name.to_ascii_lowercase();
            if lname.contains("basic render") || lname.contains("warp") {
                continue;
            }
            // IddCx adapters clone the render GPU's DXGI identity (name, PCI
            // ids, VRAM); only LUID differs. Kernel adapter type is the
            // discriminator. Fail open: a missed query must not hide a real GPU.
            match kmt::adapter_type_bits(d.AdapterLuid.LowPart, d.AdapterLuid.HighPart) {
                Some(bits) if adapter_type::hidden(bits) => {
                    tracing::debug!(
                        name = %name,
                        bits = %format!("{bits:#x}"),
                        "skipping non-render DXGI adapter (indirect display / display-only / software)"
                    );
                    continue;
                }
                _ => {}
            }
            out.push(GpuInfo {
                id: String::new(),
                name,
                vendor_id: d.VendorId,
                device_id: d.DeviceId,
                occurrence: 0,
                vram_bytes: d.DedicatedVideoMemory as u64,
                handle: GpuHandle {
                    luid_low: d.AdapterLuid.LowPart,
                    luid_high: d.AdapterLuid.HighPart,
                },
            });
        }
    }
    // DXGI lists the primary-display owner first; exclusive mode moves the
    // primary mid-session and would reshuffle twins. LUIDs are boot-stable.
    out.sort_by_key(|g| ((g.handle.luid_high as i64) << 32) | g.handle.luid_low as i64);
    assign_ids(&mut out);
    out
}

#[cfg(target_os = "linux")]
pub fn enumerate() -> Vec<GpuInfo> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("renderD"))
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    let mut out = Vec::new();
    for node in nodes {
        let sys = format!("/sys/class/drm/{node}/device");
        let read_hex = |f: &str| -> u32 {
            std::fs::read_to_string(format!("{sys}/{f}"))
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
                .unwrap_or(0)
        };
        let vendor_id = read_hex("vendor");
        let device_id = read_hex("device");
        // Only amdgpu exposes VRAM in sysfs; 0 elsewhere. Linux auto-select
        // is NVIDIA-presence + render node, not VRAM.
        let vram_bytes = std::fs::read_to_string(format!("{sys}/mem_info_vram_total"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let vendor_label = match vendor_id {
            VENDOR_NVIDIA => "NVIDIA".to_string(),
            VENDOR_AMD => "AMD".to_string(),
            VENDOR_INTEL => "Intel".to_string(),
            other => format!("GPU 0x{other:04x}"),
        };
        out.push(GpuInfo {
            id: String::new(),
            name: format!("{vendor_label} GPU ({node})"),
            vendor_id,
            device_id,
            occurrence: 0,
            vram_bytes,
            handle: GpuHandle {
                render_node: Some(PathBuf::from(format!("/dev/dri/{node}"))),
            },
        });
    }
    assign_ids(&mut out);
    out
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn enumerate() -> Vec<GpuInfo> {
    Vec::new()
}

// --- Persisted preference ---

/// Auto: env substring, else max VRAM. Manual: GPU chosen in the web console.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
    #[default]
    Auto,
    Manual,
}

/// Stable identity of the manually preferred GPU (not LUID/index; see [`GpuInfo::id`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreferredGpu {
    pub vendor_id: u32,
    pub device_id: u32,
    #[serde(default)]
    pub occurrence: u32,
    /// Name at selection time: last-resort matcher, and the label when the GPU is absent.
    #[serde(default)]
    pub name: String,
}

/// Persisted GPU preference (`<config>/gpu-settings.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuPreference {
    #[serde(default)]
    pub mode: GpuMode,
    /// `Some` when `mode == Manual`. Kept across Auto so the console can restore the last pick.
    #[serde(default)]
    pub gpu: Option<PreferredGpu>,
}

/// Preference store: in-memory value + JSON file. Private dir, temp write +
/// atomic rename; in-memory rollback if the disk write fails.
pub struct GpuPrefStore {
    path: PathBuf,
    cur: Mutex<GpuPreference>,
}

impl GpuPrefStore {
    /// Missing/corrupt file ⇒ Auto. Never fail host startup over settings.
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<GpuPreference>(&bytes) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "gpu-settings.json unreadable — using default (Auto)");
                    GpuPreference::default()
                }
            },
            Err(_) => GpuPreference::default(),
        };
        GpuPrefStore {
            path,
            cur: Mutex::new(cur),
        }
    }

    pub fn get(&self) -> GpuPreference {
        self.cur.lock().unwrap().clone()
    }

    /// Persist then apply. Memory changes only after the disk write succeeds.
    pub fn set(&self, pref: GpuPreference) -> Result<()> {
        pf_paths::replace_secret_file(&self.path, &serde_json::to_vec_pretty(&pref)?)?;
        *self.cur.lock().unwrap() = pref;
        Ok(())
    }
}

/// Process-wide preference store, loaded once. Selection happens inside
/// capture/encode setup where no app state is threaded.
pub fn prefs() -> &'static GpuPrefStore {
    static STORE: OnceLock<GpuPrefStore> = OnceLock::new();
    STORE.get_or_init(|| GpuPrefStore::load_from(pf_paths::config_dir().join("gpu-settings.json")))
}

// --- Selection ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickSource {
    Preference,
    Env,
    /// Max dedicated VRAM (Windows) / platform default (Linux display).
    Auto,
    /// Manual preference set but that GPU is absent; fell back to auto.
    PreferenceMissing,
}

impl PickSource {
    pub fn tag(self) -> &'static str {
        match self {
            PickSource::Preference => "preference",
            PickSource::Env => "env",
            PickSource::Auto => "auto",
            PickSource::PreferenceMissing => "preference_missing",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SelectedGpu {
    pub info: GpuInfo,
    pub source: PickSource,
}

/// Match the preferred GPU: exact (vendor, device, occurrence), then same
/// model (a twin renumbered), then exact name (ids changed, marketing name
/// survived).
pub fn find_preferred(gpus: &[GpuInfo], want: &PreferredGpu) -> Option<usize> {
    gpus.iter()
        .position(|g| {
            g.vendor_id == want.vendor_id
                && g.device_id == want.device_id
                && g.occurrence == want.occurrence
        })
        .or_else(|| {
            gpus.iter()
                .position(|g| g.vendor_id == want.vendor_id && g.device_id == want.device_id)
        })
        .or_else(|| {
            if want.name.is_empty() {
                return None;
            }
            gpus.iter().position(|g| g.name == want.name)
        })
}

/// Pure selection: **manual preference > env substring > max VRAM**. `None`
/// only when `gpus` is empty. An unmatched env substring falls through to
/// max-VRAM (same as unset) instead of returning no adapter.
pub fn pick(
    gpus: &[GpuInfo],
    pref: &GpuPreference,
    env_substr: Option<&str>,
) -> Option<(usize, PickSource)> {
    let mut preference_missing = false;
    if pref.mode == GpuMode::Manual
        && let Some(want) = &pref.gpu
    {
        match find_preferred(gpus, want) {
            Some(i) => return Some((i, PickSource::Preference)),
            None => preference_missing = true,
        }
    }
    if let Some(sub) = env_substr.filter(|s| !s.is_empty()) {
        let sub = sub.to_ascii_lowercase();
        if let Some(i) = gpus
            .iter()
            .position(|g| g.name.to_ascii_lowercase().contains(&sub))
        {
            return Some((i, PickSource::Env));
        }
    }
    let i = gpus
        .iter()
        .enumerate()
        .max_by_key(|(_, g)| g.vram_bytes)
        .map(|(i, _)| i)?;
    Some((
        i,
        if preference_missing {
            PickSource::PreferenceMissing
        } else {
            PickSource::Auto
        },
    ))
}

/// Capture (`resolve_render_adapter_luid`) and encoder-vendor dispatch both
/// consume this, so they agree. Pure query — callers log (this runs per
/// serverinfo poll).
#[cfg(target_os = "windows")]
pub fn selected_gpu() -> Option<SelectedGpu> {
    let gpus = enumerate();
    let pref = prefs().get();
    let env = pf_host_config::config()
        .render_adapter
        .clone()
        .filter(|s| !s.is_empty());
    let (i, source) = pick(&gpus, &pref, env.as_deref())?;
    Some(SelectedGpu {
        info: gpus.into_iter().nth(i)?,
        source,
    })
}

/// Manual preference wins; else NVIDIA-presence → NVIDIA, else the VAAPI
/// node owner. Encode dispatch in `encode::open_video` / [`linux_render_node`]
/// is authoritative; this is the console's view.
#[cfg(target_os = "linux")]
pub fn selected_gpu() -> Option<SelectedGpu> {
    let gpus = enumerate();
    let pref = prefs().get();
    let mut preference_missing = false;
    if pref.mode == GpuMode::Manual
        && let Some(want) = &pref.gpu
    {
        match find_preferred(&gpus, want) {
            Some(i) => {
                return Some(SelectedGpu {
                    info: gpus.into_iter().nth(i)?,
                    source: PickSource::Preference,
                });
            }
            None => preference_missing = true,
        }
    }
    let source = if preference_missing {
        PickSource::PreferenceMissing
    } else {
        PickSource::Auto
    };
    if linux_nvidia_present()
        && let Some(i) = gpus.iter().position(|g| g.vendor_id == VENDOR_NVIDIA)
    {
        return Some(SelectedGpu {
            info: gpus.into_iter().nth(i)?,
            source,
        });
    }
    let node = linux_render_node();
    let i = gpus
        .iter()
        .position(|g| g.handle.render_node.as_deref() == Some(node.as_path()))
        .unwrap_or(0);
    Some(SelectedGpu {
        info: gpus.into_iter().nth(i)?,
        source,
    })
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn selected_gpu() -> Option<SelectedGpu> {
    None
}

/// Linux encode dispatch consults this; auto keeps NVIDIA-presence behavior.
/// None unless `mode == Manual` and that GPU is present.
pub fn manual_selection() -> Option<GpuInfo> {
    let pref = prefs().get();
    if pref.mode != GpuMode::Manual {
        return None;
    }
    let want = pref.gpu?;
    let gpus = enumerate();
    let i = find_preferred(&gpus, &want)?;
    gpus.into_iter().nth(i)
}

/// Trimmed `PUNKTFUNK_RENDER_NODE` override; empty = unset. Parsed here so
/// every call site agrees on trim. Callers that must not consult the console
/// preference (PyroWave's oracle: no oracle may change ITS device) read this
/// directly; [`linux_render_node`] layers the preference on top.
#[cfg(target_os = "linux")]
pub fn render_node_env() -> Option<PathBuf> {
    std::env::var("PUNKTFUNK_RENDER_NODE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Live env read of `PUNKTFUNK_RENDER_NODE` after manual preference (see `config.rs`).
#[cfg(target_os = "linux")]
pub fn linux_render_node() -> PathBuf {
    if let Some(g) = manual_selection()
        && let Some(node) = g.handle.render_node
    {
        return node;
    }
    render_node_env().unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"))
}

/// Same device-node check as `encode::nvidia_present`, duplicated rather
/// than widening that private fn.
#[cfg(target_os = "linux")]
fn linux_nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// Cache key for per-GPU probe caches (`can_encode_444`, `windows_codec_support`).
/// Changes when the *selection* changes, preference edits included.
pub fn selection_key() -> String {
    match selected_gpu() {
        Some(sel) => {
            #[cfg(target_os = "windows")]
            {
                format!(
                    "{}:{:08x}{:08x}",
                    sel.info.id, sel.info.handle.luid_high as u32, sel.info.handle.luid_low
                )
            }
            #[cfg(not(target_os = "windows"))]
            {
                sel.info.id
            }
        }
        None => String::new(),
    }
}

// --- Live "in use" record ---

#[derive(Clone, Debug)]
pub struct ActiveGpu {
    /// [`GpuInfo::id`]; empty for the CPU/software path.
    pub id: String,
    pub name: String,
    pub vendor_id: u32,
    pub backend: &'static str,
}

struct ActiveState {
    gpu: ActiveGpu,
    sessions: u32,
}

static ACTIVE: Mutex<Option<ActiveState>> = Mutex::new(None);

/// RAII marker for one live encode session. Held by the encoder wrapper
/// `open_video` returns, so every successful open is paired with a drop.
pub struct ActiveSession(());

impl Drop for ActiveSession {
    fn drop(&mut self) {
        let mut st = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = st.as_mut() {
            st.sessions = st.sessions.saturating_sub(1);
        }
    }
}

/// Record a session opening on `gpu`. Concurrent sessions share one GPU;
/// the latest record wins and a counter tracks liveness.
pub fn session_begin(gpu: ActiveGpu) -> ActiveSession {
    let mut st = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let sessions = st.as_ref().map(|s| s.sessions).unwrap_or(0) + 1;
    *st = Some(ActiveState { gpu, sessions });
    ActiveSession(())
}

/// Live encode GPU + session count. `sessions == 0` means last-used, idle.
pub fn active() -> Option<(ActiveGpu, u32)> {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|s| (s.gpu.clone(), s.sessions))
}

/// Render-GPU LUID for the Windows pipeline: the IDD-push ring, the IddCx
/// `SET_RENDER_ADAPTER` pin, and NVENC/AMF/QSV (via the captured frame's
/// device) all follow [`selected_gpu`]. A missing preference falls back to
/// auto with a warning — never `None` — so a stale pick cannot stop streaming.
///
/// Lives here so capture and encode depend on GPU selection as a peer, not
/// via the orchestrator.
#[cfg(target_os = "windows")]
pub fn resolve_render_adapter_luid() -> Option<windows::Win32::Foundation::LUID> {
    match selected_gpu() {
        Some(sel) => {
            tracing::info!(
                adapter = sel.info.name,
                vram_mb = sel.info.vram_bytes / (1024 * 1024),
                source = sel.source.tag(),
                "render adapter selected"
            );
            if sel.source == PickSource::PreferenceMissing {
                tracing::warn!(
                    "the preferred GPU is not present — auto-selected the adapter above \
                     (fix or clear the preference in the web console)"
                );
            }
            Some(sel.info.luid())
        }
        None => {
            tracing::warn!("no suitable render adapter found for SET_RENDER_ADAPTER");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(vendor: u32, device: u32, name: &str, vram_gb: u64) -> GpuInfo {
        GpuInfo {
            id: String::new(),
            name: name.into(),
            vendor_id: vendor,
            device_id: device,
            occurrence: 0,
            vram_bytes: vram_gb * 1024 * 1024 * 1024,
            handle: GpuHandle::default(),
        }
    }

    fn hybrid() -> Vec<GpuInfo> {
        let mut v = vec![
            gpu(VENDOR_INTEL, 0x7d55, "Intel(R) Arc(TM) Graphics", 0),
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
        ];
        assign_ids(&mut v);
        v
    }

    fn manual(vendor: u32, device: u32, occurrence: u32, name: &str) -> GpuPreference {
        GpuPreference {
            mode: GpuMode::Manual,
            gpu: Some(PreferredGpu {
                vendor_id: vendor,
                device_id: device,
                occurrence,
                name: name.into(),
            }),
        }
    }

    #[test]
    fn auto_picks_max_vram() {
        let (i, src) = pick(&hybrid(), &GpuPreference::default(), None).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Auto);
    }

    #[test]
    fn manual_preference_beats_env_and_vram() {
        let pref = manual(VENDOR_INTEL, 0x7d55, 0, "Intel(R) Arc(TM) Graphics");
        let (i, src) = pick(&hybrid(), &pref, Some("nvidia")).unwrap();
        assert_eq!(i, 0);
        assert_eq!(src, PickSource::Preference);
    }

    #[test]
    fn env_substring_beats_vram_and_is_case_insensitive() {
        let mut gpus = vec![
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
            gpu(VENDOR_INTEL, 0x7d55, "Intel(R) Arc(TM) Graphics", 0),
        ];
        assign_ids(&mut gpus);
        let (i, src) = pick(&gpus, &GpuPreference::default(), Some("ARC")).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Env);
    }

    #[test]
    fn unmatched_env_falls_back_to_max_vram() {
        let (i, src) = pick(&hybrid(), &GpuPreference::default(), Some("radeon")).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Auto);
    }

    #[test]
    fn missing_preferred_gpu_falls_back_and_says_so() {
        let pref = manual(VENDOR_AMD, 0x744c, 0, "AMD Radeon RX 7900 XTX");
        let (i, src) = pick(&hybrid(), &pref, None).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::PreferenceMissing);
    }

    #[test]
    fn preferred_matches_same_model_when_occurrence_gone() {
        // Preference stored occurrence 1; only one twin remains.
        let mut gpus = vec![
            gpu(VENDOR_INTEL, 0x7d55, "Intel(R) Arc(TM) Graphics", 0),
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
        ];
        assign_ids(&mut gpus);
        let pref = manual(VENDOR_NVIDIA, 0x2c05, 1, "NVIDIA GeForce RTX 5070 Ti");
        let (i, src) = pick(&gpus, &pref, None).unwrap();
        assert_eq!(i, 1);
        assert_eq!(src, PickSource::Preference);
    }

    #[test]
    fn preferred_matches_by_name_when_ids_changed() {
        let pref = manual(VENDOR_NVIDIA, 0xffff, 0, "Intel(R) Arc(TM) Graphics");
        let (i, src) = pick(&hybrid(), &pref, None).unwrap();
        assert_eq!(i, 0);
        assert_eq!(src, PickSource::Preference);
    }

    #[test]
    fn empty_inventory_selects_nothing() {
        assert!(pick(&[], &GpuPreference::default(), Some("nvidia")).is_none());
    }

    #[test]
    fn ids_disambiguate_twins() {
        let mut gpus = vec![
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
            gpu(VENDOR_NVIDIA, 0x2c05, "NVIDIA GeForce RTX 5070 Ti", 16),
        ];
        assign_ids(&mut gpus);
        assert_eq!(gpus[0].id, "10de-2c05-0");
        assert_eq!(gpus[1].id, "10de-2c05-1");
    }

    #[test]
    fn store_round_trips_and_survives_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpu-settings.json");
        let store = GpuPrefStore::load_from(path.clone());
        assert_eq!(store.get(), GpuPreference::default());
        let pref = manual(VENDOR_INTEL, 0x7d55, 0, "Intel(R) Arc(TM) Graphics");
        store.set(pref.clone()).unwrap();
        assert_eq!(store.get(), pref);
        assert_eq!(GpuPrefStore::load_from(path.clone()).get(), pref);
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            GpuPrefStore::load_from(path).get(),
            GpuPreference::default()
        );
    }

    #[test]
    fn session_counter_tracks_begin_and_drop() {
        // ACTIVE is process-global; this is the only test that touches it.
        let a = session_begin(ActiveGpu {
            id: "10de-2c05-0".into(),
            name: "GPU A".into(),
            vendor_id: VENDOR_NVIDIA,
            backend: "nvenc",
        });
        let (gpu0, n0) = active().unwrap();
        assert_eq!((gpu0.name.as_str(), n0), ("GPU A", 1));
        let b = session_begin(ActiveGpu {
            id: "10de-2c05-0".into(),
            name: "GPU A".into(),
            vendor_id: VENDOR_NVIDIA,
            backend: "nvenc",
        });
        assert_eq!(active().unwrap().1, 2);
        drop(a);
        assert_eq!(active().unwrap().1, 1);
        drop(b);
        assert_eq!(active().unwrap().1, 0);
    }

    /// Captured `D3DKMT_ADAPTERTYPE` words: IddCx ghost and Basic Render hide;
    /// real GPUs stay.
    #[test]
    fn adapter_type_hides_idd_ghost_keeps_real_gpus() {
        assert!(adapter_type::hidden(0x0342)); // IddCx ghost: indirect + display-only
        assert!(!adapter_type::hidden(0x031b)); // real dGPU
        assert!(!adapter_type::hidden(0x2323)); // real iGPU
        assert!(adapter_type::hidden(0x0105)); // software (Basic Render)
    }

    /// Nothing [`enumerate`] returns may be `hidden`. A failed kernel query is
    /// fail-open (the adapter stays).
    #[cfg(target_os = "windows")]
    #[test]
    fn enumerate_excludes_non_render_adapters() {
        for g in enumerate() {
            let bits = kmt::adapter_type_bits(g.handle.luid_low, g.handle.luid_high);
            eprintln!(
                "enumerated: {} ({}) kmt_bits={}",
                g.name,
                g.id,
                bits.map_or("<query failed>".into(), |b| format!("{b:#x}")),
            );
            if let Some(bits) = bits {
                assert!(
                    !adapter_type::hidden(bits),
                    "{} ({}) should have been filtered (bits {bits:#x})",
                    g.name,
                    g.id
                );
            }
        }
    }
}
