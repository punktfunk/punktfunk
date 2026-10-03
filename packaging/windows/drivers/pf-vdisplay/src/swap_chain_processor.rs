//! The swap-chain drain worker: one thread per assignment that consumes the IddCx swap-chain (so
//! the virtual monitor stays a usable display) and hands each acquired surface to the monitor's
//! encode pool.
//!
//! The OS presents the composited desktop to the driver through a swap-chain; the driver MUST
//! consume it (acquire → finished-processing) or the monitor stalls. This binds the pooled render
//! device (`IddCxSwapChainSetDevice`), loops acquire/finish, and inside that window issues exactly
//! one fused GPU pass into a free pool slot — or drops at the pool. Nothing else: telemetry is
//! stamped after `FinishedProcessingFrame`, because the window is what DWM waits on for this head.
//!
//! A bypass pool issues no pass where it can: the encoder reads the acquired surface, which
//! stays the driver's until the next acquire returns a frame. So the worker holds that acquire
//! until the access unit is out, and `FinishedProcessingFrame` still goes out at once. Once the
//! desktop goes still the worker copies that surface into the pool, the one pass of the run.
//!
//! The `wdk_iddcx` DDI wrappers return a RAW `NTSTATUS` (`i32`) that is HRESULT-shaped for the
//! swap-chain DDIs, so we classify it by hand (`hr >= 0` = success; `0x8000_000A` = E_PENDING;
//! `hr < 0 && != E_PENDING` = error) rather than with `nt_success`.

use std::{
    mem::size_of,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use wdk_sys::iddcx::{
    IDARG_IN_RELEASEANDACQUIREBUFFER2, IDARG_IN_SETREALTIMEGPUPRIORITY,
    IDARG_IN_SWAPCHAINSETDEVICE, IDARG_OUT_RELEASEANDACQUIREBUFFER2, IDDCX_SWAPCHAIN,
};
// `HANDLE` is the shared wdk-sys typedef (`crate::types`) re-used by the iddcx bindings — take it from
// the crate root, which is guaranteed to export it (the iddcx module only re-exports it if bindgen
// re-declared it there). It is the same type as `IDARG_IN_SETSWAPCHAIN.hNextSurfaceAvailable`.
use wdk_sys::{HANDLE, NTSTATUS, WDFOBJECT, call_unsafe_wdf_function_binding};
use windows::{
    Win32::{
        Foundation::{HANDLE as WHANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Graphics::{
            Direct3D11::ID3D11Texture2D,
            Dxgi::{IDXGIDevice, IDXGIResource},
        },
        System::Threading::{SetEvent, WaitForMultipleObjects, WaitForSingleObject},
    },
    core::Interface,
};

use crate::{
    direct_3d_device::Direct3DDevice,
    monitor::Monitor,
    worker::{Mmcss, OwnedHandle, Sendable},
};

/// E_PENDING — `ReleaseAndAcquireBuffer2` returns this (HRESULT-shaped) when the swap-chain is valid but
/// DWM has composed no new frame yet; wait on the surface-available event and retry.
const E_PENDING: u32 = 0x8000_000A;
/// Idle-wait timeout. Sessions and stops arrive on the wake event, so this exists ONLY to keep
/// the drain heartbeat ticking over a desktop that composes nothing: the host convicts this
/// worker when it reads that stamp older than `max(gap/2, 250 ms)` (`pf-capture` stall
/// attribution) or 2 s (`pf_frame::health`, which then runs its recovery ladder). An INFINITE
/// wait here would report every idle desktop as a stalled worker.
const IDLE_WAIT_MS: u32 = 125;

/// HRESULT-shaped success test for the swap-chain DDIs (raw `NTSTATUS`/HRESULT: success iff non-negative).
#[inline]
fn hr_success(hr: NTSTATUS) -> bool {
    hr >= 0
}

/// Whether the swap-chain processing device's GPU scheduling is raised to REALTIME (the IddCx
/// 1.9 `IddCxSetRealtimeGPUPriority` DDI — "higher priority than any regular application can
/// set"). Default **ON**: minimum latency at every layer — a GPU-saturating game must not starve
/// the leg that feeds every captured frame into the encoder. The 2026-08 default-OFF (after an
/// RX 9070 XT field A/B blamed this raise for a metronomic ~1.8 s capture-stall class) at most
/// masked that still-unattributed stall — confirmed cases kept arriving with the raise off —
/// while regressing loaded NVIDIA boxes into feed starvation, so it was reverted. The per-box
/// A/B escape hatch remains: `setx /M PFVD_NO_RT_GPU 1` (any value) + a device restart disables
/// the raise — read via [`crate::log::knob`] (process environment, then the machine one, because WUDFHost's
/// own environment is stale until a reboot. The old `PFVD_RT_GPU` opt-in ladder
/// (off/thread/realtime) is gone; a stale `PFVD_RT_GPU` now just matches the default.
fn rt_gpu_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::log::knob("PFVD_NO_RT_GPU").is_none())
}

