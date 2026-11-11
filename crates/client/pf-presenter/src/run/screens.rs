//! Screens past the first: a borderless fullscreen window each, on the client monitor it
//! stands for, with a presenter and a decoder of its own. The host draws the pointer into
//! their pictures, so the local one hides over them.

use super::events::finger_to_frame;
use super::*;
use crate::touch::Abs;
use pf_client_core::session::ScreenDecode;
use punktfunk_core::quic::v2::msg::{Screen, SCREENS_MAX};
use sdl3::rect::Rect;
use sdl3::video::{Display, Window};

/// The client's monitors as the stream's screens: the window's display first at `mode`, each
/// other at its own native mode, scaled as [`scaled_ask`] says. Empty when SDL cannot say.
pub(super) fn client_screens(
    video: &sdl3::VideoSubsystem,
    window: &Window,
    mode: Mode,
    (render_scale, max_dim): (f64, u32),
) -> Vec<(Screen, Display)> {
    let (Ok(first), Ok(displays)) = (window.get_display(), video.displays()) else {
        return Vec::new();
    };
    let Ok(origin) = first.get_bounds() else {
        return Vec::new();
    };
    let primary = video.get_primary_display().ok();
    let mut out = vec![(
        Screen {
            mode,
            primary: primary == Some(first),
            ..Default::default()
        },
        first,
    )];
    for d in displays.into_iter().filter(|d| *d != first) {
        let (Ok(bounds), Ok(m)) = (d.get_bounds(), d.get_mode()) else {
            continue;
        };
        if out.len() == SCREENS_MAX {
            break;
        }
        let native = native_mode(m.w, m.h, m.pixel_density, m.refresh_rate);
        let (own, view) = scaled_ask(native, render_scale, max_dim);
        let (x, y) = offset_px(origin, bounds, mode, own);
        out.push((
            Screen {
                index: out.len() as u8,
                mode: own,
                x,
                y,
                primary: primary == Some(d),
                view,
                ..Default::default()
            },
            d,
        ));
    }
    out
}

/// A further monitor's mode at `native` and the view beside it: above 1x its pixels × the render
/// scale, shown at its own, as screen 0's are. At 1x and below, its own pixels and no view.
fn scaled_ask(native: Mode, render_scale: f64, max_dim: u32) -> (Mode, Option<(u32, u32)>) {
    match super::stream::window_mode(native.width, native.height, render_scale, max_dim) {
        ((width, height), Some(view)) => (
            Mode {
                width,
                height,
                ..native
            },
            Some(view),
        ),
        _ => (native, None),
    }
}

/// Where `at` sits from `origin`, both in desktop coordinates, in the pixels of `mode` shown
/// on `origin`: the first screen's picture is the unit the host places the others in. A
/// monitor wholly left of or above the first ends at its scaled far edge and reaches back by
/// `own`, its own mode, so a density step between the two leaves no gap.
fn offset_px(origin: Rect, at: Rect, mode: Mode, own: Mode) -> (i32, i32) {
    let px = |d: i32, pixels: u32, span: u32| {
        (i64::from(d) * i64::from(pixels) / i64::from(span.max(1))) as i32
    };
    let x = if at.right() <= origin.x() {
        px(at.right() - origin.x(), mode.width, origin.width()) - own.width as i32
    } else {
        px(at.x() - origin.x(), mode.width, origin.width())
    };
    let y = if at.bottom() <= origin.y() {
        px(at.bottom() - origin.y(), mode.height, origin.height()) - own.height as i32
    } else {
        px(at.y() - origin.y(), mode.height, origin.height())
    };
    (x, y)
}

/// What a present did with a screen's picture; a busy one comes back owned.
enum Took {
    Shown,
    Stale,
    Busy(Option<DecodedImage>),
}

fn took(presented: Presented<'_>) -> Took {
    match presented {
        Presented::Shown => Took::Shown,
        Presented::Stale => Took::Stale,
        Presented::Busy(input, _) => Took::Busy(match input {
            #[cfg(target_os = "linux")]
            FrameInput::Dmabuf(d) => Some(DecodedImage::NativeDmabuf(d)),
            #[cfg(windows)]
            FrameInput::D3d11(d) => Some(DecodedImage::D3d11(d)),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            FrameInput::PyroWave(f) => Some(DecodedImage::PyroWave(f)),
            FrameInput::NativeVk(v) => Some(DecodedImage::NativeVk(v)),
            FrameInput::Cpu(_) | FrameInput::Redraw => None,
        }),
    }
}

