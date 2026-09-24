//! Headless keyboard/mouse/touch on KWin via `org_kde_kwin_fake_input`.
//!
//! KWin advertises this restricted global only to a client whose installed `.desktop`
//! lists it under `X-KDE-Wayland-Interfaces` (`io.unom.Punktfunk.Host.desktop`). Binding
//! is the grant — no RemoteDesktop portal and no "Allow remote control?" dialog — so it
//! works with nobody at the seat. Connect on the session `$WAYLAND_DISPLAY`. Keys are
//! raw Linux evdev codes; KWin maps them through the session keymap (no keymap upload).
//! Absolute pointer/touch use *logical* compositor pixels (post scale). At scale ≠ 1 the
//! logical edge is `physical / scale`; a streamed pixel coordinate then lands `scale×`
//! too far toward the bottom-right. Track each output's logical rectangle via
//! `xdg-output` and map the normalized client position into it. The target is the head
//! named by [`crate::stream_output`]; the event's `w×h` is the client's own size and only
//! picks a head when no name is published.
//!
//! Pin: install the host `.desktop` and re-login (KWin caches the grant per-exe).
//! Same path as `krdpserver`. See `docs-site/content/docs/(guide)/(desktops)/kde.md`.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]

use super::{gs_button_to_evdev, vk_to_evdev, InputEvent, InputInjector};
use crate::scroll::{ScrollBackend, ScrollMapper, ScrollOp};
use anyhow::{Context, Result};
use punktfunk_core::input::{InputKind, PRECISE_PX_PER_DETENT, SCROLL_FLAG_PRECISE};
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

// Inline scanner (no build.rs). Path is relative to CARGO_MANIFEST_DIR.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod fake {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/fake-input.xml");
}

use fake::org_kde_kwin_fake_input::OrgKdeKwinFakeInput as FakeInput;

/// `keyboard_key` arrived at v4; bind no higher.
const MAX_VERSION: u32 = 4;

const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;
/// GameStream `code` for a horizontal wheel (same as `gamestream::input`).
const SCROLL_HORIZONTAL: u32 = 1;

/// Axis units one wheel click is worth. `fake_input` carries a bare `axis` — no source, no
/// v120 — and KWin forwards it as such, so every toolkit falls back to the legacy convention
/// of ten units per click (Qt and Chromium multiply the value by 12 to get 120-space, GTK
/// divides by 10). The value is therefore a CLICK COUNT, never a distance.
const UNITS_PER_DETENT: f64 = 10.0;

/// Pixels a client scrolls for one of those clicks: three text lines, the price the Windows
/// injector puts on a detent too. A precise delta is a distance and clicks are all this
/// channel has, so it converts through here — without that a 10 px flick buys a whole click
/// and the page runs an order of magnitude too far. Only a source-carrying backend (libei,
/// wlroots) scrolls a gesture by its true distance.
pub(crate) const PRECISE_CLICK_PX: f64 = 60.0;

/// Axis units for one scroll event: `x` is the wire's WHEEL_DELTA(120) delta, `precise` its
/// [`SCROLL_FLAG_PRECISE`] bit. Vertical is negated by the caller, not here.
fn axis_value(x: i32, precise: bool) -> f64 {
    let detents = f64::from(x) / 120.0;
    if precise {
        detents * PRECISE_PX_PER_DETENT / PRECISE_CLICK_PX * UNITS_PER_DETENT
    } else {
        detents * UNITS_PER_DETENT
    }
}

struct OutputTrack {
    /// Registry id; also dispatch user-data so events find this entry.
    name: u32,
    wl_output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
    geo: Geo,
}

/// `wl_output.name`, physical mode, and logical rectangle (abs coords).
/// `logical_w == 0` until xdg-output reports size.
#[derive(Default)]
struct Geo {
    output_name: Option<String>,
    mode_w: i32,
    mode_h: i32,
    logical_x: i32,
    logical_y: i32,
    logical_w: i32,
    logical_h: i32,
}

