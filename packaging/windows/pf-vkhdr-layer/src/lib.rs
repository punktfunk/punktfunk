//! Vulkan implicit layer `VK_LAYER_PUNKTFUNK_hdr_inject` for IddCx displays.
//!
//! Some Windows ICDs accept HDR swapchains on indirect displays but omit their HDR surface formats.
//! The layer intercepts both surface-format queries and appends HDR10/scRGB formats when that
//! surface's monitor is a Punktfunk virtual display in HDR (advanced color on, not ACM). Existing
//! formats are deduplicated; physical, SDR and already-HDR surfaces pass through unchanged.
//!
//! `vkCreateWin32SurfaceKHR` supplies the `VkSurfaceKHR -> HWND` association used by the HDR gate.
//! Every other command follows the Vulkan loader's normal dispatch chain. Win32 query structures
//! use typed C-layout mirrors rather than target-specific offsets.
//!
//! Safety rests on the loader-layer protocol and Vulkan valid usage: live dispatchable handles,
//! valid create-info chains, NUL-terminated command names, and valid count/array pairs.
//! `DISABLE_PF_VKHDR=1` disables the layer; `PF_VKHDR_EXCLUDE` skips named executables.
//! `PF_VKHDR_LOG=1` writes diagnostics to `%TEMP%\pf_vkhdr_layer.log`.

#![allow(non_snake_case)]
#![allow(clippy::too_many_arguments)]
// HWND / HMONITOR etc. deliberately mirror the Win32 names.
#![allow(clippy::upper_case_acronyms)]

use ash::vk;
use ash::vk::Handle;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::{Mutex, OnceLock};

// ---- Vulkan loader<->layer glue (vk_layer.h; not in the core registry / ash) ----
// The loader's private create-info sTypes squat on 47/48 (NOT the 1000211xxx range).
const LOADER_INSTANCE_CREATE_INFO: i32 = 47;
const LOADER_DEVICE_CREATE_INFO: i32 = 48;
const VK_LAYER_LINK_INFO: i32 = 0;
const LAYER_NEGOTIATE_INTERFACE_STRUCT: i32 = 1;

type PfnGipa = vk::PFN_vkGetInstanceProcAddr;
type PfnGdpa = vk::PFN_vkGetDeviceProcAddr;
type PfnGpdpa = unsafe extern "system" fn(vk::Instance, *const c_char) -> vk::PFN_vkVoidFunction;

#[repr(C)]
struct BaseIn {
    s_type: vk::StructureType,
    p_next: *const c_void,
}
#[repr(C)]
struct LayerInstanceLink {
    p_next: *mut LayerInstanceLink,
    next_gipa: PfnGipa,
    next_gpdpa: Option<PfnGpdpa>,
}
#[repr(C)]
struct LayerInstanceCreateInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    function: i32,
    u: *mut LayerInstanceLink,
}
#[repr(C)]
struct LayerDeviceLink {
    p_next: *mut LayerDeviceLink,
    next_gipa: PfnGipa,
    next_gdpa: PfnGdpa,
}
#[repr(C)]
struct LayerDeviceCreateInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    function: i32,
    u: *mut LayerDeviceLink,
}
#[repr(C)]
pub struct NegotiateLayerInterface {
    s_type: i32,
    p_next: *mut c_void,
    loader_layer_interface_version: u32,
    pfn_gipa: Option<PfnGipa>,
    pfn_gdpa: Option<PfnGdpa>,
    pfn_gpdpa: Option<PfnGpdpa>,
}
// Layout proofs for the vk_layer.h mirrors: the loader allocates them, the layer reads and writes
// them by field, so each field must sit where the header puts it. The two create-info mirrors
// stop at the first union member, so they are prefixes the layer never allocates.
const _: () = {
    use std::mem::{offset_of, size_of};
    let word = size_of::<usize>();
    assert!(size_of::<BaseIn>() == 2 * word);
    assert!(offset_of!(BaseIn, p_next) == word);
    assert!(size_of::<LayerInstanceLink>() == 3 * word);
    assert!(offset_of!(LayerInstanceLink, next_gipa) == word);
    assert!(offset_of!(LayerInstanceLink, next_gpdpa) == 2 * word);
    assert!(offset_of!(LayerInstanceCreateInfo, function) == 2 * word);
    assert!(offset_of!(LayerInstanceCreateInfo, u) == 3 * word);
    assert!(size_of::<LayerDeviceLink>() == 3 * word);
    assert!(offset_of!(LayerDeviceLink, next_gipa) == word);
    assert!(offset_of!(LayerDeviceLink, next_gdpa) == 2 * word);
    assert!(offset_of!(LayerDeviceCreateInfo, function) == 2 * word);
    assert!(offset_of!(LayerDeviceCreateInfo, u) == 3 * word);
    assert!(size_of::<NegotiateLayerInterface>() == 6 * word);
    assert!(offset_of!(NegotiateLayerInterface, loader_layer_interface_version) == 2 * word);
    assert!(offset_of!(NegotiateLayerInterface, pfn_gipa) == 3 * word);
    assert!(offset_of!(NegotiateLayerInterface, pfn_gdpa) == 4 * word);
    assert!(offset_of!(NegotiateLayerInterface, pfn_gpdpa) == 5 * word);
};

// raw mirror of VkSurfaceFormat2KHR (avoid ash lifetime generics in fn-pointer types)
#[repr(C)]
struct Win32SurfaceCreateInfoRaw {
    _s_type: vk::StructureType,
    _p_next: *const c_void,
    _flags: u32,
    _hinstance: *const c_void,
    hwnd: *const c_void,
}

#[repr(C)]
struct PhysicalDeviceSurfaceInfo2Raw {
    _s_type: vk::StructureType,
    _p_next: *const c_void,
    surface: vk::SurfaceKHR,
}
// Both read one field out of the caller's struct; ash's generated types are the layout oracle.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Win32SurfaceCreateInfoRaw>() == size_of::<vk::Win32SurfaceCreateInfoKHR>());
    assert!(
        offset_of!(Win32SurfaceCreateInfoRaw, hwnd)
            == offset_of!(vk::Win32SurfaceCreateInfoKHR, hwnd)
    );
    assert!(
        size_of::<PhysicalDeviceSurfaceInfo2Raw>()
            == size_of::<vk::PhysicalDeviceSurfaceInfo2KHR>()
    );
    assert!(
        offset_of!(PhysicalDeviceSurfaceInfo2Raw, surface)
            == offset_of!(vk::PhysicalDeviceSurfaceInfo2KHR, surface)
    );
};

