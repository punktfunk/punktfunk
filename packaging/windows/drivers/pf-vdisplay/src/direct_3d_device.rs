//! The render-side D3D11 device the swap-chain processor binds to the IddCx swap-chain.
//!
//! The D3D/DXGI types are the `windows` crate (refcounted COM, no manual Drop); the swap-chain/LUID
//! hand-off to the wdk-sys IddCx world happens via raw pointers in `swap_chain_processor.rs`.
//!
//! Binding this device to the swap-chain keeps the monitor a live display, and the encode pool
//! runs its GPU passes on its immediate context from each swap-chain processor thread. The device
//! is POOLED across processors (one per render LUID, [`pooled_device`]), so two live monitors'
//! worker threads share it concurrently — creation must NOT pass
//! `D3D11_CREATE_DEVICE_SINGLETHREADED`, and the immediate context is `SetMultithreadProtected`
//! (it has no internal locking of its own).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::{
    Win32::{
        Foundation::{BOOL, E_FAIL, LUID},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
                D3D11_CREATE_DEVICE_PREVENT_ALTERING_LAYER_SETTINGS_FROM_REGISTRY,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
            },
            Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory5},
        },
    },
    core::{Error, Interface},
};

#[derive(Debug)]
pub struct Direct3DDevice {
    // The following are already refcounted, so they're safe to use directly without additional drop impls
    _dxgi_factory: IDXGIFactory5,
    _adapter: IDXGIAdapter1,
    pub device: ID3D11Device,
    /// The shared immediate context — the encode pool's passes and copies run on it from each
    /// swap-chain processor thread. Pooled across processors, so it is `SetMultithreadProtected` at
    /// init: an immediate context has no internal locking, and two concurrent monitors' workers would
    /// otherwise race it (undefined behavior inside the UMD).
    pub device_context: ID3D11DeviceContext,
    /// This device object's epoch (immunity plan D2): minted from [`DEVICE_EPOCH`] at `init`, so a
    /// TDR recreate on the SAME LUID is a different epoch and LUID equality is never mistaken for
    /// D3D-object compatibility. The pool refuses a surface from another epoch, and the seed
    /// hands a kept frame out only to its own.
    epoch: u32,
    /// Set once ANY worker observed this device removed (`GetDeviceRemovedReason` failed). Every
    /// worker on the entry stops using it and [`pooled_device`] recreates instead of handing it out.
    removed: AtomicBool,
}

impl Direct3DDevice {
    /// See the `epoch` field.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Whether a worker has flagged this device removed ([`Self::mark_removed`]).
    pub fn is_removed(&self) -> bool {
        self.removed.load(Ordering::Acquire)
    }

    /// Flag the device removed for every holder + the pool (one observation is enough — a removed
    /// device never comes back).
    pub fn mark_removed(&self) {
        if !self.removed.swap(true, Ordering::AcqRel) {
            dbglog!(
                "[pf-vd] D3D device epoch {} marked REMOVED — workers stop, pool recreates",
                self.epoch
            );
        }
    }

    /// Create the render device for `adapter_luid`. Go through [`pooled_device`], which owns the
    /// one-device-per-LUID rule and the reason for it.
    pub fn init(adapter_luid: LUID) -> Result<Self, Error> {
        // SAFETY: a plain DXGI factory-creation call; `?` returns the error on failure.
        let dxgi_factory =
            unsafe { CreateDXGIFactory2::<IDXGIFactory5>(DXGI_CREATE_FACTORY_FLAGS(0))? };

        // SAFETY: `dxgi_factory` is the live factory just created; `adapter_luid` is a by-value LUID.
        let adapter = unsafe { dxgi_factory.EnumAdapterByLuid::<IDXGIAdapter1>(adapter_luid)? };

        let mut device = None;
        let mut device_context = None;

        // NO `D3D11_CREATE_DEVICE_SINGLETHREADED`: the pool shares this device across every
        // swap-chain processor on the LUID, so that flag's single-caller guarantee is false.
        const BASE: D3D11_CREATE_DEVICE_FLAG = D3D11_CREATE_DEVICE_FLAG(
            D3D11_CREATE_DEVICE_BGRA_SUPPORT.0
                | D3D11_CREATE_DEVICE_PREVENT_ALTERING_LAYER_SETTINGS_FROM_REGISTRY.0,
        );
        let mut create = |flags: D3D11_CREATE_DEVICE_FLAG| {
            // SAFETY: `adapter` is a live IDXGIAdapter1; `device`/`device_context` are valid local
            // out-params (checked for None below); the flag set + SDK version are valid constants.
            unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    None,
                    flags,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut device_context),
                )
            }
        };
        // `IMFDXGIDeviceManager::ResetDevice` demands VIDEO_SUPPORT: without it Media Foundation
        // fails `SET_D3D_MANAGER` with a bare E_FAIL. The fallback keeps capture on an adapter
        // that refuses the flag — no other backend needs it.
        if let Err(e) = create(BASE | D3D11_CREATE_DEVICE_VIDEO_SUPPORT) {
            dbglog!(
                "[pf-vd] D3D11 device refused VIDEO_SUPPORT ({e:?}) — retrying without it; the \
                 Media Foundation backend will not open on this adapter"
            );
            create(BASE)?;
        }

        let device = device.ok_or_else(|| Error::new(E_FAIL, "ID3D11Device not found"))?;
        let device_context =
            device_context.ok_or_else(|| Error::new(E_FAIL, "ID3D11DeviceContext not found"))?;

        // The pool hands this device (and its immediate context) to every processor on the LUID, and
        // an immediate context is not thread-safe by itself — turn on the runtime's per-call critical
        // section. (D3D11.4 interface, guaranteed on the Win11-22H2 OS floor; if the cast ever fails
        // we log and continue — a single monitor is still safe, concurrent ones would not be.)
        match device_context.cast::<ID3D11Multithread>() {
            Ok(mt) => {
                // SAFETY: plain setter on the live context's multithread interface; the returned
                // previous-state BOOL carries no obligation.
                unsafe {
                    let _ = mt.SetMultithreadProtected(BOOL::from(true));
                }
            }
            Err(e) => dbglog!(
                "[pf-vd] ID3D11Multithread unavailable ({e:?}) — immediate context left unprotected"
            ),
        }

        let epoch = DEVICE_EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
        dbglog!("[pf-vd] Direct3DDevice::init OK — epoch {epoch}");

        Ok(Self {
            _dxgi_factory: dxgi_factory,
            _adapter: adapter,
            device,
            device_context,
            epoch,
            removed: AtomicBool::new(false),
        })
    }
}

