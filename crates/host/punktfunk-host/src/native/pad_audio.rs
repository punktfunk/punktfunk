//! Per-pad DualSense audio (wire `0xD1`).
//!
//! Capture of the pad's own device — WASAPI loopback ([`crate::audio::pad_endpoint`]) on
//! Windows, the per-pad PipeWire sink (`crate::audio::pad_sink`) on Linux — is de-interleaved
//! into speaker (front) and voice-coil (back) pairs, silence-gated, Opus-encoded at 48 kHz
//! CBR, and sent as [`PAD_AUDIO_MAGIC`](punktfunk_core::quic::PAD_AUDIO_MAGIC) datagrams.
//!
//! One thread per pad, spawned and reaped by [`super::input`]. Capture death reopens with
//! backoff; seq stays monotonic across reopens so the client sees a gap, not a restart.

use super::*;

/// The shared pipeline; a host with no pad source has no use for it.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "tests only on macOS"))]
mod engine;
#[cfg(target_os = "windows")]
#[path = "pad_audio/windows.rs"]
mod plat;
#[cfg(target_os = "linux")]
#[path = "pad_audio/linux.rs"]
mod plat;
/// Other hosts have no virtual DualSense audio source; [`host_cap`] never advertises the cap.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod plat {
    use super::*;
    pub(super) fn host_cap(_asked: bool) -> bool {
        false
    }
    pub(in crate::native) fn spawn(
        _conn: super::super::link::SessionLink,
        _pad: u8,
        _slot: u8,
        _kinds: u8,
        _edge: bool,
        _stop: Arc<AtomicBool>,
    ) -> Option<PadAudioHandle> {
        None
    }
}
pub(super) use plat::spawn;

/// Newest streamer per OS slot, keyed by the endpoint it shows. A quiet capturer wakes up to
/// 5 s after its stop, by which time the next session may be capturing the same endpoint; a
/// hide then would disable it under that capture. So a streamer hides only while it is still
/// the newest, and show/hide run under the slot's lock so they cannot interleave.
#[cfg(any(target_os = "windows", test))]
pub(super) struct ShowGen([std::sync::Mutex<u32>; punktfunk_core::input::MAX_PADS]);

#[cfg(any(target_os = "windows", test))]
impl ShowGen {
    pub(super) const fn new() -> Self {
        Self([const { std::sync::Mutex::new(0) }; punktfunk_core::input::MAX_PADS])
    }

    /// Run `show` as the slot's newest streamer; the returned generation is its ticket.
    pub(super) fn show(&self, slot: u8, show: impl FnOnce()) -> u32 {
        let mut newest = self.0[slot as usize]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *newest += 1;
        show();
        *newest
    }

    /// Run `hide` only if `ticket` is still the slot's newest streamer.
    pub(super) fn hide_if_newest(&self, slot: u8, ticket: u32, hide: impl FnOnce()) -> bool {
        let newest = self.0[slot as usize]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *newest != ticket {
            return false;
        }
        hide();
        true
    }
}

#[cfg(target_os = "windows")]
pub(super) static SHOWN: ShowGen = ShowGen::new();

/// [`stop`](PadAudioHandle::stop) flags and joins; [`signal`](PadAudioHandle::signal) only flags
/// so the input thread can overlap joins instead of serializing the ~5 s quiet-endpoint timeout.
pub(super) struct PadAudioHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PadAudioHandle {
    pub(super) fn signal(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Bounded by the capturer's ~5 s quiet-endpoint recv. Mid-session reaps go through a detached
    /// reaper (`input.rs::PadAudioSlots::stop`); session teardown joins inline (10 s grace).
    pub(super) fn stop(mut self) {
        self.reap();
    }

    fn reap(&mut self) {
        self.signal();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Fallback if `stop()` never ran (reaper-spawn failure).
impl Drop for PadAudioHandle {
    fn drop(&mut self) {
        self.reap();
    }
}

/// Advertise [`HOST_CAP_PAD_AUDIO`](punktfunk_core::quic::HOST_CAP_PAD_AUDIO) when the client asked
/// ([`CLIENT_CAP_PAD_AUDIO`](punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO)), `PUNKTFUNK_PAD_AUDIO` ≠ "0",
/// and a source exists: Windows has a provisioned endpoint; Linux has a reachable PipeWire daemon
/// (sinks mint lazily at spawn).
pub(super) fn host_cap(client_caps: u8) -> bool {
    plat::host_cap(client_caps & punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_cap_requires_the_client_bit() {
        // Without CLIENT_CAP_PAD_AUDIO the answer is no on every platform (Windows env +
        // provisioning legs are environment-dependent — not unit-tested here).
        assert!(!host_cap(0));
        assert!(!host_cap(punktfunk_core::quic::CLIENT_CAP_CURSOR));
    }
}

#[cfg(test)]
mod show_gen_tests {
    use super::ShowGen;
    use std::cell::Cell;

    /// The old streamer wakes after the new one showed the endpoint: its hide must not run.
    #[test]
    fn a_superseded_streamer_leaves_the_endpoint_shown() {
        let lease = ShowGen::new();
        let hidden = Cell::new(0);
        let old = lease.show(0, || {});
        let new = lease.show(0, || {});
        assert!(!lease.hide_if_newest(0, old, || hidden.set(hidden.get() + 1)));
        assert_eq!(hidden.get(), 0, "a stale ticket ran the hide");
        assert!(lease.hide_if_newest(0, new, || hidden.set(hidden.get() + 1)));
        assert_eq!(hidden.get(), 1);
        // Slots are independent, and the plain order still hides.
        let other = lease.show(1, || {});
        assert!(lease.hide_if_newest(1, other, || {}));
    }
}
