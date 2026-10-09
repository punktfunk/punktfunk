//! Virtual pointer and keyboard on wlroots (`zwlr_virtual_pointer_manager_v1`,
//! `zwp_virtual_keyboard_manager_v1`).
//!
//! Absolute motion is mapped onto the `wl_output` the pointer was created
//! with; there is no re-aim request. The streamed head is published by name
//! in [`crate::stream_output`] and rebound by [`WlrootsInjector::retarget`].
//! A miss binds no output (whole-layout mapping). First-advertised is the
//! operator's physical display, never the session head.

use super::{gs_button_to_evdev, vk_to_evdev, InputEvent, InputInjector};
use crate::head_pick::{self, HeadFacts, StreamTarget};
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
}

#[derive(Default)]
struct Globals {
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<WlSeat>,
    /// Every advertised head, advertisement order. The first is the operator's
    /// physical display; the streamed head is added later and is never first.
    outputs: Vec<Output>,
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
                    let proxy = registry.bind(name, version.min(WL_OUTPUT_MAX), qh, name);
                    state.outputs.push(Output {
                        global: name,
                        proxy,
                        name: None,
                    });
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
    /// Buttons held on `pointer`. Released before destroy; the compositor will not.
    pressed: Vec<u32>,
    keyboard: ZwpVirtualKeyboardV1,
    /// Keys held on `keyboard` (evdev). Only a transition reaches `xkb_state`: it counts
    /// presses per key, and this injector outlives the session, so an unmatched down
    /// would pin its modifier for the host's lifetime.
    held_keys: Vec<u16>,
    xkb_state: xkb::State,
    _keymap_file: std::fs::File, // compositor mmaps this memfd; drop would unmap it
    text: Option<TextKeyboard>,
    /// Scroll lowering, legacy and normalized; holds the sub-detent residue.
    scroll: ScrollMapper,
}

/// Distinct chars before the text keymap restarts. Keycodes start at 9; xkb max is 255.
const TEXT_KEYMAP_MAX: usize = 200;

/// Separate `zwp_virtual_keyboard` for [`InputKind::TextInput`]. Keymap re-uploads
/// must not touch the main device's layout or modifier state.
struct TextKeyboard {
    keyboard: ZwpVirtualKeyboardV1,
    /// `chars[i]` types on wire keycode `i + 1` (xkb `i + 9`).
    chars: Vec<char>,
    _keymap_file: Option<std::fs::File>, // compositor mmaps this memfd; drop would unmap it
}

