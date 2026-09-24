//! Hand-written C ABI mirror of AMF (`amf/public/include`, GPUOpen 1.4.36).
//!
//! Every interface is a struct of one vtable pointer. Derived tables PREPEND the
//! base slots (`AMFInterface` → `AMFPropertyStorage` → `AMFData` →
//! `AMFBuffer`/`AMFSurface`), so a derived pointer is usable through a base
//! mirror. Unused slots are [`Slot`] placeholders so later offsets stay at the
//! C index. `AMF_STD_CALL` is `extern "system"`; the two DLL entries are
//! `extern "C"` (`AMF_CDECL_CALL`).
//!
//! Layout is stable on every runtime at or above [`AMF_MIN_VERSION`] (1.4.30).
//! `const` size/offset asserts below pin the PODs and the slots `amf.rs` calls.
//! Evidence: `design/native-amf-encoder.md`.

use std::ffi::c_void;

/// Named only for the codes this module branches on; others stay numeric (`result_name`).
pub type AmfResult = i32;
pub const AMF_OK: AmfResult = 0;
pub const AMF_EOF: AmfResult = 23;
pub const AMF_REPEAT: AmfResult = 24;
pub const AMF_INPUT_FULL: AmfResult = 25;
pub const AMF_NEED_MORE_INPUT: AmfResult = 44;

pub fn result_name(r: AmfResult) -> &'static str {
    match r {
        0 => "AMF_OK",
        1 => "AMF_FAIL",
        2 => "AMF_UNEXPECTED",
        3 => "AMF_ACCESS_DENIED",
        4 => "AMF_INVALID_ARG",
        5 => "AMF_OUT_OF_RANGE",
        6 => "AMF_OUT_OF_MEMORY",
        7 => "AMF_INVALID_POINTER",
        8 => "AMF_NO_INTERFACE",
        9 => "AMF_NOT_IMPLEMENTED",
        10 => "AMF_NOT_SUPPORTED",
        11 => "AMF_NOT_FOUND",
        12 => "AMF_ALREADY_INITIALIZED",
        13 => "AMF_NOT_INITIALIZED",
        14 => "AMF_INVALID_FORMAT",
        15 => "AMF_WRONG_STATE",
        17 => "AMF_NO_DEVICE",
        18 => "AMF_DIRECTX_FAILED",
        23 => "AMF_EOF",
        24 => "AMF_REPEAT",
        25 => "AMF_INPUT_FULL",
        26 => "AMF_RESOLUTION_CHANGED",
        28 => "AMF_INVALID_DATA_TYPE",
        29 => "AMF_INVALID_RESOLUTION",
        30 => "AMF_CODEC_NOT_SUPPORTED",
        31 => "AMF_SURFACE_FORMAT_NOT_SUPPORTED",
        32 => "AMF_SURFACE_MUST_BE_SHARED",
        36 => "AMF_ENCODER_NOT_PRESENT",
        44 => "AMF_NEED_MORE_INPUT",
        _ => "AMF_<unnamed>",
    }
}

/// `AMF_FULL_VERSION` 1.4.36.0 — claimed to `AMFInit`, then capped at the runtime's
/// own version in `load_factory` so an older accepted runtime is asked only for the
/// ABI it actually provides.
pub const AMF_HEADER_VERSION: u64 = (1u64 << 48) | (4u64 << 32) | (36u64 << 16);

/// ABI floor (`AMF_FULL_VERSION` 1.4.30.0), not a feature floor. Every mirrored
/// vtable is byte-identical in the 1.4.30 and 1.4.34 headers (Result.h only appends
/// a code); later additions this path uses are string-keyed properties
/// (`required=false`). 1.4.30 admits the Polaris/Vega driver branch, frozen at
/// 1.4.31. Below it the loader declines; the vtable offsets are not guaranteed.
pub const AMF_MIN_VERSION: u64 = (1u64 << 48) | (4u64 << 32) | (30u64 << 16);

pub const AMF_SURFACE_NV12: i32 = 1;
pub const AMF_SURFACE_P010: i32 = 10;

/// `InitDX11` version argument: header `AMF_DX11_1` is 111, not 11.
pub const AMF_DX11_1: i32 = 111;

