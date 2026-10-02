//! Input: SDL events into capture, touch into the gesture engine, and the pad mask.

use super::pace::bump_stats_tier;
use super::*;
use sdl3::keyboard::{Keycode, Scancode};

impl Shell {
    /// One SDL event: the console sees it first, then capture, touch, the chords and the
    /// window. `Break` is the window closing.
    pub(super) fn on_event(
        &mut self,
        stream: &mut Option<StreamState>,
        event: Event,
    ) -> Result<ControlFlow<Outcome>> {
        // Console UI sees input first: a consumed event never reaches capture/forwarding.
        if let Some(o) = self.overlay.as_mut() {
            if o.handle_event(&event) {
                self.scroll_routing.consumed(&event);
                return Ok(ControlFlow::Continue(()));
            }
            // Mouse/touch: console hit-tests in its own pixel space. Consumed while
            // the console is up; ignored while streaming (those belong to `Capture`).
            if let Some(input) = overlay_pointer(&event, &self.window) {
                if o.handle_pointer(input) {
                    self.scroll_routing.consumed(&event);
                    return Ok(ControlFlow::Continue(()));
                }
            }
        }
        match event {
            Event::Quit { .. } => {
                if let Some(st) = stream {
                    st.request_quit();
                }
                return Ok(ControlFlow::Break(Outcome::Ended(None)));
            }
            Event::Window { win_event, .. } => self.on_window_event(stream, win_event)?,
            // The panel's rate changed under the window (60 ↔ 165 Hz in the OS
            // settings): no window event fires, and the grid describes the old rate.
            Event::Display {
                display_event: DisplayEvent::CurrentModeChanged | DisplayEvent::DesktopModeChanged,
                display,
                ..
            } if self.window.get_display().is_ok_and(|d| d == display) => {
                self.presenter.retarget_glass(&self.window);
                if let Some(m) = super::shell::native_mode_of(&self.window) {
                    self.native = m;
                }
                if let Some(st) = stream.as_mut() {
                    st.relearn_grid(&self.window);
                }
            }
            // Windows never auto-repeats injected input, so a held key needs these.
            // Chords and toggles fire on the first press only.
            Event::KeyDown {
                scancode: Some(sc),
                repeat: true,
                ..
            } => {
                if let Some(cap) = capture_mut(stream) {
                    cap.on_key_repeat(sc);
                }
            }
            Event::KeyDown {
                keycode,
                scancode: Some(sc),
                keymod,
                repeat: false,
                ..
            } => self.on_key_down(stream, keycode, sc, keymod),
            Event::KeyUp {
                scancode: Some(sc), ..
            } => {
                if let Some(cap) = capture_mut(stream) {
                    cap.on_key_up(sc);
                }
            }
            Event::MouseMotion {
                x, y, xrel, yrel, ..
            } => self.on_mouse_motion(stream, x, y, xrel, yrel),
            Event::MouseButtonDown { mouse_btn, .. } => {
                if let Some(cap) = capture_mut(stream) {
                    if !cap.captured() {
                        // The engaging click is not forwarded. `engage` refuses when
                        // access covers neither pointer nor keyboard — the click then
                        // does nothing.
                        if cap.engage() {
                            self.capture_on(cap);
                        }
                    } else {
                        cap.on_button_down(mouse_btn);
                    }
                }
            }
            Event::MouseButtonUp { mouse_btn, .. } => {
                if let Some(cap) = capture_mut(stream) {
                    cap.on_button_up(mouse_btn);
                }
            }
            Event::MouseWheel { x, y, .. } => {
                // The overlay consumes SDL wheels before native/fallback routing.
                self.scroll_routing.wheel(capture_mut(stream), x, y);
            }
            Event::FingerDown {
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
                ..
            } => self.on_finger(
                stream,
                FingerPhase::Down,
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
            ),
            Event::FingerMotion {
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
                ..
            } => self.on_finger(
                stream,
                FingerPhase::Move,
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
            ),
            Event::FingerUp {
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
                ..
            } => self.on_finger(
                stream,
                FingerPhase::Up,
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
            ),
            // FrameWake (and any other user event): pure wake-up — the frame drain
            // runs this iteration either way.
            Event::User { .. } => {}
            other => self.pump.handle_event(other),
        }
        Ok(ControlFlow::Continue(()))
    }

