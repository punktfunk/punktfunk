//! Windows host-process session tuning, matching Apollo/Sunshine `streaming_will_start`.
//!
//! Default Windows process: NORMAL class, ~15.6 ms timer, GPU/display may idle.
//! A GPU-saturating game then starves capture/encode/send, and the coarse timer
//! floors frame pacing. This raises the process class, gives DWM and hot threads
//! MMCSS, drops the timer to 1 ms, and keeps the (virtual) display awake.
//!
//! Raw C-ABI FFI (winmm/kernel32/dwmapi/avrt) so the crate does not pull extra
//! windows-rs features. No-op off Windows. Per-thread MMCSS and execution-state
//! revert at thread exit. Process-wide bits are refcounted over hot threads and
//! revert when the last one exits — a 24/7 host must not keep HIGH class and a
//! 1 ms global timer after the session ends.
//! See `design/host-latency-plan.md`.

#[cfg(target_os = "windows")]
mod imp {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    use std::sync::Mutex;

    type Handle = *mut c_void;
    type Bool = i32;

    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(uPeriod: u32) -> u32;
        fn timeEndPeriod(uPeriod: u32) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn SetPriorityClass(hProcess: Handle, dwPriorityClass: u32) -> Bool;
        fn SetThreadExecutionState(esFlags: u32) -> u32;
        fn PowerCreateRequest(Context: *const ReasonContext) -> Handle;
        fn PowerSetRequest(PowerRequest: Handle, RequestType: i32) -> Bool;
        fn PowerClearRequest(PowerRequest: Handle, RequestType: i32) -> Bool;
        fn CloseHandle(hObject: Handle) -> Bool;
    }

    /// `REASON_CONTEXT` (minwinbase.h), simple-string flavour.
    /// Layout: `Version` (ULONG), `Flags` (DWORD), then `SimpleReasonString`
    /// (LPWSTR) under `POWER_REQUEST_CONTEXT_SIMPLE_STRING` — 4+4, 8-aligned pointer.
    #[repr(C)]
    struct ReasonContext {
        version: u32,
        flags: u32,
        simple_reason: *const u16,
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmEnableMMCSS(fEnableMMCSS: Bool) -> i32; // HRESULT
    }
    #[link(name = "avrt")]
    unsafe extern "system" {
        fn AvSetMmThreadCharacteristicsW(TaskName: *const u16, TaskIndex: *mut u32) -> Handle;
    }

    const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
    const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    const POWER_REQUEST_CONTEXT_VERSION: u32 = 0; // DIAGNOSTIC_REASON_VERSION
    const POWER_REQUEST_CONTEXT_SIMPLE_STRING: u32 = 0x0000_0001;
    const POWER_REQUEST_DISPLAY_REQUIRED: i32 = 0;
    const POWER_REQUEST_SYSTEM_REQUIRED: i32 = 1;
    const INVALID_HANDLE_VALUE: isize = -1;

    /// RAII display+system availability (`PowerRequestDisplayRequired`,
    /// visible in `powercfg /requests`).
    ///
    /// Object-lifetime, unlike the thread-bound `ES_*` flags in [`on_hot_thread`]
    /// (OS reverts those at thread exit). Prevention only: no power request
    /// turns an already-off display back on — that wake is the virtual-mouse kick.
    pub struct DisplayWakeRequest(Handle);

    // SAFETY: the wrapped power-request HANDLE is a kernel object handle — a plain opaque value
    // that any thread may use; this type never aliases it (set at new, cleared+closed at drop).
    unsafe impl Send for DisplayWakeRequest {}

    impl DisplayWakeRequest {
        /// `None` if the kernel refuses; the session still streams.
        pub fn new() -> Option<DisplayWakeRequest> {
            let h = power_request("punktfunk streaming session")?;
            // SAFETY: `h` is the just-created, owned power-request handle; the type is a plain enum.
            unsafe {
                PowerSetRequest(h, POWER_REQUEST_DISPLAY_REQUIRED);
                PowerSetRequest(h, POWER_REQUEST_SYSTEM_REQUIRED);
            }
            Some(DisplayWakeRequest(h))
        }
    }

    /// A new power-request handle, or `None` if the kernel refuses. The caller owns it.
    fn power_request(reason: &str) -> Option<Handle> {
        let reason: Vec<u16> = reason.encode_utf16().chain([0]).collect();
        let ctx = ReasonContext {
            version: POWER_REQUEST_CONTEXT_VERSION,
            flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            simple_reason: reason.as_ptr(),
        };
        // SAFETY: `ctx` (and the reason buffer it points into) outlives the call, which copies
        // the string into the kernel object.
        let h = unsafe { PowerCreateRequest(&ctx) };
        (!h.is_null() && h as isize != INVALID_HANDLE_VALUE).then_some(h)
    }

    /// RAII system availability only (`PowerRequestSystemRequired`): an idle timer can't
    /// sleep the machine, while the display may still turn off.
    pub struct SystemWakeRequest(Handle);

    // SAFETY: as for `DisplayWakeRequest`: an opaque kernel handle, set at new, closed at drop.
    unsafe impl Send for SystemWakeRequest {}

    impl SystemWakeRequest {
        /// `None` if the kernel refuses. `reason` shows in `powercfg /requests`.
        pub fn new(reason: &str) -> Option<SystemWakeRequest> {
            let h = power_request(reason)?;
            // SAFETY: `h` is the just-created, owned power-request handle; the type is a plain enum.
            unsafe { PowerSetRequest(h, POWER_REQUEST_SYSTEM_REQUIRED) };
            Some(SystemWakeRequest(h))
        }
    }

    impl Drop for SystemWakeRequest {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the owned, still-open handle from `new`, dropped exactly once.
            unsafe {
                PowerClearRequest(self.0, POWER_REQUEST_SYSTEM_REQUIRED);
                CloseHandle(self.0);
            }
        }
    }

    impl Drop for DisplayWakeRequest {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the owned, still-open power-request handle (created in `new`,
            // dropped exactly once); clear + close are plain handle calls.
            unsafe {
                PowerClearRequest(self.0, POWER_REQUEST_DISPLAY_REQUIRED);
                PowerClearRequest(self.0, POWER_REQUEST_SYSTEM_REQUIRED);
                CloseHandle(self.0);
            }
        }
    }

    /// Live hot-session threads. Mutex, not atomic: the 0↔1 transitions carry
    /// apply/revert, and an interleaved fetch_add/fetch_sub can leave a running
    /// session untuned.
    static HOT_THREADS: Mutex<usize> = Mutex::new(0);

    /// Process-wide bits, applied on the first hot thread. Best-effort: a
    /// non-elevated host may not get HIGH class.
    fn tune_process() {
        // SAFETY: each call is a C-ABI FFI into winmm/kernel32/dwmapi declared with a matching
        // `extern "system"` signature; every argument is a plain integer (no pointers/buffers escape),
        // and `GetCurrentProcess()` returns the current-process pseudo-handle (a constant, always valid,
        // never closed).
        unsafe {
            // 1 ms (default ~15.6 ms) is the floor for frame pacing and sub-ms encode|send sleeps.
            timeBeginPeriod(1);
            // Capture is bounded by DWM compose rate; MMCSS keeps that ceiling under a saturating game.
            DwmEnableMMCSS(1);
            // HIGH class so a CPU-saturating game cannot deschedule capture/encode/send.
            SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
            tracing::info!("windows session tuning applied (timer 1ms, DWM MMCSS, HIGH priority)");
        }
    }

    /// Mirror of [`tune_process`], run when the last hot thread exits.
    ///
    /// Must not log: this runs from [`HotThreadGuard`]'s `Drop`, a TLS
    /// destructor. Other thread-locals (including `tracing`'s registry) may
    /// already be gone; a panic that escapes a TLS destructor aborts the process.
    /// Revert is FFI only, inside the refcount lock, so it stays atomic against
    /// a concurrent session start.
    fn untune_process() {
        // SAFETY: same FFI surface as `tune_process` — plain-integer arguments, constant
        // pseudo-handle, no pointers or buffers. Sound in a TLS destructor: no Rust TLS is read.
        unsafe {
            timeEndPeriod(1); // pairs the timeBeginPeriod(1)
            DwmEnableMMCSS(0);
            SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS);
        }
    }

    /// One per hot thread, parked in TLS by [`on_hot_thread`]. Drop runs at
    /// thread exit, the same lifetime MMCSS and execution-state already ride.
    struct HotThreadGuard;

    impl Drop for HotThreadGuard {
        fn drop(&mut self) {
            // TLS destructor: panic-free, no logging. A poisoned lock skips the
            // revert rather than panicking (best-effort, like every call here).
            if let Ok(mut n) = HOT_THREADS.lock() {
                *n -= 1;
                if *n == 0 {
                    untune_process();
                }
            }
        }
    }

    thread_local! {
        static HOT_THREAD: std::cell::OnceCell<HotThreadGuard> =
            const { std::cell::OnceCell::new() };
    }

    /// Register this capture/encode/send thread. First in applies process
    /// tuning; last out reverts. MMCSS handle is leaked and `ES_*` is bound
    /// to this thread — the OS reverts both at thread exit, where the TLS
    /// guard drops too.
    pub fn on_hot_thread() {
        HOT_THREAD.with(|slot| {
            if slot.get().is_none() {
                {
                    let mut n = HOT_THREADS.lock().unwrap();
                    *n += 1;
                    if *n == 1 {
                        tune_process();
                    }
                }
                let _ = slot.set(HotThreadGuard);
            }
        });
        // SAFETY: C-ABI FFI declared with matching `extern "system"` signatures. SetThreadExecutionState
        // takes only flag bits. `task` is a local NUL-terminated UTF-16 buffer ("Games\0") alive for the
        // whole block, so `task.as_ptr()` is a valid LPCWSTR for the call, and `&mut idx` is a live local
        // u32 the call writes the task index into. The returned MMCSS handle is intentionally leaked (the
        // OS reverts the characteristics at thread exit), so there is nothing to free or double-free.
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            let task: Vec<u16> = "Games\0".encode_utf16().collect();
            let mut idx: u32 = 0;
            let _ = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut idx);
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::{on_hot_thread, DisplayWakeRequest, SystemWakeRequest};

/// No-op on non-Windows (Linux uses `setpriority` nice + CUDA stream priority —
/// see `native::boost_thread_priority` and `zerocopy::cuda`).
#[cfg(not(target_os = "windows"))]
pub fn on_hot_thread() {}
