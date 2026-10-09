//! Operator-actionable causes for `NVENCSTATUS` failures, shared by the
//! Windows (`encode/windows/nvenc.rs`) and Linux (`encode/linux/nvenc_cuda.rs`)
//! direct-SDK backends. [`call_err`] folds the cause into the `anyhow::Error`
//! so `{e:#}` logs already name the real failure.
//!
//! `NV_ENC_ERR_INVALID_VERSION` is two failures: header/kernel skew (no session
//! has opened) and exhausted per-process driver state (a session already did).
//! [`note_session_opened`] latches the handshake; [`explain`] splits on it.
//! Pin the split with the tests below.

use std::sync::atomic::{AtomicBool, Ordering};

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// Set after any successful `nvEncOpenEncodeSessionEx` in this process.
/// `NvEncodeAPIGetMaxSupportedVersion` is userspace-only and cannot stand in:
/// a userspace/kernel skew still fails at open.
static SESSION_OPENED: AtomicBool = AtomicBool::new(false);

/// Latch after every successful `open_encode_session_ex`. [`explain`] uses it
/// to rule out a version skew for the rest of the process.
pub fn note_session_opened() {
    SESSION_OPENED.store(true, Ordering::Relaxed);
}

/// Split `NV_ENC_ERR_INVALID_VERSION` on whether a session already opened.
/// Pure so both halves are testable without the process-wide latch.
fn invalid_version(session_opened: bool) -> String {
    if session_opened {
        // Skew cannot recur after a successful open; this is per-process
        // driver state (unreturned resource or a lost device).
        return "this process already opened an NVENC session successfully, so this is NOT a driver \
                version mismatch — that cannot come and go within a process, and a reboot is not \
                the fix. The NVIDIA driver state in THIS process is exhausted or wedged: restart \
                the Punktfunk host service to clear it, and please report this with the host log \
                so it can be fixed properly"
            .to_string();
    }
    format!(
        "the NVIDIA driver is older than this build's NVENC headers (needs NVENC API {}.{} or \
         newer), or the userspace and kernel-module driver versions are mismatched — common right \
         after a driver update without a reboot. Update the NVIDIA driver, or reboot if you just \
         updated it (a host restart is the usual fix).",
        nv::NVENCAPI_MAJOR_VERSION,
        nv::NVENCAPI_MINOR_VERSION,
    )
}