    fn on_window_event(
        &mut self,
        stream: &mut Option<StreamState>,
        win_event: WindowEvent,
    ) -> Result<()> {
        match win_event {
            WindowEvent::FocusLost => {
                self.scroll_routing.focus_lost();
                // The compositor may take variable refresh away from an unfocused window.
                if let Some(st) = stream.as_mut() {
                    st.forget_refresh_verdict();
                }
                if let Some(cap) = capture_mut(stream) {
                    if cap.release(false) {
                        self.capture_off();
                        tracing::info!("focus lost — input released");
                    }
                }
                // Controllers go with keyboard and mouse. SDL already stops
                // delivering presses here, but nothing zeroed what the host still
                // believes is held — masking flushes it neutral.
                self.focus_lost = true;
            }
            WindowEvent::FocusGained => {
                if let Some(st) = stream.as_mut() {
                    st.forget_refresh_verdict();
                }
                // Unlike capture, the controller mask has no "the user meant it"
                // variant — it only mirrors who owns the pad — so regaining focus
                // always lifts its half.
                self.focus_lost = false;
                // An auto-release (Alt-Tab) undoes itself; a chord release stays
                // until the user opts in. With the ring up the grab waits for its close.
                if let Some(cap) = capture_mut(stream) {
                    if cap.should_reengage() && cap.engage() && !self.ring_was_open {
                        self.capture_on(cap);
                        tracing::info!("focus gained — input recaptured");
                    }
                }
            }
            WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..) => {
                // A driver that refuses the new size must not end the session.
                // A refused fullscreen swapchain costs the fullscreen, not the
                // stream: fall back to the geometry that was already working.
                // A windowed failure still propagates — no smaller state to fall back to.
                if let Err(e) = self.presenter.recreate_swapchain(&self.window) {
                    if !self.fullscreen {
                        return Err(e);
                    }
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "swapchain recreate failed — leaving fullscreen"
                    );
                    self.fullscreen = false;
                    if let Err(e) = self.window.set_fullscreen(false) {
                        tracing::warn!(error = %e, "fullscreen exit failed");
                    }
                    if let Some(st) = stream.as_mut() {
                        st.forget_refresh_verdict();
                    }
                    return Ok(());
                }
                self.presenter.present(
                    &self.window,
                    FrameInput::Redraw,
                    self.overlay_frame.as_ref(),
                )?;
                // Match-window: restamp the debounce. The request fires once
                // ~400 ms pass with no further size events, never per drag-frame.
                if self.opts.match_window.is_some() {
                    if let Some(st) = stream.as_mut() {
                        st.resize_pending = Some(Instant::now());
                    }
                }
            }
            // Dragged to another monitor: latch grid and VRR verdict belong to
            // the old panel. A 60 Hz-seeded clock must not keep pacing a 144 Hz panel.
            WindowEvent::DisplayChanged(..) => {
                self.presenter.retarget_glass(&self.window);
                // The next stream asks this display for its mode, not the one at open.
                if let Some(m) = super::shell::native_mode_of(&self.window) {
                    self.native = m;
                }
                if let Some(st) = stream.as_mut() {
                    st.relearn_grid(&self.window);
                }
            }
            WindowEvent::Exposed => {
                self.presenter.present(
                    &self.window,
                    FrameInput::Redraw,
                    self.overlay_frame.as_ref(),
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    /// A first press: one of the loop's own chords, else a key for the host. Super stays
    /// local while shortcut capture is off: the local shell acts on it, so forwarding it
    /// opens the host's launcher as well. Its up and repeats follow the down.
    fn on_key_down(
        &mut self,
        stream: &mut Option<StreamState>,
        keycode: Option<Keycode>,
        sc: Scancode,
        keymod: Mod,
    ) {
        let Some(chord) = chord_of(keycode, sc, keymod) else {
            if !self.opts.inhibit_shortcuts && matches!(sc, Scancode::LGui | Scancode::RGui) {
                return;
            }
            if let Some(cap) = capture_mut(stream) {
                cap.on_key_down(sc);
            }
            return;
        };
        match chord {
            Chord::Capture => {
                if let Some(cap) = capture_mut(stream) {
                    if cap.captured() {
                        cap.release(true);
                        self.capture_off();
                    } else if cap.engage() {
                        self.capture_on(cap);
                    }
                    tracing::info!(captured = cap.captured(), "chord: release/engage");
                }
            }
            // Mouse model flip. Applies immediately when engaged; a released
            // stream just changes what the next engage does.
            Chord::MouseModel => {
                if let Some(st) = stream.as_mut() {
                    let mut flipped = false;
                    if let Some(cap) = st.capture.as_mut() {
                        match cap.toggle_desktop() {
                            Some(desktop) => {
                                if cap.captured() {
                                    self.capture_on(cap);
                                }
                                flipped = true;
                                tracing::info!(desktop, "chord: mouse mode");
                            }
                            None => tracing::info!(
                                "chord: mouse mode — host has no absolute pointer \
                                 (gamescope), staying captured"
                            ),
                        }
                    }
                    // A manual flip outranks the standing hint until the host's
                    // intent next changes (the hint edge clears this).
                    if flipped {
                        st.hint_override = true;
                    }
                }
            }
            Chord::Disconnect => {
                if let Some(st) = stream {
                    tracing::info!("chord: disconnect");
                    st.request_quit();
                    self.capture_off();
                }
            }
            Chord::Stats => {
                bump_stats_tier(&mut self.stats_verbosity, stream);
                tracing::info!(tier = ?self.stats_verbosity, "chord: stats verbosity");
            }
            // Quick-action ring at the window centre (a locked pointer has no
            // position worth opening at).
            Chord::Ring => {
                if let (Some(o), true) = (self.overlay.as_mut(), stream.is_some()) {
                    let (pw, ph) = self.window.size_in_pixels();
                    o.ring_input(RingInput::Toggle {
                        x: pw as f32 / 2.0,
                        y: ph as f32 / 2.0,
                    });
                }
            }
            // Mic mute — per session, never persisted. The uplink keeps running;
            // only sending stops. A session with no mic says so instead of
            // swallowing the chord.
            Chord::Mic => {
                if let Some(st) = stream {
                    match st.handle.mic.toggle() {
                        Some(muted) => tracing::info!(muted, "chord: microphone mute"),
                        None => tracing::info!(
                            "chord: microphone mute — this session streams no \
                             microphone (turn it on in Settings)"
                        ),
                    }
                }
            }
            Chord::Fullscreen => {
                self.fullscreen = !self.fullscreen;
                tracing::debug!(fullscreen = self.fullscreen, "fullscreen toggle");
                if let Some(st) = stream.as_mut() {
                    st.forget_refresh_verdict();
                }
                if let Err(e) = self.window.set_fullscreen(self.fullscreen) {
                    tracing::warn!(error = %e, fullscreen = self.fullscreen, "fullscreen toggle failed");
                }
            }
        }
    }

    fn on_mouse_motion(
        &mut self,
        stream: &mut Option<StreamState>,
        x: f32,
        y: f32,
        xrel: f32,
        yrel: f32,
    ) {
        let Some(st) = stream.as_mut() else {
            return;
        };
        let video = st.last_video;
        // The echo of our own follow-warp is not the user moving.
        if Instant::now() >= st.warp_echo_until {
            st.last_user_motion = Instant::now();
        }
        let Some(cap) = st.capture.as_mut() else {
            return;
        };
        if cap.desktop() {
            // Desktop model: window position through the placement. Before the first
            // decoded frame there is nothing to map onto — dropped, like touch.
            if let Some(video) = video {
                let (lw, lh) = self.window.size();
                let nx = x / lw.max(1) as f32;
                let ny = y / lh.max(1) as f32;
                cap.on_motion_abs(finger_to_frame(
                    self.opts.video_fit,
                    self.window.size_in_pixels(),
                    video,
                    nx,
                    ny,
                ));
            }
        } else if st.touch_mouse.leaks(xrel, yrel) {
            // Gaming Mode touch-as-mouse: a leaked position, not a delta — dropped,
            // and said once.
            if st.touch_mouse.take_notice() {
                tracing::warn!(
                    xrel,
                    yrel,
                    "Steam Input is replaying the touchscreen as a mouse — \
                     dropping the leaked positions"
                );
                st.session_notice = Some((
                    "Steam Input is sending the touchscreen as a mouse — \
                     pick the Punktfunk controller layout for touch"
                        .into(),
                    Instant::now(),
                ));
            }
        } else {
            cap.on_motion(xrel, yrel);
        }
    }

    /// Touchscreen fingers → the session's touch model. `x`/`y` are window-normalized;
    /// only DIRECT devices (an INDIRECT trackpad drives the mouse). A three-finger tap
    /// bumps the stats tier.
    #[allow(clippy::too_many_arguments)]
    fn on_finger(
        &mut self,
        stream: &mut Option<StreamState>,
        phase: FingerPhase,
        touch_id: u64,
        finger_id: u64,
        x: f32,
        y: f32,
        timestamp: u64,
    ) {
        if !is_direct_touch(touch_id) {
            // A finger from a device SDL does not call a touchscreen: ignored, and said
            // once — otherwise "touch arrived and was thrown away" is indistinguishable
            // from "no touch arrived".
            if let (FingerPhase::Down, Some(st)) = (phase, stream.as_mut()) {
                if !st.touch_mouse.indirect_seen {
                    st.touch_mouse.indirect_seen = true;
                    tracing::info!(
                        touch_id,
                        "finger from a non-direct touch device — ignored (a trackpad \
                         drives the mouse)"
                    );
                }
            }
            return;
        }
        if let (FingerPhase::Down, Some(st)) = (phase, stream.as_mut()) {
            if !st.touch_mouse.fingers_seen {
                tracing::info!(
                    touch_id,
                    "first touchscreen finger: direct touch reaches the client"
                );
            }
            st.touch_mouse.fingers_seen = true;
        }
        // The lift also reaches the engine, so it never keeps a finger that is gone.
        if ring_finger(&mut self.overlay, &self.window, phase, x, y) && phase != FingerPhase::Up {
            return;
        }
        for act in dispatch_finger(
            phase,
            &self.window,
            stream,
            finger_id,
            x,
            y,
            timestamp,
            self.opts.video_fit,
        ) {
            on_touch_act(act, &mut self.stats_verbosity, stream, &mut self.overlay);
        }
    }
}

impl Shell {
    /// Whether window focus, Gaming Mode's overlay or a released capture wants the pads
    /// masked. The ring is the mask's third owner; [`Shell::pad_owner_tick`] joins them.
    pub(super) fn ui_wants_mask(&self, stream: &Option<StreamState>) -> bool {
        // Who owns the pad: capture, window focus, and Gaming Mode's overlay signal.
        // Edge-triggered so an open QAM does not re-flush the pads every iteration.
        #[cfg(target_os = "linux")]
        let overlay_now = self.overlay_focus.as_ref().is_some_and(|of| of.is_open());
        #[cfg(not(target_os = "linux"))]
        let overlay_now = false;
        // Remembered, not applied: the ring is a third owner of this same mask and is only
        // known further down. Applying here too gave one boolean two latches, and whichever
        // fell last unmasked the pads while the other still wanted them masked.
        let capture_active = stream
            .as_ref()
            .and_then(|st| st.capture.as_ref())
            .map(Capture::captured);
        ui_wants_pad_mask(self.focus_lost, overlay_now, capture_active)
    }

    /// Drain forwarded cursor shape/state and drive the local OS cursor — only
    /// meaningful in the desktop mouse model (capture's relative lock hides it).
    pub(super) fn cursor_tick(&mut self, st: &mut StreamState) {
        // Host-framebuffer px → cursor-surface px: the placement scale (physical px per
        // host px) over what the backend scales a cursor by itself, so the pointer is the
        // size it has in the picture, as on Apple. Stretch keeps the shape undistorted.
        let cursor_scale = st.last_video.map_or(1.0, |video| {
            let p = video_fit::place(self.opts.video_fit, self.window.size_in_pixels(), video);
            p.scale_x.min(p.scale_y) as f32 / cursor_surface_scale(&self.window)
        });
        if let (Some(chan), Some(c)) = (st.cursor_chan.as_mut(), st.connector.as_ref()) {
            let desktop_active = st
                .capture
                .as_ref()
                .is_some_and(|cap| cap.captured() && cap.desktop());
            chan.pump(c, &self.mouse, desktop_active, cursor_scale);
            // We draw the pointer while released (a released cursor over a composited one is
            // a frozen twin), in desktop mode, or relative on the host's hint: only the state
            // it keeps forwarding can clear the hint. Without the pointer grant the host
            // pointer is someone else's, so the host draws it.
            let hint_relative = !st.hint_override && st.last_hint == Some(true);
            let client_draws = match st.capture.as_ref() {
                Some(cap) => {
                    (!cap.captured() || cap.desktop() || hint_relative)
                        && cap.grants() & punktfunk_core::quic::GRANT_POINTER != 0
                }
                None => true,
            };
            if chan.negotiated() && st.sent_client_draws != Some(client_draws) {
                st.sent_client_draws = Some(client_draws);
                let _ = c.set_cursor_render(client_draws);
            }
        }
        // Host-driven mode flip: `relative_hint` set = run captured relative; clear
        // = return to absolute. Edge-triggered so a manual chord is not fought: the
        // override latch holds until the host's intent next changes. The hint must hold
        // [`HINT_SETTLE`] with no button down (Windows hides the pointer for a click or a
        // keystroke), and a grab needs the pointer over this window.
        let hint_state = st.cursor_chan.as_ref().and_then(|ch| ch.state());
        if let Some(hs) = hint_state {
            let hint = hs.relative_hint();
            if st.last_hint != Some(hint) {
                st.last_hint = Some(hint);
                st.hint_override = false;
                st.hint_since = std::time::Instant::now();
            }
            if !st.hint_override && st.hint_since.elapsed() >= HINT_SETTLE {
                let video = st.last_video;
                let over_us = !hint || self.mouse.focused_window_id() == Some(self.window.id());
                if let Some(cap) = st.capture.as_mut() {
                    if cap.captured()
                        && !cap.buttons_held()
                        && over_us
                        && !self.ring_was_open
                        && cap.set_desktop(!hint)
                    {
                        self.capture_on(cap);
                        if cap.desktop() {
                            // Reappear where the host last had the pointer so the
                            // hand-back is seamless.
                            if let Some(video) = video {
                                let (wx, wy) = content_to_window(
                                    self.opts.video_fit,
                                    self.window.size(),
                                    self.window.size_in_pixels(),
                                    video,
                                    hs.x,
                                    hs.y,
                                );
                                self.mouse.warp_mouse_in_window(&self.window, wx, wy);
                            }
                        }
                        tracing::info!(
                            desktop = cap.desktop(),
                            "host cursor hint: mouse model flipped"
                        );
                    }
                }
            }
            // Something else moved the pointer we draw (controller mouse, a trackpad
            // gesture, an app warping it): once the user's own mouse has been still for
            // longer than a round trip, the local cursor goes where the host put it.
            let over_us = self.mouse.focused_window_id() == Some(self.window.id());
            let still = st.last_user_motion.elapsed() >= FOLLOW_HOST_AFTER;
            if let (Some(cap), Some(video)) = (st.capture.as_mut(), st.last_video) {
                let drifted = cap.last_abs().is_some_and(|(x, y)| {
                    (x - hs.x).abs() > FOLLOW_SLACK_PX || (y - hs.y).abs() > FOLLOW_SLACK_PX
                });
                if cap.captured() && cap.desktop() && hs.visible() && over_us && still && drifted {
                    let (wx, wy) = content_to_window(
                        self.opts.video_fit,
                        self.window.size(),
                        self.window.size_in_pixels(),
                        video,
                        hs.x,
                        hs.y,
                    );
                    st.warp_echo_until = Instant::now() + WARP_ECHO;
                    self.mouse.warp_mouse_in_window(&self.window, wx, wy);
                    cap.followed_host((hs.x, hs.y));
                }
            }
        }
    }

    /// SDL text input tracks overlay editing (IME / Steam OSK), edge-wise.
    pub(super) fn text_input_tick(&mut self) {
        let want_text = self.overlay.as_ref().is_some_and(|o| o.text_input_active());
        if want_text != self.text_input_on {
            self.text_input_on = want_text;
            let ti = self.sdl_video.text_input();
            if want_text {
                ti.start(&self.window);
            } else {
                ti.stop(&self.window);
            }
        }
    }

    /// Who owns the pads: pad chords, the ring, the mask, and the escape and disconnect
    /// holds. `want_mask_ui` is [`Shell::ui_wants_mask`] from before the pump tick.
    pub(super) fn pad_owner_tick(&mut self, stream: &mut Option<StreamState>, want_mask_ui: bool) {
        // Select chords on a pad. `Select+A` puts the ring at the window centre, where its
        // highlight starts on the centre so `Select+A` then `A` opens the sheet; `Select+X`
        // steps the stats tier, the same move the keyboard chord and the dial's own slot make.
        while let Ok((pad, chord)) = self.chord_rx.try_recv() {
            if self.overlay.is_none() || stream.is_none() {
                continue;
            }
            match chord {
                SelectChord::Ring => {
                    if let Some(o) = self.overlay.as_mut() {
                        self.ring_opener = Some(pad);
                        let (pw, ph) = self.window.size_in_pixels();
                        o.ring_input(RingInput::Toggle {
                            x: pw as f32 / 2.0,
                            y: ph as f32 / 2.0,
                        });
                    }
                }
                SelectChord::Stats => {
                    bump_stats_tier(&mut self.stats_verbosity, stream);
                    tracing::info!(tier = ?self.stats_verbosity, "chord: stats verbosity");
                }
            }
        }
        // While the ring is up, or the console holds a launch over the live stream, the
        // pad belongs to the overlay: masked off the wire, polled into menu events. The
        // three gates that keep pad input off client UI flip together.
        let ring_open = stream.is_some()
            && self
                .overlay
                .as_ref()
                .is_some_and(|o| o.ring_open() || o.holds_stream());
        if ring_open != self.ring_was_open {
            self.ring_was_open = ring_open;
            self.gamepad.set_ring_nav(ring_open);
            if !ring_open {
                self.ring_opener = None;
            }
            // The ring eats every pointer event, so a button already down would stay pressed
            // on the host without a flush. It also needs a pointer to aim with: under a lock
            // the cursor is hidden and every event carries the position the lock froze, so
            // capture hands the local one back while it is up and takes the window on close.
            if let Some(cap) = capture_mut(stream) {
                if ring_open {
                    cap.flush_held();
                }
                if cap.captured() {
                    if ring_open {
                        self.capture_off();
                    } else {
                        self.capture_on(cap);
                    }
                }
            }
        }
        // One owner for the mask: any gate that wants the pads keeps them masked.
        let want_mask = want_mask_ui || ring_open;
        if want_mask != self.mask_applied {
            self.mask_applied = want_mask;
            self.gamepad.set_masked(want_mask);
        }
        if ring_open {
            while let Ok(ev) = self.menu_rx.try_recv() {
                if let Some(o) = self.overlay.as_mut() {
                    o.handle_menu(ev);
                }
            }
        }

        // Controller escape chord: release capture and leave fullscreen. Gamescope has
        // nothing to release into and no pointer to click back with, while a release masks
        // the pads — there the chord only starts the disconnect hold.
        while self.escape_rx.try_recv().is_ok() {
            if in_gamescope() {
                continue;
            }
            if let Some(cap) = capture_mut(stream) {
                if cap.release(true) {
                    self.capture_off();
                }
            }
            if self.fullscreen && !self.opts.fullscreen {
                self.fullscreen = false;
                let _ = self.window.set_fullscreen(false);
                if let Some(st) = stream.as_mut() {
                    st.forget_refresh_verdict();
                }
            }
        }
        // Escape chord held past the threshold: the controller's disconnect.
        if self.disconnect_rx.try_recv().is_ok() {
            if let Some(st) = stream {
                tracing::info!("controller chord: disconnect");
                st.request_quit();
                self.capture_off();
            }
        }
    }
}

/// A key the loop answers itself instead of forwarding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Chord {
    Capture,
    MouseModel,
    Disconnect,
    Stats,
    Ring,
    Mic,
    Fullscreen,
}

/// Ctrl+Alt+Shift with Q, M, D, S, O or V, and F11 or Alt+Enter for fullscreen (some Fn
/// layers send a media key for plain F11). A letter matches the key the layout prints it
/// on (AZERTY's Q sits on Scancode::A), or its position on a layout without that letter.
pub(super) fn chord_of(keycode: Option<Keycode>, sc: Scancode, keymod: Mod) -> Option<Chord> {
    let alt = keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
    if alt
        && keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
        && keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD)
    {
        let table = [
            (Keycode::Q, Scancode::Q, Chord::Capture),
            (Keycode::M, Scancode::M, Chord::MouseModel),
            (Keycode::D, Scancode::D, Chord::Disconnect),
            (Keycode::S, Scancode::S, Chord::Stats),
            (Keycode::O, Scancode::O, Chord::Ring),
            (Keycode::V, Scancode::V, Chord::Mic),
        ];
        if let Some(&(.., chord)) = table
            .iter()
            .find(|(k, s, _)| keycode == Some(*k) || sc == *s)
        {
            return Some(chord);
        }
    }
    (sc == Scancode::F11 || (sc == Scancode::Return && alt)).then_some(Chord::Fullscreen)
}

