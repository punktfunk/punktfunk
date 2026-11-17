//! DriverEntry + driver_add — the IddCx device bring-up. wdk-build links the UMDF
//! `WdfDriverStubUm` whose `FxDriverEntryUm` forwards to the exported `DriverEntry`. Adapter creation is
//! deferred to the first `EvtDeviceD0Entry`; monitors are created on demand by the control
//! plane. Instrumented with `dbglog!` for on-glass bring-up.

use wdk_iddcx::nt_success;
use wdk_sys::{
    GUID, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDF_DRIVER_CONFIG,
    WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS, WDFDEVICE, WDFDRIVER,
    call_unsafe_wdf_function_binding, iddcx,
};

use crate::callbacks;

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    dbglog!("[pf-vd] DriverEntry");
    // Names the backends linked into this DLL, so a loaded driver says so on glass.
    dbglog!(
        "[pf-vd] encode: {} linked",
        crate::encode::backends_linked().join(" ")
    );
    // Before the first encoder opens: PyroWave's Vulkan instance hangs in session 0 without
    // these.
    crate::encode::thread::disable_implicit_vulkan_layers();
    crate::log::install_tracing_bridge();
    let mut config = WDF_DRIVER_CONFIG {
        Size: core::mem::size_of::<WDF_DRIVER_CONFIG>() as ULONG,
        EvtDriverDeviceAdd: Some(driver_add),
        ..Default::default()
    };
    // SAFETY: driver + registry_path are loader-provided; config is valid for the call.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            WDF_NO_HANDLE.cast::<WDFDRIVER>()
        )
    };
    dbglog!("[pf-vd] WdfDriverCreate -> {st:#x}");
    st
}

