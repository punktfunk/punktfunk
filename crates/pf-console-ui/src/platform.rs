//! Which platform the shell fronts. Screens are the same everywhere; which
//! settings rows mean something, and which native sub-screens exist, is not.
//! Ask this enum so the row tables stay one union and no screen carries a `cfg`.
//! See `design/android-skia-console-port.md`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Vulkan session binary (Linux/Windows, Steam Deck included).
    Desktop,
    /// Android client's GL host.
    Android,
    /// LG webOS TV client's GL host (`design/webos-skia-console-port.md`). A TV remote and a
    /// pad, no touch and no window manager — so it shares Android's glyph legend but none of
    /// its phone-sensor rows.
    WebOS,
    /// The browser client (`design/web-client.md`). Keyboard, mouse and pad like the desktop,
    /// so it takes the desktop's glyphs and ring — but the page binds no live chords, so the
    /// rows that name one describe the setting alone.
    Web,
    /// The Apple clients' Metal host (`design/console-ui-element-layer.md` WP4): iPhone, iPad,
    /// Mac and Apple TV behind one shell. Touch and pads like Android, but the OS answers for
    /// motion and text, so those rows are not the shell's to offer.
    Apple,
    /// The browser client packaged as a Samsung TV app (`design/tizen-client-implementation-plan.md`).
    /// The same page and the same rows as [`Platform::Web`], but held by a remote, so it takes
    /// the TV glyph legend — and Samsung requires Back at the root to exit, so it can quit.
    Tizen,
}

impl Platform {
    /// Every variant. A platform absent from a universal row list offers no settings
    /// row at all, which is a blank screen rather than a missing control — walk this
    /// instead of retyping the set.
    pub const ALL: [Platform; 6] = [
        Platform::Desktop,
        Platform::Android,
        Platform::WebOS,
        Platform::Web,
        Platform::Apple,
        Platform::Tizen,
    ];

    /// Whether the app may close itself. An Apple app and a browser page cannot; a packaged TV
    /// page must, because Back at its root is how a Samsung app exits.
    pub fn can_quit(self) -> bool {
        !matches!(self, Platform::Apple | Platform::Web)
    }
}

/// A native screen the platform owns. The shell sends
/// [`crate::model::ConsoleCmd::OpenPlatformScreen`] and suspends input until the host
/// reports the screen closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformScreen {
    Licenses,
}

impl PlatformScreen {
    /// Stable id the host matches on; crosses JNI as a string.
    pub fn id(self) -> &'static str {
        match self {
            PlatformScreen::Licenses => "licenses",
        }
    }
}
