//! Typed `DisplayConfigGetDeviceInfo` / `DisplayConfigSetDeviceInfo` packets: one builder and
//! one SAFETY proof behind every per-target read and the advanced-colour write.

use std::mem::size_of;

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo,
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_DEVICE_INFO_TYPE,
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
};
use windows::Win32::Foundation::LUID;

/// A `DISPLAYCONFIG_*` packet: `#[repr(C)]` plain data whose first field is its
/// `DISPLAYCONFIG_DEVICE_INFO_HEADER`.
///
/// # Safety
/// `header` must return that first field, so a pointer to the packet is a pointer to the
/// header the OS reads `size_of::<Self>()` bytes behind. Every field is integer data, so any
/// bytes the OS leaves are a valid `Self`.
unsafe trait Packet: Default {
    const TYPE: DISPLAYCONFIG_DEVICE_INFO_TYPE;
    fn header(&mut self) -> &mut DISPLAYCONFIG_DEVICE_INFO_HEADER;
}

macro_rules! packet {
    ($t:ty, $ty:expr) => {
        const _: () = assert!(std::mem::offset_of!($t, header) == 0);
        // SAFETY: every `DISPLAYCONFIG_*` packet here, `windows`' or our own, is `#[repr(C)]`
        // integer data that starts with `header: DISPLAYCONFIG_DEVICE_INFO_HEADER` (asserted above).
        unsafe impl Packet for $t {
            const TYPE: DISPLAYCONFIG_DEVICE_INFO_TYPE = $ty;
            fn header(&mut self) -> &mut DISPLAYCONFIG_DEVICE_INFO_HEADER {
                &mut self.header
            }
        }
    };
}

packet!(
    DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME
);
packet!(
    DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME
);
packet!(
    DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO
);
packet!(
    DISPLAYCONFIG_SDR_WHITE_LEVEL,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL
);
packet!(
    DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE
);

/// `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_2`, Windows 11 24H2 (info type 15). `windows` 0.62 has
/// no binding; the layout is wingdi.h's.
#[repr(C)]
#[derive(Default)]
pub(crate) struct AdvancedColorInfo2 {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    /// Bit 6 `wideColorSupported`, bit 7 `wideColorUserEnabled`.
    pub value: u32,
    color_encoding: i32,
    pub bits_per_color_channel: u32,
    /// `DISPLAYCONFIG_ADVANCED_COLOR_MODE`: 0 SDR, 1 WCG, 2 HDR.
    pub active_color_mode: i32,
}

/// `DISPLAYCONFIG_SET_WCG_STATE`, Windows 11 24H2 (info type 17). Bit 0 is `enableWcg`.
#[repr(C)]
#[derive(Default)]
struct SetWcgState {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    value: u32,
}

packet!(AdvancedColorInfo2, DISPLAYCONFIG_DEVICE_INFO_TYPE(15));
packet!(SetWcgState, DISPLAYCONFIG_DEVICE_INFO_TYPE(17));

/// A packet of type `T` addressed to `(adapter, id)`.
fn packet<T: Packet>(adapter: LUID, id: u32) -> T {
    let mut p = T::default();
    let h = p.header();
    h.r#type = T::TYPE;
    h.size = size_of::<T>() as u32;
    h.adapterId = adapter;
    h.id = id;
    p
}

/// Read `T` for `(adapter, id)`; `None` when the OS refuses (no such target, or no monitor).
fn get<T: Packet>(adapter: LUID, id: u32) -> Option<T> {
    let mut p = packet::<T>(adapter, id);
    // SAFETY: `header.size` is `size_of::<T>()` and the pointer covers the whole local packet
    // (`Packet`: the header is its first field), so the OS writes only inside it. The local
    // outlives this synchronous call, which retains nothing.
    let rc = unsafe { DisplayConfigGetDeviceInfo((&mut p as *mut T).cast()) };
    (rc == 0).then_some(p)
}

/// A NUL-terminated UTF-16 field as a `String`, up to the first NUL.
pub(crate) fn utf16z(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// A target's monitor: its PnP interface path, friendly name and connector class.
pub(crate) struct TargetName {
    pub device_path: String,
    pub friendly: String,
    pub tech: DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY,
}

/// `None` when no queryable monitor sits on the target.
pub(crate) fn target_name(adapter: LUID, target_id: u32) -> Option<TargetName> {
    let n = get::<DISPLAYCONFIG_TARGET_DEVICE_NAME>(adapter, target_id)?;
    Some(TargetName {
        device_path: utf16z(&n.monitorDevicePath),
        friendly: utf16z(&n.monitorFriendlyDeviceName),
        tech: n.outputTechnology,
    })
}

/// The `\\.\DisplayN` GDI name of a source.
pub(crate) fn source_gdi_name(adapter: LUID, source_id: u32) -> Option<String> {
    get::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>(adapter, source_id)
        .map(|n| utf16z(&n.viewGdiDeviceName))
}

/// The `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` bitfield of a target.
pub(crate) fn advanced_color_bits(adapter: LUID, target_id: u32) -> Option<u32> {
    let info = get::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>(adapter, target_id)?;
    // SAFETY: POD union — `value` overlays a same-sized bitfield.
    Some(unsafe { info.Anonymous.value })
}

/// The 24H2 colour report of a target; `None` before 24H2 or with no monitor.
pub(crate) fn advanced_color_info2(adapter: LUID, target_id: u32) -> Option<AdvancedColorInfo2> {
    get::<AdvancedColorInfo2>(adapter, target_id)
}

/// Turn SDR wide colour (auto colour management) on or off. The raw
/// `DisplayConfigSetDeviceInfo` result: 0 is success.
pub(crate) fn set_wcg_state(adapter: LUID, target_id: u32, enable: bool) -> i32 {
    let mut s = packet::<SetWcgState>(adapter, target_id);
    s.value = enable as u32;
    // SAFETY: as `set_advanced_color_state` — the OS reads this packet's own size behind its
    // header and retains nothing.
    unsafe { DisplayConfigSetDeviceInfo((&s as *const SetWcgState).cast()) }
}

/// The target's SDR white level (1000 = 80 nits); `None` when unreported.
pub(crate) fn sdr_white_level(adapter: LUID, target_id: u32) -> Option<u32> {
    get::<DISPLAYCONFIG_SDR_WHITE_LEVEL>(adapter, target_id)
        .map(|w| w.SDRWhiteLevel)
        .filter(|&level| level > 0)
}

/// Turn a target's advanced colour (HDR) on or off. The raw `DisplayConfigSetDeviceInfo`
/// result: 0 is success.
pub(crate) fn set_advanced_color_state(adapter: LUID, target_id: u32, enable: bool) -> i32 {
    let mut s = packet::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>(adapter, target_id);
    // Bit 0 is `enableAdvancedColor`.
    s.Anonymous.value = enable as u32;
    // SAFETY: `header.size` is this packet's size and the pointer covers the whole local
    // (`Packet`: the header is its first field); the OS reads that many bytes and retains nothing.
    unsafe {
        DisplayConfigSetDeviceInfo((&s as *const DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE).cast())
    }
}