/// `AllocBuffer` memory type for the CPU-filled HDR-metadata buffer.
pub const AMF_MEMORY_HOST: i32 = 1;

/// Payload of `*InHDRMetadata`. Units match HEVC ST.2086 /
/// [`pf_frame::HdrMeta`]: chromaticities 1/50000, mastering luminance
/// 0.0001 cd/m², CLL/FALL in nits.
#[repr(C)]
pub struct AmfHdrMetadata {
    pub red_primary: [u16; 2],
    pub green_primary: [u16; 2],
    pub blue_primary: [u16; 2],
    pub white_point: [u16; 2],
    pub max_mastering_luminance: u32,
    pub min_mastering_luminance: u32,
    pub max_content_light_level: u16,
    pub max_frame_average_light_level: u16,
}

/// `AMFGuid` (core/Platform.h): `data4[8]` is the flattened `data41..data48`.
#[repr(C)]
pub struct AmfGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// `IID_AMFBuffer` — `QueryInterface` the encoder's output `AMFData` to reach `GetNative`/`GetSize`.
pub const IID_AMF_BUFFER: AmfGuid = AmfGuid {
    data1: 0xb04b_7248,
    data2: 0xb6f0,
    data3: 0x4321,
    data4: [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
};

pub const AMF_VARIANT_BOOL: i32 = 1;
pub const AMF_VARIANT_INT64: i32 = 2;
pub const AMF_VARIANT_RATE: i32 = 7;
pub const AMF_VARIANT_INTERFACE: i32 = 12;

/// `AMFVariantStruct`: 4-byte tag + 4 pad + 16-byte union = 24 bytes, payload at 8.
/// Passed by value to `SetProperty` (Win64 hidden-ref for >8-byte aggregates,
/// matching the C compiler). Two fully-initialised `u64`s: no uninit union bytes
/// cross the FFI.
#[repr(C)]
pub struct AmfVariant {
    pub vtype: i32,
    pub payload: [u64; 2],
}

impl AmfVariant {
    pub fn zeroed() -> Self {
        AmfVariant {
            vtype: 0, // AMF_VARIANT_EMPTY
            payload: [0, 0],
        }
    }
    pub fn from_i64(v: i64) -> Self {
        AmfVariant {
            vtype: AMF_VARIANT_INT64,
            payload: [v as u64, 0],
        }
    }
    pub fn from_bool(v: bool) -> Self {
        AmfVariant {
            vtype: AMF_VARIANT_BOOL,
            payload: [v as u64, 0],
        }
    }
    /// `AMFRate { num, den }` as two LE `amf_uint32`s in the union's first 8 bytes.
    pub fn from_rate(num: u32, den: u32) -> Self {
        AmfVariant {
            vtype: AMF_VARIANT_RATE,
            payload: [num as u64 | ((den as u64) << 32), 0],
        }
    }
    /// `AMFInterface*` in the union's first 8 bytes. `SetProperty` AddRefs when it
    /// copies the variant in (C++ `AMFVariant` temporaries Release on destruct), so
    /// the caller keeps sole ownership of the pointer it already holds.
    pub fn from_interface(p: *mut c_void) -> Self {
        AmfVariant {
            vtype: AMF_VARIANT_INTERFACE,
            payload: [p as usize as u64, 0],
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        (self.vtype == AMF_VARIANT_INT64).then_some(self.payload[0] as i64)
    }
    pub fn as_bool(&self) -> Option<bool> {
        (self.vtype == AMF_VARIANT_BOOL).then_some(self.payload[0] != 0)
    }
}

/// Uncalled vtable slot. Same size/align as a function pointer; keeps later slots at their C offsets.
pub type Slot = *const c_void;

// AMFFactory is not refcounted — a process singleton (core/Factory.h).
#[repr(C)]
pub struct AmfFactory {
    pub vtbl: *const AmfFactoryVtbl,
}
#[repr(C)]
pub struct AmfFactoryVtbl {
    pub create_context:
        unsafe extern "system" fn(*mut AmfFactory, *mut *mut AmfContext) -> AmfResult,
    pub create_component: unsafe extern "system" fn(
        *mut AmfFactory,
        *mut AmfContext,
        *const u16,
        *mut *mut AmfComponent,
    ) -> AmfResult,
    pub set_cache_folder: Slot,
    pub get_cache_folder: Slot,
    pub get_debug: Slot,
    pub get_trace: Slot,
    pub get_programs: Slot,
}

#[repr(C)]
pub struct AmfContext {
    pub vtbl: *const AmfContextVtbl,
}
#[repr(C)]
pub struct AmfContextVtbl {
    // AMFInterface
    pub acquire: Slot,
    pub release: unsafe extern "system" fn(*mut AmfContext) -> i32,
    pub query_interface: Slot,
    // AMFPropertyStorage
    pub set_property: Slot,
    pub get_property: Slot,
    pub has_property: Slot,
    pub get_property_count: Slot,
    pub get_property_at: Slot,
    pub clear: Slot,
    pub add_to: Slot,
    pub copy_to: Slot,
    pub add_observer: Slot,
    pub remove_observer: Slot,
    // AMFContext
    pub terminate: unsafe extern "system" fn(*mut AmfContext) -> AmfResult,
    pub init_dx9: Slot,
    pub get_dx9_device: Slot,
    pub lock_dx9: Slot,
    pub unlock_dx9: Slot,
    pub init_dx11: unsafe extern "system" fn(*mut AmfContext, *mut c_void, i32) -> AmfResult,
    pub get_dx11_device: Slot,
    pub lock_dx11: Slot,
    pub unlock_dx11: Slot,
    pub init_opencl: Slot,
    pub get_opencl_context: Slot,
    pub get_opencl_command_queue: Slot,
    pub get_opencl_device_id: Slot,
    pub get_opencl_compute_factory: Slot,
    pub init_opencl_ex: Slot,
    pub lock_opencl: Slot,
    pub unlock_opencl: Slot,
    pub init_opengl: Slot,
    pub get_opengl_context: Slot,
    pub get_opengl_drawable: Slot,
    pub lock_opengl: Slot,
    pub unlock_opengl: Slot,
    pub init_xv: Slot,
    pub get_xv_device: Slot,
    pub lock_xv: Slot,
    pub unlock_xv: Slot,
    pub init_gralloc: Slot,
    pub get_gralloc_device: Slot,
    pub lock_gralloc: Slot,
    pub unlock_gralloc: Slot,
    pub alloc_buffer: unsafe extern "system" fn(
        *mut AmfContext,
        i32, // AMF_MEMORY_TYPE
        usize,
        *mut *mut AmfBuffer,
    ) -> AmfResult,
    pub alloc_surface: Slot,
    pub alloc_audio_buffer: Slot,
    pub create_buffer_from_host_native: Slot,
    pub create_surface_from_host_native: Slot,
    pub create_surface_from_dx9_native: Slot,
    /// Header out-param is `AMFSurface**`. Mirrored as `AmfData` because every
    /// call here (`SetPts`, `SetProperty`, `Release`, `SubmitInput`) lives in the
    /// `AMFData` prefix; `AMFSurfaceVtbl` reproduces it slot-for-slot.
    pub create_surface_from_dx11_native: unsafe extern "system" fn(
        *mut AmfContext,
        *mut c_void,
        *mut *mut AmfData,
        *mut c_void,
    ) -> AmfResult,
    pub create_surface_from_opengl_native: Slot,
    pub create_surface_from_gralloc_native: Slot,
    pub create_surface_from_opencl_native: Slot,
    pub create_buffer_from_opencl_native: Slot,
    pub get_compute: Slot,
}

#[repr(C)]
pub struct AmfComponent {
    pub vtbl: *const AmfComponentVtbl,
}
#[repr(C)]
pub struct AmfComponentVtbl {
    // AMFInterface
    pub acquire: Slot,
    pub release: unsafe extern "system" fn(*mut AmfComponent) -> i32,
    pub query_interface: Slot,
    // AMFPropertyStorage
    pub set_property:
        unsafe extern "system" fn(*mut AmfComponent, *const u16, AmfVariant) -> AmfResult,
    pub get_property:
        unsafe extern "system" fn(*mut AmfComponent, *const u16, *mut AmfVariant) -> AmfResult,
    pub has_property: Slot,
    pub get_property_count: Slot,
    pub get_property_at: Slot,
    pub clear: Slot,
    pub add_to: Slot,
    pub copy_to: Slot,
    pub add_observer: Slot,
    pub remove_observer: Slot,
    // AMFPropertyStorageEx
    pub get_properties_info_count: Slot,
    pub get_property_info_at: Slot,
    pub get_property_info: Slot,
    pub validate_property: Slot,
    // AMFComponent
    pub init: unsafe extern "system" fn(*mut AmfComponent, i32, i32, i32) -> AmfResult,
    pub reinit: Slot,
    pub terminate: unsafe extern "system" fn(*mut AmfComponent) -> AmfResult,
    pub drain: unsafe extern "system" fn(*mut AmfComponent) -> AmfResult,
    pub flush: unsafe extern "system" fn(*mut AmfComponent) -> AmfResult,
    pub submit_input: unsafe extern "system" fn(*mut AmfComponent, *mut AmfData) -> AmfResult,
    pub query_output: unsafe extern "system" fn(*mut AmfComponent, *mut *mut AmfData) -> AmfResult,
    pub get_context: Slot,
    pub set_output_data_allocator_cb: Slot,
    pub get_caps: Slot,
    pub optimize: Slot,
}

// AMFData — usable prefix of AMFSurface (same object pointer).
#[repr(C)]
pub struct AmfData {
    pub vtbl: *const AmfDataVtbl,
}
#[repr(C)]
pub struct AmfDataVtbl {
    // AMFInterface
    pub acquire: Slot,
    pub release: unsafe extern "system" fn(*mut AmfData) -> i32,
    pub query_interface:
        unsafe extern "system" fn(*mut AmfData, *const AmfGuid, *mut *mut c_void) -> AmfResult,
    // AMFPropertyStorage
    pub set_property: unsafe extern "system" fn(*mut AmfData, *const u16, AmfVariant) -> AmfResult,
    pub get_property:
        unsafe extern "system" fn(*mut AmfData, *const u16, *mut AmfVariant) -> AmfResult,
    pub has_property: Slot,
    pub get_property_count: Slot,
    pub get_property_at: Slot,
    pub clear: Slot,
    pub add_to: Slot,
    pub copy_to: Slot,
    pub add_observer: Slot,
    pub remove_observer: Slot,
    // AMFData
    pub get_memory_type: Slot,
    pub duplicate: Slot,
    pub convert: Slot,
    pub interop: Slot,
    pub get_data_type: Slot,
    pub is_reusable: Slot,
    pub set_pts: unsafe extern "system" fn(*mut AmfData, i64),
    pub get_pts: Slot,
    pub set_duration: Slot,
    pub get_duration: Slot,
}

#[repr(C)]
pub struct AmfBuffer {
    pub vtbl: *const AmfBufferVtbl,
}
#[repr(C)]
pub struct AmfBufferVtbl {
    // AMFInterface + AMFPropertyStorage + AMFData prefix (same order as AmfDataVtbl).
    pub acquire: Slot,
    pub release: unsafe extern "system" fn(*mut AmfBuffer) -> i32,
    pub query_interface: Slot,
    pub set_property: Slot,
    pub get_property: Slot,
    pub has_property: Slot,
    pub get_property_count: Slot,
    pub get_property_at: Slot,
    pub clear: Slot,
    pub add_to: Slot,
    pub copy_to: Slot,
    pub add_observer: Slot,
    pub remove_observer: Slot,
    pub get_memory_type: Slot,
    pub duplicate: Slot,
    pub convert: Slot,
    pub interop: Slot,
    pub get_data_type: Slot,
    pub is_reusable: Slot,
    pub set_pts: Slot,
    pub get_pts: Slot,
    pub set_duration: Slot,
    pub get_duration: Slot,
    // AMFBuffer
    pub set_size: Slot,
    pub get_size: unsafe extern "system" fn(*mut AmfBuffer) -> usize,
    pub get_native: unsafe extern "system" fn(*mut AmfBuffer) -> *mut c_void,
    pub add_observer_buffer: Slot,
    pub remove_observer_buffer: Slot,
}

// Hand-written C mirror: a POD size miss or a shifted vtable slot is silent UB
// (`amf.rs` dispatches by position). `AMF_MIN_VERSION` is a version number, not
// a layout check. These `const` asserts (not `#[cfg(test)]`) are the defence;
// they cover every slot `amf.rs` calls plus each table's total size.

const SLOT: usize = core::mem::size_of::<Slot>();

/// Byte offset of vtable slot `i`. A `const fn` so clippy `erasing_op` /
/// `identity_op` do not reject `0 * SLOT` / `1 * SLOT`; writing those as `0`
/// and `SLOT` would hide the slot index the asserts are documenting.
const fn slot(i: usize) -> usize {
    i * SLOT
}

// If SLOT is not pointer-sized, the tables are not flat and every offset below is void.
const _: () = assert!(SLOT == core::mem::size_of::<usize>());
const _: () = assert!(core::mem::align_of::<Slot>() == core::mem::align_of::<usize>());

const _: () = assert!(core::mem::size_of::<AmfVariant>() == 24);
const _: () = assert!(core::mem::align_of::<AmfVariant>() == 8);
const _: () = assert!(core::mem::offset_of!(AmfVariant, payload) == 8);
const _: () = assert!(core::mem::size_of::<AmfGuid>() == 16);
const _: () = assert!(core::mem::align_of::<AmfGuid>() == 4);
const _: () = assert!(core::mem::size_of::<AmfHdrMetadata>() == 28);
const _: () = assert!(core::mem::offset_of!(AmfHdrMetadata, max_mastering_luminance) == 16);
const _: () = assert!(core::mem::offset_of!(AmfHdrMetadata, max_content_light_level) == 24);

const _: () = assert!(core::mem::size_of::<AmfFactoryVtbl>() == slot(7));
const _: () = assert!(core::mem::offset_of!(AmfFactoryVtbl, create_context) == slot(0));
const _: () = assert!(core::mem::offset_of!(AmfFactoryVtbl, create_component) == slot(1));

const _: () = assert!(core::mem::size_of::<AmfContextVtbl>() == slot(55));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, terminate) == slot(13));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, init_dx11) == slot(18));
const _: () = assert!(core::mem::offset_of!(AmfContextVtbl, alloc_buffer) == slot(43));
const _: () =
    assert!(core::mem::offset_of!(AmfContextVtbl, create_surface_from_dx11_native) == slot(49));

