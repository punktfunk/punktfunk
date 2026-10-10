use crate::input::Capture;
use crate::overlay::Overlay;
#[cfg(target_os = "linux")]
use crate::wayland_scroll::WaylandScroll;
#[cfg(target_os = "linux")]
use punktfunk_core::input::InputEvent;
use sdl3::{event::Event, video::Window};

pub(crate) struct ScrollRouting {
    #[cfg(target_os = "linux")]
    native: Option<WaylandScroll>,
    #[cfg(target_os = "linux")]
    events: Vec<InputEvent>,
    native_active: bool,
    captured_before: bool,
    blocked: bool,
}

impl ScrollRouting {
    pub(crate) fn new(window: &Window) -> Self {
        #[cfg(target_os = "linux")]
        let native = match WaylandScroll::new(window) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "native scroll capture unavailable");
                None
            }
        };
        #[cfg(not(target_os = "linux"))]
        let _ = window;
        Self {
            #[cfg(target_os = "linux")]
            native,
            #[cfg(target_os = "linux")]
            events: Vec::new(),
            native_active: false,
            captured_before: false,
            blocked: false,
        }
    }

    pub(crate) fn begin(&mut self, capture: Option<&Capture>, overlay: Option<&dyn Overlay>) {
        self.captured_before = capture.is_some_and(Capture::captured);
        self.blocked = overlay_blocks(overlay);
        self.native_active = false;
        #[cfg(target_os = "linux")]
        self.drain();
    }

    #[cfg(target_os = "linux")]
    fn drain(&mut self) {
        self.events.clear();
        let Some(native) = self.native.as_mut() else {
            return;
        };
        match native.drain() {
            Ok(events) => {
                self.native_active = native.available() || !events.is_empty();
                self.events = events;
            }
            Err(error) => {
                tracing::warn!(%error, "native scroll dispatch stopped");
                self.native = None;
            }
        }
    }

    pub(crate) fn consumed(&mut self, event: &Event) {
        self.blocked |= matches!(event, Event::MouseWheel { .. });
    }

    pub(crate) fn focus_lost(&mut self) {
        self.blocked = true;
    }

    pub(crate) fn wheel(&self, capture: Option<&mut Capture>, dx: f32, dy: f32) {
        if self.native_active || self.blocked || !self.captured_before {
            return;
        }
        if let Some(capture) = capture {
            capture.on_wheel(dx, dy);
        }
    }

    pub(crate) fn finish(&mut self, capture: Option<&mut Capture>, overlay: Option<&dyn Overlay>) {
        #[cfg(target_os = "linux")]
        {
            let Some(native) = self.native.as_mut() else {
                return;
            };
            let captured_after = capture.as_ref().is_some_and(|c| c.captured());
            self.blocked |= overlay_blocks(overlay);
            let events = if crate::scroll::forward_native_scroll(
                self.captured_before,
                captured_after,
                self.blocked,
            ) {
                std::mem::take(&mut self.events)
            } else {
                native.cancel()
            };
            if let Some(capture) = capture {
                for event in events {
                    capture.on_scroll(event);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (capture, overlay);
    }
}

fn overlay_blocks(overlay: Option<&dyn Overlay>) -> bool {
    overlay.is_some_and(|o| o.ring_open() || o.holds_stream())
}