pub(super) fn ui_wants_pad_mask(
    focus_lost: bool,
    overlay_open: bool,
    capture_active: Option<bool>,
) -> bool {
    focus_lost || overlay_open || capture_active == Some(false)
}

/// One SDL mouse/touch event as the overlay wants it: swapchain pixels. `None` for
/// events the console cannot use.
///
/// Two conversions: mouse positions are window (logical) coordinates; fingers arrive
/// window-normalized (0..1). Mixing them puts every click off by the display scale.
/// Only DIRECT touch devices; an indirect trackpad already drives the mouse.
pub(super) fn overlay_pointer(event: &Event, window: &sdl3::video::Window) -> Option<PointerInput> {
    // SDL's mouse id on mouse events synthesized from a touch (`SDL_TOUCH_MOUSEID`,
    // not re-exported by the sdl3 crate). The finger arms already forward the real
    // touch; the synthesized twin would land every tap twice.
    const TOUCH_MOUSEID: u32 = u32::MAX;
    let (pw, ph) = window.size_in_pixels();
    let (lw, lh) = window.size();
    // Logical → physical. A zero-sized window (minimized) would divide by zero.
    let sx = pw as f32 / lw.max(1) as f32;
    let sy = ph as f32 / lh.max(1) as f32;
    let button = |b: sdl3::mouse::MouseButton| match b {
        sdl3::mouse::MouseButton::Left => Some(PointerButton::Primary),
        sdl3::mouse::MouseButton::Right => Some(PointerButton::Secondary),
        _ => None,
    };
    Some(match event {
        Event::MouseMotion { which, x, y, .. } if *which != TOUCH_MOUSEID => PointerInput::Move {
            x: x * sx,
            y: y * sy,
        },
        Event::MouseButtonDown {
            which,
            mouse_btn,
            x,
            y,
            ..
        } if *which != TOUCH_MOUSEID => PointerInput::Down {
            x: x * sx,
            y: y * sy,
            button: button(*mouse_btn)?,
            touch: false,
        },
        Event::MouseButtonUp {
            which,
            mouse_btn,
            x,
            y,
            ..
        } if *which != TOUCH_MOUSEID => PointerInput::Up {
            x: x * sx,
            y: y * sy,
            button: button(*mouse_btn)?,
        },
        Event::MouseWheel {
            y,
            mouse_x,
            mouse_y,
            ..
        } => PointerInput::Wheel {
            x: mouse_x * sx,
            y: mouse_y * sy,
            dy: *y,
        },
        Event::FingerDown { touch_id, x, y, .. } if is_direct_touch(*touch_id) => {
            PointerInput::Down {
                x: x * pw as f32,
                y: y * ph as f32,
                button: PointerButton::Primary,
                touch: true,
            }
        }
        Event::FingerMotion { touch_id, x, y, .. } if is_direct_touch(*touch_id) => {
            PointerInput::Move {
                x: x * pw as f32,
                y: y * ph as f32,
            }
        }
        Event::FingerUp { touch_id, x, y, .. } if is_direct_touch(*touch_id) => PointerInput::Up {
            x: x * pw as f32,
            y: y * ph as f32,
            button: PointerButton::Primary,
        },
        // The pointer left the window mid-press: drop the press rather than let a release
        // that never comes leave a widget armed forever.
        Event::Window {
            win_event: WindowEvent::MouseLeave,
            ..
        } => PointerInput::Cancel,
        _ => return None,
    })
}