/// One further screen's window. Field order is drop order: the decode, which submits to the
/// presenter's device, then the presenter, then its window.
struct ScreenWindow {
    index: u8,
    decode: ScreenDecode,
    /// The newest picture, kept while the swapchain has no image for it.
    held: Option<DecodedFrame>,
    /// A present failed: said once, then the window keeps trying quietly.
    failing: bool,
    /// The newest picture's size: absolute input over the window maps onto it.
    video: Option<(u32, u32)>,
    presenter: Presenter,
    window: Window,
}

impl ScreenWindow {
    #[allow(clippy::too_many_arguments)]
    fn open(
        video: &sdl3::VideoSubsystem,
        display: Display,
        index: u8,
        connector: &Arc<NativeClient>,
        decoder: &str,
        opts: &SessionOpts,
        knobs: &crate::knobs::Knobs,
        backend: crate::backend::Backend,
    ) -> Result<ScreenWindow> {
        let b = display
            .get_bounds()
            .map_err(|e| anyhow::anyhow!("display bounds: {e}"))?;
        let title = format!("{} · {}", opts.window_title, index + 1);
        let mut builder = video.window(&title, b.width(), b.height());
        // SDL finds a fullscreen window's display from its position, which on Wayland is the
        // output the surface last entered; a wrong one minimizes that display's other fullscreen
        // window. A display-centred position pins `display` for the window's life.
        let at = sdl3::sys::video::SDL_WINDOWPOS_CENTERED_DISPLAY(display.to_ll());
        builder
            .position(at, at)
            .borderless()
            .high_pixel_density()
            .fullscreen();
        backend.flag_window(&mut builder);
        // Never the foreground window: the stream window keeps focus and the input grab, and a
        // click here moves no focus. Windows maps this to WS_EX_NOACTIVATE; Wayland ignores it.
        let flags = builder.flags() | sdl3::video::WindowFlags::NOT_FOCUSABLE;
        builder.set_flags(flags);
        let window = builder.build().context("SDL window")?;
        let mut presenter = Presenter::new(
            backend,
            &window,
            crate::vk::PresentPref {
                vsync: opts.vsync,
                fullscreen: true,
                ..Default::default()
            },
            knobs,
        )?;
        presenter.set_video_fit(opts.video_fit);
        presenter.present(&window, FrameInput::Redraw, None)?;
        let decode = session::start_screen(
            connector.clone(),
            index,
            decoder.to_owned(),
            presenter.vulkan_decode(),
        );
        tracing::info!(
            screen = index,
            w = b.width(),
            h = b.height(),
            "screen window open"
        );
        Ok(ScreenWindow {
            index,
            decode,
            held: None,
            failing: false,
            video: None,
            presenter,
            window,
        })
    }

    /// Show the newest picture, or the one the swapchain had no image for last time.
    fn present(&mut self) -> Result<()> {
        while let Ok(frame) = self.decode.frames.try_recv() {
            self.held = Some(frame);
        }
        let Some(frame) = self.held.take() else {
            return Ok(());
        };
        self.video = Some(frame.image.dimensions());
        let DecodedFrame {
            pts_ns,
            decoded_ns,
            repeat,
            image,
        } = frame;
        let (p, w) = (&mut self.presenter, &self.window);
        let took = match image {
            DecodedImage::Cpu(c) => match took(p.present(w, FrameInput::Cpu(&c), None)?) {
                Took::Busy(_) => Took::Busy(Some(DecodedImage::Cpu(c))),
                t => t,
            },
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(d) => took(p.present(w, FrameInput::Dmabuf(d), None)?),
            #[cfg(windows)]
            DecodedImage::D3d11(d) => took(p.present(w, FrameInput::D3d11(d), None)?),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => took(p.present(w, FrameInput::PyroWave(f), None)?),
            DecodedImage::NativeVk(v) => took(p.present(w, FrameInput::NativeVk(v), None)?),
        };
        match took {
            // The swapchain had no image for it: it goes again next pass.
            Took::Busy(Some(image)) => {
                self.held = Some(DecodedFrame {
                    pts_ns,
                    decoded_ns,
                    repeat,
                    image,
                })
            }
            Took::Stale => self.presenter.recreate_swapchain(&self.window)?,
            Took::Shown | Took::Busy(None) => {}
        }
        Ok(())
    }