extern "C" fn driver_add(_driver: WDFDRIVER, mut init: PWDFDEVICE_INIT) -> NTSTATUS {
    dbglog!("[pf-vd] driver_add");
    // Defer adapter creation to the first D0 entry.
    let mut pnp = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: core::mem::size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG,
        EvtDeviceD0Entry: Some(callbacks::device_d0_entry),
        ..Default::default()
    };
    // SAFETY: init is the framework-provided device-init; pnp is valid for the call.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, init, &mut pnp);
    }

    // A control handle's close is the owner-gone signal (`watchdog::evt_file_cleanup`), so a
    // crashed host's monitors depart at once instead of after the watchdog window. Set before
    // IddCx's own init: should the class extension take the slot, the silence watchdog still
    // covers a dead host.
    let mut files = wdk_sys::WDF_FILEOBJECT_CONFIG {
        Size: core::mem::size_of::<wdk_sys::WDF_FILEOBJECT_CONFIG>() as ULONG,
        EvtFileCleanup: Some(crate::watchdog::evt_file_cleanup),
        AutoForwardCleanupClose: wdk_sys::_WDF_TRI_STATE::WdfUseDefault,
        FileObjectClass: wdk_sys::_WDF_FILEOBJECT_CLASS::WdfFileObjectWdfCannotUseFsContexts,
        ..Default::default()
    };
    // SAFETY: init is the framework-provided device-init; files is valid for the call.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetFileObjectConfig,
            init,
            &mut files,
            WDF_NO_OBJECT_ATTRIBUTES
        );
    }

    // Build the IddCx client config. `.Size` = size_of (1.10 structs, 1.10 fw).
    let cfg = iddcx::IDD_CX_CLIENT_CONFIG {
        Size: core::mem::size_of::<iddcx::IDD_CX_CLIENT_CONFIG>() as u32,
        EvtIddCxAdapterInitFinished: Some(callbacks::adapter_init_finished),
        EvtIddCxParseMonitorDescription: Some(callbacks::parse_monitor_description),
        EvtIddCxMonitorGetDefaultDescriptionModes: Some(callbacks::monitor_get_default_modes),
        EvtIddCxMonitorQueryTargetModes: Some(callbacks::monitor_query_modes),
        EvtIddCxAdapterCommitModes: Some(callbacks::adapter_commit_modes),
        // The *2 mode DDIs and the HDR callbacks. CAN_PROCESS_FP16 (adapter.rs) obligates this set:
        // without it the OS rejects the adapter at init. The OS prefers *2 on IddCx 1.10 and falls
        // back to the v1 callbacks above down-level, so both stay registered.
        EvtIddCxParseMonitorDescription2: Some(callbacks::parse_monitor_description2),
        EvtIddCxMonitorQueryTargetModes2: Some(callbacks::monitor_query_modes2),
        EvtIddCxAdapterCommitModes2: Some(callbacks::adapter_commit_modes2),
        EvtIddCxAdapterQueryTargetInfo: Some(callbacks::query_target_info),
        EvtIddCxMonitorSetDefaultHdrMetaData: Some(callbacks::set_default_hdr_metadata),
        EvtIddCxMonitorSetGammaRamp: Some(callbacks::set_gamma_ramp),
        EvtIddCxMonitorAssignSwapChain: Some(callbacks::assign_swap_chain),
        EvtIddCxMonitorUnassignSwapChain: Some(callbacks::unassign_swap_chain),
        // Obligated for a remote-session adapter (the seat devnode's role); harmless on the console,
        // where the OS never calls it because every monitor ships an EDID.
        EvtIddCxMonitorGetPhysicalSize: Some(callbacks::monitor_get_physical_size),
        EvtIddCxDeviceIoControl: Some(callbacks::device_io_control),
        ..Default::default()
    };

    // SAFETY: init is the framework device-init; cfg is fully populated + sized. (Links IddCxStub.)
    let status = unsafe { wdk_iddcx::IddCxDeviceInitConfig(init, &cfg) };
    dbglog!("[pf-vd] IddCxDeviceInitConfig -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    let mut device: WDFDEVICE = core::ptr::null_mut();
    // Attributes rather than WDF_NO_OBJECT_ATTRIBUTES, only for the cleanup callback: it drops every
    // monitor's swap-chain worker on device removal (PnP / unload) so the worker threads don't linger
    // into teardown. Execution/Synchronization must be spelled out — a zeroed field is *Invalid*, not
    // InheritFromParent. No context type; nothing reads state back off the WDFDEVICE.
    let mut dev_attr = wdk_sys::WDF_OBJECT_ATTRIBUTES {
        Size: core::mem::size_of::<wdk_sys::WDF_OBJECT_ATTRIBUTES>() as u32,
        ExecutionLevel: wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope:
            wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        EvtCleanupCallback: Some(callbacks::device_cleanup),
        ..Default::default()
    };
    // SAFETY: init configured above; dev_attr is a valid attributes block.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfDeviceCreate, &mut init, &mut dev_attr, &mut device)
    };
    dbglog!("[pf-vd] WdfDeviceCreate -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    // SAFETY: device is the just-created WDFDEVICE.
    let status = unsafe { wdk_iddcx::IddCxDeviceInitialize(device) };
    dbglog!("[pf-vd] IddCxDeviceInitialize -> {status:#x}");
    if !nt_success(status) {
        return status;
    }

    // The host-gone watchdog timer, parented to this device — created before the interface exists,
    // so no IOCTL can arrive with the timer still missing. `adapter_init_finished` arms it.
    let status = crate::watchdog::create(device);
    if !nt_success(status) {
        return status;
    }

    // Expose the owned pf-vdisplay control interface: the host opens this GUID and drives the proto control
    // plane (IOCTL_ADD/REMOVE/PING/…) which arrives at EvtIddCxDeviceIoControl. NOT SudoVDA's GUID. (The
    // upstream uses a socket instead, so it has no interface; ours is IOCTL-based.)
    let (d1, d2, d3, d4) = pf_driver_proto::interface_guid_fields();
    let guid = GUID {
        Data1: d1,
        Data2: d2,
        Data3: d3,
        Data4: d4,
    };
    // SAFETY: device is the just-created WDFDEVICE; guid lives for the call; no reference string.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &guid,
            core::ptr::null()
        )
    };
    dbglog!("[pf-vd] WdfDeviceCreateDeviceInterface -> {status:#x}");
    status
}
