//! The compositor's own stamp for each swapchain present: `wp_presentation` feedback on SDL's
//! `wl_surface`.
//!
//! A Vulkan driver's present stamp is whatever its WSI measures; NVIDIA's lands within a
//! fraction of a millisecond of the present on a fixed-refresh panel, which no scanout can.
//! The compositor reports the flip it made, so a feedback object created on the surface
//! before the driver's commit rides that commit and comes back with the compositor's time.
//!
//! SDL owns the socket: its pump reads the events and [`SurfaceFeedback::take`] dispatches
//! them from a private queue, as the native lane does. Times arrive on CLOCK_MONOTONIC and
//! move onto the session clock as they land. The `zero_copy` kind bit is the only word a
//! client gets on whether the compositor scans its buffer out; the stats line shows it.

use crate::wl_side::{monotonic_to_realtime, release_registry, SdlWayland};
use anyhow::{Context as _, Result};
use sdl3::video::WindowContext;
use std::collections::HashMap;
use std::sync::Arc;
use wayland_client::globals::{GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::wp::content_type::v1::client::{
    wp_content_type_manager_v1 as ctm, wp_content_type_v1 as ct,
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation, wp_presentation_feedback as pfb,
};

const CLOCK_MONOTONIC: u32 = 1;

/// One present the compositor answered: the id the presenter gave it, and when the
/// compositor says it reached the screen (session clock), `None` for a discard.
pub struct Sample {
    pub present_id: u64,
    pub displayed_ns: Option<u64>,
    /// The compositor flipped the buffer itself, no copy.
    pub zero_copy: bool,
    /// The refresh the compositor reported with the frame, ns; 0 for none.
    pub refresh_ns: u32,
}

/// `wp_presentation_feedback.kind` bit: the buffer reached the screen without a copy.
const KIND_ZERO_COPY: u32 = 8;

#[derive(Default)]
struct State {
    clock_id: Option<u32>,
    /// Feedback objects in flight, by the present id they were made for.
    pending: HashMap<u64, pfb::WpPresentationFeedback>,
    samples: Vec<Sample>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore ctm::WpContentTypeManagerV1);
delegate_noop!(State: ignore ct::WpContentTypeV1);

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock_id = Some(clk_id);
        }
    }
}

impl Dispatch<pfb::WpPresentationFeedback, u64> for State {
    fn event(
        state: &mut Self,
        _: &pfb::WpPresentationFeedback,
        event: pfb::Event,
        present_id: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            pfb::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                flags,
                ..
            } => {
                state.pending.remove(present_id);
                let sec = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                let mono = sec * 1_000_000_000 + u64::from(tv_nsec);
                let kind = match flags {
                    WEnum::Value(k) => k.bits(),
                    WEnum::Unknown(v) => v,
                };
                state.samples.push(Sample {
                    present_id: *present_id,
                    displayed_ns: Some(monotonic_to_realtime(mono)),
                    zero_copy: kind & KIND_ZERO_COPY != 0,
                    refresh_ns: refresh,
                });
            }
            pfb::Event::Discarded => {
                state.pending.remove(present_id);
                state.samples.push(Sample {
                    present_id: *present_id,
                    displayed_ns: None,
                    zero_copy: false,
                    refresh_ns: 0,
                });
            }
            _ => {}
        }
    }
}

pub struct SurfaceFeedback {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    globals: Option<GlobalList>,
    surface: wl_surface::WlSurface,
    presentation: wp_presentation::WpPresentation,
    /// The window's content tag. Never destroyed here: dropping the handle sends nothing.
    _content_type: Option<ct::WpContentTypeV1>,
    dead: bool,
    // SAFETY: field drop order keeps SDL's display alive past every borrowed proxy and queue.
    _window: Arc<WindowContext>,
}

/// Feedback objects outstanding before new ones stop: a compositor that never answers must
/// not grow the map.
const MAX_PENDING: usize = 64;

impl SurfaceFeedback {
    /// The probe on SDL's Wayland connection, or `None` off Wayland or where the compositor
    /// lacks `wp_presentation` on CLOCK_MONOTONIC.
    pub fn new(window: &sdl3::video::Window) -> Result<Option<Self>> {
        if window.subsystem().current_video_driver() != "wayland" {
            return Ok(None);
        }
        let Some(sdl) = SdlWayland::of(window) else {
            return Ok(None);
        };
        let (conn, globals, mut queue) = sdl.connect::<State>("surface feedback registry")?;
        let qh = queue.handle();
        let presentation: wp_presentation::WpPresentation = match globals.bind(&qh, 1..=2, ()) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let surface = sdl.surface_proxy(&conn)?;
        // Tag the surface as a game for the compositors that act on it: Hyprland's
        // fullscreen-game refresh mode, KWin's content-type hint to the display.
        let manager: Option<ctm::WpContentTypeManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
        let content_type = manager.map(|m| {
            let t = m.get_surface_content_type(&surface, &qh, ());
            t.set_content_type(ct::Type::Game);
            t
        });
        let mut state = State::default();
        for _ in 0..4 {
            queue
                .roundtrip(&mut state)
                .context("surface feedback clock")?;
            if state.clock_id.is_some() {
                break;
            }
        }
        if state.clock_id != Some(CLOCK_MONOTONIC) {
            tracing::info!(
                clock = ?state.clock_id,
                "surface feedback: presentation clock is not CLOCK_MONOTONIC — off"
            );
            return Ok(None);
        }
        tracing::info!(
            game_tag = content_type.is_some(),
            "surface feedback armed on SDL's surface (wp_presentation)"
        );
        Ok(Some(Self {
            conn,
            queue,
            qh,
            state,
            globals: Some(globals),
            surface,
            presentation,
            _content_type: content_type,
            dead: false,
            _window: window.context(),
        }))
    }

    /// Ask the compositor to stamp the surface's next commit, which the driver's present
    /// makes. Call right before `vkQueuePresentKHR`.
    pub fn request(&mut self, present_id: u64) {
        if self.dead || self.state.pending.len() >= MAX_PENDING {
            return;
        }
        let f = self
            .presentation
            .feedback(&self.surface, &self.qh, present_id);
        self.state.pending.insert(present_id, f);
        if let Err(wayland_backend::client::WaylandError::Protocol(_)) = self.conn.flush() {
            self.dead = true;
        }
    }

    /// Dispatch what SDL's socket reads brought, and hand out the answers.
    pub fn take(&mut self) -> Vec<Sample> {
        if self.dead {
            return Vec::new();
        }
        if self.queue.dispatch_pending(&mut self.state).is_err()
            || self.conn.protocol_error().is_some()
        {
            tracing::warn!("surface feedback: Wayland error — off");
            self.dead = true;
            return Vec::new();
        }
        std::mem::take(&mut self.state.samples)
    }
}

impl Drop for SurfaceFeedback {
    fn drop(&mut self) {
        // A feedback object has no destructor: the compositor retires it with its answer.
        self.state.pending.clear();
        self.presentation.destroy();
        release_registry(&mut self.globals, &self.conn);
    }
}
