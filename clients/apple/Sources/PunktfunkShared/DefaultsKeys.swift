// One source of truth for the client's UserDefaults / @AppStorage keys. A magic-string key
// duplicated across a setting's writer (a Settings @AppStorage) and reader (e.g. a stream view
// reading UserDefaults) splits silently on a typo — the setting just stops taking effect. These
// live in the dependency-free PunktfunkShared module (re-exported by PunktfunkKit) because the app,
// the kit's views, AND the widget extension all read them — the widget needs `DefaultsKey.hosts`.

import Foundation

/// Persisted-setting keys. The string VALUES are stable on disk — rename the symbol freely, but
/// never the string (it would orphan everyone's saved value).
public enum DefaultsKey {
    public static let streamWidth = "punktfunk.width"
    public static let streamHeight = "punktfunk.height"
    public static let streamHz = "punktfunk.hz"
    /// Match-window resolution policy (design/midstream-resolution-resize.md D1/D2): when on, the
    /// stream mode FOLLOWS the session view — the connect asks for the view's pixel size and a
    /// mid-session resize (a windowed macOS window, an iPad Stage Manager / Split View scene)
    /// renegotiates the host's virtual display + encoder (`PunktfunkConnection.requestMode`), so a
    /// windowed session streams native-resolution pixels instead of scaling. Off (default): the
    /// explicit `streamWidth`/`streamHeight` are used and never auto-resized (a fullscreen session
    /// is native either way, so this degenerates to Auto-native there). Read per session by the
    /// stream views' `MatchWindowFollower`.
    public static let matchWindow = "punktfunk.matchWindow"
    /// Render-resolution multiplier (a `RenderScale` value, default 1.0): the client asks the host
    /// to render/encode at `chosen resolution × scale`, then the presenter downscales the larger
    /// decoded frame to this display in one Catmull-Rom pass. > 1 supersamples (sharper, at the cost
    /// of more bandwidth AND client decode — both grow ∝ scale²); < 1 renders below native for a
    /// weak host GPU / constrained link (the presenter upscales). Purely client-side — the host just
    /// sees a normal (larger/smaller) `Mode`, and Automatic bitrate scales with it. Clamped even +
    /// to the codec's max dimension at connect. Applies to the fixed mode and the match-window path.
    public static let renderScale = "punktfunk.renderScale"
    /// How a frame whose shape differs from the view fills it: a `VideoFit` raw value, `"fit"`
    /// (default, bars), `"crop"` or `"stretch"`. The cross-client `video_fit` key.
    public static let videoFit = "punktfunk.videoFit"
    public static let compositor = "punktfunk.compositor"
    public static let gamepadType = "punktfunk.gamepadType"
    public static let gamepadID = "punktfunk.gamepadID"
    /// The `PunktfunkConnection.GamepadType` raw value of the last controller that was actually
    /// attached — written by `GamepadManager` whenever one becomes active, never cleared on
    /// disconnect. It exists so the gamepad UI's button legends keep speaking the pad the user
    /// owns: the live controller's own `sfSymbolsName` is authoritative while it's connected, but
    /// the moment it sleeps or disconnects there is nothing left to ask, and the legends used to
    /// snap back to generic letter glyphs (i.e. Xbox) under a DualSense user's hands. Also what
    /// makes the legends right at all under `gamepadUIMode == "always"`, where the console UI is
    /// up with no pad attached by design. See `GamepadGlyphs`.
    public static let lastGamepadKind = "punktfunk.lastGamepadKind"
    /// Forward this device's controllers to the host at all (default true). Off is for a
    /// couch whose controller reaches the host another way — USB passthrough such as
    /// VirtualHere, or a pad plugged into the host — where forwarding as well would give the
    /// host two pads for one pair of hands. Read at connect: `SessionModel` then never starts
    /// `GamepadCapture`, so no slot opens, no arrival is sent and no virtual pad is built.
    public static let gamepadForwarding = "punktfunk.gamepadForwarding"
    /// Steam Controller 2 as-is passthrough: capture of a real SC2 → the host's virtual
    /// 28DE:1302, which the host's Steam drives directly. Two transports — CoreBluetooth against
    /// a paired pad's vendor GATT service (every platform), and raw USB HID against a wired pad
    /// or a Puck dongle (macOS only, `Sc2UsbLink`; apps elsewhere get no IOKit HID).
    /// This client's own key, NOT a shared one: nothing writes an unprefixed `sc2_capture`, and
    /// Android files the same decision under `android.sc2_capture`. Same gate as its
    /// `settings.sc2Capture` though, read at
    /// connect beside `gamepadForwarding`: both must be on for `SessionModel` to build an
    /// `Sc2Capture`. On iOS it is ALSO read app-lifetime, by `GamepadManager`: the same pad
    /// navigates the gamepad UI through `Sc2MenuPad` (`gamepadUIEnabled` gates that half).
    ///
    /// ⚠ The DEFAULT deliberately differs from Android's, which is ON: engaging this on Apple
    /// costs a permission question. iOS asks for CoreBluetooth at launch, now that the menu pad
    /// acquires there. The Mac is no cheaper — `Sc2Capture` takes USB only when a pad is attached
    /// right now, so a padless Mac falls back to the radio and is asked for Bluetooth, and the
    /// USB path asks for Input Monitoring (the controller interface carries the lizard keyboard
    /// collection). Android needs no prompt for an already-attached pad, so it can default on and
    /// cost nothing when none is present. Do not align the platforms without moving the prompt.
    public static let sc2Capture = "punktfunk.sc2Capture"
    /// Where a controller's SYSTEM buttons (guide + the share/QAM misc) land while streaming:
    /// `"auto"` | `"forward"` | `"local"` — the cross-client `system_buttons` key. Auto
    /// forwards on every Apple platform: the local Game Overlay is the OS's business (and on
    /// iOS 27+ the user can hand the Home button to the app in Settings), so suppressing our
    /// send would gain nothing.
    public static let systemButtons = "punktfunk.systemButtons"
    /// The hold-Select guide gesture: `"auto"` | `"on"` | `"off"` — the cross-client
    /// `guide_gesture` key. Auto arms it everywhere but macOS: iOS reserves the physical Home
    /// press for the Game Overlay (uncapturable pre-27) and tvOS never delivers it at all, so
    /// holding Select is the controller route to the host's guide there.
    public static let guideGesture = "punktfunk.guideGesture"
    public static let bitrateKbps = "punktfunk.bitrateKbps"
    /// Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
    /// can capture; the resolved count drives the in-core decode + AVAudioEngine layout.
    public static let audioChannels = "punktfunk.audioChannels"
    /// Requested audio format — `AudioFormatChoice`'s raw value: `"opus"` (the default, and every
    /// session before the lossless plane existed), or one of the lossless rows,
    /// `"lossless44_1"` / `"lossless48"` / `"lossless88_2"` / `"lossless96"` / `"lossless176_4"`.
    ///
    /// Off by default and deliberately: lossless takes 2.1–8.5 Mbps off the top of the link for
    /// stereo (three times that for 5.1, four for 7.1), OUTSIDE the ABR loop that manages the video
    /// budget, against the ~256 kbps Opus it replaces — so a user has to pick it. Since 2026-08-17
    /// this row is the ONLY opt-in: the host's half (`PUNKTFUNK_AUDIO_HIRES`) defaults ON and is an
    /// opt-OUT (`=0`), so picking a lossless row here is enough on any host that has not
    /// deliberately turned the plane off. A REQUEST: the host's
    /// five-condition gate may resolve the session back to Opus, and
    /// `PunktfunkConnection.resolvedAudioRateHz`/`resolvedAudioBits`/`resolvedAudioChannels` are
    /// what actually happened.
    public static let audioFormat = "punktfunk.audioFormat"
    /// Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, `"av1"`, or
    /// `"pyrowave"` (the opt-in wired-LAN wavelet codec — picking it advertises AND prefers it,
    /// and forces the session SDR). A soft preference — the host emits it when it can, else
    /// falls back. Drives the decoder via `Welcome.codec`.
    public static let codec = "punktfunk.codec"
    public static let micEnabled = "punktfunk.micEnabled"
    /// Echo cancellation for the mic uplink (on by default): playback + capture share ONE
    /// audio engine so the system voice processor can subtract what this device is playing
    /// from what its mic hears — without it a loudspeaker client feeds the game audio straight
    /// back to the host. Off = the raw two-engine capture path. macOS: an explicitly pinned
    /// speaker/mic or mic channel also bypasses it (the voice processor only follows the
    /// system default devices) — see SessionAudio's topology note.
    public static let echoCancel = "punktfunk.echoCancel"
    /// Ask the host to leave ITS OWN audio devices alone for this session
    /// (`PUNKTFUNK_CLIENT_CAP_KEEP_HOST_AUDIO`): it captures whatever its default playback
    /// device already is, so the speakers/headphones on the host PC keep playing while this
    /// device hears the same audio. Off (the default) is today's behaviour — the host parks
    /// playback on a silent endpoint and goes quiet for the session. Best-effort: an older
    /// host ignores the ask and re-routes as it always did.
    public static let keepHostAudio = "punktfunk.keepHostAudio"
    public static let speakerUID = "punktfunk.speakerUID"
    public static let micUID = "punktfunk.micUID"
    /// macOS: which input channel of the chosen mic device feeds the host. 0 = "Auto" (sum every
    /// channel to mono — a mic on a single input of a multi-channel interface passes at full
    /// level); n≥1 pins 1-based input channel n. Multi-channel interfaces expose the mic on ONE
    /// discrete channel, and the default N→stereo downmix grabs channels 0/1 (silence when the mic
    /// is higher up), so we fold to mono ourselves. Only meaningful for multi-channel devices.
    public static let micChannel = "punktfunk.micChannel"
    /// LEGACY (2026-07 presentation rebuild — design/apple-presentation-rebuild.md): the old
    /// user-visible stage picker's key. No longer read — the presenter is resolved from
    /// `presentPriority` below; the stage ladder survives only as the
    /// PUNKTFUNK_PRESENTER=stage1|stage2|stage3|stage4 debug env lever. Kept so a synced old
    /// value is documented, not mysterious.
    public static let presenter = "punktfunk.presenter"
    /// The user's presentation intent: "latency" (default — every frame shows as soon as the
    /// display can; jitter appears as the occasional repeat/drop) or "smooth" (a small client
    /// jitter buffer evens the cadence at the cost of added, visible display latency).
    /// Resolved once per session by SessionPresenter — see PresentPriority.
    public static let presentPriority = "punktfunk.presentPriority"
    /// Smoothness's jitter-buffer capacity in frames: 0 = Automatic (currently 2), or 1…3.
    /// Each buffered frame adds ~one refresh interval of display latency and absorbs ~one
    /// interval of arrival jitter. Only meaningful when `presentPriority` is "smooth".
    public static let smoothBuffer = "punktfunk.smoothBuffer"
    /// macOS: V-Sync the stream's presents — each decoded frame flips on the next display vsync
    /// (evenly paced, no tearing under direct scanout) instead of as soon as the GPU finishes
    /// (lowest latency — the default, OFF). Adaptive-refresh Macs present sparse input immediately
    /// and dense input once per link target. PUNKTFUNK_PRESENT_MODE=immediate|vsync|slot overrides
    /// that path for A/B. Resolved once per session; see Stage2Pipeline's header.
    public static let vsync = "punktfunk.vsync"
    /// macOS: present WINDOWED sessions in lockstep with the system compositor (the DCP
    /// "mismatched swapID's" kernel-panic mitigation — see SessionPresenter.windowedPresentMode
    /// and the MetalVideoPresenter saga notes). ON/unset (the default): windowed presents ride
    /// a Core Animation transaction — validated panic-free on the 240 Hz repro machine, at a
    /// small display-latency cost vs the raw path. OFF: windowed sessions keep the fast async
    /// image queue — ON AFFECTED SETUPS (high-refresh displays) THAT PATH KERNEL-PANICS THE
    /// WHOLE MAC, which is why the default is ON. Fullscreen always presents async (fast path)
    /// regardless. Resolved once per session; PUNKTFUNK_WINDOWED_PRESENT=async|transaction|
    /// surface overrides it for dev A/B.
    public static let windowedSafePresent = "punktfunk.windowedSafePresent"
    /// Allow variable refresh rate: hand the display link a wide frame-rate RANGE (low floor,
    /// preferred = stream rate) so a ProMotion / adaptive-sync display can vary its physical
    /// refresh to match the stream. On by default; a no-op on fixed-refresh displays. On macOS,
    /// VRR-on requests 24 Hz through the stream rate with the stream rate preferred; VRR-off
    /// fixes the link at the stream rate. iOS keeps its proven 30 Hz floor when off.
    /// Read per session/reconfigure by `SessionPresenter.syncFrameRate`.
    public static let allowVRR = "punktfunk.allowVRR"
    /// Request a 10-bit BT.2020 PQ (HDR10) stream. On by default; only takes effect when the host
    /// has HDR content AND this display supports HDR — otherwise the stream stays 8-bit SDR.
    public static let hdrEnabled = "punktfunk.hdrEnabled"
    /// Request a full-chroma 4:4:4 stream when this device can HARDWARE-decode it (`Stage444Probe`).
    /// On by default; only takes effect when the host also opted in to 4:4:4 (otherwise the stream
    /// stays 4:2:0). Sharper text/UI at the cost of more bandwidth.
    public static let enable444 = "punktfunk.enable444"
    /// Advertise 10-bit WITHOUT HDR, so an SDR desktop arrives at Main10 instead of 8-bit.
    /// Off by default, and subsumed by `hdrEnabled`, which already advertises the depth.
    /// Asks nothing of the display: the panel shows the same SDR picture, banding aside.
    public static let tenBitSdr = "punktfunk.tenBitSdr"
    public static let hosts = "punktfunk.hosts"
    /// How the host grid is ordered (a `HostSort` raw value) and what it's divided by (a
    /// `HostGrouping`). Per device, never per preset: it is this device's window on its own
    /// list, not something about how a host is streamed.
    public static let hostSort = "punktfunk.hostSort"
    public static let hostGrouping = "punktfunk.hostGrouping"
    /// The settings-preset catalog (`PresetCatalog`, one JSON blob) — design
    /// client-settings-profiles.md §4.2. Lives in the APP GROUP suite with `hosts`, not with the
    /// settings: bindings and pins are fields on the host record, and an extension that can read
    /// the hosts should be able to read what they point at.
    public static let presets = "punktfunk.presets"
    /// Where the catalog lived before the rename (design/preset-rename.md): read while `presets`
    /// is absent, and left as it was for an older build.
    public static let legacyPresets = "punktfunk.profiles"
    /// Physical-mouse model (macOS): "capture" (pointer lock + relative, the default) or
    /// "desktop" (uncaptured absolute pointer) — the cross-client `mouse_mode`. Replaces the
    /// never-shipped "punktfunk.cursorMode" (auto/always/never client-side-cursor setting,
    /// which was hidden while disabled and had no readers).
    public static let mouseMode = "punktfunk.mouseMode"
    /// Invert the scroll-wheel / two-finger-scroll direction sent to the host (both axes). Off by
    /// default: the local (natural-scrolling) sign passes through untouched. When on, the sign is
    /// negated at the connection's one outbound seam (`PunktfunkConnection.setInvertScroll`), so
    /// it flips consistently across the macOS wheel, the iOS trackpad pan, a GCMouse wheel, and the
    /// touch engine. For users whose host expects the opposite convention from their local OS
    /// preference.
    public static let invertScroll = "punktfunk.invertScroll"
    /// The in-stream quick-action ring: one JSON blob parsed by `OverlayConfig.parse` (six slots,
    /// shortcuts, the virtual pad's preset). Empty = the platform default ring. Cross-client
    /// `overlay_actions`, presetable as the whole blob.
    public static let overlayActions = "punktfunk.overlayActions"
    /// Location-based modifier mapping (a `ModifierLayout` value, default `.mac`): which Windows VK
    /// each PHYSICAL modifier position forwards to the host. `.mac` keeps ⌥ Option → Alt and
    /// ⌘ Command → Super/Win (the Apple positions). `.windows` swaps the Alt/Super ROLE between the
    /// Option and Command keys — preserving side (L/R) — so the key nearest the space bar acts as
    /// Alt and the next one as the Windows key, matching a Windows keyboard's `Ctrl / ⊞ / Alt` row.
    /// Only what's FORWARDED changes; client-local shortcuts (⌘⎋ &co.) stay on the physical ⌘ key.
    /// Read live at the wire boundary by `InputCapture`. Control/Shift never move (same position on
    /// both keyboards).
    public static let modifierLayout = "punktfunk.modifierLayout"
    /// Send system chords to the host while input is captured — the cross-client
    /// `inhibit_shortcuts`, ON by default. On the SDL clients it is SDL's keyboard grab (Alt+Tab,
    /// the Windows key); macOS has no such grab from a plain app, so `InputCapture`'s keyDown
    /// monitor implements it by taking every ⌘ chord off AppKit before a menu key equivalent can
    /// fire and forwarding it instead — which is what makes ⌘Q reach the host's compositor rather
    /// than quitting the client. Off keeps the chords local (the second-screen/work preset).
    /// The client's own reserved chords (⌘⎋, ⌃⌘F, ⌃⌥⇧…) are never forwarded either way, and — as
    /// on the SDL clients — the setting has no effect under the `desktop` mouse model, which is
    /// something you ⌘Tab *away* from. macOS-only today; nothing reads it on iOS/tvOS.
    public static let inhibitShortcuts = "punktfunk.inhibitShortcuts"
    /// iPad: capture the mouse/trackpad pointer (pointer lock → relative movement) for games,
    /// rather than forwarding an absolute cursor position. On by default. Only meaningful on iPad
    /// with a hardware mouse/trackpad; the system grants the lock only to a full-screen, frontmost
    /// scene and silently falls back to the absolute pointer when it can't (Stage Manager / Slide
    /// Over). Read by `StreamViewController.prefersPointerLocked`.
    public static let pointerCapture = "punktfunk.pointerCapture"
    /// iPhone/iPad: how touchscreen fingers drive the host — a `TouchInputMode` raw value:
    /// "trackpad" (default: relative cursor with tap-click / two-finger-scroll gestures),
    /// "pointer" (the cursor jumps to the finger), or "touch" (real multi-touch passthrough).
    /// Read live per gesture by `StreamLayerUIView`.
    public static let touchMode = "punktfunk.touchMode"
    // RETIRED: `punktfunk.libraryEnabled`, the "Show game library" switch. Pairing is the only
    // gate now — the fetch authenticates with the pinned identity, so an unpaired host could
    // only ever be refused. The Rust `library_enabled` went the same way; a stored value is
    // simply ignored, on every client. Do not reintroduce the key under a new name.
    /// How the library's titles are ordered within a group — a `LibrarySortKey` stored value
    /// (`"host"` = the host's own order, the default; `"title"` A–Z; `"platform"`; `"store"`).
    /// The cross-client `library_sort` key: the desktop console persists the same ids, and an
    /// unknown value reads as host order. Presentation only — a device preference, never part of
    /// a stream preset. Written by the library's sort/view bar and by the Collections screen.
    public static let librarySort = "punktfunk.librarySort"
    /// Which arrangement the gamepad library opens in — a `LibraryArrangement` stored value
    /// (`"shelf"` = the coverflow, the default; `"grid"`). The cross-client `library_view` key;
    /// unknown reads as shelf. Presentation only. One key, two surfaces: the library's bar and the
    /// Interface settings row both write it.
    public static let libraryView = "punktfunk.libraryView"
    /// Open a browsable library straight onto its Collections (group-by-platform tiles) instead of
    /// the shelf — the cross-client `library_collections` key. Off by default; a library that is
    /// not worth browsing (one platform, one store) opens on the shelf regardless. Presentation
    /// only.
    public static let libraryCollections = "punktfunk.libraryCollections"
    /// Where a bare launch opens — a `StartIn` stored value (`"hosts"` the default, `"library"`,
    /// `"stream"`). The cross-client `start_in` key; unknown reads as hosts, and with no default
    /// host every value degrades to the host list. Resolve through `StartScreen.resolve`, never by
    /// reading this alone. Presentation only — a device preference, never part of a preset.
    public static let startIn = "punktfunk.startIn"
    /// The host a bare launch opens on — a `StoredHost.id` uuid string, absent when there is none.
    /// The cross-client `default_host` key. Only half the answer: with exactly one paired host
    /// saved that host is the default with nothing written here, so `StartScreen.resolve` is the
    /// only correct reader. A dangling id falls through to that derived rule.
    public static let defaultHost = "punktfunk.defaultHost"
    /// The TOUCH library grid's grouping — `""` (none, the default), `"platform"` or `"store"`:
    /// one section per collated group. Touch-only: on the console the grouping is a PLACE
    /// (Collections), not a mode of the shelf, so there is no cross-client key for it. The sort it
    /// composes with is the shared `librarySort`. Presentation only.
    public static let libraryGroupBy = "punktfunk.libraryGroupBy"
    /// The Library tab's sections, in order: ids joined by commas, `-` before one switched off
    /// (`LibrarySectionLayout`). Empty means every section in its default order. Device-only,
    /// no cross-client key yet.
    public static let librarySections = "punktfunk.librarySections"
    /// The shelf the Library tab last showed: a `LibraryTarget.id` (`<host uuid>` or
    /// `<host uuid>#<preset id>`). A shelf whose host is gone falls back to the default host.
    public static let libraryShelf = "punktfunk.libraryShelf"
    /// macOS: take the window fullscreen while streaming and restore it on the host list. On by default.
    public static let fullscreenWhileStreaming = "punktfunk.fullscreenWhileStreaming"
    /// LEGACY (pre-tiered overlay): the old boolean stats-overlay toggle. Kept ONLY as the
    /// migration fallback `StatsVerbosity.current` reads when `statsVerbosity` was never
    /// written (absent-or-true → .normal, explicit false → .off). Never written anymore.
    public static let hudEnabled = "punktfunk.hudEnabled"
    /// The statistics overlay tier — a `StatsVerbosity` raw value ("off"/"compact"/"normal"/
    /// "detailed"). Absent → migrated from the legacy `hudEnabled` bool (see above). Cycle it
    /// while streaming with ⌃⌥⇧S (the cross-client Ctrl+Alt+Shift+S; macOS / hardware
    /// keyboard) or a three-finger tap (touch), matching the Android client.
    public static let statsVerbosity = "punktfunk.statsVerbosity"
    /// Which corner the statistics overlay sits in — a `HUDPlacement` raw value
    /// ("topLeading"/"topTrailing"/"bottomLeading"/"bottomTrailing"). Default top-trailing.
    public static let hudPlacement = "punktfunk.hudPlacement"
    /// The stats overlay's vocabulary: false (default) shows the figures Moonlight's overlay also
    /// shows, true the Advanced capture-to-glass view. Device-wide; a preset never carries it.
    public static let advancedStats = "punktfunk.advancedStats"
    /// iOS/iPadOS/macOS: switch the host list, settings and game library to a controller-friendly
    /// layout (the console launcher, gamepad-navigable settings, a coverflow-style library).
    /// On by default; WHEN it takes over is `gamepadUIMode`. See `GamepadUIEnvironment.isActive`.
    public static let gamepadUIEnabled = "punktfunk.gamepadUIEnabled"
    /// When `gamepadUIEnabled` actually takes over: `"connected"` (the default — only while a
    /// usable controller is attached, the behaviour this switch has always had) or `"always"`,
    /// for someone who prefers the console layout with no pad in reach (a TV-connected iPad, a
    /// Mac driven from the couch). Read only while `gamepadUIEnabled` is on, which is why the
    /// settings rows hide it when the switch is off. Anything unrecognized reads as
    /// `"connected"`. A device preference, never part of a stream preset.
    public static let gamepadUIMode = "punktfunk.gamepadUIMode"
    /// Which colour family the gamepad UI's living backdrop drifts through — a
    /// `GamepadPalette` id ("violet" = the brand default, then "oled"/"nebula"/"abyss"/"ember"/
    /// "moss"/"graphite", then the pale ones). The cross-client `ui_palette` key: the desktop
    /// console and the Android client carry the same table under the same names. Presentation
    /// only, so it is a device preference and never part of a stream preset. An unknown value
    /// reads as the default rather than failing — a newer client may have shipped a palette this
    /// build doesn't know.
    public static let uiPalette = "punktfunk.uiPalette"
    /// iPhone: ALSO play the rumble the host addresses to controller 1 (wire pad 0) on this
    /// device's own Taptic Engine — for phone-clip pads that ship without rumble motors, where
    /// the phone body is the only actuator in the player's hands. Off by default (opt-in); read
    /// once per session by `GamepadFeedback`. The toggle is shown only where the device actually
    /// has a haptic actuator (no iPad/Mac/TV).
    public static let rumbleOnDevice = "punktfunk.rumbleOnDevice"
    /// Use this device's own gyroscope as player 1's motion when the forwarded controller has
    /// none of its own — for clip-on and third-party pads without an IMU, where the device body
    /// moves with the player's hands. The rumble mirror's sibling, data flowing the other way.
    /// Off by default (opt-in); read once per session by `GamepadCapture`, whose `DeviceGyro`
    /// mirror engages only while pad 0's controller reports no rotation rate (a real gyro pad
    /// always wins). The toggle is shown only where the device has motion hardware
    /// (`DeviceGyro.isAvailable`).
    public static let gyroFromDevice = "punktfunk.gyroFromDevice"
    /// Auto-wake on connect: when connecting to a saved host that isn't advertising on mDNS, fire
    /// Wake-on-LAN and, if the dial fails, wait for it to come back before retrying (the "Waking…"
    /// overlay). On by default. Turn off if a host that's already on just isn't seen on mDNS (a
    /// routed/VPN host), so connects go straight through instead of waiting out the wake timeout.
    /// The explicit "Wake Host" action stays available regardless. Read by ContentView.startSession.
    public static let autoWake = "punktfunk.autoWake"
    /// iOS/iPadOS: keep a streaming session ALIVE when the app is backgrounded (audio background
    /// mode). Off by default (today's freeze-on-background is the default). When on, backgrounding a
    /// live session keeps audio playing and the QUIC/pump live while DROPPING video decode, and a
    /// bounded timer (`backgroundTimeoutMinutes`) auto-disconnects if the user doesn't return. Read
    /// by ContentView's scenePhase driver. Hidden on tvOS/macOS.
    public static let backgroundKeepAlive = "punktfunk.backgroundKeepAlive"
    /// iOS/iPadOS: minutes a backgrounded keep-alive session runs before auto-disconnecting (a
    /// battery/thermal/bandwidth backstop). Default 10; the UI offers 1/5/10/30. The auto-disconnect
    /// is non-deliberate (host linger kept), so a late return reconnects fast. Read on enterBackground.
    public static let backgroundTimeoutMinutes = "punktfunk.backgroundTimeoutMinutes"
}

