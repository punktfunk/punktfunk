//! AMD ADL connector/EDID emulation, shared by the `display-disturb adl-emul`
//! probe and the host `edid_lock` axis.
//!
//! A connected-but-asleep sink is still serviced by the KMD below every OS
//! lever (CCD, devnode disable, CRU). Pinning the live EDID with
//! `ADL2_Adapter_ConnectionData_Set` then `ADL_EMUL_MODE_ALWAYS` is the one
//! software stop at the source. Evidence:
//! `design/vdisplay-disturbance-immunity.md`.
//!
//! [`run`] walks every AMD adapter's connectors and returns per-op
//! [`OpRecord`]s. `edid_lock` drives [`lock_for_stream`] /
//! [`unlock_after_stream`] at Exclusive isolate; [`startup_recover`] clears
//! leftovers because pinned emulation survives process death and can survive
//! reboot. Driver reinstall is the last-resort escape.
//!
//! Best-effort: missing `atiadlxx.dll` makes the axis inert. Each rc is kept
//! so a log can tell `ADL_ERR_NOT_SUPPORTED(-8)` from `ADL_OK`.

// FFI mirrors of ADL C structs. AMD field names stay verbatim so a header diff is mechanical.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::time::Instant;

use windows::core::{s, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExA, LOAD_LIBRARY_SEARCH_SYSTEM32,
};

// adl_defines.h (GPUOpen display-library).

const ADL_OK: i32 = 0;
const ADL_MAX_PATH: usize = 256;
const ADL_MAX_DISPLAY_EDID_DATA_SIZE: usize = 1024;
const ADL_MAX_RAD_LINK_COUNT: usize = 15;

const ADL_EMUL_MODE_OFF: i32 = 0;
const ADL_EMUL_MODE_ALWAYS: i32 = 3;

const ADL_QUERY_REAL_DATA: i32 = 0;
const ADL_QUERY_EMULATED_DATA: i32 = 1;

const ADL_EMUL_STATUS_REAL_DEVICE_CONNECTED: i32 = 0x1;
const ADL_EMUL_STATUS_EMULATED_DEVICE_PRESENT: i32 = 0x2;
const ADL_EMUL_STATUS_EMULATED_DEVICE_USED: i32 = 0x4;

const AMD_VENDOR_ID: i32 = 1002;

/// Named ADL rcs from adl_defines.h. `-8` vs `-1` is consumer vs Pro.
pub fn rc_str(rc: i32) -> &'static str {
    match rc {
        0 => "ADL_OK",
        1..=4 => "ADL_OK_(warning-class)",
        -1 => "ADL_ERR",
        -2 => "ADL_ERR_NOT_INIT",
        -3 => "ADL_ERR_INVALID_PARAM",
        -5 => "ADL_ERR_INVALID_ADL_IDX",
        -8 => "ADL_ERR_NOT_SUPPORTED",
        -9 => "ADL_ERR_NULL_POINTER",
        -10 => "ADL_ERR_DISABLED_ADAPTER",
        -22 => "ADL_ERR_CALL_TO_INCOMPATIABLE_DRIVER",
        -23 => "ADL_ERR_NO_ADMINISTRATOR_PRIVILEGES",
        _ => "?",
    }
}

fn connector_type_str(t: i32) -> &'static str {
    match t {
        1 => "VGA",
        2 => "DVI-D",
        3 => "DVI-I",
        8 => "HDMI-A",
        9 => "HDMI-B",
        10 => "DP",
        11 => "eDP",
        12 => "miniDP",
        13 => "VIRTUAL",
        14 => "USB-C",
        _ => "unknown",
    }
}

// adl_structures.h, verbatim layouts.

