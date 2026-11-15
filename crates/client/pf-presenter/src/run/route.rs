//! Where an SDL event goes: the console, a further screen's window, the loop's chords, the
//! stream's capture, the gesture engine, the main window, the loop's own handlers, the
//! session's end, or nowhere.
//!
//! Pure, so the table runs on every platform. `run/events.rs` classifies the SDL event into
//! a [`Kind`], asks the console first, samples the [`RouteFacts`] with its verdict, calls
//! [`route`] and performs the [`Target`]. Each delivery is a named row of the table test.

/// What the route knows of an SDL event.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    /// A first key press: one of the loop's chords, or Super.
    Key {
        chord: bool,
        super_key: bool,
    },
    KeyRepeat,
    KeyUp,
    /// A mouse button, down or up.
    Button,
    Wheel,
    /// A touchscreen finger, any phase.
    Finger,
    Motion,
    /// A window's close button.
    Close,
    /// A window gained or lost focus.
    Focus,
    /// Any other window event: a resize, an expose, the pointer leaving.
    Window,
    /// SDL's quit: the last window closed, or the session was told to end.
    Quit,
    /// Display changes, frame wakes, pads, text input.
    Other,
}

/// What the route reads besides the kind, sampled per event.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RouteFacts {
    /// The event is on the main window.
    pub on_primary: bool,
    /// The event is on a further screen's window.
    pub on_screen: bool,
    /// The console took it, asked before routing.
    pub overlay_took: bool,
    /// The ring, or the console holding a launch, owns the keys (the pad tick's edge).
    pub ring_open: bool,
    /// Shortcut capture is on: Super goes to the host.
    pub inhibit_shortcuts: bool,
    /// The native scroll path takes this batch's wheel over the main window's surface.
    pub native_scroll: bool,
    /// The SDL wheel is blocked this batch: the console took one, focus left, the ring is up.
    pub wheel_blocked: bool,
    /// Capture held the input when this batch began.
    pub captured: bool,
}

/// Who takes the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// The console took it.
    Overlay,
    /// A further screen's window handles it.
    Screen,
    /// The loop answers the chord itself.
    Chord,
    /// The stream's input: keys, buttons, motion, the SDL wheel.
    Capture,
    /// The touch gesture engine, which hands the open ring its finger first.
    Gesture,
    /// The main window's own handling: focus, size, expose.
    Window,
    /// The loop's own: display changes, wakes, pads.
    Loop,
    /// The session ends.
    Quit,
    Drop,
}

/// A further screen keeps its window events and pointer moves; the console's verdict comes
/// next; then the session's own order: quit, chords, Super, the ring, capture.
pub(crate) fn route(k: Kind, f: &RouteFacts) -> Target {
    match k {
        // A further screen's close button gives that monitor back: the screen's to handle.
        Kind::Window | Kind::Motion | Kind::Close if f.on_screen => return Target::Screen,
        _ if f.overlay_took => return Target::Overlay,
        _ => {}
    }
    match k {
        Kind::Quit => Target::Quit,
        // SDL raises `Quit` only for the last window: with screens open this is all there is.
        Kind::Close if f.on_primary => Target::Quit,
        Kind::Close => Target::Drop,
        Kind::Key { chord: true, .. } => Target::Chord,
        // The local shell acts on Super; forwarding it would open the host's launcher too.
        Kind::Key {
            super_key: true, ..
        } if !f.inhibit_shortcuts => Target::Drop,
        // The ring had the keys it maps as the console; the rest stay off the host.
        Kind::Key { .. } | Kind::KeyRepeat if f.ring_open => Target::Drop,
        Kind::Key { .. } | Kind::KeyRepeat | Kind::KeyUp | Kind::Button | Kind::Motion => {
            Target::Capture
        }
        // Native scroll owns the main surface's wheel; a further screen's window has only SDL's.
        Kind::Wheel if (f.native_scroll && f.on_primary) || f.wheel_blocked || !f.captured => {
            Target::Drop
        }
        Kind::Wheel => Target::Capture,
        Kind::Finger => Target::Gesture,
        Kind::Focus | Kind::Window => Target::Window,
        Kind::Other => Target::Loop,
    }
}

