// Session state for the app shell: owns the connection, the input capture, the trust
// handshake phase, and the pump-thread → main-actor stats relay.

// AVFoundation: AVCaptureDevice.authorizationStatus (the mic TCC grant behind `micAvailable`)
// and, on tvOS, AVPlayer.eligibleForHDRPlayback (the TV-capability HDR gate).
import AVFoundation
import Foundation
import GameController
import os
import PunktfunkKit
import SwiftUI

#if canImport(AppKit)
    import AppKit
#elseif canImport(UIKit)
    import UIKit
#endif

/// 1 Hz latency-stage line mirrored to the unified log so the stages can be read WITHOUT the
/// on-screen HUD (Console.app, wirelessly on an iPad/Apple TV). The HUD is not a neutral
/// instrument: any visible overlay forces the metal layer through the compositor, which costs a
/// refresh period on the vsync-latched platforms — this is how to measure with it off.
private let statsLog = ClientLog(category: "stats")
/// The session's lifecycle — connect asked/landed/refused, how it ended. Until this existed a
/// client log bundle had a 1 Hz stats line and no sentence saying which host it was streaming
/// from, with what, or why it stopped; the host's own log has always said all three.
private let sessionLog = ClientLog(category: "session")
/// Mirror the 1 Hz vitals line to STDOUT as well as the unified log.
///
/// Exists for **tvOS, where the unified log is unreachable**: `log stream --device` is gone from
/// modern macOS, `log collect --device-name` needs root and then fails "Device not configured"
/// (an Apple TV has no USB to fall back to), and libimobiledevice pairs against a different
/// database than Xcode. Stdout, however, IS bridged — `xcrun devicectl device process launch
/// --console -e '{"PUNKTFUNK_STATS_STDOUT":"1"}' io.unom.punktfunk` streams these lines straight
/// to the Mac. That is the only way to read a session's numbers with the **stats overlay OFF**,
/// which matters because the overlay is itself a composited layer over the Metal one — i.e. a
/// plausible cause of the very present-floor inflation the overlay is used to measure.
/// Env-gated: no cost, and no stdout noise, unless someone is deliberately measuring.
private let statsToStdout = ProcessInfo.processInfo.environment["PUNKTFUNK_STATS_STDOUT"] == "1"

/// Pump-thread-side frame counters; a 1 Hz main-actor timer drains them into @Published
/// values. NSLock instead of an actor — the writer is the (non-async) pump thread.
final class FrameMeter: @unchecked Sendable {
    private let lock = NSLock()
    private var frames = 0
    private var bytes = 0

    func note(byteCount: Int) {
        lock.lock()
        frames += 1
        bytes += byteCount
        lock.unlock()
    }

    /// Returns and resets the per-interval counters.
    func drain() -> (frames: Int, bytes: Int) {
        lock.lock()
        defer {
            frames = 0
            bytes = 0
            lock.unlock()
        }
        return (frames, bytes)
    }
}

/// A held launch: the title, and the tile its cover flies out of.
struct LaunchHoldTarget: Equatable {
    let entry: GameEntry
    /// The shelf tile's rect in global (window) coordinates, when the launch came off a tile the
    /// hold can fly from. nil scales the cover up in place instead.
    let sourceRect: CGRect?
    /// Which launch this is, counting up for the life of the process — the hold's identity.
    ///
    /// Load-bearing, not bookkeeping: a hold that is removed and raised again lands in the same
    /// place in the view tree, and SwiftUI hands the new one the OLD one's state. Its cover then
    /// starts already landed, and the flight silently stops happening after the first launch.
    let seq: Int
}

/// The entry behind a `connect(launchID:)`, handed over out of band: the shelf's launch callbacks
/// carry only the id (five call sites, three tile views), and the launch hold needs the title, art
/// and tile rect. Keyed by id on the way out, so a stale entry can never dress a different launch.
enum LaunchedEntry {
    private static var last: (entry: GameEntry, rect: CGRect?)?

    static func remember(_ entry: GameEntry?, from rect: CGRect?) {
        last = entry.map { ($0, rect) }
    }

    /// The remembered entry for `id`, left for `take`.
    static func peek(_ id: String) -> GameEntry? {
        last?.entry.id == id ? last?.entry : nil
    }

    static func take(_ id: String, seq: Int) -> LaunchHoldTarget? {
        defer { last = nil }
        guard let last, last.entry.id == id else { return nil }
        return LaunchHoldTarget(entry: last.entry, sourceRect: last.rect, seq: seq)
    }
}

/// The values the views read for one session, each documented on its `SessionModel` property.
/// `disconnect` assigns a fresh one, so the next session starts with no hold, hint or mute.
struct SessionUI {
    var phase: SessionPhase = .idle
    var launchHold: LaunchHoldTarget?
    var launchWindowWait = false
    var launchDownload: HostDownload?
    var streamedGame: RunningGame?
    var hudLines: [PunktfunkConnection.HudLine] = []
    var notices = SessionNotices()
    var resizing = false
    var mouseCaptured = false
    var micMuted = false
    var virtualPadShown = false
    var clipboardEnabled = false
    var isBackgrounded = false
    var backgroundDeadline: Date?
}

/// How long the launch hold waits on a title the host still calls `launching`, or `running` without
/// its window (a cold Steam boot with shader work runs to minutes), and on one the host never lists
/// at all (the launch did not resolve; the host logs it and streams on).
private let launchHoldMax: TimeInterval = 120
private let launchNoLease: TimeInterval = 15
/// The demo host's pretend start: the cover's 0.75 s flight, its details, then a beat of
/// "Starting the game…".
private let demoLaunchHold: TimeInterval = 3

@MainActor
final class SessionModel: ObservableObject {
    typealias Phase = SessionPhase

    /// What the views show for this session, one field per property below. `disconnect`
    /// resets it whole.
    @Published private var ui = SessionUI()
    private(set) var phase: Phase { get { ui.phase } set { ui.phase = newValue } }
    @Published private(set) var connection: PunktfunkConnection? {
        didSet {
            #if os(macOS)
            Self.liveConnections[ObjectIdentifier(self)] = connection
            #endif
        }
    }
    #if os(macOS)
    /// Every window's live connection: a Cmd+Q runs no window's teardown.
    private static var liveConnections: [ObjectIdentifier: PunktfunkConnection] = [:]
    /// Teardowns still running off-main. Closing the last window quits the app under them.
    private static let closing = DispatchGroup()

    /// App quit: end each live session as a deliberate quit, before the process goes.
    static func quitAll() {
        for conn in liveConnections.values {
            conn.disconnectQuit()
            conn.close()
        }
        liveConnections.removeAll()
        // A close waits out plane polls of 100–200 ms. 3 s bounds a wedged one.
        _ = closing.wait(timeout: .now() + 3)
    }
    #endif
    /// The launched title whose game is not up yet: its cover flies out of the shelf tile at the
    /// tap and holds the screen — through the dial, and then over the stream — until the host's
    /// `/status` says the game left `launching` (`punktfunk-host::gamelease`), or the player asks
    /// to see. nil for a desktop connect, a launcher tile, and once revealed.
    ///
    /// Raised at `connect`, not at first frame: the shelf is still on screen at the tap, which is
    /// the only moment the cover has somewhere to fly FROM, and holding from there means one
    /// unbroken screen from the tap to the game rather than a stream of the launcher in between.
    private(set) var launchHold: LaunchHoldTarget? { get { ui.launchHold } set { ui.launchHold = newValue } }
    /// The console's own launch hold is up, as the console reports it: the console stays over
    /// the stream it dialled, and the stream takes no input, until the console lets go.
    @Published var consoleHold = false
    /// The launched game is up and the host is waiting for its window.
    private(set) var launchWindowWait: Bool { get { ui.launchWindowWait } set { ui.launchWindowWait = newValue } }
    /// The launched title's files, while the host fetches them before it opens the stream.
    private(set) var launchDownload: HostDownload? { get { ui.launchDownload } set { ui.launchDownload = newValue } }
    private var launchWatch: Task<Void, Never>?
    /// Counts launches, so each hold is a view of its own — see `LaunchHoldTarget.seq`.
    private var launchSeq = 0
    /// Counts connect ATTEMPTS. The handshake runs off-main and identifies itself on the way back;
    /// phase plus host id is not enough, because cancelling a dial and re-dialling the same host
    /// satisfies both and the abandoned attempt can land first.
    private var connectSeq = 0
    /// The host this session is for (a value copy; identity = id).
    @Published private(set) var activeHost: StoredHost? {
        didSet { Self.activeHosts[ObjectIdentifier(self)] = activeHost?.id }
    }
    /// Every window's dialing or live host. A host replaces a second session from this device,
    /// so a connect to one already live elsewhere would end that window's stream.
    private static var activeHosts: [ObjectIdentifier: StoredHost.ID] = [:]

