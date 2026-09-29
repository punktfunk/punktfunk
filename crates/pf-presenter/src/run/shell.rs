//! The window, presenter, overlay and pads the run loop owns.

use super::stream::ring_facts;
use super::*;

impl Shell {
    /// SDL, the window and its presenter, the overlay and the pads: everything the loop
    /// owns across streams. Browse needs the console, so its init failure is fatal there.
    pub(super) fn open(mut opts: SessionOpts, browse: bool) -> Result<Shell> {
        // Before any window exists: unpackaged runs adopt the shell's AppUserModelID so
        // shell⇄session windows group as one taskbar app (MSIX identity wins).
        #[cfg(windows)]
        crate::win32::set_app_user_model_id();
        // This thread presents and forwards input; a late wake is a missed refresh.
        pf_client_core::audio_rt::boost_and_log("presenter");
        sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
        // Hold Valve HIDAPI off before SDL_Init: the Deck driver clears digital mappings
        // at enumeration. A hint set after `sdl.gamepad()` only detaches a driver that
        // already killed the trackpad-mouse. They are still enabled for an attached session.
        pf_client_core::gamepad::preinit_disable_valve_hidapi();
        // Touch is forwarded as real touch below. Left on, SDL's mouse-from-touch synthesis
        // warps a synthetic mouse; under relative lock that is a large positive delta that
        // walks the host cursor into the corner.
        sdl3::hint::set("SDL_TOUCH_MOUSE_EVENTS", "0");
        // The keyboard grab exists only while shortcut capture is on, and then Alt+Tab
        // belongs to the host. SDL's default minimizes a grabbed fullscreen window on it.
        sdl3::hint::set("SDL_ALLOW_ALT_TAB_WHILE_GRABBED", "0");
        // Wayland `app_id` (and X11 WM_CLASS) so compositors match io.unom.Punktfunk.desktop.
        // Without it SDL uses a generic identity and the session window gets the default icon.
        sdl3::hint::set("SDL_APP_ID", "io.unom.Punktfunk");
        // `PUNKTFUNK_DRM_CARD=<n>` → SDL's KMSDRM device index. SDL takes the first card
        // it can open, often the wrong one on a multi-GPU box. Detecting "already mastered"
        // needs the ioctl that taking master is, so this stays an explicit operator choice.
        if let Ok(card) = std::env::var("PUNKTFUNK_DRM_CARD") {
            if card.chars().all(|c| c.is_ascii_digit()) && !card.is_empty() {
                tracing::info!(
                    card,
                    "PUNKTFUNK_DRM_CARD: pinning SDL's KMSDRM device index"
                );
                sdl3::hint::set("SDL_KMSDRM_DEVICE_INDEX", &card);
            } else {
                tracing::warn!(
                    card,
                    "PUNKTFUNK_DRM_CARD must be a card NUMBER (e.g. 0) — ignoring"
                );
            }
        }
        let sdl = sdl3::init().context("SDL init")?;
        let video = sdl.video().context("SDL video")?;
        let events = sdl.event().context("SDL events")?;
        events
            .register_custom_event::<FrameWake>()
            .map_err(|e| anyhow::anyhow!("register FrameWake event: {e}"))?;
        let window = {
            // Match-window: open at the persisted last size so the first connect's mode
            // matches the glass. 1280×720 is the fallback.
            let (ww, wh) = opts.window_size.unwrap_or((1280, 720));
            let mut b = video.window(&opts.window_title, ww.max(320), wh.max(200));
            match opts.window_pos {
                Some((x, y)) => b.position(x, y),
                None => b.position_centered(),
            };
            // HIGH_PIXEL_DENSITY: backbuffer in the panel's real pixels. Without it SDL
            // leaves a Wayland surface at buffer scale 1, so a fractionally scaled output
            // builds the swapchain in points. The flag only widens `size_in_pixels()`;
            // `size()` stays logical (persisted size and SDL mouse coords).
            b.resizable().vulkan().high_pixel_density();
            if opts.fullscreen {
                b.fullscreen();
            }
            b.build().context("SDL window")?
        };
        // SDL wheel input remains the fallback when native Wayland capture is unavailable.
        let scroll_routing = crate::scroll_routing::ScrollRouting::new(&window);
        // Exe-embedded icon onto the title bar/taskbar; a no-op for exes that embed none.
        #[cfg(windows)]
        crate::win32::stamp_window_icon(&window);
        let instance_exts = window
            .vulkan_instance_extensions()
            .map_err(|e| anyhow::anyhow!("vulkan instance extensions: {e}"))?;
        let mut presenter = Presenter::new(
            &window,
            &instance_exts,
            crate::vk::PresentPref {
                vsync: opts.vsync,
                allow_vrr: opts.allow_vrr,
                fullscreen: opts.fullscreen,
                // `vrr_fifo_opt_in` and `fifo_latest_ready` are resolved inside `Presenter::new`.
                // `..Default` keeps this site from breaking when the struct learns another field.
                ..Default::default()
            },
        )
        .context("vulkan presenter")?;
        presenter.set_video_fit(opts.video_fit);
        // A valid black frame immediately — the window is honest while the connect runs.
        presenter.present(&window, FrameInput::Redraw, None)?;

        // `PUNKTFUNK_PRESENTER=arrival` forces the latency drain without a rebuild.
        let arrival_override =
            std::env::var("PUNKTFUNK_PRESENTER").ok().as_deref() == Some("arrival");
        let present_priority = if arrival_override {
            tracing::info!("PUNKTFUNK_PRESENTER=arrival — presentation pacing disabled");
            PresentPriority::Latency
        } else {
            opts.present_priority
        };
        // Present completions wake the loop like decoded frames: a glass-gate reopen or a
        // smoothness slot must not wait out the event timeout.
        {
            let sender = events.event_sender();
            presenter.set_present_wake(Box::new(move || {
                let _ = sender.push_custom_event(FrameWake);
            }));
        }
        // Browse is "ready" the moment the library window presents — there may never be a
        // stream. Single mode announces on the first video frame instead.
        if opts.json_status && browse {
            emit(SessionLine::Ready);
        }

        let osd_scale_pref = std::env::var("PUNKTFUNK_OSD_SCALE")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(1.0);

        let mut overlay = opts.overlay.take();
        if let Some(o) = overlay.as_mut() {
            if let Err(e) = o.init(&presenter.shared_device()) {
                if browse {
                    return Err(e).context("console UI init (required for --browse)");
                }
                tracing::warn!(error = %format!("{e:#}"),
                    "console-UI overlay init failed — continuing without it");
                overlay = None;
            }
        }

        let gamepad_subsystem = sdl.gamepad().context("SDL gamepad")?;
        let (gamepad, pump) = GamepadService::pumped(gamepad_subsystem);
        let escape_rx = gamepad.escape_events();
        let chord_rx = gamepad.chord_events();
        // A Select chord eats the button pressed with Select, so it is only worth claiming where
        // the overlay it drives exists — a build without the console UI leaves A and X to the game.
        gamepad.set_chords_live(overlay.is_some());
        let disconnect_rx = gamepad.disconnect_events();
        let menu_rx = gamepad.menu_events();
        if browse {
            // Menu mode for the launcher's lifetime (an attached session supersedes translation).
            gamepad.set_menu_mode(true);
        }
        #[cfg(target_os = "linux")]
        let overlay_focus = pf_client_core::overlay_focus::OverlayFocus::start();

        let native = window
            .get_display()
            .and_then(|d| d.get_mode())
            .map(|m| native_mode(m.w, m.h, m.pixel_density, m.refresh_rate))
            .ok()
            // A zero-sized mode is as useless as no mode. Without this filter a display
            // that reports 0×0 streams a 0×0 request.
            .filter(|m: &Mode| m.width > 0 && m.height > 0)
            .unwrap_or(Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            });

