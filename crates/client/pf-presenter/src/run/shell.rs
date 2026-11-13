//! The window, presenter, overlay and pads the run loop owns.

use super::*;
use pf_client_core::overlay_actions::SlotId;
use pf_client_core::shortcuts::{Action, Device, Shortcuts};

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
        // The console draws an input method's text in progress in its field. Unclaimed,
        // Windows, XIM, IBus and Fcitx draw it in a window of their own and send none.
        sdl3::hint::set("SDL_IME_IMPLEMENTED_UI", "composition");
        // Wayland `app_id` and X11 WM_CLASS name the desktop entry a task manager groups and
        // pins this window under. The console has its own entry, so it carries that one's id.
        let app_id = if browse {
            "io.unom.punktfunk.console"
        } else {
            "io.unom.punktfunk"
        };
        sdl3::hint::set("SDL_APP_ID", app_id);
        // SDL picks XWayland where the compositor lacks `wp_fifo_v1` (KWin before 6.4, Mutter
        // before 48); the native lane and colour management need the Wayland surface. SDL
        // refuses the hint when the environment names a driver. Gamescope's socket is left to
        // SDL. Another compositor's inside gamescope, where the session's restart failed, never is.
        #[cfg(target_os = "linux")]
        if let Some(name) = pf_client_core::gamescope::foreign() {
            let handed = pf_client_core::gamescope::runtime_path(&name)
                .is_some_and(|socket| crate::wl_side::hand_sdl_display(&socket));
            let driver = if handed { "wayland" } else { "x11" };
            sdl3::hint::set("SDL_VIDEO_DRIVER", driver);
            tracing::info!(
                driver,
                "SDL moved to gamescope's display (WAYLAND_DISPLAY names another compositor)"
            );
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && !pf_client_core::gamescope::under_gamescope()
            && sdl3::hint::set("SDL_VIDEO_DRIVER", "wayland")
        {
            tracing::info!("SDL kept on Wayland (WAYLAND_DISPLAY is set)");
        }
        let knobs = crate::knobs::read();
        // `drm_card` → SDL's KMSDRM device index. SDL takes the first card it can open,
        // often the wrong one on a multi-GPU box. Detecting "already mastered" needs the
        // ioctl that taking master is, so this stays an explicit operator choice.
        if let Some(card) = knobs.drm_card.as_deref() {
            if card.chars().all(|c| c.is_ascii_digit()) && !card.is_empty() {
                tracing::info!(
                    card,
                    "PUNKTFUNK_DRM_CARD: pinning SDL's KMSDRM device index"
                );
                sdl3::hint::set("SDL_KMSDRM_DEVICE_INDEX", card);
            } else {
                tracing::warn!(
                    card,
                    "PUNKTFUNK_DRM_CARD must be a card NUMBER (e.g. 0) — ignoring"
                );
            }
        }
        let backend = crate::backend::choose(&knobs, browse);
        let sdl = sdl3::init().context("SDL init")?;
        let video = sdl.video().context("SDL video")?;
        let events = sdl.event().context("SDL events")?;
        events
            .register_custom_event::<FrameWake>()
            .map_err(|e| anyhow::anyhow!("register FrameWake event: {e}"))?;
        let window = {
            // Match-window: open at the persisted last size so the first connect's mode
            // matches the glass. 1280×720 is the fallback; a single stream then takes its
            // own size (`size_to_stream`).
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
            b.resizable().high_pixel_density();
            backend.flag_window(&mut b);
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
        let mut presenter = Presenter::new(
            backend,
            &window,
            crate::vk::PresentPref {
                vsync: opts.vsync,
                allow_vrr: opts.allow_vrr,
                fullscreen: opts.fullscreen,
                // `vrr_fifo_opt_in` and `fifo_latest_ready` are resolved inside `Presenter::new`.
                // `..Default` keeps this site from breaking when the struct learns another field.
                ..Default::default()
            },
            &knobs,
        )?;
        presenter.set_video_fit(opts.video_fit);
        // A valid black frame immediately — the window is honest while the connect runs.
        presenter.present(&window, FrameInput::Redraw, None)?;

        let arrival_override = knobs.presenter_arrival;
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

        let osd_scale_pref = knobs.osd_scale.unwrap_or(1.0);

        let mut overlay = opts.overlay.take();
        if let Some(o) = overlay.as_mut() {
            let init = match presenter.shared_device() {
                Some(shared) => o.init(&shared),
                None => Err(anyhow::anyhow!(
                    "console overlay needs the Vulkan presenter; stats stay on stdout"
                )),
            };
            if let Err(e) = init {
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
        let shortcut_rx = gamepad.shortcut_events();
        let (shortcuts, keys) =
            super::shortcuts::adopt(&opts.shortcuts, &gamepad, overlay.is_some());
        let menu_rx = gamepad.menu_events();
        if browse {
            // Menu mode for the launcher's lifetime (an attached session supersedes translation).
            gamepad.set_menu_mode(true);
        }
        #[cfg(target_os = "linux")]
        let overlay_focus = pf_client_core::overlay_focus::OverlayFocus::start();

        let native = native_mode_of(&window).unwrap_or(Mode {
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
            text_input_on: None,
            overlay_frame: None,
            stats_verbosity: opts.stats_verbosity,
            fullscreen: opts.fullscreen,
            pad_left_fullscreen: false,
            mouse,
            event_pump,
            native,
            focus_lost: false,
            mask_applied: false,
            #[cfg(target_os = "linux")]
            overlay_focus,
            menu_rx,
            shortcut_rx,
            shortcuts,
            keys,
            armed: None,
            audio_mute_at: Instant::now(),
            audio_mute_seen: 0,
            mic_muted_seen: false,
            mic_muted_at: None,
            ring_opener: None,
            ring_was_open: false,
            screen_held: true,
            pump,
            gamepad,
            overlay,
            osd_scale_pref,
            pacing_active: !arrival_override,
            knobs,
            backend,
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

    /// Resize the window to `picture`, the size the stream is shown at (its view, else its
    /// mode), within the display's usable area, before the stream starts. Match-window and
    /// fullscreen keep the size they own.
    pub(super) fn size_to_stream(&mut self, picture: (u32, u32)) {
        if self.fullscreen || self.opts.match_window {
            return;
        }
        let Ok(area) = self
            .window
            .get_display()
            .and_then(|d| d.get_usable_bounds())
        else {
            return;
        };
        // Windows and X11 report the frame; Wayland reports none.
        let frame = self.window.border_size().unwrap_or_default();
        let (w, h, x, y) = stream_window_rect(
            picture,
            self.window.pixel_density(),
            frame,
            area,
            self.opts.window_pos,
        );
        tracing::info!(w, h, "window sized to the stream");
        if self.window.set_size(w, h).is_ok() {
            use sdl3::video::WindowPos::Positioned;
            self.window.set_position(Positioned(x), Positioned(y));
        }
    }
}

/// Window size and client-area origin for a `stream`-pixel picture: its size at `density`
/// pixels per window unit, shrunk with the aspect kept until it and its `frame` (top, left,
/// bottom, right) fit `area`. Centred in `area`, or `pos` pulled back inside it.
fn stream_window_rect(
    stream: (u32, u32),
    density: f32,
    frame: (u16, u16, u16, u16),
    area: sdl3::rect::Rect,
    pos: Option<(i32, i32)>,
) -> (u32, u32, i32, i32) {
    let [top, left, bottom, right] = [frame.0, frame.1, frame.2, frame.3].map(i32::from);
    let (aw, ah) = (area.width() as i32, area.height() as i32);
    let density = if density.is_finite() && density > 0.0 {
        density
    } else {
        1.0
    };
    let (w, h) = (
        stream.0.max(1) as f32 / density,
        stream.1.max(1) as f32 / density,
    );
    let k = ((aw - left - right).max(1) as f32 / w)
        .min((ah - top - bottom).max(1) as f32 / h)
        .min(1.0);
    let (w, h) = (
        (w * k).round().max(1.0) as i32,
        (h * k).round().max(1.0) as i32,
    );
    let (lo_x, hi_x) = (area.x() + left, area.x() + aw - right - w);
    let (lo_y, hi_y) = (area.y() + top, area.y() + ah - bottom - h);
    let (x, y) = match pos {
        Some((x, y)) => (x.clamp(lo_x, hi_x.max(lo_x)), y.clamp(lo_y, hi_y.max(lo_y))),
        None => ((lo_x + hi_x) / 2, (lo_y + hi_y) / 2),
    };
    (w as u32, h as u32, x, y)
}

impl Shell {
    /// This pass's overlay: expire the access toast, settle an End game, hold or release the
    /// screen saver, gather the console's facts once and render its frame, then tell the
    /// damage tracker what changed. Browse needs the console, so a frame error is fatal there.
    pub(super) fn overlay_tick(&mut self, stream: &mut Option<StreamState>) -> Result<()> {
        if let Some(st) = stream.as_mut() {
            if st
                .session_notice
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(ACCESS_NOTICE_S))
            {
                st.session_notice = None;
            }
            if let Some(said) = st.host_notice.as_ref().and_then(|rx| rx.try_recv().ok()) {
                st.host_notice = None;
                st.session_notice = Some((said, Instant::now()));
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
        self.screen_saver_tick(stream.as_ref());
        if self.overlay.is_some() {
            let facts = self.overlay_facts(stream.as_ref());
            if let Some(o) = self.overlay.as_mut() {
                match o.frame(&facts.ctx()) {
                    Ok(f) => self.overlay_frame = f,
                    Err(e) => {
                        if self.browse {
                            return Err(e).context("console UI frame (required for --browse)");
                        }
                        tracing::warn!(error = %format!("{e:#}"),
                            "overlay frame failed — disabling the console UI");
                        // The overlay's drop frees images the last present may still sample.
                        self.presenter.wait_idle();
                        self.overlay = None;
                        self.overlay_frame = None;
                    }
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

    /// SDL holds the screen saver off while the window lives. A stream whose player turned
    /// "Keep screen awake" off gives it back once their input goes quiet, so the desktop's
    /// own saver runs over a game left alone. No stream holds.
    fn screen_saver_tick(&mut self, stream: Option<&StreamState>) {
        let hold = stream
            .and_then(|st| Some((st, st.connector.as_ref()?)))
            .is_none_or(|(st, c)| {
                punktfunk_core::client::holds_screen_awake(st.params.keep_awake, c.input_idle())
            });
        if hold != self.screen_held {
            self.screen_held = hold;
            let set = if hold {
                self.sdl_video.disable_screen_saver()
            } else {
                self.sdl_video.enable_screen_saver()
            };
            if let Err(e) = set {
                tracing::debug!(error = %e, hold, "screen saver hold not changed");
            }
        }
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

/// The window's display mode as a stream request: size, density and refresh. `None` for
/// a display that reports no mode or a zero size, which would stream a 0×0 request.
pub(super) fn native_mode_of(window: &sdl3::video::Window) -> Option<Mode> {
    window
        .get_display()
        .and_then(|d| d.get_mode())
        .ok()
        .map(|m| native_mode(m.w, m.h, m.pixel_density, m.refresh_rate))
        .filter(|m| m.width > 0 && m.height > 0)
}

/// Apply capture to the window: pointer lock (relative mouse + hidden cursor) and a
/// keyboard grab so system chords, Alt+F4 included, reach the host while captured. SDL
/// grabs per platform (low-level hook / shortcuts-inhibit / XGrabKeyboard); on Windows it
/// also closes the window on Alt+F4 under the grab unless told not to.
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
    let grab = crate::route::keyboard_grab(on, inhibit, grants & GRANT_KEYBOARD != 0);
    sdl3::hint::set("SDL_WINDOWS_CLOSE_ON_ALT_F4", if grab { "0" } else { "1" });
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

/// Overlay chrome UI scale: SDL's window display scale times `knobs.osd_scale`.
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

/// Capture hint (`ui_stream` parity — the words the user reads while released): one field per
/// keyboard shortcut that moves the capture or ends the stream, the pad's way out in place of
/// mouse mode and stats. An action with no key has no field.
pub(super) fn capture_hint(pad: bool, shortcuts: &Shortcuts) -> String {
    use pf_l10n::desktop as d;
    let key = |slot| shortcuts.hint(&Action::Slot(slot), Device::Keyboard);
    let mut fields = vec![pf_l10n::client::notice_capture_click().to_string()];
    fields.extend(key(SlotId::ReleaseInput).map(d::desktop_capture_release));
    if pad {
        fields.extend(key(SlotId::EndStream).map(d::desktop_capture_disconnect));
        fields.extend(crate::overlay::leave_line(shortcuts, Device::Pad));
    } else {
        fields.extend(key(SlotId::MouseMode).map(d::desktop_capture_mouse_mode));
        fields.extend(key(SlotId::EndStream).map(d::desktop_capture_disconnect));
        fields.extend(key(SlotId::Stats).map(d::desktop_capture_stats));
    }
    fields.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desktop's defaults read as the words the capture hint always had.
    #[test]
    fn the_default_capture_hint_names_the_default_chords() {
        use pf_l10n::{client as c, desktop as d};
        let s = pf_client_core::shortcuts::defaults(
            pf_client_core::shortcuts::Client::Desktop,
            false,
            "auto",
        );
        let pad = [
            c::notice_capture_click().to_string(),
            d::desktop_capture_release("Ctrl+Alt+Shift+Q"),
            d::desktop_capture_disconnect("Ctrl+Alt+Shift+D"),
            c::notice_exit_hold("L1 + R1 + Start + Select"),
        ];
        assert_eq!(capture_hint(true, &s), pad.join(" · "));
        let keys = [
            c::notice_capture_click().to_string(),
            d::desktop_capture_release("Ctrl+Alt+Shift+Q"),
            d::desktop_capture_mouse_mode("Ctrl+Alt+Shift+M"),
            d::desktop_capture_disconnect("Ctrl+Alt+Shift+D"),
            d::desktop_capture_stats("Ctrl+Alt+Shift+S"),
        ];
        assert_eq!(capture_hint(false, &s), keys.join(" · "));
        let leave = crate::overlay::leave_line(&s, Device::Keyboard);
        assert_eq!(leave, Some(c::notice_exit_keys("Ctrl+Alt+Shift+D")));
    }

    #[test]
    fn stream_window_fits_the_usable_area() {
        let area = sdl3::rect::Rect::new(0, 0, 1920, 1032);
        // 1080p on a 1080p panel with a taskbar and a 31 px title bar: shrunk, aspect kept,
        // the frame flush with the usable area's top and bottom.
        assert_eq!(
            stream_window_rect((1920, 1080), 1.0, (31, 1, 1, 1), area, None),
            (1778, 1000, 71, 31)
        );
        // Room to spare: the stream's own size, centred.
        let big = sdl3::rect::Rect::new(0, 0, 2560, 1400);
        assert_eq!(
            stream_window_rect((1920, 1080), 1.0, (0, 0, 0, 0), big, None),
            (1920, 1080, 320, 160)
        );
        // Wayland at 200 %: window units are points.
        assert_eq!(
            stream_window_rect((1920, 1080), 2.0, (0, 0, 0, 0), big, None).0,
            960
        );
        // A shell's spot that would spill off the right edge is pulled back inside.
        let (w, _, x, y) =
            stream_window_rect((1920, 1080), 1.0, (0, 0, 0, 0), big, Some((900, 50)));
        assert_eq!((x + w as i32, y), (2560, 50));
        // A bogus density or a zero mode never yields a zero window.
        let (w, h, ..) = stream_window_rect((0, 0), f32::NAN, (0, 0, 0, 0), big, None);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn overlay_scale_follows_dpi_and_survives_a_bogus_display() {
        assert_eq!(overlay_scale(1.0, 1.0), 1.0);
        assert_eq!(overlay_scale(1.5, 1.0), 1.5);
        assert_eq!(overlay_scale(2.0, 1.0), 2.0);
        // `knobs.osd_scale` multiplies the display's own scale, it does not replace it.
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