/// Head a normalized absolute position lands on: the one named `want` (the newest, when a
/// supersede briefly leaves two), else a mode equal to `w×h`, else the sole head.
fn pick<'a>(
    heads: impl Iterator<Item = &'a Geo> + Clone,
    want: Option<&str>,
    w: i32,
    h: i32,
) -> Option<&'a Geo> {
    let usable = heads.filter(|g| g.logical_w > 0 && g.logical_h > 0);
    if let Some(n) = want {
        if let Some(named) = usable
            .clone()
            .filter(|g| g.output_name.as_deref() == Some(n))
            .last()
        {
            return Some(named);
        }
    }
    if let Some(sized) = usable.clone().find(|g| g.mode_w == w && g.mode_h == h) {
        return Some(sized);
    }
    let mut it = usable;
    match (it.next(), it.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

#[derive(Default)]
struct State {
    fake: Option<FakeInput>,
    xdg_mgr: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputTrack>,
}

impl State {
    fn ensure_xdg_output(o: &mut OutputTrack, mgr: &ZxdgOutputManagerV1, qh: &QueueHandle<State>) {
        if o.xdg_output.is_none() {
            o.xdg_output = Some(mgr.get_xdg_output(&o.wl_output, qh, o.name));
        }
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "org_kde_kwin_fake_input" => {
                    state.fake = Some(registry.bind(name, version.min(MAX_VERSION), qh, ()));
                }
                "wl_output" => {
                    // `mode` is v1, `name` v4; bind ≤ the proxy max (4).
                    let wl_output: WlOutput = registry.bind(name, version.min(4), qh, name);
                    let mut o = OutputTrack {
                        name,
                        wl_output,
                        xdg_output: None,
                        geo: Geo::default(),
                    };
                    if let Some(mgr) = state.xdg_mgr.clone() {
                        State::ensure_xdg_output(&mut o, &mgr, qh);
                    }
                    state.outputs.push(o);
                }
                "zxdg_output_manager_v1" => {
                    let mgr: ZxdgOutputManagerV1 = registry.bind(name, version.min(3), qh, ());
                    for o in state.outputs.iter_mut() {
                        State::ensure_xdg_output(o, &mgr, qh);
                    }
                    state.xdg_mgr = Some(mgr);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| {
                    if o.name == name {
                        if let Some(x) = &o.xdg_output {
                            x.destroy();
                        }
                        false
                    } else {
                        true
                    }
                });
            }
            _ => {}
        }
    }
}

// fake_input emits no events.
impl Dispatch<FakeInput, ()> for State {
    fn event(
        _: &mut Self,
        _: &FakeInput,
        _: <FakeInput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.iter_mut().find(|o| o.name == *name) else {
            return;
        };
        match event {
            // A monitor also advertises non-current modes; only Current is the live size.
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                o.geo.mode_w = width;
                o.geo.mode_h = height;
            }
            wl_output::Event::Name { name } => o.geo.output_name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(o) = state.outputs.iter_mut().find(|o| o.name == *name) {
            match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => {
                    o.geo.logical_x = x;
                    o.geo.logical_y = y;
                }
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    o.geo.logical_w = width;
                    o.geo.logical_h = height;
                }
                _ => {}
            }
        }
    }
}

// The manager has no events.
impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct KwinFakeInjector {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    fake: FakeInput,
    last_refresh: Option<Instant>,
    /// Normalized-scroll lowering onto the bare `axis` click channel.
    scroll: ScrollMapper,
}

/// Cap geometry roundtrips at 2 Hz. A roundtrip on every mouse-move would stall the control path.
const GEO_REFRESH: Duration = Duration::from_millis(500);

impl KwinFakeInjector {
    pub fn open() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .context("Wayland registry roundtrip")?;

        let fake = state.fake.clone().context(
            "KWin does not expose org_kde_kwin_fake_input to this client — install the host's \
             .desktop (io.unom.Punktfunk.Host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
             KWin authorizes it (the grant is cached per-exe on first connect), or this is not a \
             KWin session",
        )?;
        // Legacy handshake; an interface-authorized client is accepted with no dialog.
        fake.authenticate("Punktfunk".into(), "remote streaming input".into());
        queue
            .roundtrip(&mut state)
            .context("fake_input authenticate roundtrip")?;
        conn.flush().ok();