/// Every touch device SDL sees, as `(id, kind, name)` — logged at connect. Under
/// gamescope this is the tell for whether Steam Input hands the touchscreen through.
pub(super) fn touch_devices() -> Vec<(u64, &'static str, String)> {
    use sdl3::sys::stdinc::SDL_free;
    use sdl3::sys::touch::{
        SDL_GetTouchDeviceName, SDL_GetTouchDeviceType, SDL_GetTouchDevices, SDL_TouchDeviceType,
    };
    let kind = |t: SDL_TouchDeviceType| {
        if t == SDL_TouchDeviceType::DIRECT {
            "direct"
        } else if t == SDL_TouchDeviceType::INDIRECT_ABSOLUTE {
            "indirect-absolute"
        } else if t == SDL_TouchDeviceType::INDIRECT_RELATIVE {
            "indirect-relative"
        } else {
            "invalid"
        }
    };
    let mut n: std::ffi::c_int = 0;
    // SAFETY: SDL hands back an array it owns (freed here once read, and never touched
    // after) and names it owns (copied out before the free, never kept); a null array or
    // name is checked before use.
    unsafe {
        let ids = SDL_GetTouchDevices(&mut n);
        if ids.is_null() {
            return Vec::new();
        }
        let out = std::slice::from_raw_parts(ids, usize::try_from(n).unwrap_or(0))
            .iter()
            .map(|id| {
                let name = SDL_GetTouchDeviceName(*id);
                let name = if name.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(name)
                        .to_string_lossy()
                        .into_owned()
                };
                (id.0, kind(SDL_GetTouchDeviceType(*id)), name)
            })
            .collect();
        SDL_free(ids.cast());
        out
    }
}

