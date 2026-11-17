//! Virtual pointer and keyboard on wlroots (`zwlr_virtual_pointer_manager_v1`,
//! `zwp_virtual_keyboard_manager_v1`).
//!
//! Absolute motion is mapped onto the `wl_output` the pointer was created
//! with; there is no re-aim request. The streamed head is published by name
//! in [`crate::stream_output`] and rebound by [`WlrootsInjector::retarget`].
//! A miss binds no output (whole-layout mapping). First-advertised is the
//! operator's physical display, never the session head.
//!
//! A layout-mapped compositor (Umbriel) ignores that output and maps absolute
//! motion over the bounding box of its whole layout. There the pointer binds
//! nothing and each sample is placed in layout space from `zxdg_output_v1`.

use super::{InputEvent, InputInjector};
use crate::head_pick::{self, HeadFacts, StreamTarget};
use crate::plan::{Abs, Op, Plan};
use crate::scroll::{AxisSource, ScrollBackend, ScrollMapper, ScrollOp};
use anyhow::{bail, Context, Result};
use punktfunk_core::input::InputKind;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_output::{self, WlOutput},
    wl_pointer, wl_registry,
    wl_seat::WlSeat,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

/// v4 is the first `wl_output` with `name`; bind that high to match the streamed head.
const WL_OUTPUT_MAX: u32 = 4;

struct Output {
    /// Registry global name: `GlobalRemove` key and `WlOutput` user data.
    global: u32,
    proxy: WlOutput,
    /// `wl_output.name` (v4). Same string every client sees; `None` below v4.
    name: Option<String>,
    /// Layout mode only: the `zxdg_output_v1` reporting where the output sits.
    xdg: Option<ZxdgOutputV1>,
    /// `(x, y, w, h)` in layout coordinates; `w == 0` until reported.
    logical: (i32, i32, i32, i32),
}

impl Output {
    fn logical(&self) -> Option<Rect> {
        let (_, _, w, h) = self.logical;
        (w > 0 && h > 0).then_some(self.logical)
    }
}

/// `(x, y, w, h)` of one output in layout coordinates.
type Rect = (i32, i32, i32, i32);

/// Units per logical pixel in layout-space motion: integer units stay finer than a pixel at
/// fractional scales.
const LAYOUT_STEP: f64 = 16.0;

/// `(x, y, x_extent, y_extent)` that put fraction `(fx, fy)` of `head` at its place in the
/// bounding box of `heads`, which a layout-mapped compositor stretches absolute motion over.
fn layout_point(heads: &[Rect], head: Rect, (fx, fy): (f64, f64)) -> (u32, u32, u32, u32) {
    let all = || heads.iter().chain(std::iter::once(&head));
    let x0 = all().map(|r| r.0).min().unwrap_or(head.0);
    let y0 = all().map(|r| r.1).min().unwrap_or(head.1);
    let x1 = all().map(|r| r.0 + r.2).max().unwrap_or(head.0 + head.2);
    let y1 = all().map(|r| r.1 + r.3).max().unwrap_or(head.1 + head.3);
    let units = |logical: f64| (logical * LAYOUT_STEP) as u32;
    (
        units(f64::from(head.0 - x0) + fx * f64::from(head.2)),
        units(f64::from(head.1 - y0) + fy * f64::from(head.3)),
        units(f64::from(x1 - x0)),
        units(f64::from(y1 - y0)),
    )
}

#[derive(Default)]
struct Globals {
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<WlSeat>,
    /// Every advertised head, advertisement order. The first is the operator's
    /// physical display; the streamed head is added later and is never first.
    outputs: Vec<Output>,
    /// Absolute motion spans the whole layout ([`Backend::WlrLayout`](crate::Backend)).
    layout: bool,
    /// Layout mode only.
    xdg_mgr: Option<ZxdgOutputManagerV1>,
}

