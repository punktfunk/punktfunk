//! macOS keyboard, mouse, scroll and text as CoreGraphics events posted at the HID tap.
//!
//! [`crate::cgevent_plan`] decides every event. This file reads the desktop a pointer event
//! needs (the cursor, the stream display's bounds) and builds and posts what the plan lists.
//! Posting needs the Accessibility grant ([`post_event_access`]); without it
//! [`CgEventInjector::open`] fails and the injector service retries on its backoff.

use crate::cgevent_plan::{
    pick_bounds, ty, BoundsCache, Desk, Planner, Post, Rect, KVK_SPACE, SCREENS,
};
use crate::InputInjector;
use anyhow::{bail, Context, Result};
use objc2_core_foundation::{
    kCFPreferencesAnyApplication, CFNumber, CFPreferencesCopyAppValue, CFRetained, CFString,
    CGPoint,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayChangeSummaryFlags,
    CGDisplayRegisterReconfigurationCallback, CGEvent, CGEventField, CGEventFlags,
    CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton, CGPreflightPostEventAccess,
    CGRequestPostEventAccess, CGScrollEventUnit,
};
use punktfunk_core::input::InputEvent;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

/// Each screen's display, as [`set_stream_display`] and [`set_screen_display`] published it;
/// 0 (`kCGNullDirectDisplay`) for none.
static SCREEN_DISPLAYS: [AtomicU32; SCREENS] = [const { AtomicU32::new(0) }; SCREENS];

/// Bumped by every display reconfiguration callback and every published display.
static DISPLAY_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Display rects of the current [`DISPLAY_EPOCH`]. Key 0 is the main display.
static BOUNDS: Mutex<BoundsCache> = Mutex::new(BoundsCache::new());

static WATCH: Once = Once::new();

/// The display absolute coordinates map into, as a `CGDirectDisplayID`: counterpart of
/// Windows `stream_target`. Published at capture bring-up, then the aim generation bumps in the
/// same write, so the session's first relative move starts on this display. `None`, or a
/// display that is gone, maps onto the main display.
pub fn set_stream_display(display: Option<u32>) {
    crate::publish_aim(true, |_| publish(0, display));
}

/// [`set_stream_display`] for further screen `screen` (1 or more), with no warp: the pointer
/// stays where the first screen put it. A screen with no display, or a gone one, maps onto the
/// first screen's.
pub fn set_screen_display(screen: u8, display: Option<u32>) {
    if screen > 0 {
        publish(screen, display);
    }
}

fn publish(screen: u8, display: Option<u32>) {
    let Some(slot) = SCREEN_DISPLAYS.get(usize::from(screen)) else {
        return;
    };
    let id = display.unwrap_or(0);
    if slot.swap(id, Ordering::Relaxed) != id {
        tracing::info!(screen, display_id = id, "absolute-input display set");
    }
    DISPLAY_EPOCH.fetch_add(1, Ordering::Release);
}

/// CoreGraphics calls this on the main thread's run loop, in every mode. A blocked main thread
/// delays it, and the cache keeps the old rect until then. A mode change the host makes is
/// also republished at its capture bring-up.
extern "C-unwind" fn reconfigured(
    _: CGDirectDisplayID,
    _: CGDisplayChangeSummaryFlags,
    _: *mut c_void,
) {
    DISPLAY_EPOCH.fetch_add(1, Ordering::Release);
}

fn watch_displays() {
    let callback = reconfigured as unsafe extern "C-unwind" fn(_, _, _);
    // SAFETY: `reconfigured` is a plain `extern "C-unwind"` function with the callback's
    // signature that reads none of its arguments, so the null `userInfo` is never dereferenced.
    // It stays registered for the process's life.
    unsafe { CGDisplayRegisterReconfigurationCallback(Some(callback), std::ptr::null_mut()) };
}

/// Whether this process may post input events: the Accessibility grant. Shows nothing.
pub fn post_event_access() -> bool {
    CGPreflightPostEventAccess()
}

/// Ask for the Accessibility grant: macOS shows its prompt when the grant is missing. Returns
/// whether the grant is in place now.
pub fn request_post_event_access() -> bool {
    CGRequestPostEventAccess()
}

pub struct CgEventInjector {
    plan: Planner,
}

impl CgEventInjector {
    pub fn open() -> Result<Self> {
        if !post_event_access() {
            bail!("open the CGEvent injector: this process has no Accessibility grant");
        }
        Ok(Self {
            plan: Planner::new(double_click_interval()),
        })
    }
}

impl InputInjector for CgEventInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        post(self.plan.plan(event, Instant::now(), &desk))
    }

    fn deadline(&self) -> Option<Instant> {
        self.plan.due()
    }

    fn on_deadline(&mut self) -> Result<()> {
        post(self.plan.flush(Instant::now()))
    }
}

impl Drop for CgEventInjector {
    /// Lift what is still down, which nothing else releases.
    fn drop(&mut self) {
        let _ = post(self.plan.release(Instant::now(), &desk));
    }
}