    fn on_window_event(&mut self, event: WindowEvent) -> Result<()> {
        match event {
            WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..) => {
                self.presenter.recreate_swapchain(&self.window)?;
                self.presenter
                    .present(&self.window, FrameInput::Redraw, None)?;
            }
            WindowEvent::Exposed => {
                self.presenter
                    .present(&self.window, FrameInput::Redraw, None)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// A present failure leaves this window's last picture up; the stream goes on.
    fn note(&mut self, done: Result<()>) {
        match done {
            Ok(()) => self.failing = false,
            Err(e) if !self.failing => {
                self.failing = true;
                tracing::warn!(screen = self.index, error = %format!("{e:#}"), "screen window present failed");
            }
            Err(_) => {}
        }
    }
}

impl Drop for ScreenWindow {
    /// As [`StreamState::shutdown`]: let go of the picture, join the decode, then idle.
    fn drop(&mut self) {
        self.held = None;
        self.presenter.drop_video();
        self.decode.close();
        self.presenter.wait_idle();
    }
}

/// The stream's further screens: the monitor each asked one stands for, and a window for each
/// the host built.
#[derive(Default)]
pub(super) struct ScreenWindows {
    displays: Vec<(Screen, Display)>,
    /// Monitors whose window the player closed: no screen of this stream stands for them again.
    closed: Vec<Display>,
    windows: Vec<ScreenWindow>,
    /// The host layout the windows follow, or the one current at the last ask: windows open
    /// only from a newer one. `0` before any.
    layout_gen: u32,
}

/// `planned` past the monitors in `closed`, each screen keeping its index. Screen 0 is the
/// stream window's monitor and always stays.
fn open_ones<D: Copy + PartialEq>(planned: &[(Screen, D)], closed: &[D]) -> Vec<(Screen, D)> {
    planned
        .iter()
        .filter(|(s, d)| s.index == 0 || !closed.contains(d))
        .copied()
        .collect()
}

impl ScreenWindows {
    /// Keep `planned`'s monitors, past those the player closed, for the screens it asks; the ask
    /// itself, for the host. Every window closes, to reopen from the first host layout newer
    /// than `layout_gen`: the one current at this ask answers an older ask.
    pub(super) fn plan(&mut self, planned: &[(Screen, Display)], layout_gen: u32) -> Vec<Screen> {
        self.displays = open_ones(planned, &self.closed);
        self.windows.clear();
        self.layout_gen = layout_gen;
        self.displays.iter().map(|(s, _)| *s).collect()
    }

    /// The monitor screen 0 stands for in the current plan.
    pub(super) fn first_display(&self) -> Option<Display> {
        self.displays
            .iter()
            .find(|(s, _)| s.index == 0)
            .map(|&(_, d)| d)
    }

    /// Layout generation `newest` is one the windows have not followed yet; it is theirs now.
    fn follow(&mut self, newest: u32) -> bool {
        let new = newest != self.layout_gen;
        self.layout_gen = newest;
        new
    }

    /// The player closed screen `screen`'s window: it goes now and its monitor stays out of this
    /// stream. The ask for the screens left, screen 0 first.
    pub(super) fn close(&mut self, screen: u8) -> Vec<Screen> {
        if let Some(&(_, d)) = self.displays.iter().find(|(s, _)| s.index == screen) {
            self.closed.push(d);
        }
        self.displays.retain(|(s, _)| s.index != screen);
        self.windows.retain(|w| w.index != screen);
        self.displays.iter().map(|(s, _)| *s).collect()
    }

    /// The screen whose window `window_id` is.
    pub(super) fn screen_of(&self, window_id: u32) -> Option<u8> {
        self.windows
            .iter()
            .find(|w| w.window.id() == window_id)
            .map(|w| w.index)
    }

    /// A window event of screen `screen`'s window.
    pub(super) fn on_window_event(&mut self, screen: u8, event: WindowEvent) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.index == screen) {
            let done = w.on_window_event(event);
            w.note(done);
        }
    }

