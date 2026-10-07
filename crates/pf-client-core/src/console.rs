//! Overlay actions, pointer input, and session-phase notifications shared by the
//! Vulkan presenter and the Android GL host. Lives here, not in `pf-console-ui`:
//! that crate sits above `pf-presenter` and would cycle.

/// Pairing, discovery, and library fetches ride the console command bus,
/// not this enum.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OverlayAction {
    /// `launch` is a library title id on Hello (`None` = desktop);
    /// `title` is display-only.
    Launch {
        addr: String,
        port: u16,
        fp_hex: String,
        launch: Option<String>,
        title: String,
        /// A dangling id falls back through `trust::effective_settings`;
        /// it never blocks the connect.
        preset: Option<String>,
        /// Pin the advertised fingerprint and park the connect until the
        /// operator approves this device. `false` is an ordinary paired connect.
        request_access: bool,
        /// The profile id to play as; `None` names none and the host picks.
        #[serde(default)]
        profile: Option<String>,
    },
    /// Browse continues. A dial that already won is quit-closed.
    CancelConnect,
    /// The launch hold is done — the game is up, or the player asked to see.
    ///
    /// For a host that draws this console OVER its stream (the Vulkan session) the reveal is
    /// internal and this says nothing new. It exists for a host that must SWAP to a stream
    /// view of its own: Android leaves the console for its stream screen, and doing that at
    /// the handshake is what put a second launch screen in front of the first. The session is
    /// held from the dial landing until this arrives.
    ShowStream,
    Quit,
    /// SDL owns the clipboard and lives on the run-loop thread, so this
    /// cannot ride the console command bus.
    CopyText(String),
}

/// A touchscreen contact is always `Primary`; Back lives on glass, not a
/// second finger-button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    /// The console treats this as Back.
    Secondary,
}

/// Pointer or touch in swapchain pixels, not window coordinates. Hit-testing
/// uses last-frame overlay rects; the run loop (window owner) converts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerInput {
    Move {
        x: f32,
        y: f32,
    },
    Down {
        x: f32,
        y: f32,
        button: PointerButton,
        /// Touch waits for lift so a swipe can scroll; mouse acts on press.
        /// Only `Down` carries the flag — the console tracks the gesture.
        touch: bool,
    },
    Up {
        x: f32,
        y: f32,
        button: PointerButton,
    },
    /// `dy` > 0 scrolls away from the user.
    Wheel {
        x: f32,
        y: f32,
        dy: f32,
    },
    /// Drop any armed press without acting.
    Cancel,
}

/// Browse-mode lifecycle. OSD/HUD ignore these.
pub enum SessionPhase<'a> {
    Connecting,
    Streaming,
    /// Browse returns to the library with this message.
    Failed(&'a str),
    /// `Some` is an abnormal reason for the status strip.
    Ended(Option<&'a str>),
    /// Auto-redial after the negotiated codec ran out of decode rungs. Distinct
    /// from [`Self::Ended`] and [`Self::Failed`]: those invite a manual reconnect
    /// that is already in flight.
    Reconnecting(&'a str),
}
