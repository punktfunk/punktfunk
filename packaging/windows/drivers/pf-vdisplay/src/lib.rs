//! pf-vdisplay — the all-Rust UMDF IddCx virtual-display driver, on wdk-sys and the owned
//! `pf_driver_proto` ABI. Port notes: design/windows-host-rewrite.md §6.3.
//!
//! `entry` is DriverEntry and `driver_add`: the IddCx client config and the control interface
//! the host opens. The adapter comes up at the first `EvtDeviceD0Entry` (`callbacks`); the
//! control plane creates monitors on demand (`control`, `monitor`); each swap chain gets a drain
//! worker (`swap_chain_processor`) that feeds the in-driver encoder (`encode`). The driver
//! exports `IddMinimumVersionRequired` below.

#![allow(non_snake_case, clippy::missing_safety_doc)]
// The drivers workspace denies `unsafe_op_in_unsafe_fn` and `clippy::undocumented_unsafe_blocks`
// (`[workspace.lints]`): an unsafe op sits in an explicit `unsafe {}` block that carries its
// proof. An IddCx driver is FFI-bound (D3D11, IddCx DDIs, cross-process textures), so these
// gates audit its unsafe instead of removing it.

#[macro_use]
mod log;
mod adapter;
mod callbacks;
mod control;
mod cursor;
mod cursor_cell;
mod direct_3d_device;
mod encode;
#[cfg(feature = "encode-probe")]
mod encode_probe;
mod entry;
mod modes;
mod monitor;
mod registry;
mod swap_chain_processor;
mod watchdog;
mod worker;

use wdk_sys::NTSTATUS;

// NTSTATUS codes the driver returns (wdk-sys doesn't surface all of these as constants).
pub(crate) const STATUS_SUCCESS: NTSTATUS = 0;
pub(crate) const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002u32 as NTSTATUS;
pub(crate) const STATUS_NOT_FOUND: NTSTATUS = 0xC000_0225u32 as NTSTATUS;
pub(crate) const STATUS_DEVICE_NOT_READY: NTSTATUS = 0xC000_00A3u32 as NTSTATUS;
pub(crate) const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000Du32 as NTSTATUS;
pub(crate) const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC000_0023u32 as NTSTATUS;
pub(crate) const STATUS_ACCESS_DENIED: NTSTATUS = 0xC000_0022u32 as NTSTATUS;

/// IddCx (stub mode) requires the driver to export the minimum IddCx framework version it needs — the
/// `#ifndef IDD_STUB` branch of `IddCxFuncEnum.h` that normally emits it is compiled out under
/// `IDD_STUB`.
///
/// `10` = IddCx 1.10, the floor the driver needs: the drain loop calls
/// `IddCxSwapChainReleaseAndAcquireBuffer2` (1.10) unconditionally and the swap-chain worker uses
/// `IddCxSetRealtimeGPUPriority` (1.9); the product floor is Windows 11 22H2 / build 22621,
/// whose framework is 1.10 (the installer gate — `MIN_HOST_BUILD` in punktfunk-setup —
/// exists for this driver). A lower value would let the driver load against a SHORT
/// `IddFunctions` table, where dispatching a newer DDI reads past the populated entries.
/// With `10`, an older framework fails the bind cleanly (driver doesn't load) instead of
/// limping into undefined dispatch.
#[unsafe(no_mangle)]
pub static IddMinimumVersionRequired: wdk_sys::ULONG = 10;
