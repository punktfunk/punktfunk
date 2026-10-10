//! Windows capture GPU mechanics: the win32u GPU-preference hook and the P010
//! self-test. The D3D11 converters (`HdrP010Converter`, `VideoConverter`, …)
//! live in `pf_encode_win::convert`; [`super::idd_push`] imports them there.
//!
//! Capture identity ([`WinCaptureTarget`], [`D3d11Frame`], [`pack_luid`],
//! [`make_device`]) lives in `pf-frame` so capture, encode, and pf-vdisplay share
//! one type without a crate cycle. This module re-exports them so `crate::dxgi::*`
//! still resolves. There is no capturer here.

pub use pf_frame::dxgi::{make_device, pack_luid, D3d11Frame, PyroFrameShare, WinCaptureTarget};

// `#[path]`: this file is itself reached through a `#[path]` from `lib.rs`, so a
// bare `mod selftest;` would resolve to `windows/selftest.rs`.
#[path = "dxgi/selftest.rs"]
mod selftest;
pub use selftest::{hdr_p010_convert_bars_on_luid, hdr_p010_selftest_at};

#[cfg(target_arch = "x86_64")]
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

/// Hits on the hooked `NtGdiDdDDIGetCachedHybridQueryValue`. Patch-readback in
/// [`install_gpu_pref_hook`] only proves the bytes landed; `0` here means DXGI
/// never reached the export on this build (always on ARM64, where no patch exists).
static HYBRID_HOOK_HITS: AtomicU64 = AtomicU64::new(0);

pub fn hybrid_hook_hits() -> u64 {
    HYBRID_HOOK_HITS.load(Ordering::Relaxed)
}

// Declared here so we skip the Win32_System_Diagnostics_Debug feature for one call.
// DXGI runs the hooked export on the encode worker, possibly another core;
// FlushInstructionCache after the patch so that core does not keep the old bytes.
#[cfg(target_arch = "x86_64")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn FlushInstructionCache(h: *mut c_void, base: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}
/// Always report `D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED` (3). Replaces the
/// export in full, so there is no trampoline back to the original.
#[cfg(target_arch = "x86_64")]
unsafe extern "system" fn hybrid_query_hook(gpu_preference: *mut u32) -> i32 {
    HYBRID_HOOK_HITS.fetch_add(1, Ordering::Relaxed);
    if gpu_preference.is_null() {
        return 0xC000_000Du32 as i32; // STATUS_INVALID_PARAMETER
    }
    // SAFETY: win32u's contract for this export — the caller (DXGI) passes a writable `*mut u32`
    // out-param — and the null case has just been rejected above, so this is an in-bounds,
    // 4-aligned single-word store into the caller's live local.
    unsafe { *gpu_preference = 3 }; // D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED
    0 // STATUS_SUCCESS
}

