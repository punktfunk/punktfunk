//! Input: SDL events into capture, touch into the gesture engine, and the pad mask.

use super::pace::bump_stats_tier;
use super::*;
use crate::route::{route, Kind, RouteFacts, Target};
use pf_client_core::shortcuts::Out;
use sdl3::keyboard::{Scancode, TextInputOptions, TextInputType};

impl Shell {
    /// One SDL event: the console's verdict, then [`route`] picks who takes it, and this
    /// performs. `Break` ends the session.
    pub(super) fn on_event(
        &mut self,
        stream: &mut Option<StreamState>,
        event: Event,
    ) -> Result<ControlFlow<Outcome>> {
        let screen = event
            .get_window_id()
            .and_then(|id| stream.as_ref()?.screens.screen_of(id));
        let overlay_took = self.overlay_takes(&event, screen.is_none());
        let keys = self.key_step(&event, overlay_took);
        let fired = keys.iter().any(|o| matches!(o, Out::Fire(..)));
        let facts = RouteFacts {
            on_primary: event.get_window_id() == Some(self.window.id()),
            on_screen: screen.is_some(),
            overlay_took,
            ring_open: self.ring_was_open,
            inhibit_shortcuts: self.opts.inhibit_shortcuts,
            native_scroll: self.scroll_routing.native_active,
            wheel_blocked: self.scroll_routing.blocked,
            captured: self.scroll_routing.captured_before,
        };
        match route(kind_of(&event, fired), &facts) {
            Target::Overlay => self.scroll_routing.consumed(&event),
            Target::Screen => {
                if let Some(screen) = screen {
                    self.on_screen_event(stream, screen, event);
                }
            }
            Target::Chord => self.on_key_shortcut(stream, keys),
            Target::Capture => self.on_capture_event(stream, event),
            Target::Gesture => self.on_finger_event(stream, event),
            Target::Window => {
                if let Event::Window { win_event, .. } = event {
                    self.on_window_event(stream, win_event)?;
                }
            }
            Target::Loop => self.on_loop_event(stream, event),
            Target::Quit => {
                if let Some(st) = stream {
                    st.request_quit();
                }
                return Ok(ControlFlow::Break(Outcome::Ended(None)));
            }
            Target::Drop => {}
        }
        Ok(ControlFlow::Continue(()))
    }