extension Notification.Name {
    /// Posted by the app's Stream menu ("Release Mouse", ⌃⌥⇧Q): the key window's stream view
    /// releases input capture if it holds it. Only reachable while NOT captured (a captured
    /// session swallows the combo in InputCapture's monitor and the frozen cursor can't click
    /// menus) — it exists so the menu item is honest whenever it CAN fire, and as the shortcut's
    /// discoverable menu-bar surface.
    public static let punktfunkReleaseCapture = Notification.Name("io.unom.punktfunk.release-capture")
    /// The quick-action ring's Keyboard slot: summon the stream view's soft keyboard (iOS).
    public static let punktfunkShowSoftKeyboard = Notification.Name("io.unom.punktfunk.show-soft-keyboard")
    /// Asks a session to advance its stats tier; `object` is its connection, nil for every session.
    /// Posted by `StatsVerbosity.requestCycle`. The stored default does not move.
    public static let punktfunkStatsCycled = Notification.Name("io.unom.punktfunk.stats-cycled")

    /// Posted by the session view when the quick-action ring opens (`object` is a Bool `NSNumber`).
    /// The Mac's stream layer releases a captured mouse for as long as it is up and takes capture
    /// back on close: the ring's buttons are SwiftUI above the video, and a grabbed pointer can
    /// never reach one. macOS only — the touch clients' fingers reach the ring either way.
    public static let punktfunkRingOpen = Notification.Name("io.unom.punktfunk.ring-open")

