//! In-stream quick-action ring: input the presenter feeds, commands it asks
//! back, facts the slots draw from, and the 100 % geometry every editor uses.
//! Pin: `design/touch-client-overlay.md`.

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RingInput {
    /// `progress` 0…1; `x`/`y` are window pixels of the twist centroid.
    Turn {
        progress: f32,
        clockwise: bool,
        x: f32,
        y: f32,
    },
    /// The twist reached the commit angle: the ring stays open after the lift.
    Commit,
    /// Lifted short of commit, or wound back after one: the ring winds back in.
    Cancel,
    /// A key or chord: open at `x`/`y`, or close if open.
    Toggle { x: f32, y: f32 },
}

/// Drained by the presenter once per iteration. Host actions ride the console bus, not this enum.
#[derive(Clone, Debug, PartialEq)]
pub enum RingCommand {
    EndStream,
    /// End [`RingFacts::streamed_game`] on the host, then the stream.
    EndGame {
        app_id: String,
        title: String,
    },
    DisconnectLinger,
    CycleStats,
    ToggleMic,
    CycleTouchMode,
    ToggleScrollInvert,
    /// Platform text input, not a Keyboard key.
    Keyboard,
    RequestMode {
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    /// A custom chord as key NAMES (`ctrl`, `shift`, `escape`); see `overlay_actions::key_vk`.
    Shortcut(Vec<String>),
    /// A synthetic tap of one system button on the host's pad — a `gamepad::wire::BTN_*` bit
    /// (guide or `MISC1`), the same verb the session control socket exposes.
    TapButton(u32),
    /// Step [`RingFacts::pad_mouse_target`] to the next controller-mouse mode: off, touchpad,
    /// full.
    CyclePadMouse,
    /// Emulate the next controller type for the rest of this stream
    /// ([`crate::overlay_actions::next_pad_type`]); each pad re-plugs as it.
    CyclePadType,
    /// Flip this client's own speakers ([`punktfunk_core::client::AUDIO_MUTE_LOCAL`]). Never
    /// reaches the host, so a session joined to the same display keeps hearing the game.
    ToggleStreamMute,
}

/// 100 % scale, client design units (px on Skia, dp/pt on phones). Shared so editors cannot drift.
pub const RING_RADIUS: f32 = 120.0;
pub const SLOT_DIAMETER: f32 = 56.0;
pub const CENTRE_DIAMETER: f32 = 64.0;

/// Degrees; 0 at 3 o'clock, clockwise. Slot 0 is 12 o'clock.
pub fn slot_angle_deg(k: usize) -> f32 {
    -90.0 + 60.0 * k as f32
}

/// Offset from ring centre; y down.
pub fn slot_offset(k: usize, radius: f32) -> (f32, f32) {
    let (s, c) = slot_angle_deg(k).to_radians().sin_cos();
    (radius * c, radius * s)
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn slots_sit_at_twelve_two_four_six_eight_and_ten() {
        let (x0, y0) = slot_offset(0, 100.0);
        assert!(
            x0.abs() < 1e-3 && (y0 + 100.0).abs() < 1e-3,
            "12 o'clock is up: {x0} {y0}"
        );
        let (x3, y3) = slot_offset(3, 100.0);
        assert!(
            x3.abs() < 1e-3 && (y3 - 100.0).abs() < 1e-3,
            "6 o'clock is down"
        );
        let (x1, y1) = slot_offset(1, 100.0);
        assert!(x1 > 0.0 && y1 < 0.0, "2 o'clock is up and right");
        for k in 0..6 {
            assert!((slot_angle_deg(k) - (-90.0 + 60.0 * k as f32)).abs() < 1e-6);
        }
    }
}

/// Per-frame session facts the slots show and gate on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RingFacts {
    /// Empty means the platform default ring.
    pub overlay_actions: String,
    /// Touch-model name: `trackpad` / `pointer` / `touch` / `off`.
    pub touch_mode: String,
    pub invert_scroll: bool,
    /// Without this the `touch` model is skipped.
    pub host_accepts_touch: bool,
    pub stats_tier: String,
    pub mic_available: bool,
    pub mic_muted: bool,
    /// Why the speakers are silent: `punktfunk_core::client::AUDIO_MUTE_LOCAL` |
    /// `AUDIO_MUTE_HOST`. The slot toggles the local bit; the overlay names both.
    pub audio_mute: u8,
    /// Wire pads the controller-mouse toggle acts on: the pad that opened the ring, else every
    /// live pad. `0` = no controller.
    pub pad_mouse_target: u16,
    /// The controller-mouse mode every target pad shares; a mixed set reads as off.
    pub pad_mouse: punktfunk_core::input::PadMouseMode,
    /// The host grants pointer input; controller mouse needs it.
    pub pointer_granted: bool,
    /// The controller type the host emulates; `Auto` = each pad as itself.
    pub pad_type: punktfunk_core::config::GamepadPref,
    /// Live `(w, h, hz)`. `native_mode` is the Welcome native.
    pub mode: (u32, u32, u32),
    pub native_mode: (u32, u32, u32),
    /// Host identity for pre-fetched actions; `fp_hex` is the pinned fingerprint.
    pub addr: String,
    pub mgmt_port: u16,
    pub fp_hex: String,
    pub host_name: String,
    /// `(app_id, title)` of the game this device launched that this stream plays
    /// ([`crate::library::RunningGame::streamed_here`]). `None` offers no End game.
    pub streamed_game: Option<(String, String)>,
}