#[repr(C)]
#[derive(Clone, Copy)]
struct SurfaceFormat2Raw {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    surface_format: vk::SurfaceFormatKHR,
}
// Layout proof for the mirror: scratch arrays of it are handed to the ICD, and the caller's
// VkSurfaceFormat2KHR array is read and stamped through it, so a size or alignment drift would
// corrupt either side.
const _: () = assert!(
    std::mem::size_of::<SurfaceFormat2Raw>() == std::mem::size_of::<vk::SurfaceFormat2KHR>()
);
const _: () = assert!(
    std::mem::align_of::<SurfaceFormat2Raw>() == std::mem::align_of::<vk::SurfaceFormat2KHR>()
);

// ---- ICD function-pointer typedefs we call down to (raw pointers, no lifetimes) ----
type FnCreateInstance =
    unsafe extern "system" fn(*const c_void, *const c_void, *mut vk::Instance) -> vk::Result;
type FnDestroyInstance = unsafe extern "system" fn(vk::Instance, *const c_void);
type FnCreateDevice = unsafe extern "system" fn(
    vk::PhysicalDevice,
    *const c_void,
    *const c_void,
    *mut vk::Device,
) -> vk::Result;
type FnGetSurfFmts = unsafe extern "system" fn(
    vk::PhysicalDevice,
    vk::SurfaceKHR,
    *mut u32,
    *mut vk::SurfaceFormatKHR,
) -> vk::Result;
type FnGetSurfFmts2 = unsafe extern "system" fn(
    vk::PhysicalDevice,
    *const c_void,
    *mut u32,
    *mut c_void,
) -> vk::Result;
type FnCreateWin32Surface = unsafe extern "system" fn(
    vk::Instance,
    *const c_void,
    *const c_void,
    *mut vk::SurfaceKHR,
) -> vk::Result;
type FnDestroySurface = unsafe extern "system" fn(vk::Instance, vk::SurfaceKHR, *const c_void);

// Both maps' values are raw `extern "system"` fn pointers plus plain handles; fn pointers are
// process-global code addresses and implement Send/Sync intrinsically, so the auto traits hold
// and no `unsafe impl Send` is needed (two used to sit here as unproven markers).
struct InstanceData {
    instance: vk::Instance,
    next_gipa: PfnGipa,
    next_gpdpa: Option<PfnGpdpa>,
    destroy_instance: Option<FnDestroyInstance>,
    get_surface_formats: Option<FnGetSurfFmts>,
    get_surface_formats2: Option<FnGetSurfFmts2>,
    create_win32_surface: Option<FnCreateWin32Surface>,
    destroy_surface: Option<FnDestroySurface>,
}

struct DeviceData {
    next_gdpa: PfnGdpa,
}

fn instances() -> &'static Mutex<HashMap<usize, InstanceData>> {
    static M: OnceLock<Mutex<HashMap<usize, InstanceData>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
fn devices() -> &'static Mutex<HashMap<usize, DeviceData>> {
    static M: OnceLock<Mutex<HashMap<usize, DeviceData>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}
/// VkSurfaceKHR handle -> the HWND it was created from (so we can resolve its monitor's HDR state).
fn surface_hwnds() -> &'static Mutex<HashMap<u64, isize>> {
    static M: OnceLock<Mutex<HashMap<u64, isize>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Dispatch key of a dispatchable handle.
///
/// # Safety
/// `raw` must be the value of a **live dispatchable** Vulkan handle (`VkInstance`,
/// `VkPhysicalDevice`, `VkDevice`). The loader ABI mandates that every dispatchable object's
/// first pointer-sized word is the loader's dispatch-table pointer, which is what makes the read
/// in-bounds and initialized — and what makes it a stable per-chain key.
#[inline]
unsafe fn key(raw: u64) -> usize {
    // SAFETY: per this function's contract, `raw` points at a live dispatchable object whose
    // first pointer-sized word exists and is initialized (the loader wrote it at creation).
    unsafe { *(raw as usize as *const usize) }
}

/// Pick one field of the instance chain `raw` dispatches through. `None` when that chain was
/// never hooked, or the map is poisoned.
///
/// # Safety
/// `raw` must be a live dispatchable handle of an instance chain (`VkInstance` or
/// `VkPhysicalDevice`) — the contract of [`key`].
unsafe fn lookup<T>(raw: u64, pick: impl FnOnce(&InstanceData) -> Option<T>) -> Option<T> {
    // SAFETY: `raw` is a live dispatchable handle per this function's contract.
    let k = unsafe { key(raw) };
    instances().lock().ok()?.get(&k).and_then(pick)
}

/// Reinterpret a function address as the loader's type-erased void-function pointer.
///
/// # Safety
/// `p` must be the address of an `extern "system"` function. Whoever receives the returned PFN
/// must cast it back to the exact prototype of the command it was queried under before calling —
/// which is precisely what the Vulkan `*ProcAddr` contract obliges callers to do.
#[inline]
unsafe fn as_pfn(p: *const c_void) -> vk::PFN_vkVoidFunction {
    // SAFETY: `p` is a Vulkan loader function address with the declared system ABI. This build
    // proves the data/function pointer widths match because `transmute` otherwise cannot compile.
    Some(unsafe { std::mem::transmute::<*const c_void, unsafe extern "system" fn()>(p) })
}

/// Resolve `name` down-chain and reinterpret the result as fn-pointer type `T`.
///
/// # Safety
/// `gipa` must be a valid `vkGetInstanceProcAddr` for `inst` (or for the null instance when
/// resolving global commands), and `T` must be the exact fn-pointer prototype of the Vulkan
/// command named by `name` — `transmute_copy` erases all type checking between them.
#[inline]
unsafe fn resolve<T: Copy>(gipa: PfnGipa, inst: vk::Instance, name: &CStr) -> Option<T> {
    // SAFETY: per this function's contract `gipa` is a valid loader/ICD GetInstanceProcAddr for
    // `inst`, and `name` is a live NUL-terminated string for the duration of the call.
    let f = unsafe { gipa(inst, name.as_ptr()) };
    // SAFETY: `f` was returned for `name`, and per this function's contract `T` is that
    // command's exact prototype; both sides are fn pointers of identical size.
    f.map(|f| unsafe { std::mem::transmute_copy::<_, T>(&f) })
}

fn log(msg: &str) {
    if std::env::var_os("PF_VKHDR_LOG").is_none() {
        return;
    }
    use std::io::Write;
    let mut path = std::env::temp_dir();
    path.push("pf_vkhdr_layer.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{msg}");
    }
}