        let event_pump = sdl
            .event_pump()
            .map_err(|e| anyhow::anyhow!("SDL event pump: {e}"))?;
        let mouse = sdl.mouse();
        Ok(Shell {
            overlay_damage: OverlayDamage::default(),
            ring_keyboard: false,
            text_input_on: false,
            overlay_frame: None,
            stats_verbosity: opts.stats_verbosity,
            fullscreen: opts.fullscreen,
            mouse,
            event_pump,
            native,
            focus_lost: false,
            mask_applied: false,
            #[cfg(target_os = "linux")]
            overlay_focus,
            menu_rx,
            disconnect_rx,
            audio_mute_at: Instant::now(),
            audio_mute_seen: 0,
            ring_opener: None,
            ring_was_open: false,
            chord_rx,
            escape_rx,
            pump,
            gamepad,
            overlay,
            osd_scale_pref,
            pacing_active: !arrival_override,
            present_priority,
            presenter,
            scroll_routing,
            window,
            sdl_events: events,
            sdl_video: video,
            _sdl: sdl,
            opts,
            browse,
            pad_absence: PadAbsence::default(),
        })
    }

    /// Lock and grab for an engaged capture, in its mouse model and within its grants.
    pub(super) fn capture_on(&mut self, cap: &Capture) {
        let inhibit = self.opts.inhibit_shortcuts;
        apply_capture(
            &mut self.window,
            &self.mouse,
            true,
            cap.desktop(),
            inhibit,
            cap.grants(),
        );
    }

    /// Hand the pointer and the keyboard back to the desktop.
    pub(super) fn capture_off(&mut self) {
        let inhibit = self.opts.inhibit_shortcuts;
        apply_capture(&mut self.window, &self.mouse, false, false, inhibit, 0);
    }
}