    /// One connect's arguments, kept so a lost session or a waking host can dial them again,
    /// and which redial attempt that is.
    struct Redial {
        var host: StoredHost
        let effective: EffectiveSettings
        let gamepad: PunktfunkConnection.GamepadType
        let overrides: GameOverrides?
        let launchID: String?
        let profileID: String?
        let shelf: LibraryTarget?
        var attempt: UInt32 = 0
        var allowTofu = false
        /// Auto-wake was on: a host that refuses setup before streaming is dialed again.
        var autoWake = false
        /// When that host first refused; nil until it has.
        var wakeSince: ContinuousClock.Instant?
    }
    /// The last connect's arguments.
    private var lastDial: Redial?
    /// A lost session waiting to dial again, or dialing: the connect overlay stays up for it.
    /// Cleared once a redial streams, fails for good, or the user cancels.
    @Published private(set) var redial: Redial?
    /// The library entry this session was launched with (`connect(launchID:)`), or nil if the user
    /// just connected to the host's desktop. Kept because where the client should go when the
    /// session ends depends on where it came FROM: a title launched out of the library belongs back
    /// in that library when its game exits, not on the host-selection screen.
    private var launchedTitleID: String?
    /// WHICH library shelf that title was launched from — a host's own, or one of its pinned
    /// host+preset cards (§5.2a). The host alone would not answer it: a pinned card's shelf
    /// launches with that card's preset, so returning to the host's default shelf would quietly
    /// change what the next title streams with.
    private var launchedShelf: LibraryTarget?
    /// Set when a session ended because its game exited and it began as a library launch: the
    /// shelf to reopen. The view layer consumes it and sets it back to nil.
    @Published var returnToLibrary: LibraryTarget?
    /// The settings THIS session runs on — the globals with its preset overlaid, resolved once at
    /// connect (design/client-settings-profiles.md §4.2). The connection carries the same value
    /// for the readers that live in PunktfunkKit and can't see this model.
    @Published private(set) var settings = EffectiveSettings()
    /// The host set something for this session's game: the stats line says so.
    private var hostOverrides = false
    /// The stats-overlay tier for this session: the resolved one at connect, then whatever the
    /// live cycle surfaces (⌃⌥⇧S, the three-finger tap) move it to. Separate from the @AppStorage
    /// global so a preset that overrides the tier actually gets it, without the cycle breaking.
    @Published var statsVerbosity: StatsVerbosity = .normal
    @Published var errorMessage: String?
    /// The stats overlay's lines for the live tier and vocabulary: formatted by the core each
    /// second and on every tier change (`renderHud`).
    private(set) var hudLines: [PunktfunkConnection.HudLine] { get { ui.hudLines } set { ui.hudLines = newValue } }
    /// Mirrors StreamView's capture state (it owns the input capture; this drives the
    /// HUD's "click to capture" / "⌘⎋ releases" hint).
    var mouseCaptured: Bool { get { ui.mouseCaptured } set { ui.mouseCaptured = newValue } }
    /// The USER's in-stream mic mute (the HUD button, the Stream menu's ⌃⌥⇧A, the captured-state
    /// chord, the iOS mic disc) — session state, deliberately NOT persisted: a mute is for the
    /// people in the room right now, so every new session starts live if the mic is on at all.
    /// One of the two inputs to the effective mute; `isBackgrounded` is the other, and
    /// `applyMicMute` composes them — a user mute survives a trip through the background, and the
    /// background's privacy mute never clears the user's choice. Local and instant: it gates
    /// capture on this device, nothing is sent to the host.
    private(set) var micMuted: Bool { get { ui.micMuted } set { ui.micMuted = newValue } }
    /// The stream's hints, notices and access chip.
    private(set) var notices: SessionNotices { get { ui.notices } set { ui.notices = newValue } }
    /// Resize overlay (design/midstream-resolution-resize.md — client resize UX): true from the
    /// instant a Match-window resize starts steering toward a new size until a frame at that size
    /// decodes (or a safety timeout). Drives the blur+spinner so the unavoidable host-rebuild delay
    /// reads as a deliberate, acknowledged transition instead of a stutter. Pure state lives in
    /// `ResizeIndicator`; this mirrors its `active` for SwiftUI.
    private(set) var resizing: Bool { get { ui.resizing } set { ui.resizing = newValue } }
    /// START = follower steering (main actor), END = a new-mode IDR's coded dims (decode pump,
    /// hopped to main), TIMEOUT = safety net for a rejected/capped switch that never yields a
    /// differently-sized frame. Ticked from the 1 Hz stats timer.
    private var resizeIndicator = ResizeIndicator()
    /// The last decoded picture's size: smaller than the host's mode when the host encodes the
    /// view. nil until the first frame.
    private var picture: (width: UInt32, height: UInt32)?
    /// `picture` as the Live Activity names it, `1920×1080`; empty until the first frame.
    @Published private(set) var pictureText = ""

    /// Received AUs, for the once-a-second log gate: nothing is logged while no frame flows.
    let meter = FrameMeter()
    /// Capture→on-glass, written per presented frame by the stage-2 presenter. The A/V sync loop
    /// reads its latest sample; the overlay's own figures live in the core.
    let endToEnd = LatencyMeter()
    /// Receipt → pull wait (client queue), fed per AU by the stream view's onFrame. Apple-only,
    /// so it reaches the overlay as an extra line.
    let clientQueue = LatencyMeter()
    /// Its p50 over the last window, for `hudFacts`.
    private var queueP50Ms: Double?
    /// Everything this session started. `disconnect` stops it in reverse start order.
    private let resources = SessionResources()
    private var audio: SessionAudio? { resources.part() }
    private var gamepadCapture: GamepadCapture? { resources.part() }
    /// The in-stream ring's remote path (design/touch-client-overlay.md §2.6): the Siri Remote's
    /// short Back. Wired by the view once per session.
    var onRingChord: ((UInt32?) -> Void)?
    /// A pad or Steam Controller shortcut the session runs (design/client-shortcuts.md): the
    /// action, its phase and the wire pad that fired it, `nil` for a captured Steam Controller.
    var onShortcut: ((Shortcuts.Action, Shortcuts.Phase, UInt32?) -> Void)?
    var onRingNav: ((RingNav) -> Void)?
    /// A connect the host parked for its operator: the view shows the approval wait.
    var onAwaitingApproval: ((StoredHost) -> Void)?
    /// The saved record for a host as it is now, which a redial dials. Unset, a redial dials the
    /// copy its first dial took.
    var currentRecord: (@MainActor (StoredHost.ID) -> StoredHost?)?

    /// The ring is up: the pad belongs to it (flushed on the host, edges become navigation).
    func setRingOpen(_ open: Bool) {
        if open { refreshStreamedGame() }
        gamepadCapture?.ringOpen = open
        virtualPad?.masked = open
        // A captured SC2 never enters GamepadCapture, so it hands the ring its pad itself.
        sc2Capture?.ringOpen = open
        #if os(tvOS)
        remotePointer?.ringOpen = open
        #endif
    }

    /// One synthetic system-button tap on the host's pad (a `GamepadWire` bit) — the ring's
    /// guide / quick-access slots, which reach the host where the physical button cannot.
    func tapPadButton(_ bit: UInt32, pad: UInt32? = nil) {
        gamepadCapture?.tapButton(bit, pad: pad)
    }

    /// A system button a shortcut holds down on the host's pad, or lets go of.
    func holdPadButton(_ bit: UInt32, down: Bool, pad: UInt32?) {
        gamepadCapture?.holdButton(bit, down: down, pad: pad)
    }

    /// A shortcut to a danger action arms on its first press and fires on a second within 2 s,
    /// as the dial's does. True when this press is the second.
    func confirmShortcut(_ id: String, prompt: String) -> Bool {
        if notices.shortcutArm.value?.id == id {
            notices.shortcutArm.clear()
            return true
        }
        flash(\.shortcutArm, ShortcutArm(id: id, text: prompt), for: 2)
        return false
    }

    /// The ring's Controller type slot: what every forwarded pad declares, whether one is
    /// forwarded, and the step to the next type. This stream only; the next starts from Settings.
    var padType: PunktfunkConnection.GamepadType { gamepadCapture?.padType ?? .auto }

    var padTypeAvailable: Bool {
        virtualPadAvailable && gamepadCapture?.hasForwardedPads == true
    }

    func cyclePadType() {
        guard let capture = gamepadCapture else { return }
        capture.setPadType(capture.padType.nextInRing)
    }

    /// The virtual on-screen controller (design/touch-client-overlay.md §4): shown from the
    /// ring's `pad` slot, per session. While up it holds one wire pad, so the host sees one
    /// controller arrive and, on hide, one leave (§9). Never toggled by the ring's own open and
    /// close (§8 trap 4).
    private(set) var virtualPadShown: Bool { get { ui.virtualPadShown } set { ui.virtualPadShown = newValue } }
    var virtualPad: VirtualPadWire? { resources.part() }

