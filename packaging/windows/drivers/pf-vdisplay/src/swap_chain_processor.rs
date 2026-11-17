//! The swap-chain drain worker: one thread per assignment that consumes the IddCx swap-chain (so
//! the virtual monitor stays a usable display) and hands each acquired surface to the monitor's
//! encode pool.
//!
//! The OS presents the composited desktop to the driver through a swap-chain; the driver MUST
//! consume it (acquire → finished-processing) or the monitor stalls. This binds the pooled render
//! device (`IddCxSwapChainSetDevice`), loops acquire/finish, and inside that window issues exactly
//! one fused GPU pass into a free pool slot — or drops at the pool. Nothing else: telemetry is
//! stamped after `FinishedProcessingFrame`, because the window is what DWM waits on for this head.
//! Which call comes next is `pf_driver_proto::drain::step`; this file maps each result into it.
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

use pf_driver_proto::drain::{Acquire, Drain, Flags, Next, Seen, Wait, Why, step};
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
    encode::{
        pool::{Acquired, Attached},
        thread::qpc_now,
    },
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
/// the leg that feeds every captured frame into the encoder. `setx /M PFVD_NO_RT_GPU 1` (any
/// non-empty value) + a device restart disables the raise, for a per-box A/B. It is read via
/// [`crate::log::knob`] (process environment, then the machine one, because WUDFHost's own
/// environment is stale until a reboot). `PFVD_RT_GPU` is not read.
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
    /// that object before the device drops. `monitor` is the weak link to its owner: the worker
    /// never keeps the monitor alive, and the owner's teardown joins it. Its idle wait covers
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
            // Owns the device, which drops at the end of this closure: after the swap-chain delete.
            let ctx = Ctx {
                swap_chain: swap_chain.0,
                device,
                wake: wake.map(|w| w.0),
                surface: WHANDLE(available_buffer_event.0.cast()),
                terminate,
                monitor,
                target_id,
            };
            // This thread is the whole display's frame pump: at normal priority a display-stack
            // disturbance (DDC/HPD servicing, poller-software storms) starves it into
            // multi-hundred-ms delivery holes. Reverted when the registration drops, at exit.
            let _mmcss = Mmcss::distribution("swap-chain");

            ctx.run_core();

            dbglog!(
                "[pf-vd] swap-chain run_core RETURNED (target={target_id}) — deleting swap-chain, device drops next"
            );

            // Delete the swap-chain WDF object BEFORE the `Arc<Direct3DDevice>` drops (the swap-chain
            // referenced our device). `WdfObjectDelete` takes a WDFOBJECT.
            // SAFETY: `ctx.swap_chain` is a live IddCx swap-chain handle; we own the sole reference
            // here and the drain loop has exited.
            unsafe {
                call_unsafe_wdf_function_binding!(WdfObjectDelete, ctx.swap_chain as WDFOBJECT);
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
}

/// One drain worker's handles, built on its thread.
struct Ctx {
    swap_chain: IDDCX_SWAPCHAIN,
    device: Arc<Direct3DDevice>,
    /// This processor's wake event; `None` when it could not be created.
    wake: Option<WHANDLE>,
    /// The framework's surface-available event, live for this assignment.
    surface: WHANDLE,
    terminate: Arc<AtomicBool>,
    monitor: Weak<Monitor>,
    /// For the log lines: 0 for a monitor the registry does not hold, whose worker only drains.
    target_id: u32,
}

/// What the drain loop carries between passes besides the model's [`Drain`].
struct State {
    drain: Drain,
    /// The surface the encoder last read in place: ours until an acquire returns a frame.
    last: Option<ID3D11Texture2D>,
    /// The acquired surface and the acquire's reference to it, until the window offers it.
    surface: Option<IDXGIResource>,
    /// The frame the window just finished, for its cadence sample.
    frame: Option<Acquired>,
    /// The encode pool + session this worker feeds, re-read only when the monitor's encode
    /// generation moves (see `Monitor::encode_gen`).
    attached: Attached,
    logged_pending: bool,
    logged_frame: bool,
}

impl Ctx {
    /// The drain loop: binds the device, then does what [`step`] names until it names an exit.
    /// A retain and a finish go on at the loop top, and only the top leads to an acquire, so a
    /// stop is seen between any two frames: a frame burst that skipped it would pin the device
    /// and block the processor's join. The pool and the session stay on the monitor for the
    /// next worker; a pool built for a dead device epoch is replaced at the next `SET_ENCODE`.
    fn run_core(&self) {
        if !self.bind() {
            return;
        }
        let mut st = State {
            drain: Drain::default(),
            last: None,
            surface: None,
            frame: None,
            attached: Attached::new(),
            logged_pending: false,
            logged_frame: false,
        };
        let mut next = Next::Top;
        let why = loop {
            let seen = match next {
                Next::Top => self.top(&mut st),
                Next::Acquire => Seen::Acquired(self.acquire(&mut st)),
                Next::Wait => Seen::Waited(self.wait(&mut st)),
                Next::Retain => {
                    self.retain(&mut st);
                    self.top(&mut st)
                }
                Next::Offer {
                    display_qpc,
                    number,
                    queued,
                } => {
                    let frame = Acquired {
                        display_qpc,
                        at_qpc: qpc_now(),
                        number,
                        queued,
                    };
                    self.offer(&mut st, frame)
                }
                Next::Finish { hold } => {
                    Self::finish(&mut st, hold);
                    self.top(&mut st)
                }
                Next::Exit(why) => break why,
            };
            next = step(&mut st.drain, seen);
        };
        self.log_exit(why);
    }