fn hdr_extra() -> [vk::SurfaceFormatKHR; 2] {
    [
        vk::SurfaceFormatKHR {
            format: vk::Format::A2B10G10R10_UNORM_PACK32,
            color_space: vk::ColorSpaceKHR::HDR10_ST2084_EXT,
        },
        vk::SurfaceFormatKHR {
            format: vk::Format::R16G16B16A16_SFLOAT,
            color_space: vk::ColorSpaceKHR::EXTENDED_SRGB_LINEAR_EXT,
        },
    ]
}

/// `false` if this process is on the anti-cheat exclude list (built-in + `PF_VKHDR_EXCLUDE`).
/// Computed once per process.
fn injection_allowed_for_process() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if exe.is_empty() {
            return true;
        }
        // Conservative default skip-list for kernel-level anti-cheat titles. Users can extend or
        // clear via PF_VKHDR_EXCLUDE. (HDR injection is benign, but we err toward not being present
        // in these process' WSI path at all.)
        const DENY: &[&str] = &[
            "cs2.exe",
            "rainbowsix.exe",
            "rainbowsixgame.exe",
            "r5apex.exe",
        ];
        let mut denied = false;
        for d in DENY {
            if *d == exe {
                denied = true;
            }
        }
        if let Ok(extra) = std::env::var("PF_VKHDR_EXCLUDE") {
            for e in extra.split([',', ';']) {
                if e.trim().to_lowercase() == exe {
                    denied = true;
                }
            }
        }
        if denied {
            log(&format!("injection disabled for excluded process: {exe}"));
        }
        !denied
    })
}