impl Globals {
    /// The output absolute motion binds to by [`head_pick::pick`]; `None` maps the whole
    /// layout. A `wl_output` carries only a name, so the name and sole-head rungs decide.
    fn resolve(&self, target: &StreamTarget) -> Option<&Output> {
        let heads: Vec<HeadFacts> = (self.outputs.iter())
            .map(|o| HeadFacts {
                name: o.name.as_deref(),
                ..HeadFacts::default()
            })
            .collect();
        head_pick::pick(&heads, target, None).map(|i| &self.outputs[i])
    }

    /// The output a pointer binds to: none in layout mode, where the compositor ignores it.
    fn bind_target(&self, target: &StreamTarget) -> Option<&Output> {
        if self.layout {
            return None;
        }
        self.resolve(target)
    }

    /// `motion_absolute` arguments for `abs` aimed at `target`: in layout space in layout mode
    /// once the head's geometry is known, else on the sample's own extent.
    fn motion(&self, target: impl FnOnce() -> StreamTarget, abs: &Abs) -> (u32, u32, u32, u32) {
        let placed = self.layout.then(|| {
            let head = self.resolve(&target())?.logical()?;
            let heads: Vec<Rect> = self.outputs.iter().filter_map(Output::logical).collect();
            Some(layout_point(&heads, head, abs.normalized()))
        });
        placed.flatten().unwrap_or_else(|| {
            let (x, y) = abs.in_extent();
            (x, y, abs.w, abs.h)
        })
    }