/// Build and post each event in order. The one call in this crate that injects on macOS.
fn post(posts: Vec<Post>) -> Result<()> {
    for p in posts {
        let event = build(p).context("create a CGEvent")?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }
    Ok(())
}

/// The event `p` describes, not posted.
fn build(p: Post) -> Option<CFRetained<CGEvent>> {
    let set = |e: &CGEvent, field: CGEventField, v: i64| {
        CGEvent::set_integer_value_field(Some(e), field, v)
    };
    let (event, flags) = match p {
        Post::Key {
            ty,
            code,
            flags,
            repeat,
        } => {
            let e = CGEvent::new_keyboard_event(None, code, ty != ty::KEY_UP)?;
            CGEvent::set_type(Some(&e), CGEventType(ty));
            set(&e, CGEventField::KeyboardEventAutorepeat, i64::from(repeat));
            (e, flags)
        }
        Post::Text { ch, down } => {
            let e = CGEvent::new_keyboard_event(None, KVK_SPACE, down)?;
            let mut buf = [0u16; 2];
            let units = ch.encode_utf16(&mut buf);
            // SAFETY: `units` is `units.len()` initialized UTF-16 units in `buf`, alive across
            // the call, which copies them into the event.
            unsafe {
                CGEvent::keyboard_set_unicode_string(Some(&e), units.len() as _, units.as_ptr())
            };
            (e, 0)
        }
        Post::Mouse {
            ty,
            button,
            at,
            delta,
            clicks,
            flags,
        } => {
            let at = CGPoint::new(at.0, at.1);
            let e = CGEvent::new_mouse_event(None, CGEventType(ty), at, CGMouseButton(button))?;
            set(&e, CGEventField::MouseEventDeltaX, delta.0.into());
            set(&e, CGEventField::MouseEventDeltaY, delta.1.into());
            set(&e, CGEventField::MouseEventClickState, clicks);
            (e, flags)
        }
        Post::Scroll {
            lines,
            wheel1,
            wheel2,
            phase,
            flags,
        } => {
            let unit = if lines {
                CGScrollEventUnit::Line
            } else {
                CGScrollEventUnit::Pixel
            };
            let e = CGEvent::new_scroll_wheel_event2(None, unit, 2, wheel1, wheel2, 0)?;
            set(
                &e,
                CGEventField::ScrollWheelEventIsContinuous,
                i64::from(!lines),
            );
            set(&e, CGEventField::ScrollWheelEventScrollPhase, phase.into());
            (e, flags)
        }
    };
    CGEvent::set_flags(Some(&event), CGEventFlags(flags));
    Some(event)
}

/// The cursor, read now, and each screen's display bounds, read once per display epoch.
pub(crate) fn desk() -> Desk {
    WATCH.call_once(watch_displays);
    let cursor = CGEvent::new(None).map_or_else(CGPoint::default, |e| CGEvent::location(Some(&e)));
    let rect = |id| {
        let r = CGDisplayBounds(id);
        (r.origin.x, r.origin.y, r.size.width, r.size.height)
    };
    let epoch = DISPLAY_EPOCH.load(Ordering::Acquire);
    let mut cache = BOUNDS.lock().unwrap_or_else(|e| e.into_inner());
    let published: [Option<Rect>; SCREENS] =
        std::array::from_fn(|s| match SCREEN_DISPLAYS[s].load(Ordering::Relaxed) {
            0 => None,
            id => Some(cache.get(epoch, id, || rect(id))),
        });
    let first = pick_bounds(
        published[0],
        cache.get(epoch, 0, || rect(CGMainDisplayID())),
    );
    Desk {
        cursor: (cursor.x, cursor.y),
        screens: published.map(|r| pick_bounds(r, first)),
    }
}