impl Shell {
    /// This pass's overlay: expire the access toast, render the console over the frame
    /// context, and tell the damage tracker what changed. Browse needs the console, so a
    /// frame error is fatal there.
    pub(super) fn overlay_tick(&mut self, stream: &mut Option<StreamState>) -> Result<()> {
        if let Some(st) = stream.as_mut() {
            if st
                .session_notice
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(ACCESS_NOTICE_S))
            {
                st.session_notice = None;
            }
            // The game is gone (or already was): leave as End stream does. Anything else
            // keeps the stream and says why.
            let answer = st
                .ending_game
                .as_ref()
                .and_then(|(title, rx)| rx.try_recv().ok().map(|a| (title.clone(), a)));
            if let Some((title, answer)) = answer {
                st.ending_game = None;
                pf_client_core::library::invalidate_running(&st.fp_hex);
                use pf_client_core::library::GameEnd;
                if matches!(answer, GameEnd::Ended | GameEnd::NotRunning) {
                    st.request_quit();
                    self.capture_off();
                } else {
                    st.session_notice = Some((answer.notice(&title), Instant::now()));
                }
            }
        }

        if let Some(o) = self.overlay.as_mut() {
            let (pw, ph) = self.window.size_in_pixels();
            let (stats, hint) = match &stream {
                Some(st) if st.connector.is_some() => {
                    // No "click to capture" over a session with nothing to capture for, or
                    // while another window has focus: the hint returns with the focus.
                    let hint = match &st.capture {
                        Some(cap) if !cap.captured() && cap.can_capture() && !self.focus_lost => {
                            Some(if self.gamepad.active().is_some() {
                                HINT_WITH_PAD
                            } else {
                                HINT_KEYBOARD
                            })
                        }
                        _ => None,
                    };
                    (
                        (self.stats_verbosity != StatsVerbosity::Off && !st.osd.is_empty())
                            .then_some(st.osd.as_slice()),
                        hint,
                    )
                }
                _ => (None, None),
            };
            // Access chip: a standing pill in the stats overlay family. A pill that never
            // goes away is chrome, so it rides the stats tier. `None` for a full-control
            // permanent session — what a host that never sent access looks like.
            let access_chip = match &stream {
                Some(st)
                    if st.connector.is_some() && self.stats_verbosity != StatsVerbosity::Off =>
                {
                    st.access.chip_text(Instant::now())
                }
                _ => None,
            };
            let session_notice = stream
                .as_ref()
                .filter(|st| st.connector.is_some())
                .and_then(|st| st.session_notice.as_ref().map(|(n, _)| n.as_str()));
            let pad = self.gamepad.active();
            let pads = self.gamepad.pads();
            let resizing = stream
                .as_ref()
                .is_some_and(|st| st.connector.is_some() && st.resize_overlay.active());
            // Read live from the session's control rather than mirrored into StreamState:
            // the pump knows whether an uplink exists, and a mirrored copy would go stale
            // at session end.
            let mic_muted = stream.as_ref().is_some_and(|st| st.handle.mic.muted());
            // The badge clears itself once a local mute has been read; the mask's own clock
            // lives here because only the frame loop knows when it last moved.
            let audio_mute = stream
                .as_ref()
                .and_then(|st| st.connector.as_ref())
                .and_then(|c| {
                    let mask = c.audio_mute();
                    if mask != self.audio_mute_seen {
                        self.audio_mute_seen = mask;
                        self.audio_mute_at = Instant::now();
                    }
                    punktfunk_core::client::audio_mute_notice(mask, self.audio_mute_at.elapsed())
                });
            let ring_facts = stream
                .as_ref()
                .filter(|st| st.connector.is_some())
                .map(|st| {
                    ring_facts(
                        st,
                        &self.opts,
                        self.stats_verbosity,
                        mic_muted,
                        self.ring_opener,
                    )
                });
            let ctx = FrameCtx {
                width: pw,
                height: ph,
                ten_bit: self.presenter.ten_bit(),
                // Re-read per frame: dragging to a second monitor with a different scale
                // updates this.
                scale: overlay_scale(self.window.display_scale(), self.osd_scale_pref),
                stats,
                stats_corner: stream
                    .as_ref()
                    .map_or(punktfunk_core::hud::HudCorner::TopLeft, |st| {
                        st.params.stats_corner
                    }),
                stats_scale: stream.as_ref().map_or(1.0, |st| st.params.stats_scale),
                exit_hint: stream.as_ref().is_some_and(|st| st.params.exit_hint),
                hint,
                access: access_chip.as_deref(),
                notice: session_notice,
                mic_muted,
                audio_mute,
                resizing,
                pad: pad.as_ref().map(|p| p.name.as_str()),
                pad_pref: pad.as_ref().map(|p| p.pref),
                pads: &pads,
                ring: ring_facts.as_ref(),
            };
            match o.frame(&ctx) {
                Ok(f) => self.overlay_frame = f,
                Err(e) => {
                    if self.browse {
                        return Err(e).context("console UI frame (required for --browse)");
                    }
                    tracing::warn!(error = %format!("{e:#}"),
                        "overlay frame failed — disabling the console UI");
                    self.overlay = None;
                    self.overlay_frame = None;
                }
            }
        }
        self.overlay_damage
            .rendered(self.overlay_frame.as_ref().map(|f| f.image));
        // The native lane shows the overlay on its own surface; the swapchain path draws it.
        self.presenter
            .sync_native_overlay(self.overlay_frame.as_ref(), self.window.size());
        Ok(())
    }
}