/// Fake `D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED` so DXGI skips hybrid
/// GPU-preference resolution. Without this, DXGI reparents outputs onto the
/// preferred render GPU and ignores `SET_RENDER_ADAPTER`, so the driver's
/// swap-chain lands on an adapter its encoder cannot open.
///
/// Call once from `main.rs` before the first DXGI factory. Lasts the process
/// lifetime. [`hybrid_hook_hits`] reports whether DXGI actually calls it.
/// Also sets per-monitor DPI awareness; on ARM64 that half is all that runs.
pub fn install_gpu_pref_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    // SAFETY: this one-time hook install only touches a region it has just validated.
    // `LoadLibraryA("win32u.dll")` + `GetProcAddress("NtGdiDdDDIGetCachedHybridQueryValue")` yield the
    // live base of the real exported function, so `target` is a valid executable code pointer to at
    // least the 12 bytes the patch overwrites (an x64 prologue). The two
    // `ptr::copy_nonoverlapping`s each move exactly 12 bytes between the 12-byte stack arrays
    // (`patch`/`readback`) and `target`, which `VirtualProtect(target, 12, PAGE_EXECUTE_READWRITE, …)`
    // has just made writable (and is restored to `old` after) — source and dest never overlap (stack
    // vs. loaded module image), so every access stays in mapped, in-bounds memory.
    // `FlushInstructionCache` gets the current-process pseudo-handle + that same range. The DPI calls
    // take by-value context handles / fill the live local `&mut old`/`&mut restore` for the duration of
    // each synchronous call. Runs once via `Once::call_once`, before any DXGI use.
    HOOK.call_once(|| unsafe {
        use windows::Win32::UI::HiDpi::{
            GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            DPI_AWARENESS_PER_MONITOR_AWARE,
        };
        // 0=UNAWARE 1=SYSTEM 2=PER_MONITOR(_V2). Below 2 the OS virtualizes window and
        // cursor coordinates while host geometry is CCD physical pixels, so `SetCursorPos`
        // and the cursor blend miss on a scaled display. The host exe declares v2 in its
        // manifest (punktfunk-host/build.rs); this covers every other process using DXGI.
        let awareness = || GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()).0;
        if awareness() < DPI_AWARENESS_PER_MONITOR_AWARE.0 {
            match SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
                Ok(()) => tracing::info!("DPI awareness set: PER_MONITOR_AWARE_V2"),
                Err(e) => tracing::warn!(error = ?e,
                    "SetProcessDpiAwarenessContext failed — cursor/desktop coordinates may be \
                     DPI-virtualized against the host's physical-pixel CCD geometry"),
            }
        }
        tracing::info!(
            awareness = awareness(),
            "effective DPI awareness (need 2=PER_MONITOR for physical-pixel coordinates)"
        );
        // The patch is x86-64 machine code; a Snapdragon SoC has one GPU, so DXGI has
        // nothing to reparent there and the DPI half above is all ARM64 needs.
        #[cfg(not(target_arch = "x86_64"))]
        tracing::info!("GPU-pref hook: x86-64 only, skipped on this architecture");
        #[cfg(target_arch = "x86_64")]
        {
            use windows::core::s;
            use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
            use windows::Win32::System::Memory::{
                VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
            };
            let Ok(lib) = LoadLibraryA(s!("win32u.dll")) else {
                tracing::warn!(
                    "GPU-pref hook: win32u.dll not loadable — skipping (on a hybrid-GPU box DXGI may \
                     reparent the virtual display off the pinned render adapter → TEX_FAIL rebinds)"
                );
                return;
            };
            let Some(target) = GetProcAddress(lib, s!("NtGdiDdDDIGetCachedHybridQueryValue")) else {
                tracing::warn!(
                    "GPU-pref hook: NtGdiDdDDIGetCachedHybridQueryValue not exported — skipping"
                );
                return;
            };
            let target = target as usize as *mut u8;
            // x64 absolute jump: `mov rax, imm64; jmp rax` (12 bytes). No trampoline —
            // we never call the original, so no relocation / length-disassembler.
            let hook = hybrid_query_hook as *const () as usize;
            let mut patch = [0u8; 12];
            patch[0] = 0x48;
            patch[1] = 0xB8; // mov rax, imm64
            patch[2..10].copy_from_slice(&hook.to_le_bytes());
            patch[10] = 0xFF;
            patch[11] = 0xE0; // jmp rax
            let mut old = PAGE_PROTECTION_FLAGS(0);
            if VirtualProtect(
                target as *const c_void,
                12,
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )
            .is_err()
            {
                tracing::warn!("GPU-pref hook: VirtualProtect failed — skipping");
                return;
            }
            std::ptr::copy_nonoverlapping(patch.as_ptr(), target, 12);
            let mut restore = PAGE_PROTECTION_FLAGS(0);
            let _ = VirtualProtect(target as *const c_void, 12, old, &mut restore);
            // Patch is on the main thread; DXGI calls the export from the encode worker,
            // possibly another core with a stale i-cache that would still run the original.
            let _ = FlushInstructionCache(GetCurrentProcess(), target as *const c_void, 12);
            // CFG / hotpatch / a short stub can reject the write silently. Read it back.
            let mut readback = [0u8; 12];
            std::ptr::copy_nonoverlapping(target, readback.as_mut_ptr(), 12);
            if readback == patch {
                tracing::info!(
                    "GPU-pref hook installed + verified (win32u hybrid-query -> UNSPECIFIED): DXGI \
                     output reparenting disabled. Whether DXGI actually CALLS it shows up as \
                     hybrid_hook_hits on the IDD-push open line."
                );
            } else {
                tracing::error!(
                    want = %format!("{patch:02x?}"), got = %format!("{readback:02x?}"),
                    "GPU-pref hook patch did NOT land — hook is DEAD (on a hybrid-GPU box DXGI can \
                     still reparent the virtual display off the pinned render adapter)"
                );
            }
        }
    });
}