        // `logical_size` arrives after the registry roundtrip. Falls back to scale-1 if xdg-output is absent.
        let mut injector = Self {
            conn,
            queue,
            state,
            fake,
            last_refresh: None,
            scroll: ScrollMapper::new(ScrollBackend::Kwin),
        };
        injector.refresh_geometry();
        tracing::info!(
            outputs = injector.state.outputs.len(),
            "KWin fake_input ready (headless keyboard/mouse/touch — no portal)"
        );
        Ok(injector)
    }

    /// Throttled to [`GEO_REFRESH`]. A wl_output that appeared this round only gets
    /// `xdg_output` mid-dispatch, so `logical_size` lands on a later roundtrip — loop
    /// (bounded) until every output has a size.
    fn refresh_geometry(&mut self) {
        let now = Instant::now();
        if let Some(t) = self.last_refresh {
            if now.duration_since(t) < GEO_REFRESH {
                return;
            }
        }
        self.last_refresh = Some(now);
        for _ in 0..3 {
            if self.queue.roundtrip(&mut self.state).is_err() {
                return;
            }
            let pending = self.state.xdg_mgr.is_some()
                && self.state.outputs.iter().any(|o| o.geo.logical_w == 0);
            if !pending {
                break;
            }
        }
    }

    /// Logical rectangle for a normalized client position: the [`pick`]ed head, else
    /// `w×h` pixels at the origin (correct at scale 1).
    fn logical_target(&self, phys_w: i32, phys_h: i32) -> (f64, f64, f64, f64) {
        let want = crate::stream_output();
        let heads = self.state.outputs.iter().map(|o| &o.geo);
        match pick(heads, want.as_deref(), phys_w, phys_h) {
            Some(o) => (
                o.logical_x as f64,
                o.logical_y as f64,
                o.logical_w as f64,
                o.logical_h as f64,
            ),
            None => (0.0, 0.0, phys_w as f64, phys_h as f64),
        }
    }

    /// Execute a normalized-scroll plan on the bare `axis` channel: the only
    /// op a KWin plan emits is a click-priced axis value.
    fn inject_scroll(&mut self, event: &InputEvent) {
        for op in self.scroll.plan(event) {
            if let ScrollOp::Continuous { horizontal, value } = op {
                let axis = if horizontal {
                    AXIS_HORIZONTAL
                } else {
                    AXIS_VERTICAL
                };
                self.fake.axis(axis, value);
            }
        }
    }
}

impl InputInjector for KwinFakeInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event.kind {
            InputKind::MouseMove => {
                self.fake.pointer_motion(event.x as f64, event.y as f64);
            }
            InputKind::MouseMoveAbs => {
                let w = ((event.flags >> 16) & 0xffff) as i32;
                let h = (event.flags & 0xffff) as i32;
                if w > 0 && h > 0 {
                    self.refresh_geometry();
                    let (lx, ly, lw, lh) = self.logical_target(w, h);
                    let nx = (event.x as f64 / w as f64).clamp(0.0, 1.0);
                    let ny = (event.y as f64 / h as f64).clamp(0.0, 1.0);
                    self.fake
                        .pointer_motion_absolute(lx + nx * lw, ly + ny * lh);
                }
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                if let Some(btn) = gs_button_to_evdev(event.code) {
                    let st = u32::from(event.kind == InputKind::MouseButtonDown);
                    self.fake.button(btn, st);
                }
            }
            InputKind::MouseScroll => {
                // Wire is WHEEL_DELTA(120); vertical flips the Wayland axis sign. The app
                // reads this axis in clicks ([`UNITS_PER_DETENT`]), so a precise delta must be
                // repriced from the wire's detent to what a click scrolls; it cannot travel as
                // a distance the way it does on wlroots.
                let horizontal = event.code == SCROLL_HORIZONTAL;
                let axis = if horizontal {
                    AXIS_HORIZONTAL
                } else {
                    AXIS_VERTICAL
                };
                let sign = if horizontal { 1.0 } else { -1.0 };
                let precise = event.flags & SCROLL_FLAG_PRECISE != 0;
                self.fake.axis(axis, sign * axis_value(event.x, precise));
            }
            // Normalized scroll lowers through the shared mapper onto the same
            // bare axis; the plan carries click-priced units and the Wayland
            // vertical sign already applied.
            InputKind::Scroll => self.inject_scroll(event),
            InputKind::KeyDown | InputKind::KeyUp => {
                // Evdev code; KWin owns the keymap and modifier state — no modifiers request.
                if let Some(evdev) = vk_to_evdev(event.code as u8) {
                    let st = u32::from(event.kind == InputKind::KeyDown);
                    self.fake.keyboard_key(evdev as u32, st);
                } else {
                    tracing::debug!(vk = event.code, "unmapped VK keycode — dropped");
                }
            }
            // `code` is the touch id; w×h packed in `flags` (same abs map as MouseMoveAbs). One frame per event.
            InputKind::TouchDown | InputKind::TouchMove => {
                let w = ((event.flags >> 16) & 0xffff) as i32;
                let h = (event.flags & 0xffff) as i32;
                if w > 0 && h > 0 {
                    self.refresh_geometry();
                    let (lx, ly, lw, lh) = self.logical_target(w, h);
                    let nx = (event.x as f64 / w as f64).clamp(0.0, 1.0);
                    let ny = (event.y as f64 / h as f64).clamp(0.0, 1.0);
                    let x = lx + nx * lw;
                    let y = ly + ny * lh;
                    if event.kind == InputKind::TouchDown {
                        self.fake.touch_down(event.code, x, y);
                    } else {
                        self.fake.touch_motion(event.code, x, y);
                    }
                    self.fake.touch_frame();
                }
            }
            InputKind::TouchUp => {
                self.fake.touch_up(event.code);
                self.fake.touch_frame();
            }
            // Host-layout keycodes only; this backend does not advertise HOST_CAP_TEXT_INPUT.
            InputKind::TextInput => {}
            // Gamepads go through uinput, not the compositor.
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => {}
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .context("wayland dispatch")?;
        self.conn.flush().context("wayland flush")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(name: &str, x: i32, w: i32, h: i32) -> Geo {
        Geo {
            output_name: Some(name.into()),
            mode_w: w,
            mode_h: h,
            logical_x: x,
            logical_w: w,
            logical_h: h,
            ..Geo::default()
        }
    }