    /// Whether the pad's input can reach the host at all: the forwarding setting and the
    /// session's controller grant, the two gates a real pad's sends have.
    var virtualPadAvailable: Bool {
        settings.gamepadForwarding && connection?.canSendGamepad == true
    }

    func toggleVirtualPad() {
        if let pad = virtualPad {
            pad.close()
            resources.remove(pad)
            virtualPadShown = false
            return
        }
        guard let conn = connection, let pad = VirtualPadWire(connection: conn, manager: .shared) else { return }
        // Toggled from the ring, so the ring is up: the pad starts masked and unmasks on close.
        pad.masked = gamepadCapture?.ringOpen ?? false
        // Started last, so it closes first: its pad leaves the wire before any capture stops.
        resources.add(pad, stop: pad.close)
        virtualPadShown = true
    }
    /// The live session's Steam Controller 2 as-is passthrough (`settings.sc2Capture` &&
    /// `settings.gamepadForwarding`), started in `beginStreaming`.
    private var sc2Capture: Sc2Capture? { resources.part() }
    #if !os(tvOS)
    /// The live session's clipboard bridge (design/clipboard-and-file-transfer.md §5), when the
    /// per-host toggle is on and the host advertises `HOST_CAP_CLIPBOARD`.
    private var clipboardSync: ClipboardSync? { resources.part() }
    #endif
    /// Whether clipboard sync is live (host-acked `ClipState.enabled`) — drives the Stream menu
    /// item's title and the settings footnote. Always false on tvOS, which has no pasteboard.
    private(set) var clipboardEnabled: Bool { get { ui.clipboardEnabled } set { ui.clipboardEnabled = newValue } }
    /// The host's last `ClipState.reason` (`CLIP_REASON_*`) — why an enable was refused
    /// (backend unavailable / policy disabled / …); 0 = OK.

    #if os(tvOS)
    /// Siri Remote → host pointer while streaming (touch surface moves, press = left click,
    /// Play/Pause = right click) + the remote's deliberate exit (hold Back ≥ 1 s). See
    /// SiriRemotePointer — same trust gate/lifecycle as the gamepad capture above.
    private var remotePointer: SiriRemotePointer? { resources.part() }
    #endif

    var isBusy: Bool { phase != .idle }

    /// True while a streaming session is running in the background under the opt-in keep-alive
    /// (audio plays, video dropped, timeout armed). Drives the Live Activity's stage/countdown (M3)
    /// and is cleared on foreground or teardown. iOS/iPadOS only in practice.
    private(set) var isBackgrounded: Bool { get { ui.isBackgrounded } set { ui.isBackgrounded = newValue } }
    /// When the backgrounded keep-alive will auto-disconnect (nil unless backgrounded) — drives the
    /// Live Activity countdown. Set alongside the background timer.
    private(set) var backgroundDeadline: Date? { get { ui.backgroundDeadline } set { ui.backgroundDeadline = newValue } }

    /// Holds off display sleep (and, on macOS, the screen saver) for the life of a session —
    /// nothing about watching a stream looks like user activity to the OS, least of all a
    /// controller-only session. Acquired in `beginStreaming`, released in `disconnect`.
    private let displaySleepGuard = DisplaySleepGuard()