impl WlrootsInjector {
    pub fn open() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("connect to Wayland (is Sway up + WAYLAND_DISPLAY/XDG_RUNTIME_DIR set?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut globals = Globals::default();
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

        let want = crate::stream_target();
        let bound = globals.resolve(&want);
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
        // type as US neighbours. `crate::layout`, not empty names: those fall
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
            want = ?want.name,
            bound = ?bound_name,
            "wlroots virtual input ready (pointer + keyboard)"
        );
        Ok(Self {
            conn,
            queue,
            globals,
            pointer,
            bound_output,
            lookup_gen: crate::aim_gen(),
            pressed: Vec::new(),
            keyboard,
            held_keys: Vec::new(),
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
    /// run once per aim generation, not per motion.
    fn retarget(&mut self) {
        let want = crate::stream_target();
        if self.globals.lacks(&want) && self.lookup_gen != crate::aim_gen() {
            self.lookup_gen = crate::aim_gen();
            for _ in 0..2 {
                if self.queue.roundtrip(&mut self.globals).is_err() {
                    break;
                }
            }
        }
        let target = self.globals.resolve(&want);
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
        if !self.pressed.is_empty() {
            let t = self.now_ms();
            for btn in std::mem::take(&mut self.pressed) {
                self.pointer
                    .button(t, btn, wl_pointer::ButtonState::Released);
            }
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
        for op in ops {
            match op {
                ScrollOp::AxisSource(src) => {
                    self.pointer.axis_source(match src {
                        AxisSource::Wheel => wl_pointer::AxisSource::Wheel,
                        AxisSource::Finger => wl_pointer::AxisSource::Finger,
                    });
                }
                ScrollOp::Continuous { horizontal, value } => {
                    self.pointer.axis(t, axis(horizontal), value);
                }
                ScrollOp::DiscreteDetents {
                    horizontal,
                    value,
                    detents,
                } => {
                    self.pointer
                        .axis_discrete(t, axis(horizontal), value, detents);
                }
                ScrollOp::Stop { horizontal, .. } => {
                    self.pointer.axis_stop(t, axis(horizontal));
                }
                ScrollOp::Frame => self.pointer.frame(),
                // ei/Win32 vocabulary; a wlr plan never emits it.
                ScrollOp::Discrete120 { .. } => {}
            }
        }
    }
}

impl Drop for WlrootsInjector {
    /// Release what the devices still hold before they go: the compositor keeps a closed
    /// virtual keyboard's keys pressed (Hyprland's `release_pressed_on_close` is off by
    /// default), and an open gesture's axis interaction outlives the pointer's destroy.
    fn drop(&mut self) {
        let t = self.now_ms();
        for evdev in std::mem::take(&mut self.held_keys) {
            self.keyboard.key(t, evdev as u32, 0);
            self.xkb_state
                .update_key(xkb_keycode(evdev), key_direction(false));
        }
        self.send_modifiers();
        for btn in std::mem::take(&mut self.pressed) {
            self.pointer
                .button(t, btn, wl_pointer::ButtonState::Released);
        }
        let ops = self.scroll.cancel_all();
        self.emit_scroll_ops(t, ops);
        self.pointer.frame();
        let _ = self.conn.flush();
    }
}

impl InputInjector for WlrootsInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        let t = self.now_ms();
        match event.kind {
            InputKind::MouseMove => {
                self.pointer.motion(t, event.x as f64, event.y as f64);
                self.pointer.frame();
            }
            InputKind::MouseMoveAbs => {
                let w = (event.flags >> 16) & 0xffff;
                let h = event.flags & 0xffff;
                if w > 0 && h > 0 {
                    // Absolute motion maps onto the bound output; only this arm depends on it.
                    self.retarget();
                    let t = self.now_ms(); // `retarget` may have consumed time releasing buttons
                                           // `x == w` would map onto the edge, which a neighbouring head owns.
                    let x = event.x.clamp(0, w as i32 - 1) as u32;
                    let y = event.y.clamp(0, h as i32 - 1) as u32;
                    self.pointer.motion_absolute(t, x, y, w, h);
                    self.pointer.frame();
                }
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                if let Some(btn) = gs_button_to_evdev(event.code) {
                    let st = if event.kind == InputKind::MouseButtonDown {
                        if !self.pressed.contains(&btn) {
                            self.pressed.push(btn);
                        }
                        wl_pointer::ButtonState::Pressed
                    } else {
                        self.pressed.retain(|&b| b != btn);
                        wl_pointer::ButtonState::Released
                    };
                    self.pointer.button(t, btn, st);
                    self.pointer.frame();
                }
            }
            // Legacy and normalized scroll lower through the shared mapper;
            // its Frame ops draw the frame boundaries (a stop never shares one
            // with a delta).
            InputKind::MouseScroll | InputKind::Scroll => {
                let ops = self.scroll.plan(event);
                self.emit_scroll_ops(t, ops);
            }
            InputKind::KeyDown | InputKind::KeyUp => {
                let down = event.kind == InputKind::KeyDown;
                if let Some(evdev) = vk_to_evdev(event.code as u8) {
                    self.keyboard.key(t, evdev as u32, if down { 1 } else { 0 });
                    if note_key(&mut self.held_keys, evdev, down) {
                        self.xkb_state
                            .update_key(xkb_keycode(evdev), key_direction(down));
                    }
                    self.send_modifiers();
                } else {
                    tracing::debug!(vk = event.code, "unmapped VK keycode — dropped");
                }
            }
            InputKind::TextInput => {
                self.type_text(event.code)?;
            }
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => {}
            // No virtual-touch protocol here; touch is libei only.
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => {}
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

/// Record a key on the held set; `true` when it is a transition. A down for a held key is a
/// repeat (or the up was lost to the client OS); an up for a key not held is stale. Neither
/// may reach `xkb_state`, whose per-key press count only unwinds with matching ups.
fn note_key(held: &mut Vec<u16>, evdev: u16, down: bool) -> bool {
    let was_held = held.contains(&evdev);
    if down && !was_held {
        held.push(evdev);
    } else if !down {
        held.retain(|&k| k != evdev);
    }
    down != was_held
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

    const KEY_LEFTMETA: u16 = 125;
    const KEY_A: u16 = 30;

    /// A repeated down and a stale up are not transitions; the one up after repeats is.
    #[test]
    fn repeats_and_stale_ups_are_not_transitions() {
        let mut held = Vec::new();
        assert!(note_key(&mut held, KEY_LEFTMETA, true));
        assert!(!note_key(&mut held, KEY_LEFTMETA, true), "auto-repeat");
        assert!(!note_key(&mut held, KEY_LEFTMETA, true));
        assert!(note_key(&mut held, KEY_A, true));
        assert!(note_key(&mut held, KEY_LEFTMETA, false));
        assert!(
            !note_key(&mut held, KEY_LEFTMETA, false),
            "session-end release after a real up"
        );
        assert_eq!(held, [KEY_A]);
    }

    /// The trap the gate exists for: xkb counts presses per key, so two Super downs and one up
    /// leave Super depressed for good. Gated on transitions, the same stream clears it.
    #[test]
    fn xkb_counts_presses_so_only_transitions_may_feed_it() {
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
        let stream = [
            (KEY_LEFTMETA, true),
            (KEY_LEFTMETA, true),
            (KEY_LEFTMETA, false),
        ];
        let depressed = |gate: bool| {
            let mut st = xkb::State::new(&keymap);
            let mut held = Vec::new();
            for (k, down) in stream {
                if !gate || note_key(&mut held, k, down) {
                    st.update_key(xkb_keycode(k), key_direction(down));
                }
            }
            st.serialize_mods(xkb::STATE_MODS_DEPRESSED)
        };
        assert_ne!(depressed(false), 0, "ungated: Super pinned after the up");
        assert_eq!(depressed(true), 0, "gated: the one up clears it");
    }
}