pub struct SwapChainProcessor {
    terminate: Arc<AtomicBool>,
    /// AUTO-reset event that releases the worker's idle wait: a fresh encode session or a stop
    /// reaches it at once instead of waiting out [`IDLE_WAIT_MS`]. `None` when the event could not
    /// be created — the worker then only has its timeout. It closes as a field, after `Drop` has
    /// joined the worker, so [`Self::wake`] can never signal a closed handle.
    wake: Option<OwnedHandle>,
    thread: Option<JoinHandle<()>>,
}

impl SwapChainProcessor {
    pub fn new() -> Self {
        let wake = OwnedHandle::event(false);
        if wake.is_none() {
            dbglog!("[pf-vd] swap-chain: wake event creation failed — timeout-only idle wait");
        }
        Self {
            terminate: Arc::new(AtomicBool::new(false)),
            wake,
            thread: None,
        }
    }

    /// Release the worker's idle wait so it re-runs the loop top — after an encode session or
    /// pool lands, and from `Drop`. `SetEvent` never blocks, so a caller may hold the monitor's
    /// `swap` guard across it. No-op when the event could not be created (the worker polls).
    pub fn wake(&self) {
        if let Some(h) = &self.wake {
            // SAFETY: `h` is our own event handle; it closes only after `Drop` joins the worker.
            let _ = unsafe { SetEvent(h.as_raw()) };
        }
    }