/// Whether applied capture grabs the keyboard: engaged, shortcut capture on, the keyboard
/// granted. The grab hands the host every system chord, Alt+F4 included.
pub(crate) fn keyboard_grab(on: bool, inhibit: bool, keyboard: bool) -> bool {
    on && inhibit && keyboard
}

/// Whether the client's own UI keeps the pads off the wire: focus lost, Gaming Mode's
/// overlay open, or a capture the player let go. `capture` is a live capture's
/// `(can engage, engaged)`. One that can't engage, on grants without pointer and keyboard,
/// was never let go.
pub(crate) fn pad_mask(
    focus_lost: bool,
    overlay_open: bool,
    capture: Option<(bool, bool)>,
) -> bool {
    focus_lost || overlay_open || capture == Some((true, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Kind = Kind::Key {
        chord: false,
        super_key: false,
    };
    const CHORD: Kind = Kind::Key {
        chord: true,
        super_key: false,
    };
    const SUPER: Kind = Kind::Key {
        chord: false,
        super_key: true,
    };

    /// Every delivery the session makes, one named row each. The base is a captured stream
    /// on the main window with nothing up; each row changes what it names.
    #[test]
    fn every_event_goes_where_the_session_sends_it() {
        let main = RouteFacts {
            on_primary: true,
            captured: true,
            ..RouteFacts::default()
        };
        let screen = RouteFacts {
            on_primary: false,
            on_screen: true,
            ..main
        };
        let took = RouteFacts {
            overlay_took: true,
            ..main
        };
        let ring = RouteFacts {
            ring_open: true,
            ..main
        };
        let shortcuts = RouteFacts {
            inhibit_shortcuts: true,
            ..main
        };
        let native = RouteFacts {
            native_scroll: true,
            ..main
        };
        #[rustfmt::skip]
        let rows = [
            // Keys.
            ("a chord comes before the ring", CHORD, ring, Target::Chord),
            ("a chord goes to the loop", CHORD, main, Target::Chord),
            ("the console's verdict comes before a chord", CHORD, took, Target::Overlay),
            ("Super stays local while shortcut capture is off", SUPER, main, Target::Drop),
            ("Super reaches the host with shortcut capture on", SUPER, shortcuts, Target::Capture),
            ("Super stays off the host while the ring is up", SUPER, RouteFacts { ring_open: true, ..shortcuts }, Target::Drop),
            ("r5-8: an unmapped key stays local while the ring is up", KEY, ring, Target::Drop),
            ("the ring takes a key it maps as the console", KEY, RouteFacts { overlay_took: true, ..ring }, Target::Overlay),
            ("a key goes to capture", KEY, main, Target::Capture),
            ("a repeat goes to capture", Kind::KeyRepeat, main, Target::Capture),
            ("a repeat stays local while the ring is up", Kind::KeyRepeat, ring, Target::Drop),
            ("a key up reaches capture while the ring is up", Kind::KeyUp, ring, Target::Capture),
            // Pointer.
            ("a button goes to capture, which engages a released stream", Kind::Button, main, Target::Capture),
            ("the console's verdict wins for a button", Kind::Button, took, Target::Overlay),
            ("motion goes to capture", Kind::Motion, main, Target::Capture),
            ("the console's verdict wins for motion", Kind::Motion, took, Target::Overlay),
            ("a wheel goes to capture", Kind::Wheel, main, Target::Capture),
            ("r6-1: native scroll takes the main window's wheel", Kind::Wheel, native, Target::Drop),
            ("r6-1: a further screen's wheel reaches capture beside native scroll", Kind::Wheel, RouteFacts { native_scroll: true, ..screen }, Target::Capture),
            ("the console's verdict wins for a wheel", Kind::Wheel, took, Target::Overlay),
            ("a blocked batch drops the wheel", Kind::Wheel, RouteFacts { wheel_blocked: true, ..main }, Target::Drop),
            ("a batch that began released drops the wheel", Kind::Wheel, RouteFacts { captured: false, ..main }, Target::Drop),
            // Touch.
            ("the console's verdict wins for a finger", Kind::Finger, took, Target::Overlay),
            ("a finger goes to the gesture engine", Kind::Finger, main, Target::Gesture),
            ("a finger goes to the gesture engine with the ring up", Kind::Finger, ring, Target::Gesture),
            // Windows.
            ("a further screen keeps its window events", Kind::Window, screen, Target::Screen),
            ("a further screen keeps its pointer moves", Kind::Motion, screen, Target::Screen),
            ("a further screen's moves skip the console", Kind::Motion, RouteFacts { overlay_took: true, ..screen }, Target::Screen),
            ("a further screen's focus is the main window's", Kind::Focus, screen, Target::Window),
            ("a further screen's keys go to capture", KEY, screen, Target::Capture),
            ("a further screen's buttons go to capture", Kind::Button, screen, Target::Capture),
            ("a further screen's fingers go to the gesture engine", Kind::Finger, screen, Target::Gesture),
            ("r5-1: the main window's close ends the session", Kind::Close, main, Target::Quit),
            ("a further screen's close closes that screen", Kind::Close, screen, Target::Screen),
            ("a close on no known window does nothing", Kind::Close, RouteFacts::default(), Target::Drop),
            ("quit ends the session", Kind::Quit, RouteFacts::default(), Target::Quit),
            ("the main window's events go to its handler", Kind::Window, main, Target::Window),
            ("focus goes to the main window's handler", Kind::Focus, main, Target::Window),
            ("the console takes text input", Kind::Other, took, Target::Overlay),
            ("displays, wakes and pads go to the loop", Kind::Other, RouteFacts::default(), Target::Loop),
        ];
        for (name, kind, facts, want) in rows {
            assert_eq!(route(kind, &facts), want, "{name}");
        }
    }

    /// The grab, and with it Alt+F4, is the host's only while all three hold.
    #[test]
    fn alt_f4_reaches_the_host_only_under_the_keyboard_grab() {
        #[rustfmt::skip]
        let rows = [
            ("captured with shortcut capture on, Alt+F4 is the host's", (true, true, true), true),
            ("a released stream closes on Alt+F4", (false, true, true), false),
            ("shortcut capture off closes on Alt+F4", (true, false, true), false),
            ("no keyboard grant closes on Alt+F4", (true, true, false), false),
        ];
        for (name, (on, inhibit, keyboard), want) in rows {
            assert_eq!(keyboard_grab(on, inhibit, keyboard), want, "{name}");
        }
    }

    /// The pads are the host's unless the client's UI wants them or the player let go.
    #[test]
    fn pads_leave_the_wire_for_the_ui_or_a_released_capture() {
        const ON: Option<(bool, bool)> = Some((true, true));
        const NEVER: Option<(bool, bool)> = Some((false, false));
        #[rustfmt::skip]
        let rows = [
            ("a released capture masks the pads", (false, false, Some((true, false))), true),
            ("an engaged capture sends them", (false, false, ON), false),
            ("no capture yet sends them", (false, false, None), false),
            ("focus loss masks them", (true, false, ON), true),
            ("Gaming Mode's overlay masks them", (false, true, ON), true),
            ("controller-only access sends them", (false, false, NEVER), false),
            ("controller-only access loses them with focus", (true, false, NEVER), true),
        ];
        for (name, (focus_lost, overlay_open, capture), want) in rows {
            assert_eq!(pad_mask(focus_lost, overlay_open, capture), want, "{name}");
        }
    }
}
