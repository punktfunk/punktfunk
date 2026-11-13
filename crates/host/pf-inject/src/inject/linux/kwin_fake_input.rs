//! Headless keyboard/mouse/touch on KWin via `org_kde_kwin_fake_input`.
//!
//! KWin advertises this restricted global only to a client whose installed `.desktop`
//! lists it under `X-KDE-Wayland-Interfaces` (`io.unom.punktfunk.host.desktop`). Binding
//! is the grant — no RemoteDesktop portal and no "Allow remote control?" dialog — so it
//! works with nobody at the seat. Connect on the session `$WAYLAND_DISPLAY`. Keys are
//! raw Linux evdev codes; KWin maps them through the session keymap (no keymap upload).
//! Committed text goes as keysyms (v6, Plasma 6.5+), which KWin types in the session layout;
//! the modifiers a character needs go down as real keys around it ([`text_modifiers`]).
//! Absolute pointer/touch use *logical* compositor pixels (post scale). At scale ≠ 1 the
//! logical edge is `physical / scale`; a streamed pixel coordinate then lands `scale×`
//! too far toward the bottom-right. Track each output's logical rectangle via
//! `xdg-output` and map the normalized client position into it. The target is the head
//! [`crate::head_pick::pick`] names; a miss maps `w×h` pixels at the origin.
//!
//! Pin: install the host `.desktop` and re-login (KWin caches the grant per-exe).
//! Same path as `krdpserver`. See `docs-site/content/docs/(guide)/(desktops)/kde.md`.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]

use super::{InputEvent, InputInjector};
use crate::head_pick::{self, HeadFacts};
use crate::plan::{Abs, Op, Plan};
use crate::scroll::{ScrollBackend, ScrollMapper, ScrollOp};
use anyhow::{Context, Result};
use punktfunk_core::input::InputKind;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use xkbcommon::xkb;

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

/// `keyboard_keysym` arrived at v6; bind no higher.
const MAX_VERSION: u32 = 6;

const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

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

impl Geo {
    /// This head for [`head_pick::pick`]; `None` until xdg-output reports a logical size.
    fn facts(&self) -> Option<HeadFacts<'_>> {
        let size = |w: i32, h: i32| (w > 0 && h > 0).then(|| (w as u32, h as u32));
        let (logical_w, logical_h) = size(self.logical_w, self.logical_h)?;
        Some(HeadFacts {
            name: self.output_name.as_deref(),
            x: self.logical_x,
            y: self.logical_y,
            logical_w,
            logical_h,
            mode: size(self.mode_w, self.mode_h),
            mapping_id: None,
        })
    }
}