/// The host's double-click speed (System Settings, Mouse), or the 0.5 s macOS takes unset.
fn double_click_interval() -> Duration {
    let key = CFString::from_static_str("com.apple.mouse.doubleClick");
    // SAFETY: `kCFPreferencesAnyApplication` is an immutable CFString constant that
    // CoreFoundation defines before any code runs and never writes.
    let any = unsafe { kCFPreferencesAnyApplication };
    CFPreferencesCopyAppValue(&key, any)
        .and_then(|v| v.downcast::<CFNumber>().ok())
        .and_then(|n| n.as_f64())
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .filter(|d| !d.is_zero())
        .unwrap_or(Duration::from_millis(500))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgevent_plan::{flag, phase};
    use objc2_core_graphics::CGScrollPhase;

    /// The plan's numbers are the SDK's.
    #[test]
    fn the_plans_numbers_are_the_sdks() {
        for (ours, sdk) in [
            (ty::LEFT_DOWN, CGEventType::LeftMouseDown),
            (ty::LEFT_UP, CGEventType::LeftMouseUp),
            (ty::RIGHT_DOWN, CGEventType::RightMouseDown),
            (ty::RIGHT_UP, CGEventType::RightMouseUp),
            (ty::MOVED, CGEventType::MouseMoved),
            (ty::LEFT_DRAGGED, CGEventType::LeftMouseDragged),
            (ty::RIGHT_DRAGGED, CGEventType::RightMouseDragged),
            (ty::KEY_DOWN, CGEventType::KeyDown),
            (ty::KEY_UP, CGEventType::KeyUp),
            (ty::FLAGS_CHANGED, CGEventType::FlagsChanged),
            (ty::OTHER_DOWN, CGEventType::OtherMouseDown),
            (ty::OTHER_UP, CGEventType::OtherMouseUp),
            (ty::OTHER_DRAGGED, CGEventType::OtherMouseDragged),
        ] {
            assert_eq!(ours, sdk.0, "{sdk:?}");
        }
        for (ours, sdk) in [
            (flag::ALPHA_SHIFT, CGEventFlags::MaskAlphaShift),
            (flag::SHIFT, CGEventFlags::MaskShift),
            (flag::CONTROL, CGEventFlags::MaskControl),
            (flag::ALTERNATE, CGEventFlags::MaskAlternate),
            (flag::COMMAND, CGEventFlags::MaskCommand),
        ] {
            assert_eq!(ours, sdk.0, "{sdk:?}");
        }
        for (ours, sdk) in [
            (phase::BEGAN, CGScrollPhase::Began),
            (phase::CHANGED, CGScrollPhase::Changed),
            (phase::ENDED, CGScrollPhase::Ended),
            (phase::CANCELLED, CGScrollPhase::Cancelled),
        ] {
            assert_eq!(ours, sdk.0, "{sdk:?}");
        }
    }

    /// Each kind of post builds into the event it names. Builds only: nothing is posted.
    #[test]
    fn a_post_builds_into_the_event_it_names() {
        let get = |e: &CGEvent, f| CGEvent::integer_value_field(Some(e), f);
        let shifted = flag::SHIFT | flag::L_SHIFT;
        let e = build(Post::Key {
            ty: ty::FLAGS_CHANGED,
            code: 0x38,
            flags: shifted,
            repeat: false,
        })
        .unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::FlagsChanged);
        assert_eq!(CGEvent::flags(Some(&e)).0, shifted);
        assert_eq!(get(&e, CGEventField::KeyboardEventKeycode), 0x38);

        let e = build(Post::Key {
            ty: ty::KEY_UP,
            code: 0x00,
            flags: 0,
            repeat: true,
        })
        .unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::KeyUp);
        assert_eq!(get(&e, CGEventField::KeyboardEventAutorepeat), 1);

        let e = build(Post::Text {
            ch: '\u{1F600}',
            down: true,
        })
        .unwrap();
        let (mut len, mut units) = (0, [0u16; 4]);
        // SAFETY: `len` and `units` are live locals; the call writes at most 4 units into
        // `units` and their count into `len`.
        unsafe { CGEvent::keyboard_get_unicode_string(Some(&e), 4, &mut len, units.as_mut_ptr()) };
        assert_eq!(&units[..len as usize], [0xD83D, 0xDE00]);
        assert_eq!(CGEvent::flags(Some(&e)).0, 0);

        let e = build(Post::Mouse {
            ty: ty::OTHER_DOWN,
            button: 3,
            at: (12.0, 34.0),
            delta: (5, -6),
            clicks: 2,
            flags: flag::COMMAND | flag::L_CMD,
        })
        .unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::OtherMouseDown);
        assert_eq!(CGEvent::location(Some(&e)), CGPoint::new(12.0, 34.0));
        assert_eq!(get(&e, CGEventField::MouseEventButtonNumber), 3);
        assert_eq!(get(&e, CGEventField::MouseEventClickState), 2);
        assert_eq!(get(&e, CGEventField::MouseEventDeltaX), 5);
        assert_eq!(get(&e, CGEventField::MouseEventDeltaY), -6);
        assert_eq!(CGEvent::flags(Some(&e)).0, flag::COMMAND | flag::L_CMD);

        let e = build(Post::Scroll {
            lines: false,
            wheel1: 7,
            wheel2: -3,
            phase: phase::BEGAN,
            flags: 0,
        })
        .unwrap();
        assert_eq!(CGEvent::r#type(Some(&e)), CGEventType::ScrollWheel);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventIsContinuous), 1);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventPointDeltaAxis1), 7);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventPointDeltaAxis2), -3);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventScrollPhase), 1);

        let e = build(Post::Scroll {
            lines: true,
            wheel1: -2,
            wheel2: 0,
            phase: 0,
            flags: 0,
        })
        .unwrap();
        assert_eq!(get(&e, CGEventField::ScrollWheelEventIsContinuous), 0);
        assert_eq!(get(&e, CGEventField::ScrollWheelEventDeltaAxis1), -2);
    }

    /// Reading the preference never fails; an unset one is macOS's 0.5 s.
    #[test]
    fn the_double_click_interval_is_a_positive_duration() {
        let d = double_click_interval();
        assert!(!d.is_zero() && d <= Duration::from_secs(10), "{d:?}");
    }
}
