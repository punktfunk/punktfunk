//! qWAVE (`qos2.h`) DSCP marking — Windows path of [`super::qos::set_media_qos`].
//!
//! A plain `IP_TOS` setsockopt succeeds, then the stack strips the mark. Marking
//! needs qWAVE flow membership (`qwave.dll`). [`QOSAddSocketToFlow`] with a
//! traffic type yields the OS default (AudioVideo → DSCP 40, Voice → 56);
//! `QOSSetFlow(QOSSetOutgoingDSCPValue)` then pins CS5/CS6 to match the other
//! platforms. The pin needs elevation or the "allow non-admin DSCP" policy;
//! otherwise the traffic-type default stands (still WMM-mapped).
//!
//! Same contract as [`super::qos`]: opt-in (`dscp_enabled`), every step
//! debug-logs and continues.

// Crate-wide deny(unsafe_code) carve-out (lib.rs): platform syscall glue —
// qWAVE FFI moves flow handles, never network bytes. Proofs at each site.
#![allow(unsafe_code)]

use super::qos::MediaClass;
use std::net::UdpSocket;
use std::os::windows::io::AsRawSocket;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::QoS::{
    QOSAddSocketToFlow, QOSCreateHandle, QOSRemoveSocketFromFlow, QOSSetFlow,
    QOSSetOutgoingDSCPValue, QOSTrafficTypeAudioVideo, QOSTrafficTypeVoice, QOS_NON_ADAPTIVE_FLOW,
    QOS_VERSION,
};

/// Process-wide qWAVE handle (`QOSCreateHandle` once). `None` if qWAVE is
/// unavailable. Never closed — lives as long as the process, like the media
/// sockets whose flows it carries.
fn qos_handle() -> Option<HANDLE> {
    static SLOT: OnceLock<Option<usize>> = OnceLock::new();
    SLOT.get_or_init(|| {
        let version = QOS_VERSION {
            MajorVersion: 1,
            MinorVersion: 0,
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        // SAFETY: both pointers are valid for the duration of the synchronous call.
        if unsafe { QOSCreateHandle(&version, &mut handle) } == 0 {
            let err = std::io::Error::last_os_error();
            tracing::debug!(%err, "QOSCreateHandle failed — qWAVE DSCP marking unavailable");
            None
        } else {
            Some(handle as usize)
        }
    })
    .map(|h| h as HANDLE)
}

/// RAII qWAVE flow membership: while held, egress carries the mark; drop
/// removes the socket from the flow. Closing the socket also removes it;
/// the guard makes teardown explicit and must outlive the socket's traffic.
pub struct QosFlow {
    /// Raw `SOCKET` only — never dereferenced. qWAVE tolerates an already-closed
    /// socket on remove (the error is ignored).
    socket: u64,
    flow_id: u32,
}

impl Drop for QosFlow {
    fn drop(&mut self) {
        if let Some(handle) = qos_handle() {
            // SAFETY: handle/flow_id came from the successful add; a stale socket just errors.
            unsafe { QOSRemoveSocketFromFlow(handle, self.socket as _, self.flow_id, 0) };
        }
    }
}

/// Put a **connected** media socket on a qWAVE flow (video → AudioVideo,
/// audio → Voice), then best-effort pin CS5/CS6. `None` if a required step
/// refused (logged at debug).
pub(super) fn add_media_flow(socket: &UdpSocket, class: MediaClass) -> Option<QosFlow> {
    let handle = qos_handle()?;
    let traffic_type = match class {
        MediaClass::Video => QOSTrafficTypeAudioVideo,
        MediaClass::Audio => QOSTrafficTypeVoice,
    };
    let raw = socket.as_raw_socket();
    let mut flow_id = 0u32;
    // NULL dest: derive the 5-tuple from the connected socket (already `connect`ed).
    // SAFETY: the socket is live for the call; `flow_id` is a valid out-pointer.
    let ok = unsafe {
        QOSAddSocketToFlow(
            handle,
            raw as _,
            std::ptr::null(),
            traffic_type,
            QOS_NON_ADAPTIVE_FLOW,
            &mut flow_id,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        tracing::debug!(%err, ?class, "QOSAddSocketToFlow failed — DSCP marking skipped");
        return None;
    }
    // Guard first so an early return still removes flow membership.
    // `raw` is already `u64` (`RawSocket`); a same-type cast fails win64 clippy.
    let flow = QosFlow {
        socket: raw,
        flow_id,
    };
    // Pin the exact code point. Succeeds elevated or under "allow non-admin DSCP";
    // otherwise the traffic-type default stands (40 / 56 — WMM-useful).
    let dscp: u32 = class.dscp();
    // SAFETY: `buffer` points at 4 valid bytes for the synchronous (no OVERLAPPED) call.
    let ok = unsafe {
        QOSSetFlow(
            handle,
            flow_id,
            QOSSetOutgoingDSCPValue,
            4,
            &dscp as *const u32 as *const _,
            0,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        tracing::debug!(
            %err,
            ?class,
            "QOSSetFlow(OutgoingDSCPValue) refused — traffic-type default marking stands"
        );
    } else {
        tracing::debug!(?class, dscp, flow_id, "qWAVE flow pinned to exact DSCP");
    }
    Some(flow)
}