    /// Dial `host` with `effective`, the session's settings already resolved from the globals and
    /// its preset (§4.4). The connection carries them, so kit-side readers never re-read globals.
    /// A mode render scale makes larger goes with its view (`EffectiveSettings.streamView`).
    ///
    /// `allowTofu` is true only for an unpinned host that advertised `pair=optional` (rule 3a);
    /// any other unpinned host is refused and routed to pairing (rule 3b). A pinned host's stored
    /// fingerprint is the trust decision.
    ///
    /// `requestAccess` opens an identified connect the host parks until its operator approves,
    /// then admits on the same connection. The budget outlasts the park window, and success is
    /// the trust decision the caller pins. `host.pinnedSHA256`, when set, pins the advertised
    /// certificate for the wait.
    ///
    /// `onUnreachable` replaces the "couldn't reach" alert for a plain failure: the caller takes
    /// over (the Wake-on-LAN wait). It never fires for `requestAccess`.
    func connect(to host: StoredHost, effective: EffectiveSettings,
                 gamepad: PunktfunkConnection.GamepadType = .auto,
                 /// The host's overrides for the launched game; `effective` already carries
                 /// their HDR and pad. Their lines and refresh are fitted to this display here.
                 overrides: GameOverrides? = nil,
                 launchID: String? = nil,
                 /// The host profile to play as; nil lets the host choose.
                 profileID: String? = nil,
                 /// The library shelf this session started from, so its end can return there —
                 /// the title's shelf for a launch, and the shelf itself for a Resume, which
                 /// launches nothing. nil for a connect that did not come off one.
                 shelf: LibraryTarget? = nil,
                 allowTofu: Bool = false,
                 autoTrust: Bool = false,
                 requestAccess: Bool = false,
                 /// Auto-wake is on (Settings → General).
                 autoWake: Bool = false,
                 /// When the host first refused as still waking; nil on a first dial.
                 wakeSince: ContinuousClock.Instant? = nil,
                 /// A wake redial's launch hold, still up from the dial it repeats. nil raises
                 /// the launch's own from `LaunchedEntry`.
                 hold: LaunchHoldTarget? = nil,
                 /// The host refused `profileID` as unknown. Return true to re-dial: the error
                 /// is then not shown.
                 onProfileUnknown: (@MainActor () -> Bool)? = nil,
                 /// Opens `profileID`'s locked account; this dial alone sends it, never a redial.
                 unlock: ProfileUnlock? = nil,
                 /// The host holds this unlock dial while the profile's desktop starts.
                 onUnlockPending: (@MainActor () -> Void)? = nil,
                 /// The host closed with `ProfileLocked` and this sentence. Return true when the
                 /// caller prompts: the error is then not shown.
                 onProfileLocked: (@MainActor (String?) -> Bool)? = nil,
                 onUnreachable: (@MainActor () -> Void)? = nil) {
        guard phase == .idle else { return }
        guard !Self.activeHosts.contains(where: { $0.key != ObjectIdentifier(self) && $0.value == host.id })
        else {
            // A redial refused here has no next attempt: the connect overlay must not wait on one.
            redial = nil
            errorMessage = L10n.appleDialAlreadyStreamingHere(host: host.displayName)
            return
        }
        connectSeq += 1
        let attempt = connectSeq
        lastDial = Redial(
            host: host, effective: effective, gamepad: gamepad, overrides: overrides,
            launchID: launchID, profileID: profileID, shelf: shelf, allowTofu: allowTofu,
            autoWake: autoWake && !requestAccess, wakeSince: wakeSince)
        phase = .connecting
        activeHost = host
        launchedTitleID = launchID
        launchedShelf = shelf
        // The host never tracks a launcher tile, so there is nothing to wait for.
        launchSeq += 1
        launchHold = hold ?? launchID.flatMap { LaunchedEntry.take($0, seq: launchSeq) }
            .flatMap { $0.entry.isLauncher ? nil : $0 }
        launchWindowWait = false
        launchDownload = nil
        errorMessage = nil
        settings = effective
        hostOverrides = overrides.map { !$0.isEmpty } ?? false
        statsVerbosity = StatsVerbosity(rawValue: effective.statsVerbosity) ?? .normal
        #if os(iOS)
        // An attached monitor shows the picture (StreamViewController), so Native is its mode.
        let native = ExternalDisplay.nativeMode ?? NativeDisplay.mode
        #else
        let native = NativeDisplay.mode
        #endif
        let own = effective.streamMode(native: native)
        let mode = overrides?.fit(
            own, native: native, codecMax: RenderScale.maxDimension(codec: effective.codec)) ?? own
        let view = effective.streamView(native: native, mode: mode)
        let pin = host.pinnedSHA256
        // Screens (main-actor — screen APIs): with All monitors, a host screen per display now and
        // the promise to show more as displays come.
        #if os(macOS)
        let showsScreens = effective.screens == "all"
        let scale = (effective.renderScale, RenderScale.maxDimension(codec: effective.codec))
        let screenAsk = showsScreens ? ScreenAsk.ofThisMac(mode: mode, scale: scale) : .none
        #else
        let showsScreens = false
        let screenAsk = ScreenAsk.none
        #endif
        // Capability gate (main-actor — screen APIs): only advertise HDR when this display can
        // actually present it, so the host sends a proper SDR stream to an SDR display rather than
        // BT.2020 PQ the panel would mis-tone-map. The display self-tone-maps HDR from the mastering
        // metadata we apply (Step 2) when it IS HDR.
        let displayHDR: Bool = {
            #if os(macOS)
                // The potential headroom, not the current one: macOS allocates EDR only when content
                // asks, so the current value reads 1.0 on an idle desktop and an HDR monitor would
                // never be advertised at connect.
                return (NSScreen.main?.maximumPotentialExtendedDynamicRangeColorComponentValue ?? 1.0) > 1.0
            #elseif os(tvOS)
                // NOT the EDR headroom here: on tvOS that reflects the CURRENT output mode, and
                // Apple's recommended setup runs an SDR home screen with Match Content — an
                // HDR-capable TV would read 1.0 at connect time and never be advertised. The
                // session switches the display to HDR10 itself once streaming (AVDisplayManager —
                // see StreamViewIOS), so gate on the TV's mode-independent capability; if the
                // switch never lands, the presenter's in-shader tone-map keeps PQ safe anyway.
                return AVPlayer.eligibleForHDRPlayback
            #elseif os(visionOS)
                return true // every Vision Pro display presents HDR
            #else
                return NativeDisplay.screen.potentialEDRHeadroom > 1.0
            #endif
        }()
        let unlocking = unlock != nil
        Task.detached(priority: .userInitiated) {
            // PunktfunkConnection.init blocks on the QUIC handshake — keep it off the main
            // actor. The persistent identity is presented on every connect so a paired
            // host recognizes this Mac (nil = anonymous, fine for hosts without
            // --require-pairing; Keychain/generation failure must not block connecting).
            let identity = (try? ClientIdentityStore.shared.load())?.identity
            // The decoder probes run here too: each is a one-off decode or shader compile.
            let offer = ConnectOffer.resolve(
                effective, mode: mode, displayHDR: displayHDR,
                pyroWave: MetalWaveletDecoder.supported, av1: AV1.hardwareDecodeSupported,
                hw444EightBit: Stage444Probe.hwDecode444_8bit,
                hw444TenBit: Stage444Probe.hwDecode444_10bit)
            let connectLine = "connect \(host.displayName) \(host.address):\(host.port) "
                + "mode=\(offer.width)x\(offer.height)@\(offer.hz) "
                + "view=\(view.map { "\($0.width)x\($0.height)" } ?? "-") codec=\(effective.codec) "
                + "bitrate=\(offer.bitrateKbps)kbps "
                + "hdr=\(offer.hdr) 10bit=\(offer.tenBit) 444=\(offer.want444) "
                + "audio=\(offer.audioChannels)ch/\(offer.audioRateHz)Hz/\(offer.audioBits)bit "
                + "pinned=\(pin != nil) tofu=\(allowTofu) launch=\(launchID ?? "-") "
                + "overrides=\(overrides.map { !$0.isEmpty } ?? false) "
                + "unlock=\(unlock.map { "\($0.kind)" } ?? "-")"
            sessionLog.info("\(connectLine, privacy: .public)")
            let result = Result { try PunktfunkConnection(
                host: host.address, port: host.port,
                width: offer.width, height: offer.height, refreshHz: offer.hz, view: view,
                pinSHA256: pin, identity: identity, compositor: offer.compositor,
                gamepad: gamepad, bitrateKbps: offer.bitrateKbps, videoCaps: offer.videoCaps,
                audioChannels: offer.audioChannels,
                audioRateHz: offer.audioRateHz, audioBits: offer.audioBits,
                videoCodecs: offer.videoCodecs, preferredCodec: offer.preferredCodec,
                clientCaps: offer.clientCaps, videoFit: offer.videoFit,
                launchID: launchID, profileID: profileID,
                // Delegated approval: the host holds this connect open until the operator approves
                // it (~180 s) — outwait that window so a slow approval still lands here. Normal
                // connects keep the snappy default.
                timeoutMs: requestAccess ? 185_000 : 10_000,
                settings: effective,
                screens: screenAsk, showsScreens: showsScreens,
                unlock: unlock,
                onPending: { [weak self] in
                    Task { @MainActor in
                        guard let self, self.connectSeq == attempt, self.phase == .connecting
                        else { return }
                        // An unlock's wait is the host checking it and starting the desktop.
                        if unlocking { onUnlockPending?() } else { self.onAwaitingApproval?(host) }
                    }
                }) }
            await MainActor.run { [weak self] in
                guard let self else { return }
                // The user may have abandoned this attempt (window closed, another host
                // clicked) while the handshake was in flight — don't resurrect a session
                // for a dead window, and especially don't start its mic uplink. Matched on the
                // ATTEMPT, not just phase and host: cancel a slow dial and re-dial the same host
                // and both attempts pass that pair, so the abandoned one could be adopted.
                guard self.connectSeq == attempt, self.phase == .connecting,
                      self.activeHost?.id == host.id else {
                    if case .success(let conn) = result {
                        Task.detached { conn.close() } // joins Rust threads — off-main
                    }
                    return
                }
                switch result {
                case .success(let conn):
                    // requestAccess: the operator approved this device on the host, so the
                    // session is trusted (the caller pins it as paired).
                    self.adopt(
                        conn, host: host, trusted: pin != nil || autoTrust || requestAccess,
                        allowTofu: allowTofu)
                case .failure(let error):
                    sessionLog.warning(
                        "connect \(host.displayName, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                    self.phase = .idle
                    self.activeHost = nil
                    let recovery = DialRecovery.decide(
                        error,
                        wakeDelay: self.lastDial.flatMap {
                            PunktfunkConnection.wakeRedialDelay(
                                after: error, autoWake: $0.autoWake,
                                sinceFirst: self.sinceFirstWake)
                        },
                        hostName: host.displayName, pinned: pin != nil,
                        requestAccess: requestAccess, callerRecovers: onUnreachable != nil)
                    // The launch hold is an opaque cover only `revealStream` drops. Left up, it
                    // sits over the home screen, and on tvOS the host grid can't take focus.
                    if !recovery.keepsLaunchHold { self.revealStream() }
                    self.recover(
                        recovery, onProfileUnknown: onProfileUnknown,
                        onProfileLocked: onProfileLocked, onUnreachable: onUnreachable)
                }
            }
        }
    }

    /// Perform a failed dial's recovery. A profile handler returns true when it takes the failure
    /// over (it dials again or prompts); otherwise the recovery after it runs.
    private func recover(
        _ recovery: DialRecovery,
        onProfileUnknown: (@MainActor () -> Bool)?,
        onProfileLocked: (@MainActor (String?) -> Bool)?,
        onUnreachable: (@MainActor () -> Void)?
    ) {
        switch recovery {
        case .profileUnknown(let otherwise):
            guard onProfileUnknown?() != true else { return }
            recover(
                otherwise, onProfileUnknown: nil, onProfileLocked: nil, onUnreachable: onUnreachable)
        case .profileLocked(let said, let otherwise):
            guard onProfileLocked?(said) != true else { return }
            recover(
                otherwise, onProfileUnknown: nil, onProfileLocked: nil, onUnreachable: onUnreachable)
        case .wakeRedial(let wait):
            if let plan = lastDial { redialWhileWaking(plan, after: wait) }
        case .alert(let message):
            redial = nil
            errorMessage = message
        case .handBack:
            onUnreachable?()
        }
    }

    /// Route a dial that landed. A trusted host (pinned, auto-trusted or operator-approved)
    /// streams at once; an unpinned one advertising `pair=optional` gets the TOFU prompt over the
    /// live, blurred stream (rule 3a); any other is closed and refused (rule 3b), and the caller
    /// routes it to pairing.
    private func adopt(
        _ conn: PunktfunkConnection, host: StoredHost, trusted: Bool, allowTofu: Bool
    ) {
        let landed = "connected \(host.displayName) "
            + "mode=\(conn.width)x\(conn.height)@\(conn.refreshHz) "
            + "codec=\(conn.videoCodec) bitrate=\(conn.resolvedBitrateKbps)kbps "
            + "depth=\(conn.bitDepth) chroma=\(conn.isChroma444 ? "444" : "420") hdr=\(conn.isHDR) "
            + "audio=\(conn.resolvedAudioChannels)ch/\(conn.resolvedAudioRateHz)Hz/\(conn.resolvedAudioBits)bit "
            + "shard=\(conn.shardPayload) compositor=\(conn.resolvedCompositor.rawValue) "
            + "gamepad=\(conn.resolvedGamepad.rawValue) mgmt=\(conn.hostMgmtPort)"
        sessionLog.info("\(landed, privacy: .public)")
        redial = nil
        if trusted {
            connection = conn
            noteTouchFallback(conn)
            startStatsTimer()
            beginStreaming()
        } else if allowTofu {
            connection = conn
            noteTouchFallback(conn)
            startStatsTimer()
            phase = .awaitingTrust(fingerprint: conn.hostFingerprint)
        } else {
            Task.detached { conn.close() } // joins Rust threads — off-main
            phase = .idle
            activeHost = nil
            revealStream() // no stream is coming; the hold must not outlive the dial
            errorMessage = L10n.appleDialNotPaired(host: host.displayName)
        }
    }

    // MARK: - Background keep-alive (opt-in, iOS)

    /// Enter the backgrounded keep-alive state: keep audio playing, DROP video decode (no GPU work
    /// off-screen), mute the mic (privacy), and arm a bounded auto-disconnect. The caller
    /// (ContentView's scenePhase driver) gates this on the setting + `.streaming`; a no-op otherwise.
    /// The video-drop seam is read by both pumps every iteration (`connection.isVideoDropped`).
    func enterBackground(timeoutMinutes: Int) {
        guard phase == .streaming, let conn = connection, !isBackgrounded else { return }
        isBackgrounded = true
        conn.setVideoDropped(true)
        applyMicMute() // now muted for privacy — on top of the user's own mute, not instead of it
        // Non-deliberate on fire (keep the host linger) so a user who returns late reconnects fast,
        // exactly like today's network-drop path. min 1 minute guards a nonsense setting.
        let minutes = max(1, timeoutMinutes)
        backgroundDeadline = Date().addingTimeInterval(TimeInterval(minutes * 60))
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + .seconds(minutes * 60))
        timer.setEventHandler { [weak self] in
            // The timer fires on `.main`, so the actor's executor is the main thread here.
            MainActor.assumeIsolated { self?.disconnect(deliberate: false) }
        }
        cancelBackgroundTimer()
        resources.add(timer, stop: { timer.cancel() })
        timer.resume()
    }