#[repr(C)]
struct AdapterInfo {
    iSize: i32,
    iAdapterIndex: i32,
    strUDID: [u8; ADL_MAX_PATH],
    iBusNumber: i32,
    iDeviceNumber: i32,
    iFunctionNumber: i32,
    iVendorID: i32,
    strAdapterName: [u8; ADL_MAX_PATH],
    strDisplayName: [u8; ADL_MAX_PATH],
    iPresent: i32,
    // _WIN32 tail; this crate is Windows-only.
    iExist: i32,
    strDriverPath: [u8; ADL_MAX_PATH],
    strDriverPathExt: [u8; ADL_MAX_PATH],
    strPNPString: [u8; ADL_MAX_PATH],
    iOSDisplayIndex: i32,
}

impl AdapterInfo {
    /// All zero with `iSize` stamped, as ADL expects of each array entry.
    fn stamped() -> Self {
        let path = [0; ADL_MAX_PATH];
        Self {
            iSize: std::mem::size_of::<Self>() as i32,
            iAdapterIndex: 0,
            strUDID: path,
            iBusNumber: 0,
            iDeviceNumber: 0,
            iFunctionNumber: 0,
            iVendorID: 0,
            strAdapterName: path,
            strDisplayName: path,
            iPresent: 0,
            iExist: 0,
            strDriverPath: path,
            strDriverPathExt: path,
            strPNPString: path,
            iOSDisplayIndex: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLMSTRad {
    iLinkNumber: i32,
    rad: [u8; ADL_MAX_RAD_LINK_COUNT],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLDevicePort {
    iConnectorIndex: i32,
    aMSTRad: ADLMSTRad,
}

impl ADLDevicePort {
    /// Non-MST port. All-zero RAD is "DP root / non-DP ignored" per the header.
    fn root(connector: i32) -> Self {
        Self {
            iConnectorIndex: connector,
            aMSTRad: ADLMSTRad {
                iLinkNumber: 0,
                rad: [0; ADL_MAX_RAD_LINK_COUNT],
            },
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ADLConnectionProperties {
    iValidProperties: i32,
    iBitrate: i32,
    iNumberOfLanes: i32,
    iColorDepth: i32,
    iStereo3DCaps: i32,
    iOutputBandwidth: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLConnectionData {
    iConnectionType: i32,
    aConnectionProperties: ADLConnectionProperties,
    iNumberofPorts: i32,
    iActiveConnections: i32,
    iDataSize: i32,
    EdidData: [u8; ADL_MAX_DISPLAY_EDID_DATA_SIZE],
}

impl Default for ADLConnectionData {
    fn default() -> Self {
        Self {
            iConnectionType: 0,
            aConnectionProperties: ADLConnectionProperties::default(),
            iNumberofPorts: 0,
            iActiveConnections: 0,
            iDataSize: 0,
            EdidData: [0; ADL_MAX_DISPLAY_EDID_DATA_SIZE],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ADLConnectionState {
    iEmulationStatus: i32,
    iEmulationMode: i32,
    iDisplayIndex: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ADLConnectorInfo {
    iConnectorIndex: i32,
    iConnectorId: i32,
    iSlotIndex: i32,
    iType: i32,
    iOffset: i32,
    iLength: i32,
}

// Dynamic bind of atiadlxx.dll (ships with the AMD driver; absent elsewhere).

type AdlContext = *mut c_void;
type MallocCb = unsafe extern "C" fn(i32) -> *mut c_void;

type FnMainCreate = unsafe extern "C" fn(MallocCb, i32, *mut AdlContext) -> i32;
type FnMainDestroy = unsafe extern "C" fn(AdlContext) -> i32;
type FnNumAdapters = unsafe extern "C" fn(AdlContext, *mut i32) -> i32;
type FnAdapterInfoGet = unsafe extern "C" fn(AdlContext, *mut AdapterInfo, i32) -> i32;
type FnEdidMgmtCaps = unsafe extern "C" fn(AdlContext, i32, *mut i32) -> i32;
type FnBoardLayoutGet = unsafe extern "C" fn(
    AdlContext,
    i32,
    *mut i32,
    *mut i32,
    *mut *mut c_void,
    *mut i32,
    *mut *mut ADLConnectorInfo,
) -> i32;
type FnConnStateGet =
    unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, *mut ADLConnectionState) -> i32;
type FnConnDataGet =
    unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, i32, *mut ADLConnectionData) -> i32;
type FnConnDataSet = unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, ADLConnectionData) -> i32;
type FnConnDataRemove = unsafe extern "C" fn(AdlContext, i32, ADLDevicePort) -> i32;
type FnEmulModeSet = unsafe extern "C" fn(AdlContext, i32, ADLDevicePort, i32) -> i32;

struct Adl {
    create: FnMainCreate,
    destroy: FnMainDestroy,
    num_adapters: FnNumAdapters,
    adapter_info: FnAdapterInfoGet,
    edid_caps: FnEdidMgmtCaps,
    board_layout: FnBoardLayoutGet,
    conn_state: FnConnStateGet,
    conn_data_get: FnConnDataGet,
    conn_data_set: FnConnDataSet,
    conn_data_remove: FnConnDataRemove,
    emul_mode_set: FnEmulModeSet,
}

/// ADL allocator callback: board-layout arrays come back as out-pointers the app owns.
unsafe extern "C" fn adl_malloc(size: i32) -> *mut c_void {
    let size = size.max(1) as usize;
    // Do not panic: it would cross the extern boundary and abort the host. Null is an
    // ordinary ADL failure; the Err arm is unreachable (size ≤ i32::MAX).
    let Ok(layout) = std::alloc::Layout::from_size_align(size, 16) else {
        return std::ptr::null_mut();
    };
    // SAFETY: non-zero size, align 16. Never freed: ADL wants ADL_Main_Memory_Free
    // symmetry, and leaking the board-layout arrays is simpler than proving parity.
    unsafe { std::alloc::alloc(layout) as *mut c_void }
}

impl Adl {
    fn load() -> Option<Self> {
        // The AMD driver installs this into System32. Search only there: a plain LoadLibrary
        // tries the exe's own directory, the cwd and `%PATH%` first, and this runs as SYSTEM.
        // SAFETY: LoadLibraryEx of the driver-installed ADL runtime by its well-known name.
        let lib: HMODULE =
            unsafe { LoadLibraryExA(s!("atiadlxx.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
                .ok()?;
        // Each Fn* matches the ADL C signature (`extern "C"` is exact on x64).
        unsafe fn sym<T: Copy>(lib: HMODULE, name: PCSTR) -> Option<T> {
            debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>());
            // SAFETY: caller passes a fn-pointer type T of pointer size (asserted above);
            // GetProcAddress yields the export's address or None.
            let f = unsafe { GetProcAddress(lib, name) }?;
            // SAFETY: reinterpreting one non-null fn pointer as the export's true C signature.
            Some(unsafe { std::mem::transmute_copy::<_, T>(&f) })
        }
        // SAFETY: `lib` is the live module handle from the successful load above.
        unsafe {
            Some(Self {
                create: sym(lib, s!("ADL2_Main_Control_Create"))?,
                destroy: sym(lib, s!("ADL2_Main_Control_Destroy"))?,
                num_adapters: sym(lib, s!("ADL2_Adapter_NumberOfAdapters_Get"))?,
                adapter_info: sym(lib, s!("ADL2_Adapter_AdapterInfo_Get"))?,
                edid_caps: sym(lib, s!("ADL2_Adapter_EDIDManagement_Caps"))?,
                board_layout: sym(lib, s!("ADL2_Adapter_BoardLayout_Get"))?,
                conn_state: sym(lib, s!("ADL2_Adapter_ConnectionState_Get"))?,
                conn_data_get: sym(lib, s!("ADL2_Adapter_ConnectionData_Get"))?,
                conn_data_set: sym(lib, s!("ADL2_Adapter_ConnectionData_Set"))?,
                conn_data_remove: sym(lib, s!("ADL2_Adapter_ConnectionData_Remove"))?,
                emul_mode_set: sym(lib, s!("ADL2_Adapter_EmulationMode_Set"))?,
            })
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EmulAction {
    /// Read-only: caps + layout + per-connector state. Always safe.
    Probe,
    /// Pin live EDID + `ADL_EMUL_MODE_ALWAYS` on occupied (or named) connectors.
    Lock,
    /// `ADL_EMUL_MODE_OFF` + remove pinned EDID on all (or named) connectors.
    Unlock,
}

pub struct OpRecord {
    pub op: &'static str,
    pub target: String,
    pub took_ms: u128,
    pub rc: i32,
    pub extra: String,
}

impl OpRecord {
    pub fn ok(&self) -> bool {
        self.rc == ADL_OK
    }
}

impl std::fmt::Display for OpRecord {
    /// `op target took_ms ok rc=N(STR) extra`. The caller adds its epoch prefix.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sep = if self.extra.is_empty() { "" } else { " " };
        write!(
            f,
            "{} {} took_ms={} ok={} rc={}({}){sep}{}",
            self.op,
            self.target,
            self.took_ms,
            self.ok(),
            self.rc,
            rc_str(self.rc),
            self.extra
        )
    }
}

/// End of [`run`]. `ADL_ERR_NOT_SUPPORTED` still [`Done`]s; each op's rc is in the records.
pub enum RunOutcome {
    /// `atiadlxx.dll` missing (or an export). No emulation lever on this box.
    NoAdl,
    /// Create or adapter enumeration failed; records hold the rc.
    InitFailed(Vec<OpRecord>),
    Done(Vec<OpRecord>),
}

impl RunOutcome {
    pub fn records(&self) -> &[OpRecord] {
        match self {
            RunOutcome::NoAdl => &[],
            RunOutcome::InitFailed(r) | RunOutcome::Done(r) => r,
        }
    }
}

fn c_str(buf: &[u8]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// Whether `atiadlxx.dll` loads with every export this module binds.
///
/// The one-shot answer to "does this box have the EDID-lock lever at all". Probed once
/// and cached: the module stays loaded for the process either way.
pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| Adl::load().is_some())
}

/// Apply `action` to every AMD adapter connector, or only `connector_filter`.
/// [`EmulAction::Lock`] pins occupied connectors only unless the filter names one.
pub fn run(action: EmulAction, connector_filter: Option<i32>) -> RunOutcome {
    let mut recs: Vec<OpRecord> = Vec::new();
    let mut rec = |op: &'static str, target: &str, took_ms: u128, rc: i32, extra: String| {
        recs.push(OpRecord {
            op,
            target: target.to_owned(),
            took_ms,
            rc,
            extra,
        });
    };

    let Some(adl) = Adl::load() else {
        return RunOutcome::NoAdl;
    };

    let mut ctx: AdlContext = std::ptr::null_mut();
    let t = Instant::now();
    // SAFETY: documented init. Allocator callback; iEnumConnectedAdapters=0 (all adapters —
    // Exclusive isolate may report no connected display); valid out-slot.
    let rc = unsafe { (adl.create)(adl_malloc, 0, &mut ctx) };
    rec(
        "adl-init",
        "atiadlxx",
        t.elapsed().as_millis(),
        rc,
        String::new(),
    );
    if rc != ADL_OK {
        return RunOutcome::InitFailed(recs);
    }

    let mut count = 0i32;
    // SAFETY: live context; valid out-param.
    let rc = unsafe { (adl.num_adapters)(ctx, &mut count) };
    if rc != ADL_OK || count <= 0 {
        rec("adl-num-adapters", "all", 0, rc, format!("count={count}"));
        // SAFETY: destroying the context created above; nothing ADL-owned is used past this point.
        let _ = unsafe { (adl.destroy)(ctx) };
        return RunOutcome::InitFailed(recs);
    }
    let mut infos: Vec<AdapterInfo> = (0..count).map(|_| AdapterInfo::stamped()).collect();
    let bytes = std::mem::size_of_val(infos.as_slice()) as i32;
    // SAFETY: caller-allocated array of exactly `count` stamped entries, byte size passed as the
    // API's iInputSize contract requires.
    let rc = unsafe { (adl.adapter_info)(ctx, infos.as_mut_ptr(), bytes) };
    rec("adl-adapters", "all", 0, rc, format!("count={count}"));
    if rc != ADL_OK {
        // SAFETY: destroying the context created above; nothing ADL-owned is used past this point.
        let _ = unsafe { (adl.destroy)(ctx) };
        return RunOutcome::InitFailed(recs);
    }

    // One record per distinct (bus, vendor, present) so a walk of nothing still says why.
    let mut seen_shapes: Vec<(i32, i32, i32)> = Vec::new();
    for info in &infos {
        let shape = (info.iBusNumber, info.iVendorID, info.iPresent);
        if seen_shapes.contains(&shape) {
            continue;
        }
        seen_shapes.push(shape);
        rec(
            "adl-adapter-seen",
            &format!("bus{}", info.iBusNumber),
            0,
            ADL_OK,
            format!(
                "vendor_id={} present={} name={}",
                info.iVendorID,
                info.iPresent,
                c_str(&info.strAdapterName).trim()
            ),
        );
    }

    // One GPU is many logical adapters; walk each bus once, AMD only.
    // Probe includes non-present adapters (headless iGPU reports iPresent=0 and still answers caps).
    // Lock/Unlock require present: pinning a connector with no displays is a different experiment.
    let mut seen_buses: Vec<i32> = Vec::new();
    for info in &infos {
        let present_ok =
            info.iPresent != 0 || (matches!(action, EmulAction::Probe) && info.iBusNumber >= 0);
        if !present_ok || info.iVendorID != AMD_VENDOR_ID || seen_buses.contains(&info.iBusNumber) {
            continue;
        }
        seen_buses.push(info.iBusNumber);
        let idx = info.iAdapterIndex;
        let name = c_str(&info.strAdapterName);
        let target = format!("adapter{idx}[{}]", name.trim());

        let mut supported = 0i32;
        let t = Instant::now();
        // SAFETY: live context, adapter index from this enumeration, valid out-param.
        let rc = unsafe { (adl.edid_caps)(ctx, idx, &mut supported) };
        rec(
            "adl-edid-caps",
            &target,
            t.elapsed().as_millis(),
            rc,
            format!("supported={supported}"),
        );

        let (mut valid, mut n_slots, mut n_conn) = (0i32, 0i32, 0i32);
        let mut slots: *mut c_void = std::ptr::null_mut();
        let mut connectors: *mut ADLConnectorInfo = std::ptr::null_mut();
        let t = Instant::now();
        // SAFETY: live context + adapter index; out-pointers valid; ADL allocates the two arrays
        // through `adl_malloc` (deliberately leaked, see there).
        let rc = unsafe {
            (adl.board_layout)(
                ctx,
                idx,
                &mut valid,
                &mut n_slots,
                &mut slots,
                &mut n_conn,
                &mut connectors,
            )
        };
        rec(
            "adl-board-layout",
            &target,
            t.elapsed().as_millis(),
            rc,
            format!("connectors={n_conn} valid_flags={valid:#x}"),
        );
        let connector_list: &[ADLConnectorInfo] =
            if rc == ADL_OK && !connectors.is_null() && n_conn > 0 {
                // SAFETY: ADL just filled `connectors` with `n_conn` entries via our allocator; the
                // (leaked) buffer outlives this borrow.
                unsafe { std::slice::from_raw_parts(connectors, n_conn as usize) }
            } else {
                &[]
            };

        for c in connector_list {
            if connector_filter.is_some_and(|want| want != c.iConnectorIndex) {
                continue;
            }
            let port = ADLDevicePort::root(c.iConnectorIndex);
            let ctarget = format!(
                "adapter{idx}.connector{}[{}]",
                c.iConnectorIndex,
                connector_type_str(c.iType)
            );

            let mut state = ADLConnectionState::default();
            let t = Instant::now();
            // SAFETY: live context; port is a by-value POD naming a connector this adapter just
            // enumerated; valid out-param.
            let rc = unsafe { (adl.conn_state)(ctx, idx, port, &mut state) };
            let real = state.iEmulationStatus & ADL_EMUL_STATUS_REAL_DEVICE_CONNECTED != 0;
            rec(
                "adl-conn-state",
                &ctarget,
                t.elapsed().as_millis(),
                rc,
                format!(
                    "status={:#x} real_connected={} emulated_present={} emulated_used={} mode={} display={}",
                    state.iEmulationStatus,
                    real,
                    state.iEmulationStatus & ADL_EMUL_STATUS_EMULATED_DEVICE_PRESENT != 0,
                    state.iEmulationStatus & ADL_EMUL_STATUS_EMULATED_DEVICE_USED != 0,
                    state.iEmulationMode,
                    state.iDisplayIndex,
                ),
            );
            if rc != ADL_OK {
                continue;
            }

            match action {
                EmulAction::Probe => {
                    let mut data = ADLConnectionData::default();
                    let t = Instant::now();
                    // SAFETY: live context/port as above; REAL query fills `data` in place.
                    let rc = unsafe {
                        (adl.conn_data_get)(ctx, idx, port, ADL_QUERY_REAL_DATA, &mut data)
                    };
                    rec(
                        "adl-conn-data",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        format!(
                            "type={} edid_bytes={}",
                            data.iConnectionType, data.iDataSize
                        ),
                    );
                }
                EmulAction::Lock => {
                    if !real && connector_filter.is_none() {
                        continue; // unoccupied and unfiltered: pinning empty is a different experiment
                    }
                    // Emulation already pinned before us is the operator's (or a vendor tool's):
                    // never leased, never unlocked by our teardown (immunity plan WP11).
                    if state.iEmulationMode == ADL_EMUL_MODE_ALWAYS && connector_filter.is_none() {
                        rec(
                            "adl-lock-skip-preexisting",
                            &ctarget,
                            0,
                            ADL_OK,
                            format!("mode={}", state.iEmulationMode),
                        );
                        continue;
                    }
                    let mut data = ADLConnectionData::default();
                    let t = Instant::now();
                    // SAFETY: live context/port; REAL query first so the pin is the sink's current EDID.
                    let mut rc = unsafe {
                        (adl.conn_data_get)(ctx, idx, port, ADL_QUERY_REAL_DATA, &mut data)
                    };
                    if rc != ADL_OK {
                        // Asleep sinks can refuse a live EDID read; fall back to the driver's emulated data.
                        // SAFETY: same contract, emulated-data query.
                        rc = unsafe {
                            (adl.conn_data_get)(ctx, idx, port, ADL_QUERY_EMULATED_DATA, &mut data)
                        };
                    }
                    rec(
                        "adl-lock-read",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        format!(
                            "type={} edid_bytes={}",
                            data.iConnectionType, data.iDataSize
                        ),
                    );
                    if rc != ADL_OK || data.iDataSize <= 0 {
                        continue;
                    }
                    let t = Instant::now();
                    // SAFETY: live context/port; `data` passed by value per the ADL signature.
                    let rc = unsafe { (adl.conn_data_set)(ctx, idx, port, data) };
                    rec(
                        "adl-lock-set",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                    let t = Instant::now();
                    // SAFETY: live context/port; mode constant from the header.
                    let rc = unsafe { (adl.emul_mode_set)(ctx, idx, port, ADL_EMUL_MODE_ALWAYS) };
                    rec(
                        "adl-lock-mode-always",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                }
                EmulAction::Unlock => {
                    let t = Instant::now();
                    // SAFETY: live context/port; mode constant from the header.
                    let rc = unsafe { (adl.emul_mode_set)(ctx, idx, port, ADL_EMUL_MODE_OFF) };
                    rec(
                        "adl-unlock-mode-off",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                    let t = Instant::now();
                    // SAFETY: live context/port; removes earlier emulation data (harmless if none; the rc says so).
                    let rc = unsafe { (adl.conn_data_remove)(ctx, idx, port) };
                    rec(
                        "adl-unlock-remove",
                        &ctarget,
                        t.elapsed().as_millis(),
                        rc,
                        String::new(),
                    );
                }
            }
        }
    }

    // SAFETY: destroying the context created above; nothing ADL-owned is used past this point.
    let _ = unsafe { (adl.destroy)(ctx) };
    RunOutcome::Done(recs)
}

/// Marker that a lock was applied and not yet unlocked, plus the connector indices it pinned
/// (the lease, immunity plan WP11). Pinned emulation outlives the process and can outlive a
/// reboot; [`startup_recover`] clears leftovers.
fn journal_path() -> std::path::PathBuf {
    pf_paths::config_dir().join("edid-lock-active.json")
}

fn encode_journal(connectors: &[i32]) -> Vec<u8> {
    serde_json::json!({ "locked": true, "connectors": connectors })
        .to_string()
        .into_bytes()
}

/// The leased connector indices in a marker. An older host's bare `{"locked":true}` (or an
/// unreadable marker) is `None`, which unlocks every connector as it always did. An empty lease
/// pinned nothing, so it unlocks nothing: the operator's own pins stay.
fn decode_journal(bytes: &[u8]) -> Option<Vec<i32>> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("connectors")?.as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.as_i64().and_then(|c| i32::try_from(c).ok()))
                .collect()
        })
}

/// The connector index a lock record names: `adapterN.connectorM[TYPE]` → `M`.
fn connector_of(target: &str) -> Option<i32> {
    let rest = target.split("connector").nth(1)?;
    rest.split('[').next()?.parse().ok()
}

/// Mode-off `ADL_ERR_NOT_SUPPORTED` is a documented no-op, not a failure.
///
/// Unlock walks every connector, including ones never pinned. Some drivers
/// answer NOT_SUPPORTED when there is no emulation to turn off.
/// `adl-unlock-remove` is the call that clears a pin, so its rc still warns.
fn is_expected_noop(r: &OpRecord) -> bool {
    const ADL_ERR_NOT_SUPPORTED: i32 = -8;
    r.op == "adl-unlock-mode-off" && r.rc == ADL_ERR_NOT_SUPPORTED
}

fn tracing_log(prefix: &str, outcome: &RunOutcome) {
    match outcome {
        RunOutcome::NoAdl => tracing::info!(
            "{prefix}: atiadlxx.dll not loadable — not an AMD driver install; the ADL \
             emulation lever does not exist on this box"
        ),
        RunOutcome::InitFailed(recs) | RunOutcome::Done(recs) => {
            for r in recs {
                if r.ok() || is_expected_noop(r) {
                    tracing::info!("{prefix}: {r}");
                } else {
                    tracing::warn!("{prefix}: {r}");
                }
            }
        }
    }
}

/// Pin live EDID + `ADL_EMUL_MODE_ALWAYS` on occupied AMD connectors at stream bring-up.
/// Journal first so a crash still unlocks on the next start. Returns whether
/// [`unlock_after_stream`] is owed: true whenever ADL exists, because a partial
/// lock may have pinned some connectors.
pub fn lock_for_stream() -> bool {
    // Journal before touching the driver: a crash between the first ConnectionData_Set
    // and the journal write would leave pinned connectors with no startup unlock owed.
    if let Err(e) = std::fs::write(journal_path(), b"{\"locked\":true}") {
        tracing::warn!(
            error = %format!("{e:#}"),
            "edid_lock: crash journal write failed — continuing (the feature degrades to \
             no-crash-journal)"
        );
    }
    let outcome = run(EmulAction::Lock, None);
    tracing_log("edid_lock", &outcome);
    if matches!(outcome, RunOutcome::NoAdl) {
        tracing::info!("edid_lock: enabled but this is not an AMD driver install — axis inert");
        let _ = std::fs::remove_file(journal_path());
        return false;
    }
    let leased: Vec<i32> = outcome
        .records()
        .iter()
        .filter(|r| r.op == "adl-lock-mode-always" && r.ok())
        .filter_map(|r| connector_of(&r.target))
        .collect();
    // The lease: only these connectors are ours to unlock. Written after the fact — the bare
    // marker above already covers a crash in between (it unlocks everything, as before).
    if let Err(e) = std::fs::write(journal_path(), encode_journal(&leased)) {
        tracing::warn!(error = %format!("{e:#}"), "edid_lock: lease write failed");
    }
    tracing::info!(
        connectors = ?leased,
        "edid_lock: connector emulation pinned (software HPD dummy) — unlocked at stream teardown"
    );
    true
}

/// Undo [`lock_for_stream`]: mode off + remove pinned EDID on the LEASED connectors only, then
/// clear the journal. A marker with no lease (an older host, or a crash before the lease was
/// written) unlocks every connector. Idempotent.
pub fn unlock_after_stream() {
    let leased = std::fs::read(journal_path())
        .ok()
        .and_then(|b| decode_journal(&b));
    // The journal is the ONLY record that a connector is still pinned, and a pin outlives the
    // process (sometimes a reboot). Clearing it after a failed unlock strands the operator's
    // connector on a dummy EDID with nothing left to retry from, so keep it unless every
    // unlock reported ADL_OK or the documented no-op. `startup_recover` retries on the next start.
    let unlocked = |outcome: &RunOutcome| {
        !matches!(outcome, RunOutcome::InitFailed(_))
            && outcome
                .records()
                .iter()
                .all(|r| r.ok() || is_expected_noop(r))
    };
    let mut all_ok = true;
    match leased {
        None => {
            let outcome = run(EmulAction::Unlock, None);
            tracing_log("edid_lock", &outcome);
            all_ok = unlocked(&outcome);
        }
        Some(leased) => {
            for c in leased {
                let outcome = run(EmulAction::Unlock, Some(c));
                tracing_log("edid_lock", &outcome);
                all_ok &= unlocked(&outcome);
            }
        }
    }
    if all_ok {
        let _ = std::fs::remove_file(journal_path());
    } else {
        tracing::warn!(
            "edid_lock: unlock did not fully succeed — keeping the journal so the next host \
             start retries; a connector may still carry the pinned EDID"
        );
    }
}

/// If the journal marker exists, unlock leftover pins before any new session.
/// Pinned emulation persists past process death and can persist past reboot.
pub fn startup_recover() {
    if !journal_path().exists() {
        return;
    }
    tracing::warn!(
        "edid_lock: a previous host left connector emulation pinned (crash/kill) — unlocking"
    );
    unlock_after_stream();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lease round-trips; an older bare marker (or garbage) means "unlock everything", the
    /// pre-lease behaviour, and an empty lease unlocks nothing; the connector index comes out of
    /// the lock record's target name.
    #[test]
    fn edid_lock_lease_round_trips_and_reads_the_bare_marker() {
        assert_eq!(decode_journal(&encode_journal(&[2, 5])), Some(vec![2, 5]));
        assert_eq!(decode_journal(&encode_journal(&[])), Some(vec![]));
        assert_eq!(decode_journal(br#"{"locked":true}"#), None);
        assert_eq!(decode_journal(b"garbage"), None);
        assert_eq!(connector_of("adapter0.connector3[HDMI]"), Some(3));
        assert_eq!(connector_of("adapter0"), None);
    }
}