/// Operator-actionable cause for an NVENC status. Does not repeat the raw
/// code — callers print that alongside (see [`call_err`]).
pub fn explain(status: nv::NVENCSTATUS) -> String {
    match status {
        nv::NVENCSTATUS::NV_ENC_ERR_INVALID_VERSION => {
            invalid_version(SESSION_OPENED.load(Ordering::Relaxed))
        }
        nv::NVENCSTATUS::NV_ENC_ERR_NO_ENCODE_DEVICE => {
            "this GPU exposes no usable NVENC engine — it has no hardware video encoder, or NVENC is \
             disabled on this card"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_UNSUPPORTED_DEVICE => {
            "this GPU model is not supported by NVENC".to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_INVALID_ENCODERDEVICE
        | nv::NVENCSTATUS::NV_ENC_ERR_INVALID_DEVICE => {
            "the device/context handed to NVENC is invalid — a GPU reset or driver reload can cause \
             this"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_DEVICE_NOT_EXIST => {
            "the NVENC device no longer exists — the driver reset, or the GPU fell off the bus"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY => "the GPU is out of memory".to_string(),
        nv::NVENCSTATUS::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => {
            "NVENC rejected the client key — the GeForce concurrent-NVENC-session limit was reached, \
             or the driver is unlicensed for this many encoders"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_UNIMPLEMENTED
        | nv::NVENCSTATUS::NV_ENC_ERR_UNSUPPORTED_PARAM => {
            "this driver/GPU does not implement the requested NVENC encode mode".to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_INVALID_PARAM => {
            "NVENC rejected a parameter — an encode mode this GPU does not support".to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY => {
            "the NVENC engine is busy — retry, or reduce the number of concurrent encode sessions"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_GENERIC => {
            "the NVIDIA driver returned a generic NVENC failure — check dmesg and the driver install"
                .to_string()
        }
        other => format!("unexpected NVENC status ({other:?})"),
    }
}

/// Typed root of a failed NVENC call so callers can classify, not just print.
/// The bitrate-clamp search must treat only a parameter/caps rejection as
/// "above the ceiling"; a transient failure that shrinks the search would
/// cache a bogus one. Downcast via [`is_param_rejection`].
#[derive(Debug)]
pub struct NvCallError(pub nv::NVENCSTATUS);

impl std::fmt::Display for NvCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} — {}", self.0, explain(self.0))
    }
}

impl std::error::Error for NvCallError {}

/// Parameter/capability rejection: this config is not encodable, so the
/// clamp search may treat it as "above the ceiling". Busy, session limit,
/// OOM, device loss, and version skew must propagate instead.
pub fn is_param_rejection(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<NvCallError>(),
        Some(NvCallError(
            nv::NVENCSTATUS::NV_ENC_ERR_INVALID_PARAM
                | nv::NVENCSTATUS::NV_ENC_ERR_UNSUPPORTED_PARAM
                | nv::NVENCSTATUS::NV_ENC_ERR_UNIMPLEMENTED,
        ))
    )
}

/// `call` names the NVENC entry point. The chain carries the raw status and
/// its cause; [`NvCallError`] stays downcastable for failure-class checks.
pub fn call_err(call: &str, status: nv::NVENCSTATUS) -> anyhow::Error {
    anyhow::Error::new(NvCallError(status)).context(format!("NVENC {call} failed"))
}

/// Failed `nvEncDestroyEncoder` statuses that prove the driver holds no
/// session for the handle — refund the concurrent-session slot now. These
/// mean the session or its device is gone (TDR reclaims with the context).
/// Anything else is ambiguous: the slot may still be held. Park fail-closed
/// and retry destroy. A wrong `true` over-admits; a wrong `false` defers the
/// refund. Windows D3D11 teardown; Linux has no session budget.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn destroy_proves_no_session(status: nv::NVENCSTATUS) -> bool {
    matches!(
        status,
        nv::NVENCSTATUS::NV_ENC_ERR_DEVICE_NOT_EXIST
            | nv::NVENCSTATUS::NV_ENC_ERR_INVALID_ENCODERDEVICE
            | nv::NVENCSTATUS::NV_ENC_ERR_INVALID_PTR
            | nv::NVENCSTATUS::NV_ENC_ERR_ENCODER_NOT_INITIALIZED
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_version_before_any_session_blames_the_driver_version() {
        let msg = invalid_version(false);
        assert!(
            msg.contains("older than this build's NVENC headers"),
            "{msg}"
        );
        assert!(msg.contains("reboot if you just updated it"), "{msg}");
    }

    #[test]
    fn invalid_version_after_a_session_blames_process_state_not_the_driver() {
        let msg = invalid_version(true);
        assert!(msg.contains("NOT a driver version mismatch"), "{msg}");
        assert!(msg.contains("restart the Punktfunk host service"), "{msg}");
        assert!(
            !msg.contains("older than this build's NVENC headers"),
            "must not repeat the version-skew advice: {msg}"
        );
        assert!(
            !msg.contains("Update the NVIDIA driver"),
            "must not tell the operator to update a driver that just worked: {msg}"
        );
    }

    /// One-way latch; other statuses must keep their own text.
    #[test]
    fn note_session_opened_latches() {
        note_session_opened();
        assert!(SESSION_OPENED.load(Ordering::Relaxed));
        note_session_opened();
        assert!(SESSION_OPENED.load(Ordering::Relaxed));
        assert_eq!(
            explain(nv::NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY),
            "the GPU is out of memory"
        );
    }

    #[test]
    fn destroy_classification_refunds_only_on_proof() {
        for gone in [
            nv::NVENCSTATUS::NV_ENC_ERR_DEVICE_NOT_EXIST,
            nv::NVENCSTATUS::NV_ENC_ERR_INVALID_ENCODERDEVICE,
            nv::NVENCSTATUS::NV_ENC_ERR_INVALID_PTR,
            nv::NVENCSTATUS::NV_ENC_ERR_ENCODER_NOT_INITIALIZED,
        ] {
            assert!(
                destroy_proves_no_session(gone),
                "{gone:?} proves no session"
            );
        }
        for ambiguous in [
            nv::NVENCSTATUS::NV_ENC_ERR_GENERIC,
            nv::NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY,
            nv::NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY,
            nv::NVENCSTATUS::NV_ENC_ERR_INVALID_PARAM,
            // INVALID_DEVICE sounds gone but a confused driver also returns it.
            // Fail-closed: park and retry, do not refund.
            nv::NVENCSTATUS::NV_ENC_ERR_INVALID_DEVICE,
        ] {
            assert!(
                !destroy_proves_no_session(ambiguous),
                "{ambiguous:?} must park fail-closed"
            );
        }
    }
}