    /// The keep-alive's auto-disconnect, the one dispatch source a session starts.
    private func cancelBackgroundTimer() {
        guard let timer: DispatchSourceTimer = resources.part() else { return }
        timer.cancel()
        resources.remove(timer)
    }

    /// Return to foreground: cancel the timeout, resume mic + video, and force a clean re-anchor —
    /// request a fresh IDR (infinite GOP: it won't come on its own) and let the pump's freeze gate
    /// withhold the concealed frames until it lands (it auto-arms on the resumed frame-index gap).
    func exitBackground() {
        guard isBackgrounded else { return }
        isBackgrounded = false
        backgroundDeadline = nil
        cancelBackgroundTimer()
        applyMicMute() // back to the user's own choice — which may well still be "muted"
        if let conn = connection {
            conn.setVideoDropped(false)
            conn.requestKeyframe()
        }
    }

    // MARK: - Microphone mute (in-stream, per session)

    /// Whether this session has a mic uplink there is any point in muting (`SessionMic`): drives
    /// whether the mute control is offered.
    var micAvailable: Bool {
        SessionMic.available(
            enabled: settings.micEnabled, granted: connection?.canUseMic != false,
            authorization: AVCaptureDevice.authorizationStatus(for: .audio))
    }

    /// Flip the user's mute. The in-stream surfaces (HUD button, Stream menu, ⌃⌥⇧A while
    /// captured, the iOS mic disc, the dial) all land here.
    func toggleMicMute() {
        setMicMuted(!micMuted)
    }

    /// Set the user's mute, and say so for a moment in either direction. Ignored when the
    /// session has no microphone, so a stale surface can't raise a "muted" notice over a
    /// session that was never sending anything.
    func setMicMuted(_ muted: Bool) {
        guard micAvailable, micMuted != muted else { return }
        micMuted = muted
        applyMicMute()
        flash(\.micMute, muted)
    }

    /// Show `value` in the notice at `slot` for `seconds`. Last one wins and its clock restarts:
    /// a second hint under a still-visible first would only read as a stutter.
    private func flash<Value>(
        _ slot: WritableKeyPath<SessionNotices, Flash<Value>>, _ value: Value,
        for seconds: UInt64 = SessionNotices.hintSeconds
    ) {
        notices[keyPath: slot].show(value, for: seconds) { [weak self] in
            self?.notices[keyPath: slot].clear()
        }
    }

    /// Say so when the passthrough touch model meets a host without touch injection — the same
    /// fallback `StreamLayerUIView` applies to the fingers themselves.
    private func noteTouchFallback(_ conn: PunktfunkConnection) {
        #if os(iOS) || os(visionOS)
        guard TouchInputMode.current(conn.settings) == .touch, !conn.hostSupportsTouch else { return }
        flash(\.touchFallback, true)
        #endif
    }

    /// Hold the screen awake or hand it back, as "Keep screen awake" and the player's input
    /// say. Each second while streaming; `beginStreaming` holds it first.
    private func holdScreen(_ conn: PunktfunkConnection) {
        guard phase == .streaming else { return }
        let keepAwake = UserDefaults.standard.object(forKey: DefaultsKey.keepAwake) as? Bool
            ?? SettingDefault.keepAwake
        if conn.holdsScreenAwake(keepAwake: keepAwake) {
            displaySleepGuard.acquire()
        } else {
            displaySleepGuard.release()
        }
    }

    /// Show the exit hint at stream start, unless it is turned off. On macOS 26 with an
    /// adaptive-sync panel, also the variable-refresh stutter notice, unless the player muted it.
    private func noteStreamStart() {
        let defaults = UserDefaults.standard
        if defaults.object(forKey: DefaultsKey.exitHint) as? Bool ?? SettingDefault.exitHint {
            flash(\.exitHint, true)
        } else {
            notices.exitHint.clear()
        }
        #if os(macOS)
        notices.vrrWarning = StreamLayerView.warnsAboutVrr(
            osMajor: ProcessInfo.processInfo.operatingSystemVersion.majorVersion,
            adaptive: StreamLayerView.isAdaptiveSync(NSScreen.main),
            muted: defaults.object(forKey: DefaultsKey.vrrWarningMuted) as? Bool
                ?? SettingDefault.vrrWarningMuted)
        #endif
    }

    /// Take the variable-refresh notice down; `forever` keeps it down across sessions.
    func dismissVrrWarning(forever: Bool) {
        notices.vrrWarning = false
        if forever { UserDefaults.standard.set(true, forKey: DefaultsKey.vrrWarningMuted) }
    }

    /// How to leave, in one line, for the input in hand: the pad's End stream shortcut when a
    /// controller is connected, else this device's own way out. Touch passthrough reads no
    /// twist, so it names the edge pull (`EdgeDial`) instead.
    var exitHintText: String {
        let shortcuts = settings.resolvedShortcuts
        func line(_ device: Shortcuts.Device) -> String? {
            guard let t = shortcuts.trigger(for: .slot(.endStream), device: device) else { return nil }
            let keys = t.chord(client: shortcutClient)
            return t.hold ? L10n.noticeExitHold(keys: keys) : L10n.noticeExitKeys(keys: keys)
        }
        if !GCController.controllers().isEmpty, let pad = line(.pad) { return pad }
        #if os(tvOS)
        return L10n.noticeExitHold(keys: "Back")
        #elseif os(macOS)
        return line(.keyboard) ?? line(.pad) ?? ""
        #else
        if GCKeyboard.coalesced != nil, let keys = line(.keyboard) { return keys }
        return String(localized: touchPassthrough ? L10n.noticeRingEdgePull : L10n.noticeRingTwist)
        #endif
    }

    /// Fingers reach the host as wire touches, so no twist opens the ring.
    var touchPassthrough: Bool {
        guard let conn = connection else { return false }
        return TouchInputMode.current(conn.settings) == .touch && conn.hostSupportsTouch
    }

    /// The SC2 passthrough's claim/release edges (`Sc2Capture.onPhaseChange`, delivered on
    /// main). A claim shows the badge briefly; a release drops it at once; a refused open says
    /// where the grant lives.
    private func noteSc2Phase(_ phase: Sc2Capture.Phase) {
        switch phase {
        case .captured: flash(\.sc2Captured, true)
        case .released: notices.sc2Captured.clear()
        case .refused: flash(\.sc2Refused, true, for: SessionNotices.launchSeconds)
        }
    }

    /// Push the effective mute (`SessionMic.muted`) onto the audio engine, whichever of its two
    /// reasons changed.
    private func applyMicMute() {
        audio?.setMicMuted(SessionMic.muted(user: micMuted, backgrounded: isBackgrounded))
    }

    // MARK: - Per-client access (chip state + expiry warnings)

