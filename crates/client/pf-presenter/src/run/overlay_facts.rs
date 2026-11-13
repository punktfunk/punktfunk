//! Everything the console overlay reads in a pass, gathered once: the stream's chrome and
//! notices, the pads, the ring's facts, the window's size and scale.
//!
//! `overlay_tick` gathers while a console exists and hands [`OverlayFacts::ctx`] to
//! `Overlay::frame` once. The gather keeps the mute notices' clocks. The stream's chrome,
//! notices and ring show once it is connected.

use super::shell::{capture_hint, overlay_scale};
use super::stream::ring_facts;
use super::*;
use pf_client_core::gamepad::PadInfo;
use punktfunk_core::hud::HudCorner;

/// What `Overlay::frame` reads this pass, owned or borrowed from the stream.
pub(super) struct OverlayFacts<'s> {
    width: u32,
    height: u32,
    ten_bit: bool,
    scale: f32,
    stats: Option<&'s [HudLine]>,
    stats_corner: HudCorner,
    stats_scale: f32,
    exit_hint: bool,
    hint: Option<String>,
    access: Option<String>,
    notice: Option<&'s str>,
    mic_notice: Option<bool>,
    audio_mute: Option<&'static str>,
    resizing: bool,
    pad: Option<PadInfo>,
    pads: Vec<PadInfo>,
    ring: Option<RingFacts>,
    native: Mode,
    shortcuts: std::rc::Rc<pf_client_core::shortcuts::Shortcuts>,
}

impl OverlayFacts<'_> {
    /// The overlay's frame context over these facts.
    pub(super) fn ctx(&self) -> FrameCtx<'_> {
        FrameCtx {
            width: self.width,
            height: self.height,
            ten_bit: self.ten_bit,
            scale: self.scale,
            stats: self.stats,
            stats_corner: self.stats_corner,
            stats_scale: self.stats_scale,
            exit_hint: self.exit_hint,
            hint: self.hint.as_deref(),
            access: self.access.as_deref(),
            notice: self.notice,
            mic_notice: self.mic_notice,
            audio_mute: self.audio_mute,
            resizing: self.resizing,
            pad: self.pad.as_ref().map(|p| p.name.as_str()),
            pad_pref: self.pad.as_ref().map(|p| p.pref),
            pads: &self.pads,
            ring: self.ring.as_ref(),
            native: self.native,
            shortcuts: &self.shortcuts,
        }
    }
}

impl Shell {
    /// The console's facts for this pass, read once; the mute notices' clocks move here.
    pub(super) fn overlay_facts<'s>(
        &mut self,
        stream: Option<&'s StreamState>,
    ) -> OverlayFacts<'s> {
        let (width, height) = self.window.size_in_pixels();
        let live = stream.filter(|st| st.connector.is_some());
        let chrome = self.stats_verbosity != StatsVerbosity::Off;
        let pad = self.gamepad.active();
        // Read live from the session's control rather than mirrored into StreamState: the
        // pump knows whether an uplink exists, and a copy would go stale at session end.
        let mic_muted = stream.is_some_and(|st| st.handle.mic.muted());
        // A switch inside a stream starts the notice's clock. A stream ending muted is not
        // the player unmuting, so that edge clears it instead.
        if mic_muted != self.mic_muted_seen {
            self.mic_muted_seen = mic_muted;
            self.mic_muted_at = stream.is_some().then(Instant::now);
        }
        // The badge clears itself once a local mute has been read; the mask's clock lives
        // here because only the frame loop knows when it last moved.
        let audio_mute = live.and_then(|st| st.connector.as_ref()).and_then(|c| {
            let mask = c.audio_mute();
            if mask != self.audio_mute_seen {
                self.audio_mute_seen = mask;
                self.audio_mute_at = Instant::now();
            }
            punktfunk_core::client::audio_mute_notice(mask, self.audio_mute_at.elapsed())
        });
        OverlayFacts {
            width,
            height,
            ten_bit: self.presenter.ten_bit(),
            // Re-read per frame: a drag to a monitor of another scale changes it.
            scale: overlay_scale(self.window.display_scale(), self.osd_scale_pref),
            stats: live
                .filter(|st| chrome && !st.osd.is_empty())
                .map(|st| st.osd.as_slice()),
            stats_corner: stream.map_or(HudCorner::TopLeft, |st| st.params.stats_corner),
            stats_scale: stream.map_or(1.0, |st| st.params.stats_scale),
            exit_hint: stream.is_some_and(|st| st.params.exit_hint),
            // No "click to capture" over a session with nothing to capture for, or while
            // another window has focus: the hint returns with the focus.
            hint: live
                .and_then(|st| st.capture.as_ref())
                .filter(|cap| !cap.captured() && cap.can_capture() && !self.focus_lost)
                .map(|_| capture_hint(pad.is_some(), &self.shortcuts)),
            // A pill that never goes away is chrome, so it rides the stats tier. `None` for a
            // full-control permanent session.
            access: live
                .filter(|_| chrome)
                .and_then(|st| st.access.chip_text(Instant::now())),
            notice: live.and_then(|st| st.session_notice.as_ref().map(|(n, _)| n.as_str())),
            mic_notice: self
                .mic_muted_at
                .and_then(|at| punktfunk_core::client::mic_mute_notice(mic_muted, at.elapsed())),
            audio_mute,
            resizing: live.is_some_and(|st| st.resize_overlay.active()),
            pad,
            pads: self.gamepad.pads(),
            ring: live.map(|st| {
                ring_facts(
                    st,
                    &self.opts,
                    self.stats_verbosity,
                    mic_muted,
                    self.ring_opener,
                    self.gamepad.kind_override(),
                )
            }),
            native: self.native,
            shortcuts: self.shortcuts.clone(),
        }
    }
}