    /// True when `target` names a head this registry has not shown yet.
    fn lacks(&self, target: &StreamTarget) -> bool {
        target.name.is_some() && !self.outputs.iter().any(|o| o.name == target.name)
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
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
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_mgr = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard_mgr = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                "wl_output" => {
                    // User data is the registry global name so later events hit this entry.
                    let proxy: WlOutput = registry.bind(name, version.min(WL_OUTPUT_MAX), qh, name);
                    let xdg = (state.xdg_mgr.as_ref()).map(|m| m.get_xdg_output(&proxy, qh, name));
                    state.outputs.push(Output {
                        global: name,
                        proxy,
                        name: None,
                        xdg,
                        logical: (0, 0, 0, 0),
                    });
                }
                "zxdg_output_manager_v1" if state.layout => {
                    let mgr: ZxdgOutputManagerV1 = registry.bind(name, version.min(3), qh, ());
                    for o in state.outputs.iter_mut().filter(|o| o.xdg.is_none()) {
                        o.xdg = Some(mgr.get_xdg_output(&o.proxy, qh, o.global));
                    }
                    state.xdg_mgr = Some(mgr);
                }
                _ => {}
            },
            // Drop the gone head; the pointer may still be bound to it until the next `retarget`.
            // The connection outlives every virtual head: release a removed output's proxy.
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| {
                    if o.global != name {
                        return true;
                    }
                    if let Some(x) = &o.xdg {
                        x.destroy();
                    }
                    if o.proxy.version() >= 3 {
                        o.proxy.release();
                    }
                    false
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for Globals {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.iter_mut().find(|o| o.global == *global) {
                o.name = Some(name);
            }
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for Globals {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.iter_mut().find(|o| o.global == *global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => (o.logical.0, o.logical.1) = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                (o.logical.2, o.logical.3) = (width, height)
            }
            _ => {}
        }
    }
}

// These proxies emit no events we handle; Dispatch is still required to bind them.
macro_rules! ignore_events {
    ($($t:ty),* $(,)?) => {$(
        impl Dispatch<$t, ()> for Globals {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    WlSeat,
    ZwlrVirtualPointerManagerV1,
    ZwlrVirtualPointerV1,
    ZwpVirtualKeyboardManagerV1,
    ZwpVirtualKeyboardV1,
    ZxdgOutputManagerV1,
);

pub struct WlrootsInjector {
    conn: Connection,
    queue: EventQueue<Globals>,
    globals: Globals,
    pointer: ZwlrVirtualPointerV1,
    /// Registry global of the output `pointer` is bound to; `None` maps absolute motion over
    /// the whole layout. A global, not a name: a supersede briefly leaves two heads sharing one.
    bound_output: Option<u32>,
    /// Aim generation [`Self::retarget`] last read the socket for an unbound name.
    lookup_gen: u64,
    /// A pointer per further screen that saw absolute input, bound to that screen's output:
    /// the protocol has no re-aim, and re-making one pointer per move would drop a held button
    /// mid-drag between screens.
    screens: Vec<ScreenPointer>,
    /// The screen whose pointer moved last, `0` the first: buttons, scroll and relative motion
    /// go there.
    active: u8,
    /// What `pointer` and `keyboard` hold. Released before either goes; the compositor will not.
    plan: Plan,
    keyboard: ZwpVirtualKeyboardV1,
    /// Fed only the plan's key ops: it counts presses per key, and this injector outlives the
    /// session, so an unmatched down would pin its modifier for the host's lifetime.
    xkb_state: xkb::State,
    /// The compositor reads its own dup of this fd while handling the keymap request.
    _keymap_file: std::fs::File,
    text: Option<TextKeyboard>,
    /// Scroll lowering, legacy and normalized; holds the sub-detent residue.
    scroll: ScrollMapper,
}

/// One further screen's pointer ([`WlrootsInjector::screens`]).
struct ScreenPointer {
    screen: u8,
    pointer: ZwlrVirtualPointerV1,
    /// Registry global of its output; `None` maps over the whole layout.
    bound: Option<u32>,
    /// Screens generation it last read the socket for an unbound name.
    lookup_gen: u64,
}

/// Distinct chars before the text keymap restarts. Keycodes start at 9; xkb max is 255.
const TEXT_KEYMAP_MAX: usize = 200;

/// Separate `zwp_virtual_keyboard` for [`InputKind::TextInput`]. Keymap re-uploads
/// must not touch the main device's layout or modifier state.
struct TextKeyboard {
    keyboard: ZwpVirtualKeyboardV1,
    /// `chars[i]` types on wire keycode `i + 1` (xkb `i + 9`).
    chars: Vec<char>,
    /// The compositor reads its own dup of this fd while handling the keymap request.
    _keymap_file: Option<std::fs::File>,
}

impl WlrootsInjector {
    /// `layout`: the compositor maps absolute motion over its whole layout
    /// ([`Backend::WlrLayout`](crate::Backend)).
    pub fn open(layout: bool) -> Result<Self> {
        let conn = match pf_host_config::session::wayland_stream() {
            Some(Ok(stream)) => Connection::from_socket(stream),
            Some(Err(_)) => Err(wayland_client::ConnectError::NoCompositor),
            None => Connection::connect_to_env(),
        }
        .context("connect to Wayland (is the compositor up?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut globals = Globals {
            layout,
            ..Globals::default()
        };
        queue
            .roundtrip(&mut globals)
            .context("Wayland registry roundtrip")?;

        let pointer_mgr = globals
            .pointer_mgr
            .clone()
            .context("compositor lacks zwlr_virtual_pointer_manager_v1")?;
        let keyboard_mgr = globals
            .keyboard_mgr
            .clone()
            .context("compositor lacks zwp_virtual_keyboard_manager_v1")?;
        let seat = globals
            .seat
            .clone()
            .context("compositor advertised no wl_seat")?;

        // First roundtrip bound the outputs; `name` events land on this one. Resolve before create.
        queue
            .roundtrip(&mut globals)
            .context("Wayland output-name roundtrip")?;

        let aim = crate::aim();
        let bound = globals.bind_target(&aim.target);
        let pointer = pointer_mgr.create_virtual_pointer_with_output(
            Some(&seat),
            bound.map(|o| &o.proxy),
            &qh,
            (),
        );
        let (bound_output, bound_name) =
            (bound.map(|o| o.global), bound.and_then(|o| o.name.clone()));
        let keyboard = keyboard_mgr.create_virtual_keyboard(&seat, &qh, ());

        // Wire keys are US-positional; this keymap is the host layout or ISO keys
        // type as US neighbours. `pf_host_config::layout`, not empty names: those fall
        // through to `XKB_DEFAULT_*`, which a Wayland session does not export.
        let resolved = pf_host_config::layout::system_layout();
        let (rules, model, layout, variant, options) = resolved.names.as_args();
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            rules,
            model,
            layout,
            variant,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .with_context(|| {
            format!(
                "compile xkb keymap {} (from {})",
                resolved.names.describe(),
                resolved.source
            )
        })?;
        tracing::info!(
            layout = %resolved.names.describe(),
            source = %resolved.source,
            "virtual keyboard keymap compiled"
        );
        let keymap_str = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let xkb_state = xkb::State::new(&keymap);

        let file = memfd_with(&keymap_str)?;
        let size = keymap_str.len() as u32 + 1; // include the trailing NUL
        keyboard.keymap(1 /* XKB_V1 */, file.as_fd(), size);
        queue
            .roundtrip(&mut globals)
            .context("keymap upload roundtrip")?;
        conn.flush().ok();

        tracing::info!(
            outputs = globals.outputs.len(),
            want = ?aim.target.name,
            bound = ?bound_name,
            layout,
            "wlroots virtual input ready (pointer + keyboard)"
        );
        Ok(Self {
            conn,
            queue,
            globals,
            pointer,
            bound_output,
            lookup_gen: aim.generation,
            screens: Vec::new(),
            active: 0,
            plan: Plan::default(),
            keyboard,
            xkb_state,
            _keymap_file: file,
            text: None,
            scroll: ScrollMapper::new(ScrollBackend::Wlr),
        })
    }

    /// Recreate the pointer when the streamed head changes. The protocol maps
    /// `motion_absolute` onto the output given at create and has no re-aim.
    /// Call immediately before the motion so the new device's first position
    /// is this sample, not the compositor's default.
    ///
    /// Match by name, never by size: `MouseMoveAbs` extent is the client's
    /// letterboxed content rect, not the streamed mode. A head created since the
    /// last read takes two roundtrips, one to bind it and one for its name; they
    /// run once per aim generation, not per motion. The head and its generation
    /// come from one read of the aim. In layout mode `bind_target` is `None`: no re-make.
    fn retarget(&mut self) {
        let aim = crate::aim();
        if self.globals.lacks(&aim.target) && self.lookup_gen != aim.generation {
            self.lookup_gen = aim.generation;
            for _ in 0..2 {
                if self.queue.roundtrip(&mut self.globals).is_err() {
                    break;
                }
            }
        }
        let target = self.globals.bind_target(&aim.target);
        let to = target.map(|o| o.global);
        if to == self.bound_output {
            return;
        }
        let (target, name) = (
            target.map(|o| o.proxy.clone()),
            target.and_then(|o| o.name.clone()),
        );
        let (Some(mgr), Some(seat)) = (self.globals.pointer_mgr.clone(), self.globals.seat.clone())
        else {
            return;
        };
        // Release first; destroy mid-press leaves a stuck host button.
        if self.release_buttons(self.now_ms()) {
            self.pointer.frame();
        }
        self.pointer.destroy();
        self.pointer = mgr.create_virtual_pointer_with_output(
            Some(&seat),
            target.as_ref(),
            &self.queue.handle(),
            (),
        );
        tracing::info!(
            output = ?name,
            "wlroots virtual pointer re-aimed (absolute input now maps into this output)"
        );
        self.bound_output = to;
    }

    /// The pointer of further screen `screen`, bound to its output and made on first use;
    /// `None` without the pointer manager or seat.
    fn screen_pointer(&mut self, screen: u8) -> Option<&ZwlrVirtualPointerV1> {
        let generation = crate::aim().screens_generation;
        let target = crate::stream_target_for(screen);
        let at = self.screens.iter().position(|p| p.screen == screen);
        if self.globals.lacks(&target) && at.map(|i| self.screens[i].lookup_gen) != Some(generation)
        {
            for _ in 0..2 {
                if self.queue.roundtrip(&mut self.globals).is_err() {
                    break;
                }
            }
            if let Some(i) = at {
                self.screens[i].lookup_gen = generation;
            }
        }
        let output = self.globals.bind_target(&target);
        let bound = output.map(|o| o.global);
        if let Some(i) = at.filter(|&i| self.screens[i].bound == bound) {
            return Some(&self.screens[i].pointer);
        }
        let output = output.map(|o| o.proxy.clone());
        let (mgr, seat) = (
            self.globals.pointer_mgr.clone()?,
            self.globals.seat.clone()?,
        );
        let pointer = mgr.create_virtual_pointer_with_output(
            Some(&seat),
            output.as_ref(),
            &self.queue.handle(),
            (),
        );
        tracing::info!(screen, output = ?target.name, "wlroots virtual pointer made for a further screen");
        let made = ScreenPointer {
            screen,
            pointer,
            bound,
            lookup_gen: generation,
        };
        let i = match at {
            Some(i) => {
                std::mem::replace(&mut self.screens[i], made)
                    .pointer
                    .destroy();
                i
            }
            None => {
                self.screens.push(made);
                self.screens.len() - 1
            }
        };
        Some(&self.screens[i].pointer)
    }

    /// The pointer that moved last ([`Self::active`]).
    fn current(&self) -> &ZwlrVirtualPointerV1 {
        (self.screens.iter())
            .find(|p| p.screen == self.active)
            .map_or(&self.pointer, |p| &p.pointer)
    }

    /// Read the socket, dispatch, flush. `dispatch_pending` does not read, so
    /// without `read()` a `wl_output` created after `open` is never seen and
    /// protocol errors sit unread. `WouldBlock` is the idle case, not an error.
    fn pump(&mut self) -> Result<()> {
        // `prepare_read` refuses a guard while events are queued; dispatch first.
        self.queue
            .dispatch_pending(&mut self.globals)
            .context("wayland dispatch")?;
        if let Some(guard) = self.conn.prepare_read() {
            match guard.read() {
                Ok(_) => {
                    self.queue
                        .dispatch_pending(&mut self.globals)
                        .context("wayland dispatch (post-read)")?;
                }
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("wayland read"),
            }
        }
        self.conn.flush().context("wayland flush")?;
        Ok(())
    }

    /// Wire `time`: `CLOCK_MONOTONIC` ms, wrapping like the compositor's own stamps.
    fn now_ms(&self) -> u32 {
        (crate::monotonic_us() / 1_000) as u32
    }

    /// Release every button the current pointer holds, unframed. `true` when there was one.
    fn release_buttons(&mut self, t: u32) -> bool {
        let mut any = false;
        for op in self.plan.lift(false, true) {
            if let Op::Button { evdev, .. } = op {
                self.current()
                    .button(t, evdev, wl_pointer::ButtonState::Released);
                any = true;
            }
        }
        any
    }

    /// Emit one planned op. Only absolute motion depends on the bound output.
    fn emit(&mut self, t: u32, op: Op) -> Result<()> {
        match op {
            Op::Motion { dx, dy } => {
                self.current().motion(t, dx.into(), dy.into());
                self.current().frame();
            }
            // A screen past the session's lands on the first, as on KWin and libei.
            Op::Absolute(abs) if (1..=crate::EXTRA_SCREENS).contains(&usize::from(abs.screen)) => {
                if let Some(p) = self.screen_pointer(abs.screen).cloned() {
                    let (x, y, w, h) =
                        (self.globals).motion(|| crate::stream_target_for(abs.screen), &abs);
                    p.motion_absolute(t, x, y, w, h);
                    p.frame();
                    self.active = abs.screen;
                }
            }
            Op::Absolute(abs) => {
                self.active = 0;
                self.retarget();
                let t = self.now_ms(); // `retarget` may have consumed time releasing buttons
                let (x, y, w, h) = self.globals.motion(|| crate::aim().target, &abs);
                self.pointer.motion_absolute(t, x, y, w, h);
                self.pointer.frame();
            }
            Op::Button { evdev, down } => {
                let st = if down {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                self.current().button(t, evdev, st);
                self.current().frame();
            }
            Op::Key { evdev, down } => {
                self.keyboard.key(t, evdev.into(), down.into());
                self.xkb_state
                    .update_key(xkb_keycode(evdev), key_direction(down));
                self.send_modifiers();
            }
            Op::Text(cp) => self.type_text(cp)?,
            // No virtual-touch protocol on wlroots: this backend drops touch.
            Op::TouchDown { .. } | Op::TouchMove { .. } | Op::TouchUp { .. } => {}
        }
        Ok(())
    }

    /// One Unicode scalar on the text device. Controls are dropped; Enter/Backspace/Tab
    /// ride the VK path.
    fn type_text(&mut self, cp: u32) -> Result<()> {
        let Some(ch) = char::from_u32(cp) else {
            return Ok(());
        };
        if ch.is_control() {
            return Ok(());
        }
        if self.text.is_none() {
            let (Some(mgr), Some(seat)) =
                (self.globals.keyboard_mgr.clone(), self.globals.seat.clone())
            else {
                return Ok(());
            };
            let kb = mgr.create_virtual_keyboard(&seat, &self.queue.handle(), ());
            self.text = Some(TextKeyboard {
                keyboard: kb,
                chars: Vec::new(),
                _keymap_file: None,
            });
        }
        let t = self.now_ms();
        let text = self.text.as_mut().expect("created above");
        let code = match text.chars.iter().position(|&c| c == ch) {
            Some(i) => (i + 1) as u32,
            None => {
                if text.chars.len() >= TEXT_KEYMAP_MAX {
                    text.chars.clear();
                }
                text.chars.push(ch);
                let keymap_str = text_keymap(&text.chars);
                let file = memfd_with(&keymap_str)?;
                text.keyboard.keymap(
                    1, /* XKB_V1 */
                    file.as_fd(),
                    keymap_str.len() as u32 + 1,
                );
                text._keymap_file = Some(file);
                text.chars.len() as u32
            }
        };
        text.keyboard.key(t, code, 1);
        text.keyboard.key(t, code, 0);
        Ok(())
    }

    /// Re-assert the modifier mask after every key, repeats included: the compositor's own
    /// per-key press count can drift, and `modifiers` is what overrides it.
    fn send_modifiers(&mut self) {
        let depressed = self.xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        let latched = self.xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED);
        let locked = self.xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED);
        let group = self.xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        self.keyboard.modifiers(depressed, latched, locked, group);
    }

    /// Execute a normalized-scroll plan: `axis_source` ahead of the axis ops it
    /// describes, `axis_stop` closing a gesture, `frame` the boundaries the
    /// plan drew. `axis_stop` cannot tell a cancel from an end — the protocol
    /// has no flag for it.
    fn emit_scroll_ops(&mut self, t: u32, ops: Vec<ScrollOp>) {
        let axis = |horizontal: bool| {
            if horizontal {
                wl_pointer::Axis::HorizontalScroll
            } else {
                wl_pointer::Axis::VerticalScroll
            }
        };
        let pointer = self.current();
        for op in ops {
            match op {
                ScrollOp::AxisSource(src) => {
                    pointer.axis_source(match src {
                        AxisSource::Wheel => wl_pointer::AxisSource::Wheel,
                        AxisSource::Finger => wl_pointer::AxisSource::Finger,
                    });
                }
                ScrollOp::Continuous { horizontal, value } => {
                    pointer.axis(t, axis(horizontal), value);
                }
                ScrollOp::DiscreteDetents {
                    horizontal,
                    value,
                    detents,
                } => {
                    pointer.axis_discrete(t, axis(horizontal), value, detents);
                }
                ScrollOp::Stop { horizontal, .. } => {
                    pointer.axis_stop(t, axis(horizontal));
                }
                ScrollOp::Frame => pointer.frame(),
                // ei/Win32 vocabulary; a wlr plan never emits it.
                ScrollOp::Discrete120 { .. } => {}
            }
        }
    }
}

impl Drop for WlrootsInjector {
    /// Release what the devices still hold before they go: the compositor keeps a closed
    /// virtual keyboard's keys pressed (Hyprland's `release_pressed_on_close` is off by
    /// default on 0.57), and an open gesture's axis interaction outlives the pointer's destroy.
    fn drop(&mut self) {
        let t = self.now_ms();
        for op in self.plan.lift(true, false) {
            if let Op::Key { evdev, .. } = op {
                self.keyboard.key(t, evdev.into(), 0);
                self.xkb_state
                    .update_key(xkb_keycode(evdev), key_direction(false));
            }
        }
        self.send_modifiers();
        self.release_buttons(t);
        let ops = self.scroll.cancel_all();
        self.emit_scroll_ops(t, ops);
        self.current().frame();
        for p in self.screens.drain(..) {
            p.pointer.destroy();
        }
        let _ = self.conn.flush();
    }
}

impl InputInjector for WlrootsInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        let t = self.now_ms();
        if matches!(event.kind, InputKind::MouseScroll | InputKind::Scroll) {
            // Legacy and normalized scroll lower through the shared mapper; its Frame ops draw
            // the frame boundaries (a stop never shares one with a delta).
            let ops = self.scroll.plan(event);
            self.emit_scroll_ops(t, ops);
        } else if let Some(op) = self.plan.step(event) {
            self.emit(t, op)?;
        }
        self.pump()
    }

    fn deadline(&self) -> Option<std::time::Instant> {
        self.scroll.stop_due()
    }

    fn on_deadline(&mut self) -> Result<()> {
        let ops = self.scroll.flush_due(std::time::Instant::now());
        if ops.is_empty() {
            return Ok(());
        }
        let t = self.now_ms();
        self.emit_scroll_ops(t, ops);
        self.pump()
    }
}

fn xkb_keycode(evdev: u16) -> xkb::Keycode {
    xkb::Keycode::new(evdev as u32 + 8) // xkb keycodes are evdev + 8
}

fn key_direction(down: bool) -> xkb::KeyDirection {
    if down {
        xkb::KeyDirection::Down
    } else {
        xkb::KeyDirection::Up
    }
}

/// Keycode `i + 9` (wire `i + 1`) types `chars[i]` as Unicode keysym `U<hex>`.
/// Types/compat `include "complete"` is the `wtype` shape; system XKB data is
/// already required by `open`.
fn text_keymap(chars: &[char]) -> String {
    use std::fmt::Write as _;
    let mut keycodes = String::new();
    let mut symbols = String::new();
    for (i, ch) in chars.iter().enumerate() {
        let _ = writeln!(keycodes, "        <T{i}> = {};", i + 9);
        let _ = writeln!(symbols, "        key <T{i}> {{ [ U{:04X} ] }};", *ch as u32);
    }
    format!(
        "xkb_keymap {{\n\
             xkb_keycodes \"punktfunk-text\" {{\n\
                 minimum = 8;\n\
                 maximum = {};\n\
         {keycodes}\
             }};\n\
             xkb_types \"punktfunk-text\" {{ include \"complete\" }};\n\
             xkb_compatibility \"punktfunk-text\" {{ include \"complete\" }};\n\
             xkb_symbols \"punktfunk-text\" {{\n{symbols}    }};\n\
         }};\n",
        chars.len() + 9,
    )
}

/// Anonymous file of `s` plus a trailing NUL; the compositor's keymap mmap needs the NUL.
fn memfd_with(s: &str) -> Result<std::fs::File> {
    let name = b"punktfunk-keymap\0";
    // SAFETY: `name` is a byte-string literal with an explicit trailing NUL, so `name.as_ptr()` is a
    // valid NUL-terminated C string; `memfd_create` only reads that name (copying it) and creates an
    // anonymous file, returning a fresh fd (or -1). `MFD_CLOEXEC` is a valid flag. The 'static literal
    // outlives the synchronous call and nothing aliases it. The result is checked `< 0` below.
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is the fresh memfd `memfd_create` just returned and checked `>= 0`; it is a unique
    // open fd nothing else owns, so `File` takes sole ownership and closes it exactly once on drop —
    // no alias, no double-close.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(s.as_bytes()).context("write keymap")?;
    f.write_all(&[0]).context("write keymap NUL")?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compositor reads `x / x_extent` over the layout box: the extent is the box and `x`
    /// the head's offset in it, in 1/16 logical pixels.
    #[test]
    fn layout_motion_lands_on_the_streamed_head() {
        let desk = (0, 0, 1920, 1080);
        let ours = (1920, 0, 2560, 1600);
        assert_eq!(
            layout_point(&[desk, ours], ours, (0.5, 0.5)),
            (3200 * 16, 800 * 16, 4480 * 16, 1600 * 16)
        );
        // A head left of the origin moves the box's origin, not the head.
        let left = (-1920, 200, 1920, 1080);
        assert_eq!(
            layout_point(&[left, ours], ours, (0.0, 0.0)),
            (3840 * 16, 0, 6400 * 16, 1600 * 16)
        );
        // The far edge stays on the head, never on its neighbour.
        let edge = f64::from(crate::plan::ABS_EDGE);
        let right = (4480, 0, 1920, 1080);
        let (x, y, _, _) = layout_point(&[ours, right], ours, (edge, edge));
        assert!(x < 2560 * 16 && y < 1600 * 16, "{x} {y}");
        // A sole head maps onto itself.
        assert_eq!(
            layout_point(&[ours], ours, (0.25, 0.75)),
            (640 * 16, 1200 * 16, 2560 * 16, 1600 * 16)
        );
    }

    const VK_LWIN: u32 = 0x5B;
    const VK_SHIFT: u32 = 0x10;
    const VK_LSHIFT: u32 = 0xA0;

    /// xkb counts presses per key, so two Super downs and one up leave Super depressed for
    /// good. Fed only the plan's key ops, the same stream clears it. A Shift the ring presses
    /// and lifts under a held Left Shift leaves Shift depressed until Left Shift goes up.
    #[test]
    fn xkb_counts_presses_so_only_the_plans_keys_may_feed_it() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let Some(keymap) = xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) else {
            eprintln!("no xkb data on this box — skipped");
            return;
        };
        let depressed = |stream: &[(u32, bool)], gate: bool| {
            let mut st = xkb::State::new(&keymap);
            let mut plan = Plan::default();
            for &(vk, down) in stream {
                let kind = if down {
                    InputKind::KeyDown
                } else {
                    InputKind::KeyUp
                };
                let ev = InputEvent {
                    kind,
                    _pad: [0; 3],
                    code: vk,
                    x: 0,
                    y: 0,
                    flags: 0,
                };
                let key = match plan.step(&ev) {
                    Some(Op::Key { evdev, down }) => Some((evdev, down)),
                    _ => None,
                };
                let wire = crate::vk_to_evdev(vk as u8).map(|evdev| (evdev, down));
                if let Some((evdev, down)) = if gate { key } else { wire } {
                    st.update_key(xkb_keycode(evdev), key_direction(down));
                }
            }
            st.serialize_mods(xkb::STATE_MODS_DEPRESSED)
        };
        let repeat = [(VK_LWIN, true), (VK_LWIN, true), (VK_LWIN, false)];
        assert_ne!(depressed(&repeat, false), 0, "ungated: Super pinned");
        assert_eq!(depressed(&repeat, true), 0, "gated: the one up clears it");
        let ring = [(VK_LSHIFT, true), (VK_SHIFT, true), (VK_SHIFT, false)];
        assert_ne!(depressed(&ring, true), 0, "Left Shift still held");
        let lifted = [ring.as_slice(), &[(VK_LSHIFT, false)]].concat();
        assert_eq!(depressed(&lifted, true), 0);
    }
}