    /// Refresh the published access state from the connection's LIVE grants + countdown —
    /// called by the 1 Hz stats tick, which is also what makes a mid-session `AccessUpdate`
    /// (a console edit) reach the chip and the capture gates within a second. Every write is
    /// guarded: `notices` publishes on any assignment, and a full-and-permanent session (every
    /// old host) must publish nothing. The capture release and the T−5 m / T−1 m toasts (§7)
    /// are `AccessWarning`'s; engaging a capture is gated at the stream views.
    private func updateAccessState() {
        guard let conn = connection else { return }
        let grants = conn.accessGrants
        let level = PunktfunkConnection.AccessLevel(grants: grants)
        let remaining = conn.accessExpiresInSeconds
        if notices.accessLevel != level { notices.accessLevel = level }
        if notices.accessRemainingSecs != remaining { notices.accessRemainingSecs = remaining }
        let limited = level != .fullControl || remaining != 0
        if notices.accessLimited != limited { notices.accessLimited = limited }
        if AccessWarning.releasesCapture(captured: mouseCaptured, grants: grants) {
            NotificationCenter.default.post(name: .punktfunkReleaseCapture, object: nil)
        }
        let (warned, toast) = AccessWarning.step(
            remaining: remaining,
            warned: AccessWarning(fiveMinutes: notices.accessWarned5m, oneMinute: notices.accessWarned1m))
        if notices.accessWarned5m != warned.fiveMinutes { notices.accessWarned5m = warned.fiveMinutes }
        if notices.accessWarned1m != warned.oneMinute { notices.accessWarned1m = warned.oneMinute }
        switch toast {
        case .endsIn(let secs)?:
            flash(\.accessWarning, L10n.grantEndsIn(time: Self.accessCountdown(secs)))
        case .endsSoon?: flash(\.accessWarning, String(localized: L10n.grantEndsSoon))
        case nil: break
        }
    }

    /// The host's launch line, once per verdict, on its own clock so it never hides the access
    /// warning.
    private func showLaunchNotice(_ text: String) {
        notices.launchShown = text
        flash(\.launch, text, for: SessionNotices.launchSeconds)
    }

    /// "1 h 58 m" / "12 m" — the countdown wording the chip and the warnings share, for a
    /// minute or more.
    static func accessCountdown(_ secs: UInt32) -> String {
        let s = Int(secs)
        if s >= 3600 {
            return L10n.grantRemainingHoursMinutes(
                hours: String(s / 3600), minutes: String((s % 3600) / 60))
        }
        return L10n.grantRemainingMinutes(minutes: String(s / 60))
    }

    /// The chip's tail: "ends in 12 m", or "ends in under 1 m" in the last minute.
    static func accessChipEnd(_ secs: UInt32) -> String {
        secs < 60
            ? String(localized: L10n.grantChipEndsSoon)
            : L10n.grantChipEnds(time: accessCountdown(secs))
    }

    /// Move this session's overlay tier: a Settings change to the stored tier, or `cycleStats`.
    /// The stored tier stays what the next session starts at.
    func setStatsVerbosity(_ tier: StatsVerbosity) {
        guard statsVerbosity != tier else { return }
        statsVerbosity = tier
        settings.statsVerbosity = tier.rawValue
        renderHud()
    }

    /// Advance this session's overlay one tier (⌃⌥⇧S, the three-finger tap, the Stream menu).
    func cycleStats() { setStatsVerbosity(statsVerbosity.next()) }

    /// The ring changes this session only; saved defaults and presets remain unchanged.
    func setInvertScroll(_ invert: Bool) {
        guard settings.invertScroll != invert, let conn = connection,
              conn.canSendPointer, conn.setInvertScroll(invert) else { return }
        settings.invertScroll = invert
    }

    /// Take the physical controllers for this session (its window came to the front).
    func claimControllers() { gamepadCapture?.claim() }

    /// The user confirmed the fingerprint: returns it for pinning and enters streaming.
    func confirmTrust() -> Data? {
        guard case .awaitingTrust(let fingerprint) = phase else { return nil }
        beginStreaming()
        return fingerprint
    }

    func rejectTrust() {
        disconnect()
    }

    /// Tear the session down: stop what it started, then reset `ui` whole. `deliberate` (the
    /// default) means a user-initiated quit — signal `disconnectQuit()` so the host skips the
    /// keep-alive linger; `sessionEnded()` (a host-ended / dropped session) passes `false` to
    /// leave the linger intact.
    func disconnect(deliberate: Bool = true) {
        if deliberate { redial = nil }
        if connection != nil {
            let line = "disconnect \(activeHost?.displayName ?? "-") deliberate=\(deliberate) phase=\(phase)"
            sessionLog.info("\(line, privacy: .public)")
        }
        // What this host has up is about to change, so the cards must ask again rather than wait
        // out the cache's TTL naming the game the user just left.
        if let host = activeHost { NowPlayingStore.shared.invalidate(host) }
        let conn = connection
        #if os(macOS)
        let closing = Self.closing
        closing.enter()
        #endif
        // Every part's main half now, while the connection is up; the joins, then the close,
        // off-main. A deliberate quit must reach the host before the close.
        resources.stop {
            if deliberate { conn?.disconnectQuit() }
            conn?.close()
            #if os(macOS)
            closing.leave()
            #endif
        }
        // A user-ended stream that STARTED on a shelf goes back to it — the same rule a game
        // exit follows, because "I'm done with this game" arrives both ways. Gated on having
        // actually streamed, so a refused or cancelled dial still ends where it was raised
        // from. `sessionEnded` sets it for the paths it owns, and disconnects non-deliberately.
        if deliberate, phase == .streaming, let shelf = launchedShelf {
            returnToLibrary = shelf
        }
        // After the captures: the SC2 capture's `.released` edge never lands once it is
        // deallocated, so the reset drops its badge, and no pending clear fires into the next.
        notices.cancelTimers()
        ui = SessionUI()
        launchWatch?.cancel()
        launchWatch = nil
        connection = nil
        activeHost = nil
        // Read by `sessionEnded` BEFORE it calls us, so clearing here can't rob it of the answer.
        launchedTitleID = nil
        launchedShelf = nil
        // Drop the previous session's grant too — the shared box outlives the session, and a new
        // link may never come up (a non-deadline rung has none at all).
        PresentLinkInfo.shared.clear()
        resizeIndicator = ResizeIndicator() // no stale target/timer into the next session
        picture = nil
        pictureText = ""
    }

    /// Called (via the main actor) when the pump hits end-of-session: reads why off the ended
    /// connection, tears the session down, then takes the `SessionEndRoute` those facts name.
    func sessionEnded() {
        guard let conn = connection else { return }
        let name = activeHost?.displayName ?? "host"
        // WHY it ended, asked while the connection is still up — `disconnect` tears it down.
        let reason = conn.sessionEndReason
        let rejection = conn.endRejection
        let endLine = "session ended by \(name) reason=\(reason) "
            + "rejection=\(rejection.map { String(describing: $0) } ?? "-")"
        sessionLog.info("\(endLine, privacy: .public)")
        // The shelf, not the launch id, says this came from the library: a Resume launches nothing.
        let shelf = activeHost == nil ? nil : launchedShelf
        let hold = launchHold
        let route = SessionEndRoute.decide(
            phase: phase, reason: reason, rejection: rejection, said: conn.endRejectionMessage,
            wakeDelay: conn.wakeRedialDelay(
                autoWake: lastDial?.autoWake ?? false, sinceFirst: sinceFirstWake),
            dialed: lastDial?.host, live: conn.hostFingerprint, shelf: shelf != nil)
        disconnect(deliberate: false) // host/network ended it — keep the linger for a reconnect
        switch route {
        case .wakeRedial(let wait):
            // `disconnect` dropped the hold; a wake redial keeps it up to the game.
            launchHold = hold
            if let plan = lastDial { redialWhileWaking(plan, after: wait) }
        case .lossRedial(let pin):
            guard var plan = lastDial else { return }
            plan.host.pinnedSHA256 = pin
            redialAfterLoss(plan)
        case .message(let message):
            errorMessage = message.text(host: name)
        case .library:
            returnToLibrary = shelf
        case .none:
            break
        }
    }

