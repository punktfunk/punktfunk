//! The Windows App Runtime on Windows 10.
//!
//! windows-reactor adds the runtime to an unpackaged process with `TryCreatePackageDependency`
//! and `AddPackageDependency`, which Windows 11 introduced. build.rs delay-loads their API set,
//! so the exe starts without them, and a miss reaches `__pfnDliFailureHook2`. The hook answers
//! with stand-ins that do the same job through the runtime's bootstrap DLL, which the installer
//! and the portable zip ship beside the exe. A packaged run never calls either function.

use std::ffi::{c_char, c_void, CStr};
use std::ptr::{null, null_mut};

use windows::core::{s, w, HRESULT, PCWSTR};
use windows::Win32::libloaderapi::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
};

const DLI_FAIL_LOAD_LIB: u32 = 3;
const DLI_FAIL_GET_PROC: u32 = 4;

/// delayimp.h's `DelayLoadInfo` up to `dlp`, with `DelayLoadProc` flattened in place.
#[repr(C)]
struct DelayLoadInfo {
    _cb: u32,
    _pidd: *const c_void,
    _ppfn: *mut c_void,
    _sz_dll: *const c_char,
    import_by_name: i32,
    proc_name: *const c_char,
}

type DliHook = extern "system" fn(u32, Option<&DelayLoadInfo>) -> *const c_void;

// delayimp.lib reads this symbol after a failed delay-load; this definition replaces its null.
#[unsafe(no_mangle)]
#[used]
#[allow(non_upper_case_globals)]
static __pfnDliFailureHook2: DliHook = delay_load_failed;

/// The helper passes a `DelayLoadInfo` that stays live for the call; `Option<&T>` has the
/// nullable pointer's ABI.
extern "system" fn delay_load_failed(notify: u32, info: Option<&DelayLoadInfo>) -> *const c_void {
    match (notify, info) {
        // Any module will do: its lookup misses next and comes back as DLI_FAIL_GET_PROC.
        // SAFETY: a null name returns the exe's own handle, valid for the process lifetime.
        (DLI_FAIL_LOAD_LIB, _) => (unsafe { GetModuleHandleW(None) }) as *const c_void,
        (DLI_FAIL_GET_PROC, Some(info)) if info.import_by_name != 0 => {
            // SAFETY: with `import_by_name` set, `proc_name` is a NUL-terminated name in the
            // exe's import table.
            let name = unsafe { CStr::from_ptr(info.proc_name) };
            match name.to_bytes() {
                b"TryCreatePackageDependency" => try_create_package_dependency as *const c_void,
                b"AddPackageDependency" => add_package_dependency as *const c_void,
                _ => null(),
            }
        }
        _ => null(),
    }
}

/// Stands in for `TryCreatePackageDependency`, with reactor's signature: the helper writes
/// this function into reactor's import slot. `PACKAGE_VERSION` passes as its `u64` on x64 and
/// arm64; reactor's out-pointer is the address of a local, so it arrives as `Option<&mut>`.
/// The bootstrap DLL adds a runtime of at least `min_version`; reactor names no other
/// framework, so the remaining arguments go unread.
#[allow(clippy::too_many_arguments)]
extern "system" fn try_create_package_dependency(
    _user: *mut c_void,
    _family: PCWSTR,
    min_version: u64,
    _architectures: i32,
    _lifetime_kind: i32,
    _lifetime_artifact: PCWSTR,
    _options: i32,
    dependency_id: Option<&mut *mut u16>,
) -> HRESULT {
    // Null leaves reactor nothing to free.
    if let Some(id) = dependency_id {
        *id = null_mut();
    }
    bootstrap(min_version)
}

/// Stands in for `AddPackageDependency`: the bootstrap already added the runtime, so there is
/// no context and no package name to return. Both out-pointers are addresses of reactor's
/// locals.
extern "system" fn add_package_dependency(
    _dependency_id: PCWSTR,
    _rank: i32,
    _options: i32,
    context: Option<&mut *mut c_void>,
    package_full_name: Option<&mut *mut u16>,
) -> HRESULT {
    if let Some(context) = context {
        *context = null_mut();
    }
    // reactor frees the name with HeapFree, which accepts null.
    if let Some(name) = package_full_name {
        *name = null_mut();
    }
    HRESULT(0)
}

/// `MddBootstrapInitialize2` for major 2; release 2.0+ ignores the minor half. Options 0 shows
/// no UI of its own: no match returns STATEREPOSITORY_E_DEPENDENCY_NOT_RESOLVED, and reactor
/// offers the runtime download as it does on Windows 11.
fn bootstrap(min_version: u64) -> HRESULT {
    type Initialize = unsafe extern "system" fn(u32, PCWSTR, u64, i32) -> HRESULT;
    // SAFETY: a static name. The default dirs are the exe's directory and System32, never the
    // working directory.
    let module = unsafe {
        LoadLibraryExW(
            w!("Microsoft.WindowsAppRuntime.Bootstrap.dll"),
            None,
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS as u32,
        )
    };
    if module.is_null() {
        return HRESULT::from_thread();
    }
    // SAFETY: `module` stays loaded for the process lifetime; the name is a static C string.
    let Some(initialize) = (unsafe { GetProcAddress(module, s!("MddBootstrapInitialize2")) })
    else {
        return HRESULT::from_thread();
    };
    // SAFETY: MddBootstrap.h declares MddBootstrapInitialize2 with this signature; a null
    // version tag selects the stable release.
    unsafe {
        let initialize: Initialize = std::mem::transmute(initialize);
        initialize(0x0002_0000, PCWSTR::null(), min_version, 0)
    }
}