/// Is this SDL touch device a real touchscreen (DIRECT, window-relative)? Trackpads
/// report INDIRECT and drive the mouse — their finger events must not be forwarded
/// as touch passthrough. An unknown/invalid id reads as not-direct.
pub(super) fn is_direct_touch(touch_id: u64) -> bool {
    use sdl3::sys::touch::{SDL_GetTouchDeviceType, SDL_TouchDeviceType, SDL_TouchID};
    // SAFETY: `SDL_GetTouchDeviceType` is a query on an id SDL issued; the TouchID
    // wrapper is a newtype over that id and does not take ownership of any handle.
    unsafe { SDL_GetTouchDeviceType(SDL_TouchID(touch_id)) == SDL_TouchDeviceType::DIRECT }
}

/// Route one SDL touchscreen finger into the session's [`Capture`]. SDL delivers
/// window-normalized `x`/`y` (0..1); the dispatcher hands physical window pixels
/// (trackpad ballistics) and the frame position under `fit` (pointer + passthrough).
/// Down/Move before the first decoded frame are dropped; an Up always dispatches so
/// a lift can release a held contact.
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_finger(
    phase: FingerPhase,
    window: &sdl3::video::Window,
    stream: &mut Option<StreamState>,
    finger_id: u64,
    x: f32,
    y: f32,
    timestamp: u64,
    fit: VideoFit,
) -> Vec<Act> {
    let Some(st) = stream.as_mut() else {
        return Vec::new();
    };
    let (pw, ph) = window.size_in_pixels();
    let (wx, wy) = (x * pw as f32, y * ph as f32);
    let abs = match st.last_video {
        Some(video) => finger_to_frame(fit, (pw, ph), video, x, y),
        None if phase == FingerPhase::Up => Abs {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        None => return Vec::new(),
    };
    let Some(cap) = st.capture.as_mut() else {
        return Vec::new();
    };
    // `wx`/`wy` are physical px; the gesture engine prices scroll in DIP.
    cap.set_touch_density(window.display_scale());
    cap.dispatch_finger(
        phase,
        finger_id,
        wx,
        wy,
        abs,
        timestamp as f64 / 1_000_000.0,
    )
}

/// Three-finger tap bumps the stats tier; a two-finger twist turns the quick-action ring.
pub(super) fn on_touch_act(
    act: Act,
    verbosity: &mut StatsVerbosity,
    stream: &mut Option<StreamState>,
    overlay: &mut Option<Box<dyn Overlay>>,
) {
    let input = match act {
        Act::CycleStats => return bump_stats_tier(verbosity, stream),
        Act::Dial {
            progress,
            clockwise,
            x,
            y,
        } => RingInput::Turn {
            progress,
            clockwise,
            x,
            y,
        },
        Act::DialCommit => RingInput::Commit,
        Act::DialCancel => RingInput::Cancel,
        _ => return,
    };
    if let Some(o) = overlay.as_mut() {
        o.ring_input(input);
    }
}

/// A touchscreen finger while the ring is up goes to the ring as a pointer, not the
/// gesture engine. Returns true when the ring took it.
pub(super) fn ring_finger(
    overlay: &mut Option<Box<dyn Overlay>>,
    window: &sdl3::video::Window,
    phase: FingerPhase,
    x: f32,
    y: f32,
) -> bool {
    let Some(o) = overlay.as_mut().filter(|o| o.ring_open()) else {
        return false;
    };
    let (pw, ph) = window.size_in_pixels();
    let (x, y) = (x * pw as f32, y * ph as f32);
    let input = match phase {
        FingerPhase::Down => PointerInput::Down {
            x,
            y,
            button: PointerButton::Primary,
            touch: true,
        },
        FingerPhase::Move => PointerInput::Move { x, y },
        FingerPhase::Up => PointerInput::Up {
            x,
            y,
            button: PointerButton::Primary,
        },
    };
    o.handle_pointer(input);
    true
}

/// Window-normalized position → frame pixel, with the frame size as the wire extent.
/// Same placement as the blit; a finger in a bar or on a cropped edge clamps onto the
/// visible frame.
pub(super) fn finger_to_frame(
    fit: VideoFit,
    surface: (u32, u32),
    video: (u32, u32),
    x: f32,
    y: f32,
) -> Abs {
    let p = video_fit::place(fit, surface, video);
    let (fx, fy) = p.to_frame(
        f64::from(x) * f64::from(surface.0),
        f64::from(y) * f64::from(surface.1),
    );
    Abs {
        x: fx.round() as i32,
        y: fy.round() as i32,
        w: video.0,
        h: video.1,
    }
}

/// Inverse of [`finger_to_frame`] for the reappear warp: a host-frame pixel → logical
/// window coordinates (what `warp_mouse_in_window` takes). Coordinates outside the
/// visible frame clamp onto it.
pub(super) fn content_to_window(
    fit: VideoFit,
    logical: (u32, u32),
    surface: (u32, u32),
    video: (u32, u32),
    x: i32,
    y: i32,
) -> (f32, f32) {
    let p = video_fit::place(fit, surface, video);
    let (px, py) = p.to_view(f64::from(x), f64::from(y));
    // Physical → logical (HiDPI): the window's logical size over its pixel size.
    let lx = px * f64::from(logical.0) / f64::from(surface.0.max(1));
    let ly = py * f64::from(logical.1) / f64::from(surface.1.max(1));
    (lx as f32, ly as f32)
}

/// The scale the backend applies to a custom cursor surface on its own. Wayland applies the
/// display scale: SDL hands the compositor the bitmap's pixel size as a surface-local viewport
/// destination. X11 and Windows show the surface at 1:1 physical pixels.
///
/// `SDL_GetWindowDisplayScale` returns `0.0` when it cannot resolve the display; dividing by 0
/// would blow the cursor up to nothing usable.
pub(super) fn cursor_surface_scale(window: &sdl3::video::Window) -> f32 {
    if window.subsystem().current_video_driver() != "wayland" {
        return 1.0;
    }
    let scale = window.display_scale();
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

/// How long the host's relative hint must hold before the mouse model follows it: longer
/// than the hide Windows does for a click, short against a game grabbing the pointer.
pub(super) const HINT_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// Mouse stillness before the local cursor follows host-driven motion: past a round trip, so
/// the host's echo of the user's own motion is never mistaken for someone else's.
pub(super) const FOLLOW_HOST_AFTER: std::time::Duration = std::time::Duration::from_millis(250);

/// How long motion events after a follow-warp are its echo, not the user.
pub(super) const WARP_ECHO: std::time::Duration = std::time::Duration::from_millis(50);

/// Rounding between window and frame pixels; a smaller difference is the same spot.
pub(super) const FOLLOW_SLACK_PX: i32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// Chords need all three modifiers and match by printed letter or by position; the
    /// fullscreen keys need none.
    #[test]
    fn chords_match_by_letter_or_position_under_all_three_modifiers() {
        let all = Mod::LCTRLMOD | Mod::RALTMOD | Mod::LSHIFTMOD;
        assert_eq!(
            chord_of(Some(Keycode::Q), Scancode::Q, all),
            Some(Chord::Capture)
        );
        // AZERTY prints Q where QWERTY has A.
        assert_eq!(
            chord_of(Some(Keycode::Q), Scancode::A, all),
            Some(Chord::Capture)
        );
        assert_eq!(chord_of(None, Scancode::M, all), Some(Chord::MouseModel));
        for (k, s, c) in [
            (Keycode::D, Scancode::D, Chord::Disconnect),
            (Keycode::S, Scancode::S, Chord::Stats),
            (Keycode::O, Scancode::O, Chord::Ring),
            (Keycode::V, Scancode::V, Chord::Mic),
        ] {
            assert_eq!(chord_of(Some(k), s, all), Some(c));
        }
        assert_eq!(chord_of(Some(Keycode::A), Scancode::A, all), None);
        let two = Mod::LCTRLMOD | Mod::LALTMOD;
        assert_eq!(chord_of(Some(Keycode::Q), Scancode::Q, two), None);
        assert_eq!(
            chord_of(Some(Keycode::F11), Scancode::F11, Mod::NOMOD),
            Some(Chord::Fullscreen)
        );
        let alt_enter = chord_of(Some(Keycode::Return), Scancode::Return, Mod::RALTMOD);
        assert_eq!(alt_enter, Some(Chord::Fullscreen));
        assert_eq!(
            chord_of(Some(Keycode::Return), Scancode::Return, Mod::NOMOD),
            None
        );
    }

    #[test]
    fn released_capture_masks_pads_until_capture_returns() {
        assert!(ui_wants_pad_mask(false, false, Some(false)));
        assert!(!ui_wants_pad_mask(false, false, Some(true)));
        assert!(!ui_wants_pad_mask(false, false, None));
        assert!(ui_wants_pad_mask(true, false, Some(true)));
        assert!(ui_wants_pad_mask(false, true, Some(true)));
    }

    #[test]
    fn content_to_window_inverts_the_letterbox() {
        // 1920×1080 video letterboxed in a 1600×1200 (4:3) window at 2× HiDPI: scale =
        // 1600/1920, dh = 900, oy = 150 (physical).
        let logical = (800u32, 600u32);
        let surface = (1600u32, 1200u32);
        let video = (1920u32, 1080u32);
        let (wx, wy) = content_to_window(VideoFit::Fit, logical, surface, video, 960, 540);
        assert!((wx - 400.0).abs() < 1.0, "wx = {wx}");
        assert!((wy - 300.0).abs() < 1.0, "wy = {wy}");
        // Roundtrip: normalized window pos → the same host frame pixel.
        let (nx, ny) = (wx / logical.0 as f32, wy / logical.1 as f32);
        let abs = finger_to_frame(VideoFit::Fit, surface, video, nx, ny);
        assert_eq!((abs.w, abs.h), video);
        assert!((abs.x - 960).abs() <= 1, "x = {}", abs.x);
        assert!((abs.y - 540).abs() <= 1, "y = {}", abs.y);
        // Out-of-range host coords clamp onto the video, never the bars.
        let (_, wy_clamped) = content_to_window(VideoFit::Fit, logical, surface, video, 0, 10_000);
        assert!(wy_clamped <= 300.0 + 225.0 + 1.0, "wy = {wy_clamped}"); // ≤ bottom of content
    }

    #[test]
    fn crop_maps_the_window_onto_the_visible_frame() {
        // 1920×1080 cropped into a 3216×1440 phone-shaped window: sides full, the top
        // and bottom ~110 frame rows cut. The window's top edge is frame row ~110.
        let surface = (3216, 1440);
        let video = (1920, 1080);
        let top = finger_to_frame(VideoFit::Crop, surface, video, 0.0, 0.0);
        assert_eq!((top.x, top.y, top.w, top.h), (0, 110, 1920, 1080));
        let corner = finger_to_frame(VideoFit::Crop, surface, video, 1.0, 1.0);
        assert_eq!((corner.x, corner.y), (1920, 970));
        // Stretch reaches every frame edge from every window edge.
        let s = finger_to_frame(VideoFit::Stretch, surface, video, 1.0, 1.0);
        assert_eq!((s.x, s.y), (1920, 1080));
    }

    fn frame_at(fit: VideoFit, surface: (u32, u32), x: f32, y: f32) -> (i32, i32, u32, u32) {
        let a = finger_to_frame(fit, surface, (1920, 1080), x, y);
        (a.x, a.y, a.w, a.h)
    }

    #[test]
    fn finger_maps_across_a_perfectly_filled_surface() {
        // Video exactly fills the window: normalized finger maps straight through.
        let s = (1920, 1080);
        assert_eq!(frame_at(VideoFit::Fit, s, 0.0, 0.0), (0, 0, 1920, 1080));
        assert_eq!(
            frame_at(VideoFit::Fit, s, 1.0, 1.0),
            (1920, 1080, 1920, 1080)
        );
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.5), (960, 540, 1920, 1080));
    }

    #[test]
    fn finger_rebases_onto_the_letterboxed_frame() {
        // 16:9 video in 16:10 glass (1280×800) letterboxes: the picture is 1280×720,
        // centered with 40px bars. A finger in the top bar clamps to the frame's top edge.
        let s = (1280, 800);
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.5), (960, 540, 1920, 1080));
        // y=0.01 → window pixel 8, above the 40px bar → clamps to frame top (0).
        assert_eq!(frame_at(VideoFit::Fit, s, 0.5, 0.01), (960, 0, 1920, 1080));
        assert_eq!(
            frame_at(VideoFit::Fit, s, 1.0, 1.0),
            (1920, 1080, 1920, 1080)
        );
    }
}