const _: () = assert!(core::mem::size_of::<AmfComponentVtbl>() == slot(28));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, set_property) == slot(3));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, init) == slot(17));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, terminate) == slot(19));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, drain) == slot(20));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, flush) == slot(21));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, submit_input) == slot(22));
const _: () = assert!(core::mem::offset_of!(AmfComponentVtbl, query_output) == slot(23));

const _: () = assert!(core::mem::size_of::<AmfDataVtbl>() == slot(23));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, query_interface) == slot(2));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, set_property) == slot(3));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, get_property) == slot(4));
const _: () = assert!(core::mem::offset_of!(AmfDataVtbl, set_pts) == slot(19));

const _: () = assert!(core::mem::size_of::<AmfBufferVtbl>() == slot(28));
const _: () = assert!(core::mem::offset_of!(AmfBufferVtbl, release) == slot(1));
const _: () = assert!(core::mem::offset_of!(AmfBufferVtbl, get_size) == slot(24));
const _: () = assert!(core::mem::offset_of!(AmfBufferVtbl, get_native) == slot(25));

// `AMFBuffer`/`AMFSurface` are driven through `AmfData` via the shared prefix;
// if the two mirrors disagree on a shared slot, that reinterpretation is wrong.
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, release) == core::mem::offset_of!(AmfBufferVtbl, release)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, set_property)
        == core::mem::offset_of!(AmfBufferVtbl, set_property)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, get_property)
        == core::mem::offset_of!(AmfBufferVtbl, get_property)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, set_pts) == core::mem::offset_of!(AmfBufferVtbl, set_pts)
);
const _: () = assert!(
    core::mem::offset_of!(AmfDataVtbl, get_duration)
        == core::mem::offset_of!(AmfBufferVtbl, get_duration)
);

// DLL entry points (core/Factory.h): `AMF_CDECL_CALL` → `extern "C"`, not `"system"`.
pub type AmfQueryVersionFn = unsafe extern "C" fn(*mut u64) -> AmfResult;
pub type AmfInitFn = unsafe extern "C" fn(u64, *mut *mut AmfFactory) -> AmfResult;