    /// Dial a lost session again on the schedule every client shares: the dial that started it,
    /// to the host's record as it is now, pinned as the session proved. The host resumes the
    /// session it still holds. A redial that can't reach the host is the next attempt, and a
    /// spent schedule says the connection is lost.
    private func redialAfterLoss(_ plan: Redial) {
        guard let wait = PunktfunkConnection.SessionEndReason.lost.redialDelay(attempt: plan.attempt)
        else {
            redial = nil
            errorMessage = L10n.appleEndedLost(host: plan.host.displayName)
            return
        }
        var next = plan
        next.attempt += 1
        redial = next
        sessionLog.info("connection lost — redial \(next.attempt) after \(wait, privacy: .public)")
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: wait)
            guard let self, self.redial?.attempt == next.attempt, self.phase == .idle else { return }
            self.connect(
                to: next.host.redialed(
                    current: self.currentRecord?(next.host.id), pin: next.host.pinnedSHA256),
                effective: next.effective, gamepad: next.gamepad,
                overrides: next.overrides, launchID: next.launchID, profileID: next.profileID,
                shelf: next.shelf,
                onUnreachable: { [weak self] in self?.redialAfterLoss(next) })
        }
    }

    /// Time since the host this dial is for first refused as a host still waking.
    private var sinceFirstWake: Duration? { lastDial?.wakeSince.map { ContinuousClock.Instant.now - $0 } }

    /// Dial `plan` again after `wait`, to the host's record as it is now: the core's answer for
    /// a woken host that answered before it could stream. The connect overlay stays up for the
    /// redial, and Cancel ends it. The launch hold still up rides into the redial; one the player
    /// dismissed stays down.
    private func redialWhileWaking(_ plan: Redial, after wait: Duration) {
        var next = plan
        next.wakeSince = next.wakeSince ?? .now
        redial = next
        let seq = connectSeq
        sessionLog.info("host not ready to stream — dialing again after \(wait, privacy: .public)")
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: wait)
            guard let self, self.connectSeq == seq, self.redial != nil, self.phase == .idle
            else { return }
            self.connect(
                to: next.host.redialed(current: self.currentRecord?(next.host.id)),
                effective: next.effective, gamepad: next.gamepad,
                overrides: next.overrides, launchID: next.launchID, profileID: next.profileID,
                shelf: next.shelf,
                allowTofu: next.allowTofu, autoWake: next.autoWake, wakeSince: next.wakeSince,
                hold: self.launchHold)
        }
    }

    /// Resize overlay START (main actor — from the Match-window follower's `onResizeTarget`): the
    /// window began differing from the live mode, so a `Reconfigure` toward `(width, height)`,
    /// shown at `view`, is imminent. Show the blur+spinner immediately, before the debounced
    /// request even leaves.
    func resizeTargeted(width: UInt32, height: UInt32, view: (width: UInt32, height: UInt32)?) {
        resizeIndicator.steering(
            width: width, height: height, view: view, now: ProcessInfo.processInfo.systemUptime)
        resizing = resizeIndicator.active
    }

    /// Resize overlay END (main actor — hopped from the decode pump's `onDecodedSize`): a new-mode
    /// IDR decoded at `(width, height)`. Notes the picture's size for the stats line, and clears
    /// the overlay only when that matches the mode or view we're steering to (a same-size
    /// loss-recovery IDR, or the initial connect IDR, is a no-op).
    func resizeDecoded(width: Int, height: Int) {
        let size = (width: UInt32(max(width, 0)), height: UInt32(max(height, 0)))
        picture = size
        let text = "\(size.width)×\(size.height)"
        if text != pictureText { pictureText = text } // an unchanged size must not re-render
        resizeIndicator.decoded(width: size.width, height: size.height)
        resizing = resizeIndicator.active
    }

    /// The game this device launched that this stream plays (`RunningGame.streamedHere`): the
    /// ring's End game. Read from `/status` when the stream starts and when the ring opens.
    private(set) var streamedGame: RunningGame? { get { ui.streamedGame } set { ui.streamedGame = newValue } }

    /// Ask the host what this stream plays. Best-effort: a failed read keeps the last answer, and
    /// an answer lands only in the session it was asked for.
    func refreshStreamedGame() {
        guard let host = activeHost, !DemoMode.isDemo(host),
              case .success(let target) = MgmtTarget.make(host: host, port: connection?.hostMgmtPort)
        else { return }
        let seq = connectSeq
        Task { [weak self] in
            let games = await LibraryClient.running(target)
            guard let self, self.connectSeq == seq, self.activeHost?.id == host.id,
                  self.phase == .streaming else { return }
            self.streamedGame = games.first(where: \.streamedHere)
        }
    }

    /// End the streamed game on the host, then the stream. A refusal keeps the stream and says why.
    /// The answer acts only on the session it was asked from, never a newer one to the same host.
    func endStreamedGame() {
        guard let game = streamedGame, let appID = game.appID, let host = activeHost,
              case .success(let target) = MgmtTarget.make(host: host, port: connection?.hostMgmtPort)
        else { return }
        let seq = connectSeq
        Task { [weak self] in
            let outcome = await LibraryClient.endGame(appID: appID, on: target)
            guard let self, self.connectSeq == seq, self.activeHost?.id == host.id else { return }
            if outcome.gameGone {
                self.disconnect()
            } else {
                self.showLaunchNotice(outcome.notice(title: game.title))
            }
        }
    }

    /// Drop the launch hold and let the stream through.
    func revealStream() {
        launchWatch?.cancel()
        launchWatch = nil
        launchHold = nil
        launchWindowWait = false
        launchDownload = nil
    }

    /// Poll the host once a second for the launched title's state and take `LaunchHoldRule`'s
    /// step. Same lane and identity as the shelf's Resume badge.
    private func watchLaunch() {
        guard let hold = launchHold?.entry, let host = activeHost else { return }
        // The demo host has no `/status` and its title is up at once; hold as a real start would,
        // so the cover's flight and the title card play before the stream shows.
        if DemoMode.isDemo(host) {
            launchWatch?.cancel()
            launchWatch = Task { [weak self] in
                try? await Task.sleep(nanoseconds: UInt64(demoLaunchHold * Double(NSEC_PER_SEC)))
                if !Task.isCancelled { self?.revealStream() }
            }
            return
        }
        guard case .success(let target) = MgmtTarget.make(host: host, port: connection?.hostMgmtPort)
        else {
            revealStream()
            return
        }
        launchWatch?.cancel()
        launchWatch = Task { [weak self] in
            var began = Date()
            while !Task.isCancelled {
                let answer = await LibraryClient.statusIfAnswered(target)
                // A cancelled read comes back as a miss.
                guard !Task.isCancelled else { return }
                let title = answer.map {
                    LaunchHoldRule.Title(of: hold, games: $0.games, downloads: $0.downloads)
                }
                let step = LaunchHoldRule.step(
                    title, elapsed: Date().timeIntervalSince(began), cap: launchHoldMax,
                    noLease: launchNoLease)
                switch step {
                case .keep(let windowWait, let download):
                    if step.restartsClock { began = Date() }
                    if self?.launchDownload != download { self?.launchDownload = download }
                    if self?.launchWindowWait != windowWait { self?.launchWindowWait = windowWait }
                case .unchanged:
                    break
                case .reveal:
                    // A stopped download reveals too: the host's notice says why.
                    self?.revealStream()
                    return
                }
                try? await Task.sleep(nanoseconds: NSEC_PER_SEC)
            }
        }
    }

    /// Start what a trusted session runs: audio and the pads wait for the trust decision, so no
    /// host sound, mic uplink or forwarded pad comes before it.
    ///
    /// `resources` stops the parts newest first, so they start here in the reverse of the
    /// teardown order. On main, while the connection is up: `GamepadCapture` before
    /// `Sc2Capture`, whose claim release would otherwise re-forward the SC2's twin onto a live
    /// capture; then the remote pointer. Off-main: audio, the feedback drain, then the
    /// clipboard, which still needs the connection, then the close.
    private func beginStreaming() {
        guard let conn = connection else { return }
        // Input capture itself is owned by StreamView (engaged by the captureEnabled
        // flip this phase change causes, released/re-engaged by the user from there).
        phase = .streaming
        watchLaunch()
        refreshStreamedGame()
        displaySleepGuard.acquire()
        resources.add(displaySleepGuard, stop: displaySleepGuard.release)
        noteStreamStart()
        #if !os(tvOS)
        // Opt-in per host, advertised by the host, and granted to this device (per-client access
        // §5): the host would refuse an ungranted ask anyway, and not asking keeps the UI honest.
        if activeHost?.clipboardSync == true, conn.hostSupportsClipboard, conn.canUseClipboard {
            startClipboardSync(conn)
        }
        #endif
        // The host's rumble, lightbar, player LEDs and triggers, back onto the pad each names.
        let feedback = GamepadFeedback(connection: conn, manager: .shared)
        feedback.start()
        resources.add(feedback, join: { feedback.stop() })
        // Devices and the mic switch come from the session's resolved settings ("" = system
        // default), so a preset that turns the mic on applies to the uplink too.
        let audio = SessionAudio(connection: conn)
        audio.start(
            speakerUID: settings.speakerUID,
            micUID: settings.micUID,
            micChannel: settings.micChannel,
            // Deny-at-setup for an ungranted mic (per-client access §5): no MIC bit, no
            // uplink at all — a capture the host would only drop is pure privacy downside.
            micEnabled: settings.micEnabled && conn.canUseMic,
            echoCancel: settings.echoCancel,
            spatialAudio: settings.spatialAudio,
            // The A/V sync reference: the presenter's own capture→on-glass meter. The stage-1
            // fallback stamps nothing, so the loop then declines to correct.
            videoLatency: endToEnd)
        resources.add(audio, join: { audio.stop() })
        #if os(tvOS)
        let pointer = SiriRemotePointer(connection: conn)
        pointer.onDisconnectRequest = { [weak self] in self?.disconnect() }
        // The remote's short Back is the ring's opener on tvOS — the same hook the pad chord
        // uses, so the view wires one closure for both.
        pointer.onShortBack = { [weak self] in self?.onRingChord?(nil) }
        pointer.onRingNav = { [weak self] nav in self?.onRingNav?(nav) }
        pointer.start()
        resources.add(pointer, stop: pointer.stop)
        #endif
        // Steam Controller 2 as-is passthrough (opt-in, gated like Android and on the controller
        // grant): its raw reports go to the host, and Steam's writes come back through the
        // feedback's hidRaw sink. The wire slot is claimed on the first state report.
        if settings.sc2Capture, settings.gamepadForwarding, conn.canSendGamepad {
            Sc2Capture.holdTwins(manager: .shared) // before GamepadCapture forwards a twin
            let sc2 = Sc2Capture(connection: conn, manager: .shared)
            // A captured SC2's raw feed bypasses GC, so it brings its own escape chord.
            sc2.onDisconnectRequest = { [weak self] in self?.disconnect() }
            // The shortcuts and the ring's pad path, the same hooks GamepadCapture gets below.
            sc2.onShortcut = { [weak self] action, phase in self?.onShortcut?(action, phase, nil) }
            sc2.onRingNav = { [weak self] nav in self?.onRingNav?(nav) }
            // Claim/release → the bottom-stack badge (the capture's only UI surface).
            sc2.onPhaseChange = { [weak self] phase in self?.noteSc2Phase(phase) }
            feedback.setHidRawSink(sc2.onHidRaw)
            sc2.start()
            // The sink goes first: no raw replay onto a dying capture.
            resources.add(sc2, stop: { feedback.setHidRawSink(nil); sc2.stop() })
        }
        // Every controller GamepadManager selected, each on its own wire pad. With forwarding
        // off the host gets the pads elsewhere: capture still runs the shortcuts.
        let capture = GamepadCapture(
            connection: conn, manager: .shared, forwarding: settings.gamepadForwarding,
            systemForward: settings.systemButtonsForward)
        // End stream (by default L1+R1+Start+Select held 1 s) — on tvOS the only controller way
        // out of a stream (B/Menu is swallowed during sessions; see ContentView).
        capture.onDisconnectRequest = { [weak self] in self?.disconnect() }
        // A pad with a gyro that this session cannot carry — say so once, briefly, and name the
        // setting that fixes it. Already main-actor (GamepadCapture fires it there).
        capture.onMotionUnreachable = { [weak self] kind in self?.flash(\.motionUnreachable, kind) }
        capture.onShortcut = { [weak self] action, phase, pad in self?.onShortcut?(action, phase, pad) }
        capture.onRingNav = { [weak self] nav in self?.onRingNav?(nav) }
        capture.start()
        capture.onOwnershipChange = { [weak feedback] owned in feedback?.setSilenced(!owned) }
        resources.add(capture, stop: capture.stop)
    }

    #if !os(tvOS)
    /// Create + start the session's clipboard bridge and route its host acks into the published
    /// UI state. `ClipboardSync.start()` sends the enable; the host's `.state` answer flips
    /// `clipboardEnabled`, or leaves it false. Its stop disables sync on the wire and, on iOS,
    /// pulls an unpasted host offer down to bytes, so it joins before the connection closes.
    private func startClipboardSync(_ conn: PunktfunkConnection) {
        let sync = ClipboardSync(connection: conn)
        sync.onState = { [weak self] enabled, _, _ in
            Task { @MainActor in self?.clipboardEnabled = enabled }
        }
        sync.start()
        resources.add(sync, join: { sync.stop() })
    }
    #endif

    /// Flip clipboard sync mid-session (the Stream menu). Off → on requires the host cap; on →
    /// off tears the bridge down (off-main — the drain join must not block the main actor) and
    /// tells the host, which drops any selection we own there. No-op on tvOS or while idle.
    func toggleClipboardSync() {
        #if !os(tvOS)
        guard let conn = connection, phase == .streaming else { return }
        if let sync = clipboardSync {
            resources.remove(sync)
            clipboardEnabled = false
            Task.detached { sync.stop() }
        } else if conn.hostSupportsClipboard, conn.canUseClipboard {
            startClipboardSync(conn)
        }
        #endif
    }

    private func startStatsTimer() {
        // The meters outlive the session: the pump can deliver a frame after disconnect, so a new
        // session's first window would otherwise carry the dead session's samples.
        endToEnd.reset()
        clientQueue.reset()
        queueP50Ms = nil
        let timer = Timer(timeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            Task { @MainActor in
                // Resize-overlay safety net: clear a stuck overlay when a targeted size never
                // decodes (a rejected/capped switch). The decoded-frame END clears it promptly on
                // success; this only fires after the timeout.
                self.resizeIndicator.tick(now: ProcessInfo.processInfo.systemUptime)
                // Published only on change: @Published fires on every assignment, and each one
                // re-renders the window over the stream.
                if self.resizing != self.resizeIndicator.active {
                    self.resizing = self.resizeIndicator.active
                }
                // Access chip + expiry warnings: the same tick that drives every other live
                // readout also walks the countdown and picks up mid-session grant edits.
                self.updateAccessState()
                guard let conn = self.connection else { return }
                self.holdScreen(conn)
                if let notice = conn.launchNotice, notice != self.notices.launchShown {
                    self.showLaunchNotice(notice)
                }
                let (frames, _) = self.meter.drain()
                // Host timings (0xCF) feed the core, which matches each to its frame. Bounded: a
                // 240 fps window is ~240 reports; a throw (closed) just ends the drain.
                var burst = 0
                while burst < 1024, (try? conn.nextHostTiming(timeoutMs: 0)) != nil { burst += 1 }
                conn.hudDrain()
                self.queueP50Ms = self.clientQueue.drain()?.p50Ms
                self.renderHud()
                // The window in the unified log, once a second while frames flow: the Advanced
                // Detailed text whatever the overlay shows (and stdout, see `statsToStdout`).
                if frames > 0 {
                    let text = self.overlayLines(conn, tier: .detailed, advanced: true)
                        .map(\.text).joined(separator: " | ")
                    statsLog.info("\(text, privacy: .public)")
                    if statsToStdout { print("pf.stats \(text)") }
                }
            }
        }
        // .common so the HUD keeps updating during window drags / menu tracking.
        RunLoop.main.add(timer, forMode: .common)
        resources.add(timer, stop: timer.invalidate)
    }

    /// Re-format the overlay from the last drained window: each second, and at once on a tier
    /// change so a cycle never waits for the next tick.
    func renderHud() {
        guard let conn = connection else {
            if !hudLines.isEmpty { hudLines = [] }
            return
        }
        let advanced = UserDefaults.standard.bool(forKey: DefaultsKey.advancedStats)
        let lines = overlayLines(conn, tier: statsVerbosity, advanced: advanced)
        if lines != hudLines { hudLines = lines } // an unchanged overlay must not re-render
    }

    /// The core's lines for `tier`, naming the decoded size beside a larger host mode.
    private func overlayLines(
        _ conn: PunktfunkConnection, tier: StatsVerbosity, advanced: Bool
    ) -> [PunktfunkConnection.HudLine] {
        PunktfunkConnection.HudLine.naming(
            picture: picture, mode: conn.currentMode(),
            in: conn.hudLines(tier: tier, advanced: advanced, facts: hudFacts()))
    }

    /// What only the app knows: the floor policy, this app's audio ring, the preset and whether
    /// the host set the game's mode or pad, and the Apple-only lines. Display and end-to-end
    /// charge what the client controls. The deadline link's pick → glass lead is pipeline depth
    /// no client paces under, so it comes off both and Detailed names it, as on Android. macOS
    /// presents straight to the display and feeds no floor, so nothing is shaved there.
    private func hudFacts() -> PunktfunkConnection.HudFacts {
        var f = PunktfunkConnection.HudFacts()
        #if !os(macOS)
        f.shaveOsFloor = true
        #endif
        if let a = audio?.stats {
            f.audioBufferMs = UInt32(clamping: a.bufferMS)
            f.avOffsetMs = Int32(clamping: a.avOffsetMS)
        }
        let byHost = hostOverrides ? String(localized: L10n.overlayHostOverride) : nil
        let preset = [settings.presetName, byHost].compactMap { $0 }
        f.preset = preset.isEmpty ? nil : preset.joined(separator: " \u{b7} ")
        // The deadline link's ask beside its readback: a readback that differs is the one clamp
        // signal the API gives, and on tvOS the screen is the only place to read it.
        if let l = PresentLinkInfo.shared.snapshot() {
            f.extras.append(.init(role: .muted, text: String(
                format: "link latency ask %.2f readback %.2f · range %.0f-%.0f Hz · drawables %lld",
                l.ask, l.latency, l.rangeMin, l.rangeMax, l.drawables)))
        }
        // Receipt → pull: ~0 when healthy; a value that persists is a standing receive backlog.
        if let q = queueP50Ms, q >= 2 {
            f.extras.append(.init(role: .muted, text: String(
                format: "client queue +%.1f (receive backlog — standing if it persists)", q)))
        }
        return f
    }
}