impl Shell {
    /// Present the overlay alone when no video frame carried it: every pass across a
    /// resize scrim (the host's rebuild gap), and once per change while browsing or
    /// after a mid-stream picture has gone still. An idle console hands back the same
    /// image, so browsing presents only what the overlay re-rendered.
    pub(super) fn present_overlay_alone(
        &mut self,
        stream: &Option<StreamState>,
        presented_video: bool,
    ) -> Result<()> {
        let resize_scrim = stream.as_ref().is_some_and(|s| s.resize_overlay.active());
        let browse_idle = self.browse && stream.as_ref().is_none_or(|s| s.connector.is_none());
        let still_picture = stream.as_ref().is_some_and(|s| s.last_video.is_some())
            && self.overlay_damage.take_due(Instant::now());
        let browse_changed = browse_idle && self.overlay_damage.take_dirty();
        if !presented_video && (resize_scrim || browse_changed || still_picture) {
            // The UI owns the screen: hand the swapchain back to SDR. A finished PQ stream
            // leaves HDR10 live, and UI presents carry no frame. Not applied to
            // `resize_scrim`: that gap is still an HDR session, and flipping would rebuild
            // the swapchain twice.
            if browse_idle {
                self.presenter.leave_hdr(&self.window)?;
            }
            self.presenter.present(
                &self.window,
                FrameInput::Redraw,
                self.overlay_frame.as_ref(),
            )?;
        }
        Ok(())
    }
}

