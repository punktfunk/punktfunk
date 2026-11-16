//! The OS mode structs the mode DDIs and `IddCxMonitorUpdateModes2` emit, stamped from the
//! numbers [`pf_driver_proto::vdisplay`] computes (and tests) on any OS.

use pf_driver_proto::vdisplay;
use wdk_sys::iddcx;

/// Stamp [`vdisplay::signal_info`]'s numbers into the OS `DISPLAYCONFIG_VIDEO_SIGNAL_INFO` —
/// one description for both mode DDI families, differing only in `v_sync_freq_divider` (0 for
/// monitor modes, 1 for target modes, per the DDI contract). `mhz` is the refresh in
/// millihertz. Total size equals active size: no fabricated blanking.
fn signal_info(
    width: u32,
    height: u32,
    mhz: u32,
    v_sync_freq_divider: u32,
) -> wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    let n = vdisplay::signal_info(width, height, mhz, v_sync_freq_divider);
    let region = wdk_sys::DISPLAYCONFIG_2DREGION {
        cx: width,
        cy: height,
    };
    let mut si = wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
        pixelRate: n.pixel_rate,
        hSyncFreq: wdk_sys::DISPLAYCONFIG_RATIONAL {
            Numerator: n.h_sync_num,
            Denominator: 1,
        },
        vSyncFreq: wdk_sys::DISPLAYCONFIG_RATIONAL {
            Numerator: n.v_sync_num,
            Denominator: n.v_sync_den,
        },
        totalSize: region,
        activeSize: region,
        scanLineOrdering:
            wdk_sys::DISPLAYCONFIG_SCANLINE_ORDERING::DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE,
        ..Default::default()
    };
    // union { AdditionalSignalInfo bitfield | videoStandard:u32 } — the proto packs the
    // vSyncFreqDivider into bits 16..21 of the "other" video standard.
    si.__bindgen_anon_1.videoStandard = n.video_standard;
    si
}

/// `DISPLAYCONFIG_VIDEO_SIGNAL_INFO` for a monitor mode (vSyncFreqDivider = 0, per the DDI contract).
pub fn display_info(width: u32, height: u32, mhz: u32) -> wdk_sys::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    signal_info(width, height, mhz, 0)
}

/// `IDDCX_TARGET_MODE` for a scan-out mode (vSyncFreqDivider = 1, per the DDI contract).
pub fn target_mode(width: u32, height: u32, mhz: u32) -> iddcx::IDDCX_TARGET_MODE {
    iddcx::IDDCX_TARGET_MODE {
        Size: core::mem::size_of::<iddcx::IDDCX_TARGET_MODE>() as u32,
        TargetVideoSignalInfo: wdk_sys::DISPLAYCONFIG_TARGET_MODE {
            targetVideoSignalInfo: signal_info(width, height, mhz, 1),
        },
        ..Default::default()
    }
}

/// The wire bit depth every `*2` (HDR) mode advertises: 8 and 10 bpc RGB, so the OS offers HDR10
/// modes, and no YCbCr. The bindgen enum is `ModuleConsts`, so the constants are plain ints and
/// OR directly.
pub fn wire_bits() -> iddcx::IDDCX_WIRE_BITS_PER_COMPONENT {
    let rgb = iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_8
        | iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_10;
    iddcx::IDDCX_WIRE_BITS_PER_COMPONENT {
        Rgb: rgb,
        YCbCr444: iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
        YCbCr422: iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
        YCbCr420: iddcx::IDDCX_BITS_PER_COMPONENT::IDDCX_BITS_PER_COMPONENT_NONE,
    }
}

/// `IDDCX_TARGET_MODE2` for a scan-out mode (HDR `*2` path): builds the v1 [`target_mode`] and copies its
/// `TargetVideoSignalInfo`, then stamps the `*2` Size + per-mode wire bit-depth ([`wire_bits`]). Rest
/// zeroed.
pub fn target_mode2(width: u32, height: u32, mhz: u32) -> iddcx::IDDCX_TARGET_MODE2 {
    let m1 = target_mode(width, height, mhz);
    iddcx::IDDCX_TARGET_MODE2 {
        Size: core::mem::size_of::<iddcx::IDDCX_TARGET_MODE2>() as u32,
        TargetVideoSignalInfo: m1.TargetVideoSignalInfo,
        BitsPerComponent: wire_bits(),
        ..Default::default()
    }
}