#[derive(Default)]
struct State {
    fake: Option<FakeInput>,
    xdg_mgr: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputTrack>,
    /// Never focused, so no key arrives: it is here for the keymap KWin sends every keyboard.
    keyboard: Option<WlKeyboard>,
    /// The session keymap, replaced whenever KWin sends another.
    keymap: Option<xkb::Keymap>,
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
                // `capabilities` says when the seat has a keyboard to ask for.
                "wl_seat" => {
                    let _: WlSeat = registry.bind(name, version.min(7), qh, ());
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| {
                    if o.name == name {
                        if let Some(x) = &o.xdg_output {
                            x.destroy();
                        }
                        // The connection outlives every virtual head: release its proxy too.
                        if o.wl_output.version() >= 3 {
                            o.wl_output.release();
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

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = event
        {
            // Read, not mapped: a file KWin shrinks under a mapping would fault this thread.
            // From offset 0, whatever the descriptor's own position is.
            use std::os::unix::fs::FileExt as _;
            let mut text = vec![0; size as usize];
            if std::fs::File::from(fd).read_exact_at(&mut text, 0).is_err() {
                return;
            }
            // The file ends in a NUL the size counts.
            let text = String::from_utf8_lossy(&text);
            let text = text.trim_end_matches('\0').to_owned();
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let format = xkb::KEYMAP_FORMAT_TEXT_V1;
            let flags = xkb::KEYMAP_COMPILE_NO_FLAGS;
            state.keymap = xkb::Keymap::new_from_string(&ctx, text, format, flags);
        }
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
    /// What this device holds, released before it goes ([`Drop`]).
    plan: Plan,
    /// The session bus, opened for the first [`active_layout`] ask.
    bus: Option<zbus::blocking::Connection>,
}

/// Cap geometry roundtrips at 2 Hz. A roundtrip on every mouse-move would stall the control path.
const GEO_REFRESH: Duration = Duration::from_millis(500);

/// The session's KWin socket, else `$WAYLAND_DISPLAY`.
fn connect() -> Result<Connection> {
    match pf_host_config::session::wayland_stream() {
        Some(Ok(stream)) => Connection::from_socket(stream),
        Some(Err(_)) => Err(wayland_client::ConnectError::NoCompositor),
        None => Connection::connect_to_env(),
    }
    .context("connect to KWin Wayland")
}

/// Whether KWin offers this host `keyboard_keysym` (fake_input v6), so committed text types.
/// Every Welcome asks, and the probe is a connect plus a blocking roundtrip, so KWin's first
/// answer is kept. A failed connect keeps nothing: the next Welcome asks again.
pub(crate) fn keysym_supported() -> bool {
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let Some(&answer) = ANSWER.get() {
        return answer;
    }
    struct Probe;
    impl Dispatch<WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let Ok(conn) = connect() else { return false };
    let Ok((globals, _queue)) = registry_queue_init::<Probe>(&conn) else {
        return false;
    };
    let answer = globals.contents().with_list(|list| {
        list.iter()
            .any(|g| g.interface == "org_kde_kwin_fake_input" && g.version >= MAX_VERSION)
    });
    *ANSWER.get_or_init(|| answer)
}

/// The keysym that types `cp`. `None` for a control character (Enter and Tab ride the VK path).
fn text_keysym(cp: u32) -> Option<u32> {
    char::from_u32(cp).filter(|c| !c.is_control())?;
    let sym = xkb::utf32_to_keysym(cp).raw();
    (sym != xkb::keysyms::KEY_NoSymbol).then_some(sym)
}

/// The keys a layout or its options may make a level chooser, as evdev codes: Left Shift,
/// Right Alt, Right Ctrl, Left Alt, both Meta keys, Menu, Caps Lock and the 102nd key.
const LEVEL_KEYS: [u16; 9] = [42, 100, 97, 56, 125, 126, 127, 58, 86];

/// The evdev keys to hold so `sym` types in `layout`: the modifier keys of the level KWin's
/// keysym request picks, its first key and level carrying `sym`. KWin sets that level's
/// modifiers in its state only, and an input method holding the keyboard grab then sees the key
/// before them and hands back a stuck Shift. Held as real keys they reach it first.
/// Empty for a level that needs none, a keysym the layout lacks (KWin types those from a
/// scratch keymap), and modifiers no [`LEVEL_KEYS`] key sets.
fn text_modifiers(keymap: &xkb::Keymap, layout: u32, sym: u32) -> Vec<u16> {
    let codes = keymap.min_keycode().raw()..keymap.max_keycode().raw();
    let level_mods = codes.into_iter().find_map(|code| {
        let key = xkb::Keycode::new(code);
        (0..keymap.num_levels_for_key(key, layout)).find_map(|level| {
            let syms = keymap.key_get_syms_by_level(key, layout, level);
            // KWin takes the first way to reach the level; none leaves the mask empty.
            let mut masks = [0];
            let _ = keymap.key_get_mods_for_level(key, layout, level, &mut masks);
            syms.iter().any(|s| s.raw() == sym).then_some(masks[0])
        })
    });
    let mut need = level_mods.unwrap_or(0);
    let mut keys = Vec::new();
    for key in LEVEL_KEYS {
        if need == 0 {
            break;
        }
        // What this key alone does to a fresh state: a plain modifier key depresses its
        // modifiers and latches or locks nothing.
        let mut state = xkb::State::new(keymap);
        state.update_mask(0, 0, 0, 0, 0, layout);
        // xkb keycodes are evdev + 8
        state.update_key(
            xkb::Keycode::new(u32::from(key) + 8),
            xkb::KeyDirection::Down,
        );
        let down = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        let kept = state.serialize_mods(xkb::STATE_MODS_LATCHED | xkb::STATE_MODS_LOCKED);
        if down != 0 && down & !need == 0 && kept == 0 {
            keys.push(key);
            need &= !down;
        }
    }
    if need == 0 {
        keys
    } else {
        Vec::new()
    }
}

/// The index of KWin's active layout in `keymap`. A client without focus hears of no layout
/// switch, so a keymap with several asks KWin's `org.kde.keyboard` service. `None` when KWin
/// gives no usable answer.
fn active_layout(
    bus: &mut Option<zbus::blocking::Connection>,
    keymap: &xkb::Keymap,
) -> Option<u32> {
    if keymap.num_layouts() <= 1 {
        return Some(0);
    }
    if bus.is_none() {
        *bus = zbus::blocking::Connection::session().ok();
    }
    let reply = bus.as_ref()?.call_method(
        Some("org.kde.keyboard"),
        "/Layouts",
        Some("org.kde.KeyboardLayouts"),
        "getLayout",
        &(),
    );
    let layout: u32 = reply.ok()?.body().deserialize().ok()?;
    (layout < keymap.num_layouts()).then_some(layout)
}

impl KwinFakeInjector {
    pub fn open() -> Result<Self> {
        let conn = connect()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .context("Wayland registry roundtrip")?;

        let fake = state.fake.clone().context(
            "KWin does not expose org_kde_kwin_fake_input to this client — install the host's \
             .desktop (io.unom.punktfunk.host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
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
            plan: Plan::default(),
            bus: None,
        };
        injector.refresh_geometry();
        tracing::info!(
            outputs = injector.state.outputs.len(),
            "KWin fake_input ready (headless keyboard/mouse/touch — no portal)"
        );
        Ok(injector)
    }

    /// Read what KWin sent: output geometry and the keymap. Throttled to [`GEO_REFRESH`]. A
    /// wl_output that appeared this round only gets `xdg_output` mid-dispatch, so
    /// `logical_size` lands on a later roundtrip — loop (bounded) until every output has a size.
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

    /// Where `abs` lands in logical compositor space: its fraction of the [`head_pick::pick`]ed
    /// head, else of `w×h` pixels at the origin (correct at scale 1).
    fn place(&mut self, abs: &Abs) -> (f64, f64) {
        self.refresh_geometry();
        let heads: Vec<HeadFacts> = self
            .state
            .outputs
            .iter()
            .filter_map(|o| o.geo.facts())
            .collect();
        let (lx, ly, lw, lh) = match head_pick::pick(
            &heads,
            &crate::stream_target_for(abs.screen),
            Some((abs.w, abs.h)),
        ) {
            Some(i) => {
                let h = heads[i];
                let (x, y) = (f64::from(h.x), f64::from(h.y));
                (x, y, f64::from(h.logical_w), f64::from(h.logical_h))
            }
            None => (0.0, 0.0, f64::from(abs.w), f64::from(abs.h)),
        };
        let (nx, ny) = abs.normalized();
        (lx + nx * lw, ly + ny * lh)
    }

    /// The modifier keys to hold around `sym`'s tap ([`text_modifiers`]), less those the
    /// client holds itself: lifting one of those would end the client's own hold. None when
    /// the keymap or the active layout is unknown.
    fn text_hold(&mut self, sym: u32) -> Vec<u16> {
        self.refresh_geometry();
        let Some(keymap) = self.state.keymap.clone() else {
            return Vec::new();
        };
        let Some(layout) = active_layout(&mut self.bus, &keymap) else {
            return Vec::new();
        };
        let mut keys = text_modifiers(&keymap, layout, sym);
        keys.retain(|&key| !self.plan.holds(key));
        keys
    }

    /// Emit one planned op. Keys are evdev codes; KWin owns the keymap and modifier state, so
    /// there is no modifiers request. Text is a keysym tap inside the modifier keys it needs.
    /// A touch is one frame per event.
    fn emit(&mut self, op: Op) {
        match op {
            Op::Motion { dx, dy } => self.fake.pointer_motion(dx.into(), dy.into()),
            Op::Absolute(abs) => {
                let (x, y) = self.place(&abs);
                self.fake.pointer_motion_absolute(x, y);
            }
            Op::Button { evdev, down } => self.fake.button(evdev, down.into()),
            Op::Key { evdev, down } => self.fake.keyboard_key(evdev.into(), down.into()),
            Op::TouchDown { id, at } => {
                let (x, y) = self.place(&at);
                self.fake.touch_down(id, x, y);
                self.fake.touch_frame();
            }
            Op::TouchMove { id, at } => {
                let (x, y) = self.place(&at);
                self.fake.touch_motion(id, x, y);
                self.fake.touch_frame();
            }
            Op::TouchUp { id } => {
                self.fake.touch_up(id);
                self.fake.touch_frame();
            }
            // Below v6 there is no keysym request, and the host offers no text.
            Op::Text(cp) => {
                if let Some(sym) = text_keysym(cp).filter(|_| self.fake.version() >= MAX_VERSION) {
                    let held = self.text_hold(sym);
                    for &key in &held {
                        self.fake.keyboard_key(key.into(), 1);
                    }
                    self.fake.keyboard_keysym(sym, 1);
                    self.fake.keyboard_keysym(sym, 0);
                    for &key in held.iter().rev() {
                        self.fake.keyboard_key(key.into(), 0);
                    }
                }
            }
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

impl Drop for KwinFakeInjector {
    /// Release what the device still holds before it goes; a key or button KWin keeps
    /// pressed for a client that vanished is a stuck one.
    fn drop(&mut self) {
        for op in self.plan.release() {
            self.emit(op);
        }
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.conn.flush();
    }
}

impl InputInjector for KwinFakeInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        if matches!(event.kind, InputKind::MouseScroll | InputKind::Scroll) {
            // Legacy and normalized scroll lower through the shared mapper onto the bare axis;
            // the plan carries click-priced units and the Wayland vertical sign already applied.
            self.inject_scroll(event);
        } else if let Some(op) = self.plan.step(event) {
            self.emit(op);
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
    use super::{text_keysym, text_modifiers};
    use xkbcommon::xkb;

    /// Each character holds the real modifier keys of its level in the active layout, and
    /// none where KWin's own keysym tap is enough.
    #[test]
    fn text_holds_the_modifier_keys_its_level_needs() {
        const SHIFT: u16 = 42;
        const ALTGR: u16 = 100;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = |layout: &str| {
            let flags = xkb::KEYMAP_COMPILE_NO_FLAGS;
            xkb::Keymap::new_from_names(&ctx, "evdev", "pc105", layout, "", None, flags)
        };
        let (Some(us), Some(de), Some(us_ru)) = (keymap("us"), keymap("de"), keymap("us,ru"))
        else {
            eprintln!("no xkb data on this box — skipped");
            return;
        };
        let rows: [(&str, &xkb::Keymap, u32, char, &[u16]); 14] = [
            ("a plain letter", &us, 0, 'a', &[]),
            ("a capital", &us, 0, 'A', &[SHIFT]),
            ("a shifted symbol", &us, 0, '<', &[SHIFT]),
            ("an unshifted symbol", &us, 0, '=', &[]),
            ("a character the layout lacks", &us, 0, 'ñ', &[]),
            ("the same symbol on its own key", &de, 0, '<', &[]),
            ("that key's shifted level", &de, 0, '>', &[SHIFT]),
            ("a third-level character", &de, 0, '€', &[ALTGR]),
            ("another third-level character", &de, 0, '@', &[ALTGR]),
            ("a fourth-level character", &de, 0, '¿', &[SHIFT, ALTGR]),
            (
                "a capital in the first of two layouts",
                &us_ru,
                0,
                'A',
                &[SHIFT],
            ),
            ("a capital the active layout lacks", &us_ru, 1, 'A', &[]),
            ("a capital of the second layout", &us_ru, 1, 'Ж', &[SHIFT]),
            (
                "that capital while the first is active",
                &us_ru,
                0,
                'Ж',
                &[],
            ),
        ];
        for (name, keymap, layout, c, want) in rows {
            let sym = text_keysym(c as u32).unwrap();
            assert_eq!(text_modifiers(keymap, layout, sym), want, "{name}");
        }
    }

    #[test]
    fn text_maps_to_layout_free_keysyms() {
        assert_eq!(text_keysym('"' as u32), Some(0x22));
        assert_eq!(text_keysym('A' as u32), Some(0x41));
        assert_eq!(text_keysym('é' as u32), Some(0xe9));
        assert_eq!(text_keysym('€' as u32), Some(0x20ac));
        assert_eq!(text_keysym('😀' as u32), Some(0x0100_0000 | 0x1f600));
        assert_eq!(text_keysym('\n' as u32), None);
        assert_eq!(text_keysym(0xd800), None);
    }
}