/// ONE shared D3D render device PER RENDER LUID, reused across every swap-chain assignment.
/// A fresh `Direct3DDevice` per assign (the swap-chain flap fires several per session) leaks the
/// NVIDIA UMD's worker threads and VRAM: they are never reclaimed on release, even after our
/// refcount drops to 0. Pooling keeps one stable thread set per adapter: the processors borrow
/// an `Arc`, so the device outlives them and is never re-created. A bounded MAP, not one slot
/// (immunity plan WP5): on a hybrid iGPU+dGPU box two live monitors on different adapters must
/// not evict each other's entry every assignment.
static DEVICE_POOL: Mutex<Vec<(i64, Arc<Direct3DDevice>)>> = Mutex::new(Vec::new());

/// How many adapters the pool keeps devices for — beyond this the oldest entry goes.
const DEVICE_POOL_CAP: usize = 4;

/// Minted on EVERY successful `Direct3DDevice::init` (see `Direct3DDevice::epoch`).
static DEVICE_EPOCH: AtomicU32 = AtomicU32::new(0);

/// HRESULT of the latest failed `Direct3DDevice::init`, `0` once one succeeds. A device that will
/// not create is why no swap-chain takes, so `SET_ENCODE` reports this code.
static LAST_INIT_ERROR: AtomicI32 = AtomicI32::new(0);

/// See [`LAST_INIT_ERROR`].
pub fn last_init_error() -> Option<i32> {
    Some(LAST_INIT_ERROR.load(Ordering::Relaxed)).filter(|&hr| hr != 0)
}

/// Get-or-create the pooled D3D device for `luid`. Re-creates when the entry is gone, was flagged
/// removed by a worker, or reports removal at checkout; the old `Arc` drops once its last
/// processor releases it. Every create sets or clears [`last_init_error`].
pub fn pooled_device(luid: LUID) -> Option<Arc<Direct3DDevice>> {
    let key = (i64::from(luid.HighPart) << 32) | i64::from(luid.LowPart);
    let mut pool = DEVICE_POOL.lock().ok()?;
    if let Some(pos) = pool.iter().position(|(k, _)| *k == key) {
        let dev = &pool[pos].1;
        // A TDR / driver reset REMOVES the pooled device permanently; handing it out again gives
        // every future swap-chain a dead device (SetDevice fail-loop → black virtual display until
        // device teardown). A worker may already have flagged it; otherwise ask the device.
        // SAFETY: plain status query on the live pooled device.
        let alive = !dev.is_removed() && unsafe { dev.device.GetDeviceRemovedReason() }.is_ok();
        if alive {
            return Some(dev.clone());
        }
        dev.mark_removed();
        dbglog!(
            "[pf-vd] pooled D3D device epoch {} was REMOVED — recreating on {key:#x}",
            dev.epoch()
        );
        pool.remove(pos);
    }
    match Direct3DDevice::init(luid) {
        Ok(d) => {
            LAST_INIT_ERROR.store(0, Ordering::Relaxed);
            let a = Arc::new(d);
            if pool.len() >= DEVICE_POOL_CAP {
                pool.remove(0);
            }
            pool.push((key, a.clone()));
            Some(a)
        }
        Err(e) => {
            LAST_INIT_ERROR.store(e.code().0, Ordering::Relaxed);
            dbglog!("[pf-vd] pooled Direct3DDevice::init failed: {e:?}");
            None
        }
    }
}
