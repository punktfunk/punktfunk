//! The `windows` 0.58 → 0.62 bridge. Every COM object the shared session and its backends see
//! is a [`bridge`]d `QueryInterface` of the driver's own 0.58 object, so the two crates never
//! wrap each other's pointer. The targets a pool slot is written in are the shared session's.

use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

use crate::direct_3d_device::Direct3DDevice;

pub use pf_encode_session::Fail;
pub use pf_encode_session::open::AdapterId;
pub use pf_encode_session::targets::{InputKind, Targets, source_format};

/// The pooled device's adapter, read once.
pub fn adapter_of(device: &Direct3DDevice) -> Option<AdapterId> {
    // SAFETY: plain queries on the live pooled device; each result is checked before use.
    let desc = unsafe {
        device
            .device
            .cast::<IDXGIDevice>()
            .ok()?
            .GetAdapter()
            .ok()?
            .GetDesc()
            .ok()?
    };
    Some(AdapterId {
        luid: windows62::Win32::Foundation::LUID {
            LowPart: desc.AdapterLuid.LowPart,
            HighPart: desc.AdapterLuid.HighPart,
        },
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
    })
}

/// A `windows` 0.62 view of a 0.58 COM object: a real `QueryInterface`, so the result owns
/// its own reference and neither crate ever wraps the other's pointer.
pub fn bridge<T: windows62::core::Interface>(obj: &impl Interface) -> Result<T, Fail> {
    use windows62::core::Interface as _;
    let raw = obj.as_raw();
    // SAFETY: `raw` is the live COM pointer `obj` owns for the duration of this call;
    // `from_raw_borrowed` takes no reference of its own and `cast` AddRefs through QI.
    let unk = unsafe { windows62::core::IUnknown::from_raw_borrowed(&raw) };
    unk.ok_or((-8, "bridge"))?.cast::<T>().map_err(|e| {
        dbglog!("[pf-vd] encode: 0.58→0.62 bridge QI failed: {e:?}");
        (-8, "bridge")
    })
}
