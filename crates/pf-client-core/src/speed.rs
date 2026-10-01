//! Network speed test: one decode-less connect, then the core's measurement.
//!
//! Shared by every shell that offers "Test network speed…" — the Windows client's speed
//! page and its `--headless --speed-test`, and the console shell's host menu through the
//! session binary. The measurement runs over the REAL data plane, which is why it is a
//! connect and not a synthetic socket: a path that carries QUIC video is the path worth
//! measuring. What it measures is [`punktfunk_core::client::health::speed_test`]'s: the
//! ceiling the bring-up ramp proved, then one clean round under it.
//!
//! Where the answer goes is the caller's decision, not this module's — a measured bitrate
//! belongs in the layer the tested host resolves bitrate from
//! (`design/client-settings-profiles.md` §5.3).

use punktfunk_core::client::health::{self, SpeedError};
use punktfunk_core::client::{ConnectParams, NativeClient};
use punktfunk_core::config::Mode;
use std::time::Duration;

pub use punktfunk_core::client::health::{recommended_kbps, CleanRound, SpeedReport};

/// Connect to `addr`:`port`, measure, and return the report. Blocking — call it on a
/// worker thread.
///
/// The connect is deliberately minimal: 720p60, no launch, host-default bitrate. Nothing
/// here presents a frame, and asking a host to spin up a 4K encode for a two-second
/// measurement would be rude to it and slower for us.
pub fn run_speed_probe(
    addr: &str,
    port: u16,
    fp_hex: Option<&str>,
    identity: (String, String),
) -> Result<SpeedReport, String> {
    run_speed_probe_with(addr, port, fp_hex, identity, |_| {})
}

/// [`run_speed_probe`], reporting the round's live throughput (kbps) at every poll, for
/// a shell that draws the measurement as it happens.
pub fn run_speed_probe_with(
    addr: &str,
    port: u16,
    fp_hex: Option<&str>,
    identity: (String, String),
    progress: impl FnMut(u32),
) -> Result<SpeedReport, String> {
    // Pin the saved/advertised fingerprint when we have one; a manual host measures over TOFU.
    let pin = fp_hex.and_then(crate::trust::parse_hex32);
    let mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    // The host's default rate: Automatic, which is what arms the bring-up ramp. Nothing
    // presents, so every other Hello field stays default too.
    let c = NativeClient::connect(ConnectParams {
        // The DEVICE-FREE answer, not `decodable_codecs_for`: this connect creates no
        // presenter and has no `VulkanDecodeDevice` to gate AV1 on, and it decodes nothing.
        video_codecs: crate::video::decodable_codecs(),
        // Same label a real session sends — a speed test against a host that doesn't know us
        // yet should knock under this device's name, not a fingerprint placeholder.
        name: Some(punktfunk_core::client::device_name()),
        pin,
        identity: Some(identity),
        ..ConnectParams::new(addr, port, mode, Duration::from_secs(15))
    })
    .map_err(|e| {
        tracing::warn!(error = ?e, "speed test connect");
        "Couldn't start the speed test".to_string()
    })?;
    health::speed_test(&c, progress).map_err(|e| {
        tracing::warn!(error = ?e, "speed test");
        match e {
            SpeedError::Request(_) => "The host didn't start the speed test".to_string(),
            SpeedError::Declined => "The host declined the speed test".to_string(),
            SpeedError::Timeout => "The speed test didn't finish in time".to_string(),
        }
    })
}
