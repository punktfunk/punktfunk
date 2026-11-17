//! DXGI adapter enumeration for the Settings "GPU" picker.
//!
//! Streaming (decode + present) runs in the spawned `punktfunk-session` binary; the shell only
//! needs the list of real (hardware) adapters to offer on a multi-GPU box (a hybrid laptop or an
//! eGPU). The picked adapter description is persisted (`crate::trust::Settings::adapter`); the
//! session binary exports it as `PUNKTFUNK_VK_ADAPTER` at startup unless that is already set.

use windows::core::Interface;
use windows::Win32::dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

/// The adapter's human-readable description.
fn adapter_name(adapter: &IDXGIAdapter) -> String {
    let mut d = windows::Win32::dxgi::DXGI_ADAPTER_DESC::default();
    // SAFETY: a read-only COM call on the live `adapter` borrow, writing only the local
    // descriptor through the out-param; `&IDXGIAdapter` holds a reference, so it stays live.
    if unsafe { adapter.GetDesc(&mut d) }.is_ok() {
        String::from_utf16_lossy(&d.Description)
            .trim_end_matches('\0')
            .to_string()
    } else {
        "<unknown adapter>".into()
    }
}

/// Every DXGI adapter, in enumeration order.
fn all_adapters() -> Vec<IDXGIAdapter> {
    // SAFETY: DXGI factory creation takes no pointer and returns an owned factory or an error,
    // matched on below before anything uses it.
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut v = Vec::new();
    let mut i = 0u32;
    // SAFETY: a COM call on the live factory above; it takes an index and yields an owned adapter,
    // and the `Ok` pattern is what proves one came back.
    while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        if let Ok(a) = a.cast::<IDXGIAdapter>() {
            v.push(a);
        }
    }
    v
}

/// Descriptions of the real (hardware, non-WARP) GPUs — the Settings GPU picker's option list.
/// The picker only shows when this has more than one entry.
///
/// **Deduplicated by description**, because the description IS the identity everywhere
/// downstream: the pick is persisted as that string (`Settings::adapter`) and matched by
/// name in the session binary (`PUNKTFUNK_VK_ADAPTER`). So two entries with the same name
/// are one selectable choice however many times DXGI enumerates them — listing it twice
/// only offers the user a meaningless coin flip.
pub fn adapter_names() -> Vec<String> {
    const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2; // dxgi.h; windows-rs types it i32, Flags is u32
    let mut names: Vec<String> = Vec::new();
    for a in all_adapters() {
        let desc1 = a
            .cast::<windows::Win32::dxgi::IDXGIAdapter1>()
            .ok()
            .and_then(|a1| {
                let mut d = windows::Win32::dxgi::DXGI_ADAPTER_DESC1::default();
                // SAFETY: a read-only COM call on the adapter just cast, writing only the local
                // descriptor, which is discarded unless the call reports success.
                unsafe { a1.GetDesc1(&mut d) }.is_ok().then_some(d)
            });
        let name = adapter_name(&a);
        // Which adapters DXGI actually returned, and whether repeats share a LUID (one
        // adapter enumerated twice) or are distinct devices with the same description.
        if let Some(d) = &desc1 {
            tracing::debug!(
                name = %name,
                luid = format!("{:08x}-{:08x}", d.AdapterLuid.HighPart, d.AdapterLuid.LowPart),
                vendor = format_args!("{:#06x}", d.VendorId),
                device = format_args!("{:#06x}", d.DeviceId),
                flags = d.Flags,
                "DXGI adapter"
            );
        }
        if desc1.is_some_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0) {
            continue; // WARP / software renderer — never a streaming target
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}