    /// Spawn the drain worker for a freshly assigned swap-chain. It runs at MMCSS `Distribution`
    /// priority (TIME_CRITICAL if MMCSS declines), owns `swap_chain` for its lifetime and deletes
    /// that object before returning. `monitor` is the weak link to its owner: the worker never
    /// keeps the monitor alive, and the owner's teardown joins it. Its idle wait covers
    /// `available_buffer_event` (the framework's surface-available event) AND this processor's
    /// wake event, so a new session or a stop reaches an idle display immediately.
    pub fn run(
        &mut self,
        swap_chain: IDDCX_SWAPCHAIN,
        device: Arc<Direct3DDevice>,
        available_buffer_event: HANDLE,
        monitor: Weak<Monitor>,
    ) {
        let wake = self.wake.as_ref().map(|h| Sendable(h.as_raw()));
        let available_buffer_event = Sendable(available_buffer_event);
        let swap_chain = Sendable(swap_chain);
        let terminate = self.terminate.clone();
        // For the log lines: 0 for a monitor the registry does not hold, whose worker only drains.
        let target_id = monitor.upgrade().map_or(0, |m| m.target_id());

        // The raw handle outlives the closure's move: a worker that never starts still owns the
        // swap-chain and must delete it, or IddCx keeps an undrained chain.
        let sc_raw = swap_chain.0;
        let spawned = thread::Builder::new().name("pf-vd-swapchain".into()).spawn(move || {
            // Rust 2021 disjoint closure captures would otherwise grab the raw `.0` FIELDS
            // directly (defeating the `Sendable` Send wrapper, since the handles inside are
            // `!Send`). Rebind the WHOLE wrappers here so the closure captures them as `Sendable`.
            let swap_chain = swap_chain;
            let wake = wake;
            let available_buffer_event = available_buffer_event;
            // This thread is the whole display's frame pump: at normal priority a display-stack
            // disturbance (DDC/HPD servicing, poller-software storms) starves it into
            // multi-hundred-ms delivery holes. Reverted when the registration drops, at exit.
            let _mmcss = Mmcss::distribution("swap-chain");

            Self::run_core(
                swap_chain.0,
                &device,
                (wake.map(|w| w.0), available_buffer_event.0),
                &terminate,
                &monitor,
                target_id,
            );

            dbglog!(
                "[pf-vd] swap-chain run_core RETURNED (target={target_id}) — deleting swap-chain, device drops next"
            );

            // Delete the swap-chain WDF object BEFORE the `Arc<Direct3DDevice>` drops (the swap-chain
            // referenced our device). `WdfObjectDelete` takes a WDFOBJECT.
            // SAFETY: `swap_chain` is a live IddCx swap-chain handle; we own the sole reference here and
            // the drain loop has exited.
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, swap_chain.0 as WDFOBJECT);
            }
        });

        match spawned {
            Ok(join_handle) => self.thread = Some(join_handle),
            Err(e) => {
                dbglog!("[pf-vd] swap-chain worker did not start: {e} — deleting the swap-chain");
                // SAFETY: the worker never ran, so this is the sole reference to a live IddCx
                // swap-chain handle that IddCx handed us.
                unsafe {
                    call_unsafe_wdf_function_binding!(WdfObjectDelete, sc_raw as WDFOBJECT);
                }
            }
        }
    }

    /// The drain loop. It upgrades `monitor` once per pass for the delivery gate and releases
    /// the strong count before it blocks, so the owner's teardown — which joins this thread
    /// before the last `Arc` goes — is the only thing that ends the monitor. A failed upgrade
    /// means this worker was never installed on a live monitor; it exits and the epilogue
    /// deletes the swap-chain.
    fn run_core(
        swap_chain: IDDCX_SWAPCHAIN,
        device: &Direct3DDevice,
        // (this processor's wake event — `None` if it could not be created, the framework's
        // surface-available event); one parameter so the argument count stays under the lint.
        events: (Option<WHANDLE>, HANDLE),
        terminate: &AtomicBool,
        monitor: &Weak<Monitor>,
        target_id: u32,
    ) {
        let (wake, available_buffer_event) = events;
        // `as_raw()` BORROWS our single reference — IddCx AddRefs its own — and it is released right
        // after the realtime raise below. An `into_raw()` here would orphan that reference and pin the
        // D3D device (its worker threads and VRAM) past the processor's drop.
        let dxgi_device = match device.device.cast::<IDXGIDevice>() {
            Ok(d) => d,
            Err(e) => {
                dbglog!("[pf-vd] swap-chain: ID3D11Device is not an IDXGIDevice: {e:?}");
                return;
            }
        };
        let set_device = IDARG_IN_SWAPCHAINSETDEVICE {
            pDevice: dxgi_device.as_raw().cast(),
        };
        // One shot: a failure here means the OS already unassigned this swap-chain, and
        // DXGI_ERROR_ACCESS_LOST on that handle never recovers. Returning lets the thread epilogue
        // delete it so the OS mints a fresh one — the reassign is what succeeds.
        // SAFETY: driver is loaded; `swap_chain` is valid; `set_device` points to valid local storage.
        let hr = unsafe { wdk_iddcx::IddCxSwapChainSetDevice(swap_chain, &set_device) };
        if !hr_success(hr) {
            dbglog!(
                "[pf-vd] swap-chain run_core: SetDevice failed ({hr:#x}, target={target_id}) — returning for a fresh swap-chain"
            );
            drop(dxgi_device);
            return;
        }
        dbglog!(
            "[pf-vd] swap-chain run_core: SetDevice OK (target={target_id}) — entering drain loop"
        );
        // GPU-scheduling raise for the swap-chain processing device — default ON, so the leg
        // feeding every captured frame into the ring outranks a GPU-saturating game (history +
        // `PFVD_NO_RT_GPU` escape hatch: [`rt_gpu_enabled`]). Best-effort, never fatal, issued
        // while our borrowed device reference is still alive (IddCx uses it synchronously); the
        // DDI may still decline (e.g. E_NOTIMPL on pre-WDDM-3.0 hardware).
        if rt_gpu_enabled() {
            let rt = IDARG_IN_SETREALTIMEGPUPRIORITY {
                pDevice: dxgi_device.as_raw().cast(),
            };
            // SAFETY: driver is loaded; `swap_chain` is the live assigned swap-chain whose
            // device bind just succeeded; `rt.pDevice` is that same bound DXGI device,
            // alive across the synchronous call; `rt` points to valid local storage.
            let hr = unsafe { wdk_iddcx::IddCxSetRealtimeGPUPriority(swap_chain, &rt) };
            if hr_success(hr) {
                dbglog!(
                    "[pf-vd] swap-chain: processing device raised to REALTIME GPU priority (default; PFVD_NO_RT_GPU disables) (target={target_id})"
                );
            } else {
                dbglog!(
                    "[pf-vd] swap-chain: realtime GPU priority declined ({hr:#x}) — normal scheduling (target={target_id})"
                );
            }
        }
        // Release our borrowed device reference — IddCx holds its own now. (Explicit drop so NLL
        // can't release it mid-loop while the swap-chain still references the raw ptr.)
        drop(dxgi_device);

        // The encode pool + session this worker feeds, re-read only when the monitor's encode
        // generation moves (see `Monitor::encode_gen`).
        let mut attached = crate::encode::pool::Attached::new();

        let mut logged_pending = false;
        let mut logged_frame = false;
        // An acquire came back empty since the last frame: the next frame was waited for.
        let mut pended = false;
        // The surface the encoder last read in place: ours until an acquire returns a frame.
        let mut last: Option<ID3D11Texture2D> = None;
        loop {
            // Check terminate at the TOP, every iteration. The success branch below does NOT re-check it,
            // so during a CONTINUOUS frame burst (DWM rendering the freshly-activated desktop) a thread the
            // OS unassigns — or that the processor is dropping — never sees the flag and loops on, pinning
            // its D3D device (and ~36 NVIDIA worker threads). That is THE reconnect leak; it only
            // reproduced at full speed (E_PENDING gaps DO check terminate and masked it under a debugger).
            // Without this, `SwapChainProcessor::drop`'s join can also block until the burst ends.
            if terminate.load(Ordering::Relaxed) {
                break;
            }
            // A sibling worker (or the pool at checkout) saw this device REMOVED: every worker on
            // the entry stops — the swap-chain would fail on it anyway, and the OS reassigns onto a
            // fresh device (immunity plan WP5 item 6).
            if device.is_removed() {
                dbglog!(
                    "[pf-vd] swap-chain run_core: device epoch {} removed (target={target_id}) — exiting for reassignment",
                    device.epoch()
                );
                break;
            }

            let Some(owner) = monitor.upgrade() else {
                dbglog!("[pf-vd] swap-chain run_core: monitor gone (target={target_id}) — exiting");
                break;
            };
            attached.refresh(&owner);
            // Blocking from here on: hand the strong count back so this thread never decides
            // when its own monitor drops.
            drop(owner);

            // ...Buffer2 is required once CAN_PROCESS_FP16 is set. AcquireSystemMemoryBuffer=FALSE
            // keeps the GPU surface (out.MetaData.pSurface), which the fused pass below reads.
            // Built zeroed + field-assigned (driver style) so a bindgen field-set difference
            // can't break a positional struct literal.
            let mut in_args = IDARG_IN_RELEASEANDACQUIREBUFFER2::default();
            #[allow(clippy::cast_possible_truncation)]
            {
                in_args.Size = size_of::<IDARG_IN_RELEASEANDACQUIREBUFFER2>() as u32;
            }
            in_args.AcquireSystemMemoryBuffer = 0;
            // Zeroed: bindgen (`derive_default`) derives `Default` or, for a raw pointer or a
            // union such as `MetaData`'s, emits one that zero-fills.
            let mut buffer = IDARG_OUT_RELEASEANDACQUIREBUFFER2::default();
            // SAFETY: driver is loaded; `swap_chain` is valid; in/out point to valid local storage.
            let hr: NTSTATUS = unsafe {
                wdk_iddcx::IddCxSwapChainReleaseAndAcquireBuffer2(
                    swap_chain,
                    &mut in_args,
                    &mut buffer,
                )
            };

            if (hr as u32) == E_PENDING {
                // Nothing composed: heartbeat only, stamped before the wait — never inside the
                // acquire window.
                attached.note_drain();
                pended = true;
                if !logged_pending {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: E_PENDING (target={target_id}) — swap-chain valid but DWM has composed NO frame yet"
                    );
                    logged_pending = true;
                }
                let surface = WHANDLE(available_buffer_event.cast());
                // SAFETY: `surface` is the framework-provided surface-available event, live for
                // this assignment; `w` is this processor's own wake event, which it closes only
                // after joining this thread.
                let waited = unsafe {
                    match wake {
                        Some(w) => WaitForMultipleObjects(&[w, surface], false, IDLE_WAIT_MS),
                        None => WaitForSingleObject(surface, IDLE_WAIT_MS),
                    }
                };
                // Wake, surface-available or the heartbeat timeout all just re-run the loop top,
                // which re-checks terminate, device removal and the encode slots. The wake event
                // is auto-reset with ONE waiter, so a `SetEvent` raised while this thread is not
                // waiting stays latched until its next wait — no wakeup is lost.
                if waited == WAIT_TIMEOUT
                    && let Some(tex) = last.take()
                {
                    // A whole idle wait with nothing composed: the desktop is still.
                    attached.retain(&tex);
                }
                if waited == WAIT_OBJECT_0
                    || waited == WAIT_TIMEOUT
                    || waited.0 == WAIT_OBJECT_0.0 + 1
                {
                    continue;
                }
                // The wait was cancelled or something unexpected happened.
                dbglog!(
                    "[pf-vd] swap-chain run_core: idle wait -> {:#x} (target={target_id}) — exiting",
                    waited.0
                );
                break;
            } else if hr_success(hr) {
                // The OS's display time for this frame — the provenance stamp the access unit
                // this frame becomes carries to the host.
                let display_qpc = buffer.MetaData.PresentDisplayQPCTime;
                // How the frame arrived, for the cadence line: one clock read, no GPU work.
                let frame = crate::encode::pool::Acquired {
                    display_qpc,
                    at_qpc: crate::encode::thread::qpc_now(),
                    number: buffer.MetaData.PresentationFrameNumber,
                    queued: !pended,
                };
                pended = false;
                // DWM composes on the previous surface from here on.
                last = None;
                if !logged_frame {
                    dbglog!(
                        "[pf-vd] swap-chain run_core: first frame acquired (target={target_id}) — DWM is compositing the virtual display"
                    );
                    logged_frame = true;
                }
                // The acquire TRANSFERS one surface reference to the driver, and holding it
                // leaks the swap-chain's whole surface set per assign/unassign cycle: adopt it
                // unconditionally and release it before `FinishedProcessingFrame`. The queued
                // GPU pass survives that (D3D defers destruction).
                let mut held = false;
                {
                    let raw = buffer.MetaData.pSurface as *mut core::ffi::c_void;
                    if !raw.is_null() {
                        // SAFETY: `raw` is the live surface IddCx just handed us, carrying the
                        // acquire's transferred reference; `from_raw` adopts exactly that one.
                        let res = unsafe { IDXGIResource::from_raw(raw) };
                        if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                            // The one fused pass into a free pool slot, or a drop at the pool.
                            // A bypass pool may hand the encoder the surface itself instead.
                            held = attached.offer(device, &tex, display_qpc);
                            last = held.then(|| tex.clone());
                            // Spike S5: one `CopyResource` into the probe's ring, or nothing.
                            #[cfg(feature = "encode-probe")]
                            crate::encode_probe::offer(device, &tex, display_qpc, target_id);
                        }
                        // `res` drops here: the acquire's surface reference is released,
                        // pre-Finished.
                    }
                }

                // SAFETY: driver is loaded; `swap_chain` is valid.
                let hr = unsafe { wdk_iddcx::IddCxSwapChainFinishedProcessingFrame(swap_chain) };
                if !hr_success(hr) {
                    break;
                }
                // Stamped only now: nothing may sit between acquire and Finished. The present
                // stamp feeds the compose-cadence histogram both modes are compared on.
                attached.note_frame(&frame);
                // The surface is the driver's until the next acquire returns a frame, so an
                // encoder still reading it holds that acquire (bounded). Never Finished: DWM
                // composes this head's next frame on it.
                if held {
                    attached.wait_release();
                }
            } else {
                // The swap-chain was likely abandoned (e.g. DXGI_ERROR_ACCESS_LOST) — exit the loop.
                break;
            }
        }

        // Worker exit: the OS unassigned this swap-chain (typically a SIBLING display churned the
        // topology) or it errored. The pool and the encode session stay on the MONITOR for the
        // next worker; a pool built for a dead device epoch is replaced at the next `SET_ENCODE`.
    }
}

impl Drop for SwapChainProcessor {
    fn drop(&mut self) {
        if let Some(handle) = self.thread.take() {
            // Store the flag BEFORE waking: the worker re-checks it at the loop top. Without the
            // wake, an idle worker would sit out its whole timeout before seeing the flag.
            self.terminate.store(true, Ordering::Relaxed);
            self.wake();
            // The worker deletes the swap-chain object before returning. `wake` closes after
            // this, as a field.
            let _ = handle.join();
        }
    }
}
