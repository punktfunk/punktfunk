//! Windows audio: WASAPI loopback of the wiring plan's sink for capture, a minted virtual
//! device for the mic, and the devnode plumbing every pad/minted endpoint shares. The seven
//! `pub(super)` functions are the platform seam `lib.rs` calls through.

use super::*;
use anyhow::Result;

pub mod audio_control;
// `audio-probe` devtest: mint Steam-driver instances and measure render→capture /
// loopback paths for the Windows audio-substrate design.
pub mod audio_probe;
// SetupAPI + PROPVARIANT plumbing under every audio devnode we mint. Shared by pad_endpoint,
// minted, audio_probe and devnode_cleanup — only one of which provisions pads.
pub mod devnode_api;
// Uninstall sweep of every audio devnode the providers (and the probe) mint.
// `driver uninstall --audio` / installer [UninstallRun].
pub mod devnode_cleanup;
// Minted "Punktfunk Speakers/Microphone": our instances of Valve's streaming-audio
// drivers. Wiring-plan tier-0.
pub mod minted;
// WASAPI loopback of a minted pad endpoint, plus the tone/probe devtests. Capturing is a
// different job from provisioning, and the same one `wasapi_cap` does for the desktop.
pub mod pad_capture;
// DualSense pad-audio endpoint + loopback (design: pad haptics/audio). Session
// queries by pad index; CLI `pad-endpoint`.
pub mod pad_endpoint;
// Voice-chat apps pinned to the operator's output while the stream captures the silent
// sink; also the `voice-route` subcommand that writes the pins as the console user.
pub mod voice_route;
mod wasapi_cap;
mod wasapi_mic;

pub(super) fn probe_capture_rate() -> CaptureRate {
    audio_control::probe_capture_rate()
}

/// Capture thread runs `audio_control::wire_now` before resolving the endpoint — a
/// fresh plan per open, Windows endpoints churn — and parks default playback on the
/// plan's loopback sink (silent on the host) until this capturer is dropped.
pub(super) fn open_audio_capture(channels: u32, rate_hz: u32) -> Result<Box<dyn AudioCapturer>> {
    wasapi_cap::WasapiLoopbackCapturer::open(channels, rate_hz)
        .map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

/// No sink names on Windows: the wiring plan picks the endpoint.
pub(super) fn open_audio_capture_named(
    channels: u32,
    rate_hz: u32,
    _sink: Option<&str>,
    _tap: bool,
) -> Result<Box<dyn AudioCapturer>> {
    open_audio_capture(channels, rate_hz)
}

/// Every session shares the default output.
pub(super) fn per_session_sink_possible() -> bool {
    false
}

/// Render thread runs `audio_control::wire_now` so the plan both resolves the
/// endpoint and, via default-device changes, reserves it.
pub(super) fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    wasapi_mic::WasapiVirtualMic::open(channels).map(|m| Box::new(m) as Box<dyn VirtualMic>)
}

pub(super) fn open_virtual_mic_named(
    channels: u32,
    _source: Option<&str>,
) -> Result<Box<dyn VirtualMic>> {
    open_virtual_mic(channels)
}

/// Last wiring-pass assignment; `None` before the first pass.
pub(super) fn wiring_snapshot() -> Option<super::wiring_plan::Wiring> {
    audio_control::last_wiring()
}