    /// A 1080p TV beside two 1080p monitors drives the streamed head, not the first
    /// monitor whose mode equals the TV panel.
    #[test]
    fn the_named_head_beats_a_size_match() {
        let heads = [
            head("DP-2", 0, 1920, 1080),
            head("DP-1", 1920, 1920, 1080),
            head("Virtual-punktfunk-1", 3840, 2560, 1440),
        ];
        let at = |want| pick(heads.iter(), want, 1920, 1080).map(|g| g.logical_x);
        assert_eq!(at(Some("Virtual-punktfunk-1")), Some(3840));
        assert_eq!(at(Some("DP-1")), Some(1920));
        // Unpublished or vanished: the size ladder.
        assert_eq!(at(None), Some(0));
        assert_eq!(at(Some("gone")), Some(0));
    }

    /// A supersede leaves the old head alive under the same name; the newer one is live.
    #[test]
    fn a_shared_name_takes_the_newest_head() {
        let heads = [
            head("Virtual-punktfunk-1", 0, 1920, 1080),
            head("Virtual-punktfunk-1", 1920, 3840, 2160),
        ];
        let got = pick(heads.iter(), Some("Virtual-punktfunk-1"), 1920, 1080);
        assert_eq!(got.map(|g| g.logical_x), Some(1920));
    }

    /// The app multiplies this axis back by 12 to reach 120-space, so a detent has to leave
    /// as 10 units — 15 spent one and a half clicks per notch.
    #[test]
    fn a_wheel_detent_is_one_click() {
        assert_eq!(axis_value(120, false), 10.0);
        assert_eq!(axis_value(-240, false), -20.0);
    }

    /// A 60 px flick must move 60 px of content: 60 px is one click here, and one click is
    /// 10 units. Sending the distance itself (the old `PRECISE_PX_PER_DETENT` scale) made it
    /// six clicks.
    #[test]
    fn a_precise_flick_travels_its_own_distance() {
        let px = 60.0;
        let wire = (px * 12.0) as i32; // clients put a measured pixel into 120-space at 12
        assert!((axis_value(wire, true) - UNITS_PER_DETENT).abs() < 1e-9);
        assert!((axis_value(wire, false) - 60.0).abs() < 1e-9);
    }

    /// A precise delta never flips its own sign, and zero stays zero.
    #[test]
    fn precise_keeps_its_sign() {
        assert!(axis_value(-120, true) < 0.0);
        assert_eq!(axis_value(0, true), 0.0);
    }
}