    /// Posted by the app's Stream menu ("Quick Actions", ⌃⌥⇧O) and by InputCapture's monitor when the
    /// same combo fires while input is CAPTURED (a captured stream view never sees the menu's key
    /// equivalent). The session view toggles the quick-action ring. macOS only — the touch clients
    /// open the same ring with the two-finger twist, tvOS with a short Back.
    public static let punktfunkToggleQuickActions = Notification.Name("io.unom.punktfunk.toggle-quick-actions")

    /// Posted by the app's Stream menu ("Toggle Fullscreen", ⌃⌘F) and by InputCapture's monitor
    /// when the same combo fires while input is captured (the menu key-equivalent never reaches a
    /// captured stream view). The key window's `FullscreenController` flips the window's fullscreen
    /// state. macOS only.
    public static let punktfunkToggleFullscreen = Notification.Name("io.unom.punktfunk.toggle-fullscreen")

    /// Posted by InputCapture's chord path (⌃⌥⇧A) when the combo fires while input is CAPTURED —
    /// the state in which the Stream menu's identical key equivalent never reaches the app. The
    /// live session's owner (ContentView) flips the session's mic mute. Released, the menu item
    /// handles the same combo directly; both end at `SessionModel.toggleMicMute`.
    public static let punktfunkToggleMicMute = Notification.Name("io.unom.punktfunk.toggle-mic-mute")

    /// Posted by the Live Activity's / Shortcuts' End-stream intent (`EndStreamIntent.perform`,
    /// which runs in the app's process): the app tears the active session down deliberately
    /// (quit-close the host). Same cross-process-signal pattern as `punktfunkReleaseCapture` —
    /// the intent lives in PunktfunkShared and can't reach the app's `SessionModel` directly.
    public static let punktfunkEndActiveSession = Notification.Name("io.unom.punktfunk.end-active-session")

    /// Posted by the Connect App Intent (Siri/Shortcuts) with a `punktfunk://` URL as an `NSURL`
    /// `object`: one window routes it through the SAME `.onOpenURL` handler a widget tap uses. The
    /// intent uses `openAppWhenRun`, so the app is foregrounded to receive it.
    public static let punktfunkOpenDeepLink = Notification.Name("io.unom.punktfunk.open-deep-link")
}