/// Apply capture to the window: pointer lock (relative mouse + hidden cursor) and a
/// keyboard grab so system chords reach the host while captured. SDL implements the
/// grab per platform (low-level hook / shortcuts-inhibit / XGrabKeyboard).
///
/// `inhibit` is [`Settings::inhibit_shortcuts`] — off leaves system chords with the
/// local shell. It only ever *removes* a grab: releasing input always hands chords back.
///
/// The `desktop` mouse model never locks: the pointer roams freely and the local cursor
/// is hidden over the window. The keyboard grab follows `inhibit` in both models —
/// desktop mode's unlocked pointer clicking another window is the way back.
/// `desktop` only matters while `on`.
///
/// `grants`: no pointer lock without POINTER, no keyboard grab without KEYBOARD.
/// On-sites pass `Capture::grants()`; off-sites pass `0`.
pub(super) fn apply_capture(
    window: &mut sdl3::video::Window,
    mouse: &sdl3::mouse::MouseUtil,
    on: bool,
    desktop: bool,
    inhibit: bool,
    grants: u32,
) {
    use punktfunk_core::quic::{GRANT_KEYBOARD, GRANT_POINTER};
    let pointer = grants & GRANT_POINTER != 0;
    mouse.set_relative_mouse_mode(window, on && !desktop && pointer);
    // The local cursor hides only while the host's cursor stands in for it — without
    // POINTER no send lands, so hiding it would leave a keyboard-only session with no cursor.
    mouse.show_cursor(!(on && pointer));
    let grab = on && inhibit && grants & GRANT_KEYBOARD != 0;
    if !window.set_keyboard_grab(grab) && grab {
        // The one refusal SDL reports is a missing mechanism. Said once per process: the
        // answer never changes mid-session. Under gamescope that is expected (it has no
        // shortcuts of its own) so it stays at debug rather than warning once per stream.
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            let err = sdl3::get_error();
            if pf_client_core::gamescope::under_gamescope() {
                tracing::debug!(error = %err, "no keyboard grab under gamescope — chords already ours");
            } else {
                tracing::warn!(
                    error = %err,
                    "capture system shortcuts is on, but this compositor offers no way to grab \
                     the keyboard — system chords stay with the local shell"
                );
            }
        }
    }
}

/// Overlay chrome UI scale: SDL's window display scale times `PUNKTFUNK_OSD_SCALE`.
///
/// `SDL_GetWindowDisplayScale` returns `0.0` when it cannot resolve the display; a 0
/// multiplier would collapse the OSD to an invisible panel. The 4× ceiling keeps a
/// bogus scale from covering the stream.
pub(super) fn overlay_scale(display_scale: f32, pref: f32) -> f32 {
    let base = if display_scale.is_finite() && display_scale > 0.0 {
        display_scale
    } else {
        1.0
    };
    let pref = if pref.is_finite() && pref > 0.0 {
        pref
    } else {
        1.0
    };
    (base * pref).clamp(0.5, 4.0)
}

/// How long an access toast holds the pill slot. The chip keeps the standing truth.
pub(super) const ACCESS_NOTICE_S: u64 = 6;

/// Capture hints (`ui_stream` parity — the words the user reads while released).
pub(super) const HINT_KEYBOARD: &str =
    "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+M mouse mode · Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats";
pub(super) const HINT_WITH_PAD: &str =
    "Click the stream to capture input · Ctrl+Alt+Shift+Q releases · \
     Ctrl+Alt+Shift+D disconnects · hold L1 + R1 + Start + Select to leave";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_scale_follows_dpi_and_survives_a_bogus_display() {
        assert_eq!(overlay_scale(1.0, 1.0), 1.0);
        assert_eq!(overlay_scale(1.5, 1.0), 1.5);
        assert_eq!(overlay_scale(2.0, 1.0), 2.0);
        // PUNKTFUNK_OSD_SCALE multiplies the display's own scale, it does not replace it.
        assert_eq!(overlay_scale(2.0, 1.25), 2.5);
        // SDL reports 0.0 when it cannot resolve the window's display — must not collapse
        // the panel to nothing.
        assert_eq!(overlay_scale(0.0, 1.0), 1.0);
        assert_eq!(overlay_scale(f32::NAN, 1.0), 1.0);
        assert_eq!(overlay_scale(-2.0, 1.0), 1.0);
        // A garbage preference degrades to "just the DPI", never to zero.
        assert_eq!(overlay_scale(1.5, 0.0), 1.5);
        assert_eq!(overlay_scale(1.5, f32::NAN), 1.5);
        // Clamped both ways so nothing can hide the OSD or bury the stream under it.
        assert_eq!(overlay_scale(1.0, 100.0), 4.0);
        assert_eq!(overlay_scale(1.0, 0.01), 0.5);
    }
}