    /// Window position `(x, y)` over screen `screen`'s window as a point of its picture.
    pub(super) fn abs_at(&self, screen: u8, fit: VideoFit, x: f32, y: f32) -> Option<Abs> {
        let w = self.windows.iter().find(|w| w.index == screen)?;
        let (lw, lh) = w.window.size();
        Some(finger_to_frame(
            fit,
            w.window.size_in_pixels(),
            w.video?,
            x / lw.max(1) as f32,
            y / lh.max(1) as f32,
        ))
    }

    /// Open a window for each screen the host's newest layout adds, close the ones it dropped,
    /// and show each one's newest picture.
    fn tick(
        &mut self,
        video: &sdl3::VideoSubsystem,
        connector: &Arc<NativeClient>,
        decoder: &str,
        opts: &SessionOpts,
        knobs: &crate::knobs::Knobs,
        backend: crate::backend::Backend,
    ) {
        let (newest, layout) = connector.screen_layout();
        if self.follow(newest) {
            self.windows
                .retain(|w| layout.iter().any(|s| s.index == w.index));
            for s in layout.iter().filter(|s| s.index > 0) {
                let open = self.windows.iter().any(|w| w.index == s.index);
                let display = self.displays.iter().find(|(a, _)| a.index == s.index);
                let (false, Some(&(_, display))) = (open, display) else {
                    continue;
                };
                match ScreenWindow::open(
                    video, display, s.index, connector, decoder, opts, knobs, backend,
                ) {
                    Ok(w) => self.windows.push(w),
                    Err(e) => tracing::warn!(
                        screen = s.index,
                        error = %format!("{e:#}"),
                        "screen window did not open"
                    ),
                }
            }
        }
        for w in &mut self.windows {
            let done = w.present();
            w.note(done);
        }
    }
}

impl Shell {
    /// Per pass: the further screens' windows, and the local cursor hidden over them while the
    /// host's pointer is drawn into their pictures.
    pub(super) fn screens_tick(&mut self, st: &mut StreamState) {
        let Some(connector) = st.connector.clone() else {
            return;
        };
        st.screens.tick(
            &self.sdl_video,
            &connector,
            &st.params.decoder,
            &self.opts,
            &self.knobs,
            self.backend,
        );
        let desktop = st
            .capture
            .as_ref()
            .is_some_and(|cap| cap.captured() && cap.desktop());
        if desktop && self.over_screen(st) && self.mouse.is_cursor_showing() {
            self.mouse.show_cursor(false);
        }
    }

    /// Desktop-model motion over a further screen's window: an absolute move on that screen.
    pub(super) fn on_screen_motion(
        &mut self,
        stream: &mut Option<StreamState>,
        screen: u8,
        x: f32,
        y: f32,
    ) {
        let Some(st) = stream.as_mut() else {
            return;
        };
        st.cursor.user_moved(Instant::now());
        let abs = st.screens.abs_at(screen, self.opts.video_fit, x, y);
        if let (Some(cap), Some(abs)) = (st.capture.as_mut(), abs) {
            if cap.desktop() {
                cap.on_motion_abs_on(screen, abs);
            }
        }
    }

    /// The pointer is over one of the further screens' windows.
    pub(super) fn over_screen(&self, st: &StreamState) -> bool {
        self.mouse
            .focused_window_id()
            .and_then(|id| st.screens.screen_of(id))
            .is_some()
    }

    /// The client's monitors changed, or the stream window moved to another: ask the host for
    /// the screens they make now.
    pub(super) fn replan_screens(&mut self, st: &mut StreamState) {
        let Some(connector) = st.connector.as_ref() else {
            return;
        };
        let scale = (self.opts.render_scale, self.opts.render_scale_max_dim);
        let planned = client_screens(&self.sdl_video, &self.window, connector.mode(), scale);
        let ask = st.screens.plan(&planned, connector.screen_layout().0);
        tracing::info!(
            screens = ask.len(),
            "client monitors changed — asking for screens"
        );
        let _ = connector.request_screens(ask);
    }