    /// Binds the render device to the swap-chain and raises its GPU priority. `false` means the
    /// OS already unassigned this chain: DXGI_ERROR_ACCESS_LOST on that handle never recovers,
    /// so the worker returns and the epilogue deletes it for a fresh one.
    fn bind(&self) -> bool {
        let target_id = self.target_id;
        // `as_raw()` BORROWS our single reference — IddCx AddRefs its own — and it is released
        // when this returns, after the realtime raise. An `into_raw()` here would orphan that
        // reference and pin the D3D device (its worker threads and VRAM) past the processor's drop.
        let dxgi_device = match self.device.device.cast::<IDXGIDevice>() {
            Ok(d) => d,
            Err(e) => {
                dbglog!("[pf-vd] swap-chain: ID3D11Device is not an IDXGIDevice: {e:?}");
                return false;
            }
        };
        let set_device = IDARG_IN_SWAPCHAINSETDEVICE {
            pDevice: dxgi_device.as_raw().cast(),
        };
        // SAFETY: driver is loaded; `swap_chain` is valid; `set_device` points to valid local storage.
        let hr = unsafe { wdk_iddcx::IddCxSwapChainSetDevice(self.swap_chain, &set_device) };
        if !hr_success(hr) {
            dbglog!(
                "[pf-vd] swap-chain run_core: SetDevice failed ({hr:#x}, target={target_id}) — returning for a fresh swap-chain"
            );
            return false;
        }
        dbglog!(
            "[pf-vd] swap-chain run_core: SetDevice OK (target={target_id}) — entering drain loop"
        );
        // Best-effort, never fatal (see [`rt_gpu_enabled`]), issued while our borrowed device
        // reference is alive: IddCx uses it synchronously. The DDI may still decline (E_NOTIMPL
        // on pre-WDDM-3.0 hardware).
        if rt_gpu_enabled() {
            let rt = IDARG_IN_SETREALTIMEGPUPRIORITY {
                pDevice: dxgi_device.as_raw().cast(),
            };
            // SAFETY: driver is loaded; `swap_chain` is the live assigned swap-chain whose
            // device bind just succeeded; `rt.pDevice` is that same bound DXGI device,
            // alive across the synchronous call; `rt` points to valid local storage.
            let hr = unsafe { wdk_iddcx::IddCxSetRealtimeGPUPriority(self.swap_chain, &rt) };
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
        true
    }

    /// The loop top: the stop flag, then device removal, then the monitor, in the order they
    /// win. A removed device stops every worker on it; the OS reassigns onto a fresh one. The
    /// monitor is upgraded only when nothing above stops the pass, and the strong count drops
    /// before anything blocks, so this thread never decides when its own monitor drops. A
    /// failed upgrade means this worker was never installed on a live monitor.
    fn top(&self, st: &mut State) -> Seen {
        let terminate = self.terminate.load(Ordering::Relaxed);
        let device_removed = !terminate && self.device.is_removed();
        let owner = if terminate || device_removed {
            None
        } else {
            self.monitor.upgrade()
        };
        if let Some(owner) = &owner {
            st.attached.refresh(owner);
        }
        Seen::Top(Flags {
            terminate,
            device_removed,
            monitor_gone: owner.is_none(),
        })
    }

    /// Releases the last frame and acquires the next. A frame's surface is adopted here with
    /// the reference the acquire transferred, so [`Self::offer`] can release it before
    /// `FinishedProcessingFrame`.
    fn acquire(&self, st: &mut State) -> Acquire {
        // ...Buffer2 is required once CAN_PROCESS_FP16 is set. AcquireSystemMemoryBuffer=FALSE
        // keeps the GPU surface (out.MetaData.pSurface), which the fused pass reads. Built
        // zeroed + field-assigned so a bindgen field-set difference can't break a struct literal.
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
                self.swap_chain,
                &mut in_args,
                &mut buffer,
            )
        };
        if (hr as u32) == E_PENDING {
            return Acquire::Pending;
        }
        if !hr_success(hr) {
            return Acquire::Lost;
        }
        let raw = buffer.MetaData.pSurface as *mut core::ffi::c_void;
        if !raw.is_null() {
            // SAFETY: `raw` is the live surface IddCx just handed us, carrying the acquire's
            // transferred reference; `from_raw` adopts exactly that one.
            let res = unsafe { IDXGIResource::from_raw(raw) };
            st.surface = Some(res);
        }
        // `PresentDisplayQPCTime` is the provenance stamp the access unit carries to the host.
        Acquire::Frame {
            display_qpc: buffer.MetaData.PresentDisplayQPCTime,
            number: buffer.MetaData.PresentationFrameNumber,
        }
    }

    /// Nothing composed: stamps the heartbeat, then waits for the wake event, the surface event
    /// or [`IDLE_WAIT_MS`]. The wake event is auto-reset with ONE waiter, so a `SetEvent` raised
    /// while this thread is not waiting stays latched until its next wait.
    fn wait(&self, st: &mut State) -> Wait {
        // Before the wait, never inside the acquire window.
        st.attached.note_drain();
        if !st.logged_pending {
            dbglog!(
                "[pf-vd] swap-chain run_core: E_PENDING (target={}) — swap-chain valid but DWM has composed NO frame yet",
                self.target_id
            );
            st.logged_pending = true;
        }
        // SAFETY: `self.surface` is the framework-provided surface-available event, live for
        // this assignment; `w` is this processor's own wake event, which it closes only
        // after joining this thread.
        let (waited, first) = unsafe {
            match self.wake {
                Some(w) => (
                    WaitForMultipleObjects(&[w, self.surface], false, IDLE_WAIT_MS),
                    Wait::Wake,
                ),
                None => (
                    WaitForSingleObject(self.surface, IDLE_WAIT_MS),
                    Wait::Surface,
                ),
            }
        };
        match waited {
            WAIT_OBJECT_0 => first,
            WAIT_TIMEOUT => Wait::Timeout,
            w if w.0 == WAIT_OBJECT_0.0 + 1 => Wait::Surface,
            w => Wait::Failed(w.0),
        }
    }

    /// A whole idle wait with nothing composed: the desktop is still, so the pool keeps the held
    /// surface as its picture.
    fn retain(&self, st: &mut State) {
        if let Some(tex) = st.last.take() {
            st.attached.retain(&self.device, &tex);
        }
    }

    /// The acquire window: the one fused pass into a free pool slot (or a drop at the pool, or a
    /// bypass pool's hold), then `FinishedProcessingFrame`. The surface reference is released
    /// before the finish: holding it leaks the swap-chain's whole surface set per assign cycle,
    /// and the queued GPU pass survives the release (D3D defers destruction).
    fn offer(&self, st: &mut State, frame: Acquired) -> Seen {
        // DWM composes on the previous surface from here on.
        st.last = None;
        if !st.logged_frame {
            dbglog!(
                "[pf-vd] swap-chain run_core: first frame acquired (target={}) — DWM is compositing the virtual display",
                self.target_id
            );
            st.logged_frame = true;
        }
        let display_qpc = frame.display_qpc;
        st.frame = Some(frame);
        let mut hold = false;
        if let Some(res) = st.surface.take()
            && let Ok(tex) = res.cast::<ID3D11Texture2D>()
        {
            hold = st.attached.offer(&self.device, &tex, display_qpc);
            st.last = hold.then(|| tex.clone());
            // Spike S5: one `CopyResource` into the probe's ring, or nothing.
            #[cfg(feature = "encode-probe")]
            crate::encode_probe::offer(&self.device, &tex, display_qpc, self.target_id);
        }
        // SAFETY: driver is loaded; `swap_chain` is valid.
        let hr = unsafe { wdk_iddcx::IddCxSwapChainFinishedProcessingFrame(self.swap_chain) };
        Seen::Finished {
            ok: hr_success(hr),
            hold,
        }
    }

    /// After `FinishedProcessingFrame`: the heartbeat and the compose-cadence sample, then, when
    /// the encoder reads the surface in place, its bounded wait. That surface is the driver's
    /// until the next acquire returns a frame, and DWM composes this head's next frame on it.
    fn finish(st: &mut State, hold: bool) {
        if let Some(frame) = st.frame.take() {
            st.attached.note_frame(&frame);
        }
        if hold {
            st.attached.wait_release();
        }
    }

    /// The exit's log line. A stop and a lost chain log only the epilogue's line.
    fn log_exit(&self, why: Why) {
        let target_id = self.target_id;
        match why {
            Why::DeviceRemoved => {
                dbglog!(
                    "[pf-vd] swap-chain run_core: device epoch {} removed (target={target_id}) — exiting for reassignment",
                    self.device.epoch()
                );
            }
            Why::MonitorGone => {
                dbglog!("[pf-vd] swap-chain run_core: monitor gone (target={target_id}) — exiting");
            }
            Why::WaitFailed(code) => {
                dbglog!(
                    "[pf-vd] swap-chain run_core: idle wait -> {code:#x} (target={target_id}) — exiting"
                );
            }
            Why::Terminate | Why::Lost => {}
        }
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