    /// A key into the keyboard's shortcuts: a first press the console did not take, and every
    /// release, so a key the console took on its way up never stays down in the engine.
    fn key_step(&mut self, event: &Event, overlay_took: bool) -> Vec<Out> {
        match *event {
            Event::KeyDown {
                keycode,
                scancode: Some(sc),
                repeat: false,
                ..
            } if !overlay_took => self.keys.press(keycode, sc),
            Event::KeyUp {
                scancode: Some(sc), ..
            } => {
                self.keys.release(sc);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The console's verdict, asked before routing: keys and text anywhere; the pointer and
    /// fingers in its own pixels, off a further screen's window.
    fn overlay_takes(&mut self, event: &Event, off_screen: bool) -> bool {
        let Some(o) = self.overlay.as_mut() else {
            return false;
        };
        o.handle_event(event)
            || (off_screen
                && overlay_pointer(event, &self.window)
                    .is_some_and(|input| o.handle_pointer(input)))
    }

    /// A further screen's window event or pointer move; its close button gives that
    /// monitor back.
    fn on_screen_event(&mut self, stream: &mut Option<StreamState>, screen: u8, event: Event) {
        match event {
            Event::Window {
                win_event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Some(st) = stream.as_mut() {
                    self.close_screen(st, screen);
                }
            }
            Event::Window { win_event, .. } => {
                if let Some(st) = stream.as_mut() {
                    st.screens.on_window_event(screen, win_event);
                }
            }
            Event::MouseMotion { x, y, .. } => self.on_screen_motion(stream, screen, x, y),
            _ => {}
        }
    }

    /// The stream's keys, buttons, motion and SDL wheel. Windows never auto-repeats injected
    /// input, so a held key needs its repeats. A left click on a released stream engages
    /// capture and is not forwarded; other buttons stay with the desktop (a side button
    /// starts a desktop gesture). `engage` refuses without pointer or keyboard access.
    fn on_capture_event(&mut self, stream: &mut Option<StreamState>, event: Event) {
        if let Event::MouseMotion {
            which,
            x,
            y,
            xrel,
            yrel,
            ..
        } = event
        {
            self.on_mouse_motion(stream, which, x, y, xrel, yrel);
            return;
        }
        let Some(cap) = capture_mut(stream) else {
            return;
        };
        match event {
            Event::KeyDown {
                scancode: Some(sc),
                repeat: true,
                ..
            } => cap.on_key_repeat(sc),
            Event::KeyDown {
                scancode: Some(sc), ..
            } => cap.on_key_down(sc),
            Event::KeyUp {
                scancode: Some(sc), ..
            } => cap.on_key_up(sc),
            Event::MouseButtonDown { mouse_btn, .. } if cap.captured() => {
                cap.on_button_down(mouse_btn);
            }
            Event::MouseButtonDown {
                mouse_btn: sdl3::mouse::MouseButton::Left,
                ..
            } => {
                if cap.engage() {
                    self.capture_on(cap);
                }
            }
            Event::MouseButtonUp { mouse_btn, .. } => cap.on_button_up(mouse_btn),
            Event::MouseWheel { x, y, .. } => cap.on_wheel(x, y),
            _ => {}
        }
    }

    /// The loop's own events: the panel's mode, the client's monitors, frame wakes, pads.
    fn on_loop_event(&mut self, stream: &mut Option<StreamState>, event: Event) {
        match event {
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
            // A monitor came, went or moved: the screens follow the client's monitors.
            Event::Display {
                display_event: DisplayEvent::Added | DisplayEvent::Removed | DisplayEvent::Moved,
                ..
            } if stream.as_ref().is_some_and(|st| st.params.shows_screens) => {
                if let Some(st) = stream.as_mut() {
                    self.replan_screens(st);
                }
            }
            // A frame wake only wakes: the frame drain runs this pass either way.
            Event::User { .. } => {}
            other => self.pump.handle_event(other),
        }
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
                if self.opts.match_window {
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
                    // Screen 0 stands for this window's monitor: off the planned one, replan.
                    let moved = self
                        .window
                        .get_display()
                        .is_ok_and(|d| st.screens.first_display() != Some(d));
                    if st.params.shows_screens && moved {
                        self.replan_screens(st);
                    }
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

    /// Release the captured mouse and keyboard, or capture them again.
    pub(super) fn toggle_capture(&mut self, st: &mut StreamState) {
        let Some(cap) = st.capture.as_mut() else {
            return;
        };
        if cap.captured() {
            cap.release(true);
            self.capture_off();
        } else if cap.engage() {
            self.capture_on(cap);
        }
        tracing::info!(captured = cap.captured(), "chord: release/engage");
    }

    /// Flip the mouse model. Applies at once while engaged; a released stream only changes
    /// what the next engage does.
    pub(super) fn toggle_mouse_mode(&mut self, st: &mut StreamState) {
        let Some(cap) = st.capture.as_mut() else {
            return;
        };
        match cap.toggle_desktop() {
            Some(desktop) => {
                if cap.captured() {
                    self.capture_on(cap);
                }
                st.cursor.manual_flip();
                tracing::info!(desktop, "chord: mouse mode");
            }
            None => tracing::info!(
                "chord: mouse mode — host has no absolute pointer (gamescope), staying captured"
            ),
        }
    }

    pub(super) fn toggle_fullscreen(&mut self, stream: Option<&mut StreamState>) {
        self.fullscreen = !self.fullscreen;
        tracing::debug!(fullscreen = self.fullscreen, "fullscreen toggle");
        if let Some(st) = stream {
            st.forget_refresh_verdict();
        }
        if let Err(e) = self.window.set_fullscreen(self.fullscreen) {
            tracing::warn!(error = %e, fullscreen = self.fullscreen, "fullscreen toggle failed");
        }
    }

    fn on_mouse_motion(
        &mut self,
        stream: &mut Option<StreamState>,
        which: u32,
        x: f32,
        y: f32,
        xrel: f32,
        yrel: f32,
    ) {
        let Some(st) = stream.as_mut() else {
            return;
        };
        let video = st.last_video;
        st.cursor.user_moved(Instant::now());
        let Some(cap) = st.capture.as_mut() else {
            return;
        };
        let deltas = st.cursor.takes_deltas(cap.desktop(), cap.buttons_held());
        let warped = !cap.desktop() && cap.abs_ok() && warps_pointer(which);
        if (cap.desktop() && !deltas) || warped {
            // Window position through the placement. Relative mode freezes SDL's own
            // position, so a warp reads where the pointer really is. Before the first
            // decoded frame there is nothing to map onto — dropped, like touch.
            let (x, y) = if warped {
                pointer_in_window(&self.window)
            } else {
                (x, y)
            };
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
                    pf_l10n::desktop::desktop_notice_steam_touch_mouse().into(),
                    Instant::now(),
                ));
            }
        } else {
            cap.on_motion(xrel, yrel, deltas);
        }
    }

    /// A touchscreen finger event, unpacked for [`Shell::on_finger`].
    fn on_finger_event(&mut self, stream: &mut Option<StreamState>, event: Event) {
        let (phase, touch_id, finger_id, x, y, timestamp) = match event {
            Event::FingerDown {
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
                ..
            } => (FingerPhase::Down, touch_id, finger_id, x, y, timestamp),
            Event::FingerMotion {
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
                ..
            } => (FingerPhase::Move, touch_id, finger_id, x, y, timestamp),
            Event::FingerUp {
                touch_id,
                finger_id,
                x,
                y,
                timestamp,
                ..
            } => (FingerPhase::Up, touch_id, finger_id, x, y, timestamp),
            _ => return,
        };
        self.on_finger(stream, phase, touch_id, finger_id, x, y, timestamp);
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
    /// meaningful in the desktop mouse model (capture's relative lock hides it). The
    /// decisions are [`CursorModel`](crate::cursor_model::CursorModel)'s; this gathers
    /// its inputs and performs them.
    pub(super) fn cursor_tick(&mut self, st: &mut StreamState) {
        // Host-framebuffer px → cursor-surface px: the placement scale (physical px per
        // host px) over what the backend scales a cursor by itself, so the pointer is the
        // size it has in the picture, as on Apple. Stretch keeps the shape undistorted.
        let cursor_scale = st.last_video.map_or(1.0, |video| {
            let p = video_fit::place(self.opts.video_fit, self.window.size_in_pixels(), video);
            p.scale_x.min(p.scale_y) as f32 / cursor_surface_scale(&self.window)
        });
        // Over a further screen's window the host's pointer is in the picture.
        let over_screen = self.over_screen(st);
        if let (Some(chan), Some(c)) = (st.cursor_chan.as_mut(), st.connector.as_ref()) {
            let desktop_active = !over_screen
                && st
                    .capture
                    .as_ref()
                    .is_some_and(|cap| cap.captured() && cap.desktop());
            chan.pump(c, &self.mouse, desktop_active, cursor_scale);
        }
        let hint_state = st.cursor_chan.as_ref().and_then(|ch| ch.state());
        let a = st
            .cursor
            .decide(&self.cursor_inputs(st, hint_state), Instant::now());
        if let (Some(draws), Some(c)) = (a.client_draws, st.connector.as_ref()) {
            let _ = c.set_cursor_render(draws);
        }
        let Some(hs) = hint_state else {
            return;
        };
        if let (Some(desktop), Some(cap)) = (a.want_desktop, st.capture.as_mut()) {
            if cap.set_desktop(desktop) {
                self.capture_on(cap);
                // Reappear where the host last had the pointer so the hand-back is seamless.
                if let (true, Some(video)) = (cap.desktop(), st.last_video) {
                    self.warp_to_host(video, (hs.x, hs.y));
                }
                tracing::info!(
                    desktop = cap.desktop(),
                    "host cursor hint: mouse model flipped"
                );
            }
        }
        // Gathered again: the flip and its warp above change what the follow reads.
        let inputs = self.cursor_inputs(st, hint_state);
        if st.cursor.follow(&inputs, Instant::now()) {
            if let (Some(cap), Some(video)) = (st.capture.as_mut(), st.last_video) {
                self.warp_to_host(video, (hs.x, hs.y));
                cap.followed_host((hs.x, hs.y));
            }
        }
    }

    /// What the cursor decisions read: the channel, the capture, SDL's pointer focus, the
    /// ring.
    fn cursor_inputs(
        &self,
        st: &StreamState,
        host: Option<punktfunk_core::quic::CursorState>,
    ) -> crate::cursor_model::Inputs {
        use crate::cursor_model::{CaptureFacts, HostCursor, Inputs};
        Inputs {
            negotiated: st.connector.is_some()
                && st.cursor_chan.as_ref().is_some_and(|ch| ch.negotiated()),
            host: host.map(|hs| HostCursor {
                relative_hint: hs.relative_hint(),
                visible: hs.visible(),
                pos: (hs.x, hs.y),
            }),
            capture: st.capture.as_ref().map(|cap| CaptureFacts {
                captured: cap.captured(),
                desktop: cap.desktop(),
                buttons_held: cap.buttons_held(),
                pointer: cap.grants() & punktfunk_core::quic::GRANT_POINTER != 0,
                abs_ok: cap.abs_ok(),
                last_abs: cap.last_abs(),
            }),
            over_us: self.mouse.focused_window_id() == Some(self.window.id()),
            ring_open: self.ring_was_open,
            video: st.last_video.is_some(),
        }
    }

    /// Put the local cursor on the host's pointer at frame pixel `pos`.
    fn warp_to_host(&self, video: (u32, u32), pos: (i32, i32)) {
        let (wx, wy) = content_to_window(
            self.opts.video_fit,
            self.window.size(),
            self.window.size_in_pixels(),
            video,
            pos.0,
            pos.1,
        );
        self.mouse.warp_mouse_in_window(&self.window, wx, wy);
    }

    /// SDL text input tracks overlay editing and the ring's Keyboard slot (IME / Steam
    /// OSK), edge-wise. A password field starts it as a hidden password, so an IME neither
    /// shows nor learns it ([`text_input_want`]).
    pub(super) fn text_input_tick(&mut self) {
        let overlay = self.overlay.as_ref();
        let want = text_input_want(
            self.ring_keyboard,
            overlay.is_some_and(|o| o.text_input_active()),
            overlay.is_some_and(|o| o.text_input_secure()),
            steam_keyboard,
        );
        if want == self.text_input_on {
            return;
        }
        let ti = self.sdl_video.text_input();
        if self.text_input_on.is_some() {
            ti.stop(&self.window);
        }
        self.text_input_on = want;
        match want {
            Some(false) => ti.start(&self.window),
            Some(true) => {
                let hidden = TextInputOptions {
                    input_type: Some(TextInputType::PasswordHidden),
                    autocorrect: Some(false),
                    multiline: Some(false),
                    ..TextInputOptions::default()
                };
                if let Err(e) = ti.start_with_options(&self.window, hidden) {
                    // Nothing types into the field but the console's tray.
                    tracing::warn!(error = %e, "password text input did not start");
                }
            }
            None => {}
        }
    }

    /// Who owns the pads: what their shortcuts fire, the ring, the mask. `want_mask_ui` is
    /// [`Shell::ui_wants_mask`] from before the pump tick.
    pub(super) fn pad_owner_tick(&mut self, stream: &mut Option<StreamState>, want_mask_ui: bool) {
        // The worker reads the pads' shortcuts before the mask: the exit chord still works
        // with input released or the ring up.
        while let Ok((pad, action, phase)) = self.shortcut_rx.try_recv() {
            self.fire_action(stream, Some(pad), &action, phase);
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
    }
}

/// SDL text input as it should stand: `Some(secure)` on, `None` off. The ring's Keyboard slot
/// types to the host and is never a password. SDL hands a field to Steam's keyboard with no
/// password mode, so a password starts none there and the console's tray types it.
fn text_input_want(
    ring: bool,
    editing: bool,
    secure: bool,
    steam: impl FnOnce() -> bool,
) -> Option<bool> {
    let secure = secure && !ring;
    ((ring || editing) && !(secure && steam())).then_some(secure)
}

/// SDL opens Steam's keyboard for text input: Steam sets this for a game it starts in Big
/// Picture or Game Mode.
fn steam_keyboard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        sdl3::hint::get("SDL_ENABLE_STEAM_SCREEN_KEYBOARD")
            .is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
    })
}

/// What [`route`] knows of an SDL event. A key without a scancode is the pump's; `chord` is a
/// first press that fired a keyboard shortcut.
fn kind_of(event: &Event, chord: bool) -> Kind {
    match event {
        Event::Quit { .. } => Kind::Quit,
        Event::Window { win_event, .. } => match win_event {
            WindowEvent::CloseRequested => Kind::Close,
            WindowEvent::FocusLost | WindowEvent::FocusGained => Kind::Focus,
            _ => Kind::Window,
        },
        Event::KeyDown {
            scancode: Some(_),
            repeat: true,
            ..
        } => Kind::KeyRepeat,
        Event::KeyDown {
            scancode: Some(sc), ..
        } => Kind::Key {
            chord,
            super_key: matches!(sc, Scancode::LGui | Scancode::RGui),
        },
        Event::KeyUp {
            scancode: Some(_), ..
        } => Kind::KeyUp,
        Event::MouseButtonDown { .. } | Event::MouseButtonUp { .. } => Kind::Button,
        Event::MouseMotion { .. } => Kind::Motion,
        Event::MouseWheel { .. } => Kind::Wheel,
        Event::FingerDown { .. } | Event::FingerMotion { .. } | Event::FingerUp { .. } => {
            Kind::Finger
        }
        _ => Kind::Other,
    }
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

/// Does this mouse move by compositor warps? Xwayland posts a warp with no relative part on
/// its absolute `xwayland-pointer:N`; a real mouse arrives on `xwayland-relative-pointer:N`.
/// gamescope warps for the Steam Frame's laser and touch-as-mouse, and SDL's relative mode
/// turns those positions into deltas that drift off the spot.
pub(super) fn warps_pointer(which: u32) -> bool {
    use sdl3::sys::mouse::{SDL_GetMouseNameForID, SDL_MouseID};
    // SAFETY: a query on an id SDL issued; the name is SDL's, read here and never kept, and
    // a null name is checked before use.
    let name = unsafe {
        let name = SDL_GetMouseNameForID(SDL_MouseID(which));
        if name.is_null() {
            return false;
        }
        std::ffi::CStr::from_ptr(name).to_bytes()
    };
    is_warp_device(name)
}

fn is_warp_device(name: &[u8]) -> bool {
    name.starts_with(b"xwayland-pointer:")
}

/// The real pointer in logical window coordinates: the global state asks the server.
pub(super) fn pointer_in_window(window: &sdl3::video::Window) -> (f32, f32) {
    let (mut gx, mut gy) = (0.0f32, 0.0f32);
    // SAFETY: SDL writes two floats through pointers to live locals.
    unsafe { sdl3::sys::mouse::SDL_GetGlobalMouseState(&mut gx, &mut gy) };
    let (wx, wy) = window.position();
    (gx - wx as f32, gy - wy as f32)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Only Xwayland's absolute pointer carries warps; its relative and gesture siblings
    /// and every real device keep the capture model's deltas.
    #[test]
    fn only_the_xwayland_absolute_pointer_is_placed() {
        assert!(is_warp_device(b"xwayland-pointer:13"));
        assert!(!is_warp_device(b"xwayland-relative-pointer:13"));
        assert!(!is_warp_device(b"xwayland-pointer-gestures:13"));
        assert!(!is_warp_device(b"Logitech USB Receiver"));
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

    /// A password field starts hidden text input, and none where Steam's keyboard would show
    /// it. The ring's slot stays plain text.
    #[test]
    fn a_password_starts_hidden_text_input_or_none_under_steam() {
        let steam = || true;
        let elsewhere = || false;
        assert_eq!(text_input_want(false, false, false, steam), None);
        assert_eq!(text_input_want(false, true, false, steam), Some(false));
        assert_eq!(text_input_want(false, true, true, elsewhere), Some(true));
        assert_eq!(text_input_want(false, true, true, steam), None);
        assert_eq!(text_input_want(true, false, true, steam), Some(false));
    }
}