    /// The player closed a further screen's window: ask the host for the screens left.
    pub(super) fn close_screen(&mut self, st: &mut StreamState, screen: u8) {
        let ask = st.screens.close(screen);
        tracing::info!(
            screen,
            screens = ask.len(),
            "screen window closed — asking for the rest"
        );
        if let Some(connector) = st.connector.as_ref() {
            let _ = connector.request_screens(ask);
        }
    }

    /// The screens a stream starting at `mode` asks for: every client monitor when the player
    /// `wants` them and there is more than one, else none. No host layout precedes the stream.
    pub(super) fn plan_screens(
        &self,
        screens: &mut ScreenWindows,
        wants: bool,
        mode: Mode,
    ) -> Vec<Screen> {
        if !wants {
            return Vec::new();
        }
        let scale = (self.opts.render_scale, self.opts.render_scale_max_dim);
        let planned = client_screens(&self.sdl_video, &self.window, mode, scale);
        if planned.len() < 2 {
            return Vec::new();
        }
        screens.plan(&planned, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monitor right of a HiDPI first screen sits one picture-width over; one left of or
    /// above it ends at the first picture's edge, whatever its own density.
    #[test]
    fn a_monitor_sits_in_the_first_pictures_pixels() {
        let mode = |width, height| Mode {
            width,
            height,
            refresh_hz: 60,
        };
        let first = Rect::new(0, 0, 2560, 1440);
        let (hidpi, fhd) = (mode(5120, 2880), mode(1920, 1080));
        assert_eq!(
            offset_px(first, Rect::new(2560, 0, 1920, 1080), hidpi, fhd),
            (5120, 0)
        );
        assert_eq!(
            offset_px(first, Rect::new(-1920, 360, 1920, 1080), hidpi, fhd),
            (-1920, 720)
        );
        assert_eq!(
            offset_px(first, Rect::new(0, -1080, 1920, 1080), hidpi, fhd),
            (0, -1080)
        );
        // Same density: the far edge less its width is its near edge.
        assert_eq!(
            offset_px(
                first,
                Rect::new(-1920, 0, 1920, 1080),
                mode(2560, 1440),
                fhd
            ),
            (-1920, 0)
        );
    }

    /// Above 1x a further monitor asks for its pixels × the scale, shown at its own; at 1x and
    /// below, and where the codec wall leaves nothing to shrink, its own pixels and no view.
    #[test]
    fn a_further_monitor_supersamples_like_the_first() {
        let qhd = Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 144,
        };
        let at = |w, h| Mode {
            width: w,
            height: h,
            ..qhd
        };
        assert_eq!(
            scaled_ask(qhd, 1.5, 8192),
            (at(3840, 2160), Some((2560, 1440)))
        );
        assert_eq!(scaled_ask(qhd, 1.0, 8192), (qhd, None));
        assert_eq!(scaled_ask(qhd, 0.5, 8192), (qhd, None));
        assert_eq!(
            scaled_ask(at(4096, 2304), 2.0, 4096),
            (at(4096, 2304), None)
        );
    }

    /// After an ask, the layout current at the ask opens nothing; the host's next one does.
    #[test]
    fn windows_wait_for_the_layout_after_the_ask() {
        let mut screens = ScreenWindows::default();
        screens.plan(&[], 7);
        assert!(!screens.follow(7));
        assert!(screens.follow(8));
        assert!(!screens.follow(8));
    }

    /// A closed monitor stays out of every later ask, the others keep their index, and the
    /// stream window's monitor is never left out, even when it moved onto a closed one.
    #[test]
    fn a_closed_monitor_stays_out() {
        let screen = |index| Screen {
            index,
            ..Default::default()
        };
        let planned = [(screen(0), 10), (screen(1), 11), (screen(2), 12)];
        let left: Vec<u8> = open_ones(&planned, &[11])
            .iter()
            .map(|(s, _)| s.index)
            .collect();
        assert_eq!(left, [0, 2]);
        let moved = [(screen(0), 11), (screen(1), 10)];
        let left: Vec<u8> = open_ones(&moved, &[11])
            .iter()
            .map(|(s, _)| s.index)
            .collect();
        assert_eq!(left, [0, 1]);
    }
}
