//! Unhandled-SEH filter: one `tracing` ERROR, then default handling.
//!
//! Native crashes leave no Rust panic and no log line. This filter names the exception code,
//! fault address, and containing module before the process dies — the host, the session
//! binary, and the Windows shell all install it.
//!
//! Call [`install`] once, after logging init — earlier logs into the void. The filter allocates;
//! heap corruption can fault again, and the OS then terminates as it would have. Always returns
//! `EXCEPTION_CONTINUE_SEARCH` so WER, a debugger, or the service supervisor still run.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): kernel32 syscall glue that reads one
// OS-owned exception record and touches no network bytes. Proofs at each site.
#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{
    SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

/// After logging init: the filter reports through `tracing`.
pub fn install() {
    // SAFETY: `on_unhandled` is `extern "system"`, matches LPTOP_LEVEL_EXCEPTION_FILTER, and
    // has static lifetime. The previous filter is dropped: each process installs exactly once.
    unsafe {
        SetUnhandledExceptionFilter(Some(on_unhandled));
    }
}

/// Slots 0-1 are access kind (0 read / 1 write / 8 execute) and the fault address; those
/// distinguish a wild pointer from a guard page.
const STATUS_ACCESS_VIOLATION: i32 = 0xC0000005u32 as i32;

/// Best-effort: formats and logs, so heap corruption can fault again. Returns
/// `EXCEPTION_CONTINUE_SEARCH` so WER / a debugger / the supervisor still run.
unsafe extern "system" fn on_unhandled(info: *const EXCEPTION_POINTERS) -> i32 {
    let mut code: i32 = 0;
    let mut addr: usize = 0;
    let mut av_kind: Option<usize> = None;
    let mut av_target: Option<usize> = None;
    // SAFETY: `info` (and `ExceptionRecord`) are supplied by the OS for the duration of this
    // callback; both are checked non-null before the read, and only plain fields are copied out.
    unsafe {
        if !info.is_null() && !(*info).ExceptionRecord.is_null() {
            let r = &*(*info).ExceptionRecord;
            code = r.ExceptionCode;
            addr = r.ExceptionAddress as usize;
            if code == STATUS_ACCESS_VIOLATION && r.NumberParameters >= 2 {
                av_kind = Some(r.ExceptionInformation[0]);
                av_target = Some(r.ExceptionInformation[1]);
            }
        }
    }
    let module = module_at(addr);
    tracing::error!(
        code = %format!("0x{:08x}", code as u32),
        address = %format!("0x{addr:016x}"),
        module = %module.as_deref().unwrap_or("<unknown>"),
        av_kind = av_kind.map(|k| match k {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "other",
        }),
        av_target = av_target.map(|t| format!("0x{t:016x}")),
        "FATAL: unhandled native exception — the process is about to die"
    );
    EXCEPTION_CONTINUE_SEARCH
}

/// So the log names the faulting DLL rather than a raw address.
fn module_at(addr: usize) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut hmod: HMODULE = std::ptr::null_mut();
    // SAFETY: FROM_ADDRESS treats the "module name" argument as an address inside the module
    // (`addr as *const u16`). UNCHANGED_REFCOUNT skips AddRef, so this HMODULE is not Freed.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr as *const u16,
            &mut hmod,
        )
    };
    if ok == 0 {
        return None;
    }
    let mut buf = [0u16; 512];
    // SAFETY: `hmod` is the handle from GetModuleHandleExW above; `buf` is a live writable
    // slice for the call and its length is what we pass.
    let n = unsafe { GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0).then(|| String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}
