//! Native `wl_pointer` scroll capture on SDL's own Wayland connection.
//!
//! SDL owns socket reads. A private queue retains native source, counts and
//! stops without replacing SDL's listeners. Each seat owns its gesture state;
//! its pointer only forwards events while over the streaming surface.
//! The retained window context outlives the borrowed display and all proxies.

use crate::scroll::{NativeAxisSource, WaylandScrollFrame};
use anyhow::Context;
use punktfunk_core::input::InputEvent;
use sdl3::video::WindowContext;
use std::collections::HashMap;
use std::sync::Arc;
use wayland_backend::client::Backend;
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};

struct SeatState {
    seat: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    frame: WaylandScrollFrame,
    entered: bool,
}

impl SeatState {
    fn release_pointer(&mut self) {
        if let Some(pointer) = self.pointer.take() {
            pointer.release();
        }
        self.entered = false;
    }
}

struct WlState {
    seats: HashMap<u32, SeatState>,
    out: Vec<InputEvent>,
    surface_id: u32,
}

impl WlState {
    fn bind_seat(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        if version < 5 || self.seats.contains_key(&name) {
            return;
        }
        let seat = registry.bind(name, version.min(9), qh, name);
        self.seats.insert(
            name,
            SeatState {
                seat,
                pointer: None,
                frame: WaylandScrollFrame::new(),
                entered: false,
            },
        );
    }
}

pub struct WaylandScroll {
    conn: Connection,
    queue: EventQueue<WlState>,
    state: WlState,
    globals: Option<GlobalList>,
    // SAFETY: field drop order keeps SDL's display alive past every borrowed proxy and queue.
    _window: Arc<WindowContext>,
}

impl WaylandScroll {
    pub fn new(window: &sdl3::video::Window) -> anyhow::Result<Option<Self>> {
        if window.subsystem().current_video_driver() != "wayland" {
            return Ok(None);
        }
        // SAFETY: the live window owns both pointers; neither pointer transfers ownership.
        let (display, surface) = unsafe {
            let props = sdl3::sys::video::SDL_GetWindowProperties(window.raw());
            (
                sdl3::sys::properties::SDL_GetPointerProperty(
                    props,
                    sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_DISPLAY_POINTER,
                    std::ptr::null_mut(),
                ),
                sdl3::sys::properties::SDL_GetPointerProperty(
                    props,
                    sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER,
                    std::ptr::null_mut(),
                ),
            )
        };
        if display.is_null() || surface.is_null() {
            return Ok(None);
        }
        // SAFETY: window.context() is retained until after the foreign backend is dropped.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) =
            registry_queue_init::<WlState>(&conn).context("initialize scroll registry")?;
        // SAFETY: surface is SDL's live wl_surface proxy on this display.
        let surface_id = unsafe {
            (wayland_sys::client::wayland_client_handle().wl_proxy_get_id)(surface.cast())
        };
        let mut capture = Self {
            conn,
            queue,
            state: WlState {
                seats: HashMap::new(),
                out: Vec::new(),
                surface_id,
            },
            globals: Some(globals),
            _window: window.context(),
        };
        let globals = capture.globals.as_ref().unwrap();
        for global in globals.contents().clone_list() {
            if global.interface == "wl_seat" {
                capture.state.bind_seat(
                    globals.registry(),
                    global.name,
                    global.version,
                    &capture.queue.handle(),
                );
            }
        }
        capture
            .queue
            .roundtrip(&mut capture.state)
            .context("initialize scroll seats")?;
        capture.conn.flush().context("request scroll pointers")?;
        Ok(Some(capture))
    }

    pub fn available(&self) -> bool {
        self.state.seats.values().any(|s| s.pointer.is_some())
    }

    pub fn drain(&mut self) -> anyhow::Result<Vec<InputEvent>> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("dispatch scroll events")?;
        if let Some(error) = self.conn.protocol_error() {
            anyhow::bail!("scroll protocol: {error}");
        }
        match self.conn.flush() {
            Err(wayland_backend::client::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
            Ok(()) => {}
        }
        Ok(std::mem::take(&mut self.state.out))
    }

    pub fn cancel(&mut self) -> Vec<InputEvent> {
        self.state.out.clear();
        self.state
            .seats
            .values_mut()
            .flat_map(|s| s.frame.cancel())
            .collect()
    }
}

impl Drop for WaylandScroll {
    fn drop(&mut self) {
        for seat in self.state.seats.values_mut() {
            seat.release_pointer();
            seat.seat.release();
        }
        if let Some(globals) = self.globals.take() {
            let id = globals.registry().id();
            globals.destroy();
            let _ = self.conn.backend().destroy_object(&id);
        }
        let _ = self.conn.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WlState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_seat" => {
                state.bind_seat(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(mut seat) = state.seats.remove(&name) {
                    state.out.extend(seat.frame.cancel());
                    seat.release_pointer();
                    seat.seat.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, u32> for WlState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        name: &u32,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let Some(slot) = state.seats.get_mut(name) else {
                return;
            };
            let has_pointer = matches!(capabilities, WEnum::Value(caps) if caps.contains(wl_seat::Capability::Pointer));
            if has_pointer && slot.pointer.is_none() {
                slot.pointer = Some(seat.get_pointer(qh, *name));
            } else if !has_pointer && slot.pointer.is_some() {
                state.out.extend(slot.frame.cancel());
                slot.release_pointer();
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, u32> for WlState {
    fn event(
        state: &mut Self,
        _pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        name: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(seat) = state.seats.get_mut(name) else {
            return;
        };
        use wl_pointer::Event;
        match event {
            Event::Enter { surface, .. } => {
                state.out.extend(seat.frame.cancel());
                seat.entered = surface.id().protocol_id() == state.surface_id;
            }
            Event::Leave { .. } => {
                seat.entered = false;
                state.out.extend(seat.frame.cancel());
            }
            _ if !seat.entered => {}
            Event::Axis { axis, value, .. } => {
                if let Some(a) = axis_index(axis) {
                    seat.frame.axis(a, value);
                }
            }
            Event::AxisSource { axis_source } => {
                seat.frame.axis_source(match axis_source {
                    WEnum::Value(
                        wl_pointer::AxisSource::Wheel | wl_pointer::AxisSource::WheelTilt,
                    ) => NativeAxisSource::Wheel,
                    WEnum::Value(wl_pointer::AxisSource::Finger) => NativeAxisSource::Finger,
                    WEnum::Value(wl_pointer::AxisSource::Continuous) => {
                        NativeAxisSource::Continuous
                    }
                    _ => NativeAxisSource::Unknown,
                });
            }
            Event::AxisStop { axis, .. } => {
                if let Some(a) = axis_index(axis) {
                    seat.frame.stop(a);
                }
            }
            Event::AxisDiscrete { axis, discrete } => {
                if let Some(a) = axis_index(axis) {
                    seat.frame.discrete(a, discrete);
                }
            }
            Event::AxisValue120 { axis, value120 } => {
                if let Some(a) = axis_index(axis) {
                    seat.frame.value120(a, value120);
                }
            }
            Event::Frame => state.out.extend(seat.frame.frame()),
            _ => {}
        }
    }
}

fn axis_index(axis: WEnum<wl_pointer::Axis>) -> Option<u32> {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => Some(0),
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => Some(1),
        _ => None,
    }
}