// ---- Win32 / DisplayConfig: is the surface's monitor HDR-enabled right now? ----
mod hdr {
    use std::ffi::c_void;
    pub type HWND = isize;
    pub type HMONITOR = isize;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Rect {
        pub l: i32,
        pub t: i32,
        pub r: i32,
        pub b: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct MonitorInfoExW {
        pub cb: u32,
        pub rc_monitor: Rect,
        pub rc_work: Rect,
        pub flags: u32,
        pub sz_device: [u16; 32],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Luid {
        pub low: u32,
        pub high: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Rational {
        pub num: u32,
        pub den: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Source {
        pub adapter: Luid,
        pub id: u32,
        pub mode_idx: u32,
        pub status: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Target {
        pub adapter: Luid,
        pub id: u32,
        pub mode_idx: u32,
        pub tech: i32,
        pub rotation: i32,
        pub scaling: i32,
        pub refresh: Rational,
        pub scanline: i32,
        pub available: i32,
        pub status: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PathInfo {
        pub src: Source,
        pub tgt: Target,
        pub flags: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ModeInfo {
        pub _b: [u8; 64],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Header {
        pub typ: i32,
        pub size: u32,
        pub adapter: Luid,
        pub id: u32,
    }
    #[repr(C)]
    pub struct AdvInfo {
        pub header: Header,
        pub value: u32,
        pub enc: i32,
        pub bpc: i32,
    }
    #[repr(C)]
    pub struct SourceName {
        pub header: Header,
        pub gdi: [u16; 32],
    }
    /// `DISPLAYCONFIG_TARGET_DEVICE_NAME`.
    #[repr(C)]
    pub struct TargetName {
        pub header: Header,
        pub flags: u32,
        pub tech: i32,
        pub edid_manufacture_id: u16,
        pub edid_product_code_id: u16,
        pub connector_instance: u32,
        pub friendly: [u16; 64],
        pub path: [u16; 128],
    }
    // The documented sizes of the Win32 records: a request carries its own in `header.size`,
    // which DisplayConfigGetDeviceInfo refuses unless exact, and the query arrays are stepped
    // by the kernel at the real stride.
    const _: () = {
        use std::mem::{offset_of, size_of};
        assert!(size_of::<Luid>() == 8);
        assert!(size_of::<Header>() == 20 && offset_of!(Header, id) == 16);
        assert!(size_of::<Source>() == 20 && offset_of!(Source, id) == 8);
        assert!(size_of::<Target>() == 48 && offset_of!(Target, id) == 8);
        assert!(size_of::<PathInfo>() == 72 && offset_of!(PathInfo, tgt) == 20);
        assert!(size_of::<ModeInfo>() == 64);
        assert!(size_of::<MonitorInfoExW>() == 104 && offset_of!(MonitorInfoExW, sz_device) == 40);
        assert!(size_of::<AdvInfo>() == 32 && offset_of!(AdvInfo, value) == 20);
        assert!(size_of::<SourceName>() == 84 && offset_of!(SourceName, gdi) == 20);
        assert!(size_of::<TargetName>() == 420 && offset_of!(TargetName, path) == 164);
    };

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MonitorFromWindow(h: HWND, flags: u32) -> HMONITOR;
        fn GetMonitorInfoW(h: HMONITOR, mi: *mut MonitorInfoExW) -> i32;
        fn GetDisplayConfigBufferSizes(flags: u32, np: *mut u32, nm: *mut u32) -> i32;
        fn QueryDisplayConfig(
            flags: u32,
            np: *mut u32,
            pa: *mut PathInfo,
            nm: *mut u32,
            ma: *mut ModeInfo,
            topo: *mut c_void,
        ) -> i32;
        fn DisplayConfigGetDeviceInfo(p: *mut c_void) -> i32;
    }
    const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
    const GET_SOURCE_NAME: i32 = 1;
    const GET_TARGET_NAME: i32 = 2;
    const GET_ADVANCED_COLOR_INFO: i32 = 9;
    /// The EDID manufacturer the Punktfunk virtual display carries (pf-win-display's match).
    const PF_EDID_MANUFACTURER: &str = "PNK";
    const MONITOR_DEFAULTTONEAREST: u32 = 2;

    // Safe fn: every invariant below is local (out-params point at live locals / exact-length
    // Vecs); callers carry no obligations.
    fn active_paths() -> Vec<PathInfo> {
        let (mut np, mut nm) = (0u32, 0u32);
        // SAFETY: both out-pointers come from live local `u32`s that outlive the call.
        if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) } != 0
            || np == 0
        {
            return Vec::new();
        }
        // SAFETY: PathInfo is a #[repr(C)] aggregate of integers — all-zero is a valid value.
        let mut pa: Vec<PathInfo> = vec![unsafe { std::mem::zeroed() }; np as usize];
        // SAFETY: ModeInfo is an opaque byte blob — all-zero is a valid value.
        let mut ma: Vec<ModeInfo> = vec![unsafe { std::mem::zeroed() }; nm as usize];
        // SAFETY: `pa`/`ma` hold exactly `np`/`nm` elements and those counts are passed by
        // pointer as the in/out capacities: QueryDisplayConfig writes at most that many entries
        // (if the topology grew between the two calls it returns ERROR_INSUFFICIENT_BUFFER
        // rather than writing past the end) and shrinks np/nm to what it actually wrote.
        if unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut np,
                pa.as_mut_ptr(),
                &mut nm,
                ma.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        } != 0
        {
            return Vec::new();
        }
        pa.truncate(np as usize);
        pa
    }

    fn target_hdr_enabled(p: &PathInfo) -> bool {
        // SAFETY: AdvInfo is a #[repr(C)] aggregate of integers — all-zero is a valid value.
        let mut ai: AdvInfo = unsafe { std::mem::zeroed() };
        ai.header.typ = GET_ADVANCED_COLOR_INFO;
        ai.header.size = std::mem::size_of::<AdvInfo>() as u32;
        ai.header.adapter = p.tgt.adapter;
        ai.header.id = p.tgt.id;
        // SAFETY: the request header carries this struct's exact size, which is the documented
        // bound for DisplayConfigGetDeviceInfo's write; `ai` is a live local across the call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut ai as *mut _ as *mut c_void) } != 0 {
            return false;
        }
        // value bitfield: bit1 advancedColorEnabled, bit2 wideColorEnforced. Enabled with wide
        // colour enforced is ACM on an SDR panel, not HDR.
        (ai.value & 0b110) == 0b010
    }

    /// The target is a Punktfunk virtual display; its monitor path names our EDID manufacturer.
    fn target_is_ours(p: &PathInfo) -> bool {
        // SAFETY: TargetName is a #[repr(C)] aggregate of integers — all-zero is a valid value.
        let mut tn: TargetName = unsafe { std::mem::zeroed() };
        tn.header.typ = GET_TARGET_NAME;
        tn.header.size = std::mem::size_of::<TargetName>() as u32;
        tn.header.adapter = p.tgt.adapter;
        tn.header.id = p.tgt.id;
        // SAFETY: the request header carries this struct's exact size, which is the documented
        // bound for DisplayConfigGetDeviceInfo's write; `tn` is a live local across the call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut tn as *mut _ as *mut c_void) } != 0 {
            return false;
        }
        let end = tn
            .path
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(tn.path.len());
        String::from_utf16_lossy(&tn.path[..end])
            .to_ascii_uppercase()
            .contains(PF_EDID_MANUFACTURER)
    }

    fn inject_for(p: &PathInfo) -> bool {
        target_is_ours(p) && target_hdr_enabled(p)
    }

    fn source_gdi(p: &PathInfo) -> [u16; 32] {
        // SAFETY: SourceName is a #[repr(C)] aggregate of integers — all-zero is a valid value.
        let mut sn: SourceName = unsafe { std::mem::zeroed() };
        sn.header.typ = GET_SOURCE_NAME;
        sn.header.size = std::mem::size_of::<SourceName>() as u32;
        sn.header.adapter = p.src.adapter;
        sn.header.id = p.src.id;
        // SAFETY: the request header carries this struct's exact size, which is the documented
        // bound for DisplayConfigGetDeviceInfo's write; `sn` is a live local across the call.
        let _ = unsafe { DisplayConfigGetDeviceInfo(&mut sn as *mut _ as *mut c_void) };
        sn.gdi
    }

    /// Is the display this surface lives on a Punktfunk virtual display in HDR? Physical panels
    /// and ACM never qualify: their ICD lists its own formats. `hwnd == 0`/unknown falls back to
    /// "any of our displays is in HDR".
    ///
    /// Safe fn: `MonitorFromWindow` with `DEFAULTTONEAREST` tolerates any HWND value — including
    /// a destroyed or foreign one (our map can be stale) — so callers carry no obligations.
    pub fn enabled_for(hwnd: HWND) -> bool {
        let paths = active_paths();
        if hwnd != 0 {
            // SAFETY: MonitorFromWindow accepts arbitrary HWND values with DEFAULTTONEAREST
            // (falling back to the nearest/primary monitor) and takes nothing by pointer.
            let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
            // SAFETY: MonitorInfoExW is a #[repr(C)] aggregate of integers — all-zero is valid.
            let mut mi: MonitorInfoExW = unsafe { std::mem::zeroed() };
            mi.cb = std::mem::size_of::<MonitorInfoExW>() as u32;
            // SAFETY: `mi.cb` carries the struct's exact size, which is the documented bound for
            // GetMonitorInfoW's write; `mi` is a live local across the call.
            if unsafe { GetMonitorInfoW(mon, &mut mi) } != 0 {
                for p in &paths {
                    if source_gdi(p) == mi.sz_device {
                        return inject_for(p);
                    }
                }
            }
        }
        paths.iter().any(inject_for)
    }
}

/// Should we inject HDR formats for this surface right now? Safe fn — the surface handle is only
/// used as a map key, and a stale/unknown HWND degrades to the any-display fallback.
fn should_inject(surface: vk::SurfaceKHR) -> bool {
    if !injection_allowed_for_process() {
        return false;
    }
    let hwnd = surface_hwnds()
        .lock()
        .ok()
        .and_then(|m| m.get(&surface.as_raw()).copied())
        .unwrap_or(0);
    hdr::enabled_for(hwnd)
}

// ---- entry point ----

/// Layer negotiation entry point; the loader calls this export when it loads the DLL.
///
/// # Safety
/// `p` must be null or point to a live `NegotiateLayerInterface` the caller has exclusive access
/// to for the duration of the call. The Vulkan loader — the only intended caller of this
/// export — guarantees exactly that for the negotiate handshake.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    p: *mut NegotiateLayerInterface,
) -> vk::Result {
    if p.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: `p` is non-null, and per this export's contract the loader hands us exclusive
    // access to a live NegotiateLayerInterface for the duration of the call.
    let s = unsafe { &mut *p };
    if s.s_type != LAYER_NEGOTIATE_INTERFACE_STRUCT {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    if s.loader_layer_interface_version > 2 {
        s.loader_layer_interface_version = 2;
    }
    s.pfn_gipa = Some(layer_gipa);
    s.pfn_gdpa = Some(layer_gdpa);
    s.pfn_gpdpa = Some(layer_gpdpa);
    log("negotiate: VK_LAYER_PUNKTFUNK_hdr_inject active (v2)");
    vk::Result::SUCCESS
}

// ---- proc-addr dispatch ----

unsafe extern "system" fn layer_gipa(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    // SAFETY: `p_name` is non-null, and vkGetInstanceProcAddr's valid-usage rules require pName
    // to be a valid NUL-terminated string for the duration of the call.
    let name = unsafe { CStr::from_ptr(p_name) }.to_bytes();
    // SAFETY: every arm hands `as_pfn` the address of the `extern "system"` hook this layer
    // substitutes for exactly that command name; the *ProcAddr contract obliges the caller to
    // cast the returned PFN back to the named command's prototype before invoking it, which is
    // what makes the type-erased transmute inside `as_pfn` sound for each arm.
    let hook = unsafe {
        match name {
            b"vkGetInstanceProcAddr" => as_pfn(layer_gipa as *const c_void),
            b"vkGetDeviceProcAddr" => as_pfn(layer_gdpa as *const c_void),
            b"vkCreateInstance" => as_pfn(create_instance as *const c_void),
            b"vkDestroyInstance" => as_pfn(destroy_instance as *const c_void),
            b"vkCreateDevice" => as_pfn(create_device as *const c_void),
            b"vkGetPhysicalDeviceSurfaceFormatsKHR" => as_pfn(get_surface_formats as *const c_void),
            b"vkGetPhysicalDeviceSurfaceFormats2KHR" => {
                as_pfn(get_surface_formats2 as *const c_void)
            }
            b"vkCreateWin32SurfaceKHR" => as_pfn(create_win32_surface as *const c_void),
            b"vkDestroySurfaceKHR" => as_pfn(destroy_surface as *const c_void),
            _ => None,
        }
    };
    if hook.is_some() {
        return hook;
    }
    if instance == vk::Instance::null() {
        return None;
    }
    // SAFETY: `instance` is non-null, and vkGetInstanceProcAddr's valid-usage rules make a
    // non-null instance argument a live instance handle.
    let next = unsafe { lookup(instance.as_raw(), |d| Some(d.next_gipa)) };
    // SAFETY: `next` is the down-chain GetInstanceProcAddr captured from the loader's link at
    // create_instance for this very chain; `p_name` is still valid NUL-terminated.
    next.and_then(|gipa| unsafe { gipa(instance, p_name) })
}

unsafe extern "system" fn layer_gpdpa(
    instance: vk::Instance,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    // SAFETY: `p_name` is non-null, and the GetPhysicalDeviceProcAddr contract mirrors
    // vkGetInstanceProcAddr's: pName is a valid NUL-terminated string for the call.
    let name = unsafe { CStr::from_ptr(p_name) }.to_bytes();
    // SAFETY: both arms hand `as_pfn` the address of the `extern "system"` hook this layer
    // substitutes for exactly that command name; the *ProcAddr contract obliges the caller to
    // cast the returned PFN back to that command's prototype before invoking it.
    let hook = unsafe {
        match name {
            b"vkGetPhysicalDeviceSurfaceFormatsKHR" => as_pfn(get_surface_formats as *const c_void),
            b"vkGetPhysicalDeviceSurfaceFormats2KHR" => {
                as_pfn(get_surface_formats2 as *const c_void)
            }
            _ => None,
        }
    };
    if hook.is_some() {
        return hook;
    }
    if instance == vk::Instance::null() {
        return None;
    }
    // SAFETY: `instance` is non-null and (per the caller's contract) a live instance handle.
    let next = unsafe { lookup(instance.as_raw(), |d| d.next_gpdpa) };
    // SAFETY: `next` is the down-chain GPDPA captured from the loader's link at create_instance
    // for this very chain; `p_name` is still valid NUL-terminated.
    next.and_then(|gpdpa| unsafe { gpdpa(instance, p_name) })
}

unsafe extern "system" fn layer_gdpa(
    device: vk::Device,
    p_name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    // SAFETY: `p_name` is non-null, and vkGetDeviceProcAddr's valid-usage rules require pName to
    // be a valid NUL-terminated string for the duration of the call.
    let name = unsafe { CStr::from_ptr(p_name) }.to_bytes();
    if name == b"vkGetDeviceProcAddr" {
        // SAFETY: `layer_gdpa` is an `extern "system"` fn whose prototype is exactly what the
        // caller will cast the returned PFN back to for this name (*ProcAddr contract).
        return unsafe { as_pfn(layer_gdpa as *const c_void) };
    }
    if device == vk::Device::null() {
        return None;
    }
    let next = {
        let g = match devices().lock() {
            Ok(g) => g,
            Err(_) => return None,
        };
        // SAFETY: `device` is non-null, and vkGetDeviceProcAddr's valid-usage rules make it a
        // live device handle — a dispatchable object whose first word is the dispatch key.
        g.get(&unsafe { key(device.as_raw()) }).map(|d| d.next_gdpa)
    };
    // SAFETY: `next` is the down-chain GetDeviceProcAddr captured from the loader's link at
    // create_device for this very chain; `p_name` is still valid NUL-terminated.
    next.and_then(|gdpa| unsafe { gdpa(device, p_name) })
}

// ---- instance chain ----

/// Walk `p_ci`'s pNext chain for the loader's layer-link node.
///
/// # Safety
/// `p_ci` must point to a valid create-info struct whose `pNext` chain is a well-formed list in
/// which every node begins with the `{sType, pNext}` header — the loader builds exactly that
/// chain for the create call that invokes us.
unsafe fn find_instance_link(p_ci: *const c_void) -> *mut LayerInstanceCreateInfo {
    // SAFETY: per this function's contract, `p_ci` and every non-null `pNext` reached from it
    // point at nodes beginning with a {sType, pNext} header; a node whose sType says it is the
    // loader's instance create-info really is a LayerInstanceCreateInfo (the loader wrote it).
    unsafe {
        let mut node = (*(p_ci as *const BaseIn)).p_next as *const BaseIn;
        while !node.is_null() {
            if (*node).s_type.as_raw() == LOADER_INSTANCE_CREATE_INFO {
                let lci = node as *mut LayerInstanceCreateInfo;
                if (*lci).function == VK_LAYER_LINK_INFO {
                    return lci;
                }
            }
            node = (*node).p_next as *const BaseIn;
        }
    }
    ptr::null_mut()
}

unsafe extern "system" fn create_instance(
    p_ci: *const c_void,
    p_alloc: *const c_void,
    p_inst: *mut vk::Instance,
) -> vk::Result {
    // SAFETY: the loader invokes this hook with a valid VkInstanceCreateInfo whose pNext chain
    // it built — the chain find_instance_link's contract requires.
    let lci = unsafe { find_instance_link(p_ci) };
    if lci.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: `lci` is non-null and points into the loader's live chain. Its `u` link (checked
    // non-null before use) is this layer's link entry; reading the down-chain pointers out of it
    // and advancing `(*lci).u` to the next link is the layer protocol every layer must follow
    // before calling down.
    let (next_gipa, next_gpdpa) = unsafe {
        let link = (*lci).u;
        if link.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let gipa = (*link).next_gipa;
        let gpdpa = (*link).next_gpdpa;
        (*lci).u = (*link).p_next;
        (gipa, gpdpa)
    };

    // SAFETY: `next_gipa` is the loader-supplied down-chain GIPA; vkCreateInstance is a global
    // command resolvable from the null instance, and FnCreateInstance mirrors its prototype.
    let create: FnCreateInstance =
        match unsafe { resolve(next_gipa, vk::Instance::null(), c"vkCreateInstance") } {
            Some(f) => f,
            None => return vk::Result::ERROR_INITIALIZATION_FAILED,
        };
    // SAFETY: forwarding the loader's own arguments unchanged to the down-chain create, which
    // expects exactly this (p_ci with the link advanced, the caller's allocator, the caller's
    // out-pointer).
    let res = unsafe { create(p_ci, p_alloc, p_inst) };
    if res != vk::Result::SUCCESS {
        return res;
    }
    // SAFETY: vkCreateInstance requires pInstance to be a valid pointer, and on SUCCESS the
    // down-chain just wrote the new instance handle through it.
    let inst = unsafe { *p_inst };
    // SAFETY: `inst` is the live instance created above; `next_gipa` resolves its
    // instance-level commands, and every Fn* typedef used here mirrors the prototype of the
    // exact command name it is resolved from.
    let data = unsafe {
        InstanceData {
            instance: inst,
            next_gipa,
            next_gpdpa,
            destroy_instance: resolve(next_gipa, inst, c"vkDestroyInstance"),
            get_surface_formats: resolve(next_gipa, inst, c"vkGetPhysicalDeviceSurfaceFormatsKHR"),
            get_surface_formats2: resolve(
                next_gipa,
                inst,
                c"vkGetPhysicalDeviceSurfaceFormats2KHR",
            ),
            create_win32_surface: resolve(next_gipa, inst, c"vkCreateWin32SurfaceKHR"),
            destroy_surface: resolve(next_gipa, inst, c"vkDestroySurfaceKHR"),
        }
    };
    if let Ok(mut g) = instances().lock() {
        // SAFETY: `inst` is the live dispatchable handle created above — its first word is the
        // dispatch key.
        g.insert(unsafe { key(inst.as_raw()) }, data);
    }
    log("create_instance: hooked");
    vk::Result::SUCCESS
}

unsafe extern "system" fn destroy_instance(inst: vk::Instance, p_alloc: *const c_void) {
    if inst == vk::Instance::null() {
        return;
    }
    let data = instances()
        .lock()
        .ok()
        // SAFETY: `inst` is non-null, and vkDestroyInstance requires a live instance handle —
        // still live during this call — whose first word is the dispatch key.
        .and_then(|mut g| g.remove(&unsafe { key(inst.as_raw()) }));
    if let Some(d) = data
        && let Some(f) = d.destroy_instance
    {
        // SAFETY: `f` is the down-chain vkDestroyInstance resolved for this very instance at
        // create time; forwarding the caller's own arguments unchanged.
        unsafe { f(inst, p_alloc) };
    }
}

// ---- device chain (pass-through; keeps device-level dispatch working) ----

/// Walk `p_ci`'s pNext chain for the loader's device layer-link node.
///
/// # Safety
/// Same contract as [`find_instance_link`]: `p_ci` must point to a valid create-info struct
/// whose `pNext` chain is a well-formed list of `{sType, pNext}`-headed nodes.
unsafe fn find_device_link(p_ci: *const c_void) -> *mut LayerDeviceCreateInfo {
    // SAFETY: per this function's contract, `p_ci` and every non-null `pNext` reached from it
    // point at nodes beginning with a {sType, pNext} header; a node whose sType says it is the
    // loader's device create-info really is a LayerDeviceCreateInfo (the loader wrote it).
    unsafe {
        let mut node = (*(p_ci as *const BaseIn)).p_next as *const BaseIn;
        while !node.is_null() {
            if (*node).s_type.as_raw() == LOADER_DEVICE_CREATE_INFO {
                let lci = node as *mut LayerDeviceCreateInfo;
                if (*lci).function == VK_LAYER_LINK_INFO {
                    return lci;
                }
            }
            node = (*node).p_next as *const BaseIn;
        }
    }
    ptr::null_mut()
}

unsafe extern "system" fn create_device(
    pdev: vk::PhysicalDevice,
    p_ci: *const c_void,
    p_alloc: *const c_void,
    p_dev: *mut vk::Device,
) -> vk::Result {
    // SAFETY: the loader invokes this hook with a valid VkDeviceCreateInfo whose pNext chain it
    // built — the chain find_device_link's contract requires.
    let lci = unsafe { find_device_link(p_ci) };
    if lci.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: `lci` is non-null and points into the loader's live chain. Its `u` link (checked
    // non-null before use) is this layer's link entry; reading the down-chain pointers and
    // advancing `(*lci).u` is the layer protocol, exactly as in create_instance.
    let (next_gipa, next_gdpa) = unsafe {
        let link = (*lci).u;
        if link.is_null() {
            return vk::Result::ERROR_INITIALIZATION_FAILED;
        }
        let gipa = (*link).next_gipa;
        let gdpa = (*link).next_gdpa;
        (*lci).u = (*link).p_next;
        (gipa, gdpa)
    };

    // SAFETY: vkCreateDevice requires `pdev` to be a live physical-device handle — a
    // dispatchable object sharing its instance's dispatch table, so its first word is the same
    // dispatch key create_instance stored.
    let inst =
        unsafe { lookup(pdev.as_raw(), |d| Some(d.instance)) }.unwrap_or(vk::Instance::null());

    // SAFETY: `next_gipa` is the loader-supplied down-chain GIPA for this create call, `inst` is
    // the (possibly null) instance owning `pdev`, and FnCreateDevice mirrors vkCreateDevice's
    // prototype.
    let create: FnCreateDevice = match unsafe { resolve(next_gipa, inst, c"vkCreateDevice") } {
        Some(f) => f,
        None => return vk::Result::ERROR_INITIALIZATION_FAILED,
    };
    // SAFETY: forwarding the loader's own arguments unchanged to the down-chain create.
    let res = unsafe { create(pdev, p_ci, p_alloc, p_dev) };
    if res != vk::Result::SUCCESS {
        return res;
    }
    // SAFETY: vkCreateDevice requires pDevice to be a valid pointer, and on SUCCESS the
    // down-chain just wrote the new device handle through it.
    let dev = unsafe { *p_dev };
    if let Ok(mut g) = devices().lock() {
        // SAFETY: `dev` is the live dispatchable handle created above — its first word is the
        // dispatch key.
        g.insert(unsafe { key(dev.as_raw()) }, DeviceData { next_gdpa });
    }
    vk::Result::SUCCESS
}

// ---- surface tracking (so we can resolve a surface's monitor) ----

unsafe extern "system" fn create_win32_surface(
    inst: vk::Instance,
    p_ci: *const c_void,
    p_alloc: *const c_void,
    p_surface: *mut vk::SurfaceKHR,
) -> vk::Result {
    // SAFETY: vkCreateWin32SurfaceKHR requires `inst` to be a live instance handle.
    let down = match unsafe { lookup(inst.as_raw(), |d| d.create_win32_surface) } {
        Some(f) => f,
        None => return vk::Result::ERROR_EXTENSION_NOT_PRESENT,
    };
    // SAFETY: `down` is the down-chain vkCreateWin32SurfaceKHR resolved for this instance at
    // create time; forwarding the caller's own arguments unchanged.
    let res = unsafe { down(inst, p_ci, p_alloc, p_surface) };
    if res == vk::Result::SUCCESS {
        // SAFETY: the down-chain call succeeded, so `p_ci` is the live, aligned
        // VkWin32SurfaceCreateInfoKHR it just consumed. The C-layout mirror exposes its HWND
        // without architecture-specific byte arithmetic.
        let hwnd = unsafe { (*(p_ci as *const Win32SurfaceCreateInfoRaw)).hwnd as isize };
        if let Ok(mut m) = surface_hwnds().lock() {
            // SAFETY: vkCreateWin32SurfaceKHR requires pSurface to be a valid pointer, and on
            // SUCCESS the down-chain just wrote the new surface handle through it.
            m.insert(unsafe { *p_surface }.as_raw(), hwnd);
        }
    }
    res
}

unsafe extern "system" fn destroy_surface(
    inst: vk::Instance,
    surface: vk::SurfaceKHR,
    p_alloc: *const c_void,
) {
    if let Ok(mut m) = surface_hwnds().lock() {
        m.remove(&surface.as_raw());
    }
    // SAFETY: vkDestroySurfaceKHR requires `inst` to be a live instance handle.
    if let Some(f) = unsafe { lookup(inst.as_raw(), |d| d.destroy_surface) } {
        // SAFETY: `f` is the down-chain vkDestroySurfaceKHR resolved for this instance at
        // create time; forwarding the caller's own arguments unchanged.
        unsafe { f(inst, surface, p_alloc) };
    }
}

// ---- the actual fix: append HDR surface formats (self-gated on display HDR state) ----

unsafe extern "system" fn get_surface_formats(
    pdev: vk::PhysicalDevice,
    surface: vk::SurfaceKHR,
    p_count: *mut u32,
    p_formats: *mut vk::SurfaceFormatKHR,
) -> vk::Result {
    if p_count.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: vkGetPhysicalDeviceSurfaceFormatsKHR requires `pdev` to be a live physical-device
    // handle.
    let down = match unsafe { lookup(pdev.as_raw(), |d| d.get_surface_formats) } {
        Some(f) => f,
        None => return vk::Result::ERROR_INITIALIZATION_FAILED,
    };

    let mut n = 0u32;
    // SAFETY: count-query form of the two-call idiom — null pFormats plus a count out-pointer
    // backed by the live local `n`; the caller's pdev/surface are forwarded unchanged.
    let r = unsafe { down(pdev, surface, &mut n, ptr::null_mut()) };
    if r != vk::Result::SUCCESS {
        return r;
    }
    let mut real = vec![vk::SurfaceFormatKHR::default(); n as usize];
    if n > 0 {
        // SAFETY: `real` holds exactly `n` elements and `n` is passed as the in/out capacity, so
        // the down-chain writes at most `n` entries and shrinks `n` to what it wrote.
        let r = unsafe { down(pdev, surface, &mut n, real.as_mut_ptr()) };
        if r != vk::Result::SUCCESS {
            return r;
        }
    }
    real.truncate(n as usize);

    let mut aug = real;
    if !aug.is_empty() && should_inject(surface) {
        for e in hdr_extra() {
            if !aug
                .iter()
                .any(|x| x.format == e.format && x.color_space == e.color_space)
            {
                aug.push(e);
            }
        }
    }

    if p_formats.is_null() {
        // SAFETY: vkGetPhysicalDeviceSurfaceFormatsKHR requires pSurfaceFormatCount to be a
        // valid u32 pointer in both call forms.
        unsafe { *p_count = aug.len() as u32 };
        return vk::Result::SUCCESS;
    }
    // SAFETY: with a non-null pSurfaceFormats the spec requires *pSurfaceFormatCount to be
    // readable and pSurfaceFormats to point at at least that many elements; `m` is clamped to
    // both that capacity and our own length, and the caller's buffer cannot overlap the
    // freshly-allocated `aug`.
    unsafe {
        let m = (*p_count as usize).min(aug.len());
        ptr::copy_nonoverlapping(aug.as_ptr(), p_formats, m);
        *p_count = m as u32;
        if m < aug.len() {
            vk::Result::INCOMPLETE
        } else {
            vk::Result::SUCCESS
        }
    }
}

/// A non-null array goes down to the driver as the application built it, so the driver fills each
/// element's `pNext` chain, and the missing HDR formats are appended after the driver's own.
/// The driver never saw those entries: a struct the application chained to one stays as it was.
/// A count query has no array, so the driver fills a scratch one to deduplicate against.
unsafe extern "system" fn get_surface_formats2(
    pdev: vk::PhysicalDevice,
    p_info: *const c_void,
    p_count: *mut u32,
    p_formats: *mut c_void,
) -> vk::Result {
    if p_info.is_null() || p_count.is_null() {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    }
    // SAFETY: vkGetPhysicalDeviceSurfaceFormats2KHR requires `pdev` to be a live physical-device
    // handle.
    let down = match unsafe { lookup(pdev.as_raw(), |d| d.get_surface_formats2) } {
        Some(f) => f,
        None => return vk::Result::ERROR_INITIALIZATION_FAILED,
    };
    // SAFETY: the spec requires `p_info` to be a live, aligned VkPhysicalDeviceSurfaceInfo2KHR.
    // The C-layout mirror reads its typed surface field without relying on one target's padding.
    let surface = unsafe { (*(p_info as *const PhysicalDeviceSurfaceInfo2Raw)).surface };

    if p_formats.is_null() {
        let mut n = 0u32;
        // SAFETY: count-query form of the two-call idiom — null pSurfaceFormats plus a count
        // out-pointer backed by the live local `n`; the caller's pdev/pSurfaceInfo are forwarded
        // unchanged (the spec requires pSurfaceInfo to stay valid for the call).
        let r = unsafe { down(pdev, p_info, &mut n, ptr::null_mut()) };
        if r != vk::Result::SUCCESS {
            return r;
        }
        let mut real: Vec<SurfaceFormat2Raw> = (0..n)
            .map(|_| SurfaceFormat2Raw {
                s_type: vk::StructureType::SURFACE_FORMAT_2_KHR,
                p_next: ptr::null_mut(),
                surface_format: vk::SurfaceFormatKHR::default(),
            })
            .collect();
        if n > 0 {
            // SAFETY: `real` holds exactly `n` properly-stamped VkSurfaceFormat2KHR mirrors
            // (SurfaceFormat2Raw layout-asserted at the type), and `n` is passed as the in/out
            // capacity, so the down-chain writes at most `n` entries.
            let r = unsafe { down(pdev, p_info, &mut n, real.as_mut_ptr() as *mut c_void) };
            if r != vk::Result::SUCCESS {
                return r;
            }
        }
        real.truncate(n as usize);
        let extra = if !real.is_empty() && should_inject(surface) {
            missing_hdr(&real).len()
        } else {
            0
        };
        // SAFETY: vkGetPhysicalDeviceSurfaceFormats2KHR requires pSurfaceFormatCount to be a
        // valid u32 pointer in both call forms.
        unsafe { *p_count = (real.len() + extra) as u32 };
        return vk::Result::SUCCESS;
    }

    // SAFETY: with a non-null pSurfaceFormats the spec requires *pSurfaceFormatCount to be a
    // readable u32 holding the array's capacity.
    let cap = unsafe { *p_count } as usize;
    // SAFETY: the caller's own array and count go down unchanged, so valid usage for this call is
    // exactly valid usage for the down-chain call.
    let r = unsafe { down(pdev, p_info, p_count, p_formats) };
    if r != vk::Result::SUCCESS {
        // INCOMPLETE: the driver's own formats already fill the array.
        return r;
    }
    // SAFETY: the down-chain wrote the count through `p_count`; it is clamped to the capacity.
    let n = (unsafe { *p_count } as usize).min(cap);
    if n == 0 || !should_inject(surface) {
        return r;
    }
    // SAFETY: the array holds `cap` VkSurfaceFormat2KHR — whose layout SurfaceFormat2Raw mirrors
    // (asserted at the type) — and the down-chain initialised the first `n <= cap`.
    let (count, complete) = unsafe { append_hdr(p_formats as *mut SurfaceFormat2Raw, n, cap) };
    // SAFETY: `p_count` is the caller's valid count pointer, read above.
    unsafe { *p_count = count as u32 };
    if complete {
        vk::Result::SUCCESS
    } else {
        vk::Result::INCOMPLETE
    }
}

/// The HDR formats `real` does not list yet.
fn missing_hdr(real: &[SurfaceFormat2Raw]) -> Vec<vk::SurfaceFormatKHR> {
    hdr_extra()
        .into_iter()
        .filter(|e| {
            !real.iter().any(|x| {
                x.surface_format.format == e.format && x.surface_format.color_space == e.color_space
            })
        })
        .collect()
}

/// Writes the HDR formats `out[..n]` lacks into `out[n..cap]`. Returns the new count and whether
/// every one fit. Only sType and surfaceFormat of an appended element are written.
///
/// # Safety
/// `out` points at `cap` VkSurfaceFormat2KHR elements, the first `n <= cap` of them initialised.
unsafe fn append_hdr(out: *mut SurfaceFormat2Raw, n: usize, cap: usize) -> (usize, bool) {
    // SAFETY: the first `n` elements are initialised, per this function's contract.
    let missing = missing_hdr(unsafe { std::slice::from_raw_parts(out, n) });
    let fit = missing.len().min(cap - n);
    for (i, sf) in missing.iter().take(fit).enumerate() {
        // SAFETY: `n + i < n + fit <= cap`, inside the array this function's contract describes.
        unsafe {
            let dst = out.add(n + i);
            (*dst).s_type = vk::StructureType::SURFACE_FORMAT_2_KHR;
            (*dst).surface_format = *sf;
        }
    }
    (n + fit, fit == missing.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elem(tag: usize, f: vk::SurfaceFormatKHR) -> SurfaceFormat2Raw {
        SurfaceFormat2Raw {
            s_type: vk::StructureType::SURFACE_FORMAT_2_KHR,
            p_next: tag as *mut c_void,
            surface_format: f,
        }
    }

    fn sdr() -> vk::SurfaceFormatKHR {
        vk::SurfaceFormatKHR {
            format: vk::Format::B8G8R8A8_UNORM,
            color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
        }
    }

    fn array(cap: usize, n: usize) -> Vec<SurfaceFormat2Raw> {
        (0..cap)
            .map(|i| elem(i + 1, if i < n { sdr() } else { Default::default() }))
            .collect()
    }

    #[test]
    fn appended_elements_keep_the_applications_chain() {
        let mut out = array(4, 2);
        // SAFETY: `out` holds 4 elements and the first 2 are initialised.
        let (count, complete) = unsafe { append_hdr(out.as_mut_ptr(), 2, 4) };
        assert_eq!((count, complete), (4, true));
        assert_eq!(out[2].surface_format.format, hdr_extra()[0].format);
        assert_eq!(out[3].surface_format.format, hdr_extra()[1].format);
        for (i, e) in out.iter().enumerate() {
            assert_eq!(e.p_next as usize, i + 1);
        }
    }

    #[test]
    fn a_short_array_reports_incomplete() {
        let mut out = array(3, 2);
        // SAFETY: `out` holds 3 elements and the first 2 are initialised.
        let (count, complete) = unsafe { append_hdr(out.as_mut_ptr(), 2, 3) };
        assert_eq!((count, complete), (3, false));
    }

    #[test]
    fn a_format_the_driver_listed_is_not_repeated() {
        let mut out = array(4, 2);
        out[1].surface_format = hdr_extra()[0];
        // SAFETY: `out` holds 4 elements and the first 2 are initialised.
        let (count, complete) = unsafe { append_hdr(out.as_mut_ptr(), 2, 4) };
        assert_eq!((count, complete), (3, true));
        assert_eq!(out[2].surface_format.format, hdr_extra()[1].format);
    }
}
