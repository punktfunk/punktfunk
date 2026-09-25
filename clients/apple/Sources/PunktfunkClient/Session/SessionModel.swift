// Session state for the app shell: owns the connection, the input capture, the trust
// handshake phase, and the pump-thread → main-actor stats relay.

// AVFoundation: AVCaptureDevice.authorizationStatus (the mic TCC grant behind `micAvailable`)
// and, on tvOS, AVPlayer.eligibleForHDRPlayback (the TV-capability HDR gate).
import AVFoundation
import Foundation
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

    static func take(_ id: String, seq: Int) -> LaunchHoldTarget? {
        defer { last = nil }
        guard let last, last.entry.id == id else { return nil }
        return LaunchHoldTarget(entry: last.entry, sourceRect: last.rect, seq: seq)
    }
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
    enum Phase: Equatable {
        case idle
        case connecting
        /// Connected to an unpinned host: the stream is live (and pumping — the opening
        /// IDR must not be missed) but input/cursor capture wait for the user to confirm
        /// the observed fingerprint.
        case awaitingTrust(fingerprint: Data)
        case streaming
    }

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var connection: PunktfunkConnection?
    /// The launched title whose game is not up yet: its cover flies out of the shelf tile at the
    /// tap and holds the screen — through the dial, and then over the stream — until the host's
    /// `/status` says the game left `launching` (`punktfunk-host::gamelease`), or the player asks
    /// to see. nil for a desktop connect, a launcher tile, and once revealed.
    ///
    /// Raised at `connect`, not at first frame: the shelf is still on screen at the tap, which is
    /// the only moment the cover has somewhere to fly FROM, and holding from there means one
    /// unbroken screen from the tap to the game rather than a stream of the launcher in between.
    @Published private(set) var launchHold: LaunchHoldTarget?
    /// The console's own launch hold is up, as the console reports it: the console stays over
    /// the stream it dialled, and the stream takes no input, until the console lets go.
    @Published var consoleHold = false
    /// The launched game is up and the host is waiting for its window.
    @Published private(set) var launchWindowWait = false
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
    /// The stats-overlay tier for this session: the resolved one at connect, then whatever the
    /// live cycle surfaces (⌃⌥⇧S, the three-finger tap) move it to. Separate from the @AppStorage
    /// global so a preset that overrides the tier actually gets it, without the cycle breaking.
    @Published var statsVerbosity: StatsVerbosity = .normal
    @Published var errorMessage: String?
    /// The stats overlay's lines for the live tier and vocabulary: formatted by the core each
    /// second and on every tier change (`renderHud`).
    @Published private(set) var hudLines: [PunktfunkConnection.HudLine] = []
    /// Mirrors StreamView's capture state (it owns the input capture; this drives the
    /// HUD's "click to capture" / "⌘⎋ releases" hint).
    @Published var mouseCaptured = false
    /// The USER's in-stream mic mute (the HUD button, the Stream menu's ⌃⌥⇧A, the captured-state
    /// chord, the iOS mic disc) — session state, deliberately NOT persisted: a mute is for the
    /// people in the room right now, so every new session starts live if the mic is on at all.
    /// One of the two inputs to the effective mute; `isBackgrounded` is the other, and
    /// `applyMicMute` composes them — a user mute survives a trip through the background, and the
    /// background's privacy mute never clears the user's choice. Local and instant: it gates
    /// capture on this device, nothing is sent to the host.
    @Published private(set) var micMuted = false
    /// The kind a controller declared when it turned out this session cannot carry its motion —
    /// set once per such pad, cleared after `motionHintSeconds`. Nil the rest of the time.
    ///
    /// It exists because the failure is otherwise entirely silent: the gyro simply does nothing,
    /// with no way for the player to tell a dead sensor from a session that resolved a backend
    /// without a motion plane. The fix is a settings change, so the hint has to name it.
    @Published private(set) var motionUnreachableKind: PunktfunkConnection.GamepadType?
    /// Drops `motionUnreachableKind` again — held so a second pad's hint replaces the first
    /// cleanly, and so ending the session cancels a pending clear rather than letting it fire
    /// into a torn-down model.
    private var motionHintTimer: Task<Void, Never>?
    /// How long the motion hint stays up — the start-of-stream shortcut banner's 6 s, since the
    /// two share the bottom-centre stack and a player reads them the same way.
    private static let motionHintSeconds: UInt64 = 6
    /// True while the "Steam Controller passing through" badge shows — set on the SC2
    /// capture's claim edge (stream start, or the pad powering on mid-session), auto-dropped
    /// after `motionHintSeconds` like the motion hint it stacks with, and dropped EARLY on a
    /// release edge so the badge can never outlive the passthrough it announces. The badge is
    /// the capture's ONLY UI surface: the raw BLE device never enters GameController, so the
    /// Controllers page cannot list it.
    @Published private(set) var sc2CapturedHint = false
    /// Drops `sc2CapturedHint` — same contract as `motionHintTimer` (restart on a new claim,
    /// cancel on teardown rather than firing into a torn-down model). Only `noteSc2Phase` and
    /// the disconnect teardown touch it.
    private var sc2HintTimer: Task<Void, Never>?
    /// The touch model is passthrough, but this host drops contacts (no `HOST_CAP2_TOUCH`): the
    /// stream view runs the trackpad model instead, and this says so once, for
    /// `motionHintSeconds`, in the same bottom-centre slot. Otherwise the setting is silently
    /// ignored and every finger vanishes.
    @Published private(set) var touchFallbackNotice = false
    private var touchHintTimer: Task<Void, Never>?
    /// Resize overlay (design/midstream-resolution-resize.md — client resize UX): true from the
    /// instant a Match-window resize starts steering toward a new size until a frame at that size
    /// decodes (or a safety timeout). Drives the blur+spinner so the unavoidable host-rebuild delay
    /// reads as a deliberate, acknowledged transition instead of a stutter. Pure state lives in
    /// `ResizeIndicator`; this mirrors its `active` for SwiftUI.
    @Published private(set) var resizing = false
    /// START = follower steering (main actor), END = a new-mode IDR's coded dims (decode pump,
    /// hopped to main), TIMEOUT = safety net for a rejected/capped switch that never yields a
    /// differently-sized frame. Ticked from the 1 Hz stats timer.
    private var resizeIndicator = ResizeIndicator()

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
    private var statsTimer: Timer?
    private var audio: SessionAudio?
    private var gamepadCapture: GamepadCapture?
    /// The in-stream ring's pad path (design/touch-client-overlay.md §2.6), forwarded from the
    /// capture so the view can wire them once per session. The chord carries the pad's wire
    /// index; a remote's Back and a captured Steam Controller carry none.
    var onRingChord: ((UInt32?) -> Void)?
    var onRingNav: ((RingNav) -> Void)?

    /// The ring is up: the pad belongs to it (flushed on the host, edges become navigation).
    func setRingOpen(_ open: Bool) {
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
    func tapPadButton(_ bit: UInt32) {
        gamepadCapture?.tapButton(bit)
    }

    /// The virtual on-screen controller (design/touch-client-overlay.md §4): shown from the
    /// ring's `pad` slot, per session. While up it holds one wire pad, so the host sees one
    /// controller arrive and, on hide, one leave (§9). Never toggled by the ring's own open and
    /// close (§8 trap 4).
    @Published private(set) var virtualPadShown = false
    private(set) var virtualPad: VirtualPadWire?

    /// Whether the pad's input can reach the host at all: the forwarding setting and the
    /// session's controller grant, the two gates a real pad's sends have.
    var virtualPadAvailable: Bool {
        settings.gamepadForwarding && connection?.canSendGamepad == true
    }

    func toggleVirtualPad() {
        if let pad = virtualPad {
            pad.close()
            virtualPad = nil
            virtualPadShown = false
            return
        }
        guard let conn = connection, let pad = VirtualPadWire(connection: conn, manager: .shared) else { return }
        // Toggled from the ring, so the ring is up: the pad starts masked and unmasks on close.
        pad.masked = gamepadCapture?.ringOpen ?? false
        virtualPad = pad
        virtualPadShown = true
    }
    private var gamepadFeedback: GamepadFeedback?
    /// The live session's Steam Controller 2 as-is passthrough (`settings.sc2Capture` &&
    /// `settings.gamepadForwarding`) — built beside GamepadCapture/GamepadFeedback in
    /// `beginStreaming`, torn down in `disconnect` in the Android order (unhook the hidRaw
    /// sink → feedback stops → capture stops, which also frees its wire index).
    private var sc2Capture: Sc2Capture?
    #if !os(tvOS)
    /// The live session's clipboard bridge (design/clipboard-and-file-transfer.md §5) — created
    /// by `beginStreaming` when the per-host toggle is on and the host advertises
    /// `HOST_CAP_CLIPBOARD`; stopped (off-main, drain joined) in `disconnect`.
    private var clipboardSync: ClipboardSync?
    #endif
    /// Whether clipboard sync is live (host-acked `ClipState.enabled`) — drives the Stream menu
    /// item's title and the settings footnote. Always false on tvOS, which has no pasteboard.
    @Published private(set) var clipboardEnabled = false
    /// The host's last `ClipState.reason` (`CLIP_REASON_*`) — why an enable was refused
    /// (backend unavailable / policy disabled / …); 0 = OK.
    @Published private(set) var clipboardReason: UInt8 = 0

    // MARK: - Per-client access (design/per-client-access.md §7)

    /// The session's access preset, derived live from the grants mask (§3.2 — the label is
    /// never stored). `.fullControl` against every old host and for every full-grant device,
    /// so nothing below changes today's look there.
    @Published private(set) var accessLevel: PunktfunkConnection.AccessLevel = .fullControl
    /// Seconds until this session's access expires; `0` = permanent. Ticks down at the 1 Hz
    /// stats cadence — the chip's countdown renders straight from it.
    @Published private(set) var accessRemainingSecs: UInt32 = 0
    /// Anything about this session's access differs from full-and-permanent — the visibility
    /// gate for the chip (and the tvOS stats-overlay line). False = today's look, untouched.
    @Published private(set) var accessLimited = false
    /// The transient expiry-warning toast ("Access ends in 5 m") — non-nil for a few seconds
    /// around the T−5 m / T−1 m marks the host also warns at via `AccessUpdate`.
    @Published private(set) var accessWarning: String?
    /// One-shot latches for the two warning marks (reset per session).
    private var accessWarned5m = false
    private var accessWarned1m = false
    /// Auto-dismiss for `accessWarning` — held so a newer warning replaces a pending clear.
    private var accessWarningTimer: Task<Void, Never>?
    /// The host's line for a launch that did not give the player their game, up for
    /// `launchNoticeSeconds`. `launchNoticeShown` keeps one verdict from re-raising it.
    @Published private(set) var launchNotice: String?
    private var launchNoticeShown: String?
    private var launchNoticeTimer: Task<Void, Never>?
    /// Long enough to read a sentence with its cause.
    private static let launchNoticeSeconds: UInt64 = 10
    #if os(tvOS)
    /// Siri Remote → host pointer while streaming (touch surface moves, press = left click,
    /// Play/Pause = right click) + the remote's deliberate exit (hold Back ≥ 1 s). See
    /// SiriRemotePointer — same trust gate/lifecycle as the gamepad capture above.
    private var remotePointer: SiriRemotePointer?
    #endif

    var isBusy: Bool { phase != .idle }

    /// True while a streaming session is running in the background under the opt-in keep-alive
    /// (audio plays, video dropped, timeout armed). Drives the Live Activity's stage/countdown (M3)
    /// and is cleared on foreground or teardown. iOS/iPadOS only in practice.
    @Published private(set) var isBackgrounded = false
    /// When the backgrounded keep-alive will auto-disconnect (nil unless backgrounded) — drives the
    /// Live Activity countdown. Set alongside `backgroundTimer`.
    @Published private(set) var backgroundDeadline: Date?
    /// Bounded auto-disconnect for a backgrounded keep-alive session. Fires on `.main`.
    private var backgroundTimer: DispatchSourceTimer?

    /// Holds off display sleep (and, on macOS, the screen saver) for the life of a session —
    /// nothing about watching a stream looks like user activity to the OS, least of all a
    /// controller-only session. Acquired in `beginStreaming`, released in `disconnect`.
    private let displaySleepGuard = DisplaySleepGuard()

    /// `allowTofu` gates the trust-on-first-use prompt for an unpinned host: it is only true
    /// when the host EXPLICITLY advertised `pair=optional` (rule 3a). For any other unpinned host
    /// — `pair=required`, a manually-typed host, or a discovered host with no/unknown `pair`
    /// field — TOFU is forbidden (rule 3b): the connect refuses rather than offering trust, and
    /// the user is routed to PIN pairing by the caller. (A pinned host connects regardless: its
    /// stored fingerprint is the trust decision.)
    ///
    /// `requestAccess` is the no-PIN delegated-approval path: open an identified connect the host
    /// PARKS until the operator clicks Approve in its console, then admits the SAME connection (no
    /// reconnect). The handshake budget is widened to exceed the host's park window, and a
    /// successful connect streams directly (the approval IS the trust decision) — the caller pins
    /// the observed fingerprint as paired. `host.pinnedSHA256`, when set, pins the advertised cert
    /// for the wait; nil = trust-on-first-use.
    /// `onUnreachable`, when set, replaces the "could not connect" alert for a plain connect
    /// failure: the caller takes over recovery (the Wake-on-LAN wait for a host that stopped
    /// advertising). It never fires for the delegated-approval path, whose failure text carries
    /// its own instructions.
    /// `effective` is the whole stream mode + input/audio configuration for this session, already
    /// resolved from the globals and the session's preset by the caller — the ONE place that
    /// resolution happens (§4.4). The connection carries it, so the kit-side readers (the
    /// presenter, the input paths, the match-window follower) see the values this connect asked
    /// the host for, instead of re-reading the globals mid-session.
    func connect(to host: StoredHost, effective: EffectiveSettings,
                 gamepad: PunktfunkConnection.GamepadType = .auto,
                 launchID: String? = nil,
                 /// The library shelf this session started from, so its end can return there —
                 /// the title's shelf for a launch, and the shelf itself for a Resume, which
                 /// launches nothing. nil for a connect that did not come off one.
                 shelf: LibraryTarget? = nil,
                 allowTofu: Bool = false,
                 autoTrust: Bool = false,
                 requestAccess: Bool = false,
                 onUnreachable: (@MainActor () -> Void)? = nil) {
        guard phase == .idle else { return }
        guard !Self.activeHosts.contains(where: { $0.key != ObjectIdentifier(self) && $0.value == host.id })
        else {
            errorMessage = "\(host.displayName) is already streaming in another window."
            return
        }
        connectSeq += 1
        let attempt = connectSeq
        phase = .connecting
        activeHost = host
        launchedTitleID = launchID
        launchedShelf = shelf
        // The host never tracks a launcher tile, so there is nothing to wait for.
        launchSeq += 1
        launchHold = launchID.flatMap { LaunchedEntry.take($0, seq: launchSeq) }
            .flatMap { $0.entry.isLauncher ? nil : $0 }
        launchWindowWait = false
        errorMessage = nil
        settings = effective
        statsVerbosity = StatsVerbosity(rawValue: effective.statsVerbosity) ?? .normal
        #if os(iOS)
        // An attached monitor shows the picture (StreamViewController), so its size and rate win.
        let mode = ExternalDisplay.streamMode(effective)
            ?? effective.streamMode(native: NativeDisplay.mode)
        #else
        let mode = effective.streamMode(native: NativeDisplay.mode)
        #endif
        let (width, height, hz) = (mode.width, mode.height, mode.hz)
        let compositor = PunktfunkConnection.Compositor(
            rawValue: UInt32(clamping: effective.compositor)) ?? .auto
        var bitrateKbps = UInt32(clamping: effective.bitrateKbps)
        let audioChannels = UInt8(clamping: effective.audioChannels)
        // The format this session ASKS for, at every channel count: only the host's gate knows the
        // datagram size, so a request it cannot fit comes back declined in `Welcome`. The request
        // is never the answer — SessionAudio and the stats overlay read what the host granted
        // (`resolvedAudioRateHz`, `resolvedAudioBits`, `isLosslessAudio`).
        let audioFormat = effective.audioFormatChoice
        let (audioRateHz, audioBits) = audioFormat.wire
        let hdrEnabled = effective.hdrEnabled
        let preferredCodec = PunktfunkConnection.codecByte(effective.codec)
        // PyroWave is always Automatic bitrate (ABR overhaul RFC §5.2): a fixed kbps is
        // ill-defined for the all-intra codec (bpp is the operating point) and used to bypass
        // the host's operator ceiling — send 0 and let the host pin its per-mode rate. Gated
        // like the advertisement below: a device that failed the Metal probe never offers the
        // codec, falls back to H.26x, and the user's rate must survive there. The stored
        // setting is untouched, so switching codecs back restores it.
        if preferredCodec == PunktfunkConnection.codecPyroWave, MetalWaveletDecoder.supported {
            bitrateKbps = 0
        }
        let pin = host.pinnedSHA256
        // Capability gate (main-actor — screen APIs): only advertise HDR when this display can
        // actually present it, so the host sends a proper SDR stream to an SDR display rather than
        // BT.2020 PQ the panel would mis-tone-map. The display self-tone-maps HDR from the mastering
        // metadata we apply (Step 2) when it IS HDR.
        let displayHDR: Bool = {
            #if os(macOS)
                // POTENTIAL, not current, headroom: `maximumExtendedDynamicRangeColorComponentValue`
                // is the CURRENTLY-ALLOCATED headroom, which macOS hands out on demand — on an idle
                // SDR desktop it reads 1.0 even with HDR enabled and active (external HDR displays
                // like the Samsung G95SC allocate EDR only when content asks). Gating on it means an
                // HDR monitor never gets advertised at connect time. `maximumPotential…` is the
                // mode-independent capability (the macOS analogue of the tvOS/iOS gates below).
                return (NSScreen.main?.maximumPotentialExtendedDynamicRangeColorComponentValue ?? 1.0) > 1.0
            #elseif os(tvOS)
                // NOT the EDR headroom here: on tvOS that reflects the CURRENT output mode, and
                // Apple's recommended setup runs an SDR home screen with Match Content — an
                // HDR-capable TV would read 1.0 at connect time and never be advertised. The
                // session switches the display to HDR10 itself once streaming (AVDisplayManager —
                // see StreamViewIOS), so gate on the TV's mode-independent capability; if the
                // switch never lands, the presenter's in-shader tone-map keeps PQ safe anyway.
                return AVPlayer.eligibleForHDRPlayback
            #else
                return UIScreen.main.potentialEDRHeadroom > 1.0
            #endif
        }()
        let hdrCapable = hdrEnabled && displayHDR
        // 4:4:4 opt-IN (default off): full chroma is a per-client choice — a clear win for
        // desktop/text work, but at a fixed bitrate it spends bits on chroma that game content
        // doesn't visibly need, and the encode/decode pixel rate rises. The host allows it by
        // default (PUNKTFUNK_444, default on), so this toggle is the one real switch; the
        // hardware-decode probe below still gates what can actually be advertised.
        let want444 = effective.enable444
        // 10-bit without HDR: an SDR desktop at Main10, which costs a little bandwidth and takes
        // the banding out of gradients. `hdrCapable` already advertises the depth, so this only
        // adds the arm where HDR is off or the display cannot show it.
        let tenBit = hdrCapable || effective.tenBitSdr
        let connectLine = "connect \(host.displayName) \(host.address):\(host.port) "
            + "mode=\(width)x\(height)@\(hz) codec=\(effective.codec) bitrate=\(bitrateKbps)kbps "
            + "hdr=\(hdrCapable) 10bit=\(tenBit) 444=\(want444) "
            + "audio=\(audioChannels)ch/\(audioRateHz)Hz/\(audioBits)bit "
            + "pinned=\(pin != nil) tofu=\(allowTofu) launch=\(launchID ?? "-")"
        sessionLog.info("\(connectLine, privacy: .public)")
        Task.detached(priority: .userInitiated) {
            // PunktfunkConnection.init blocks on the QUIC handshake — keep it off the main
            // actor. The persistent identity is presented on every connect so a paired
            // host recognizes this Mac (nil = anonymous, fine for hosts without
            // --require-pairing; Keychain/generation failure must not block connecting).
            let identity = (try? ClientIdentityStore.shared.load())?.identity
            // 4:4:4 only when allowed AND decodable in real time. PyroWave's Metal decoder takes
            // it on any device past its probe. HEVC needs HARDWARE 4:4:4, at BOTH depths when
            // 10-bit is advertised (the host may still send 8-bit). `chromaFormat` is the answer.
            let pyroWave =
                preferredCodec == PunktfunkConnection.codecPyroWave && MetalWaveletDecoder.supported
            let canDecode444 =
                pyroWave
                || (tenBit
                    ? (Stage444Probe.hwDecode444_8bit && Stage444Probe.hwDecode444_10bit)
                    : Stage444Probe.hwDecode444_8bit)
            let videoCaps = PunktfunkConnection.videoCaps(
                tenBit: tenBit, hdr: hdrCapable, chroma444: want444 && canDecode444)
            // This client's VideoToolbox path decodes H.264 and HEVC everywhere, and AV1 when
            // this device has an AV1 hardware decoder (M3-class Macs, A17 Pro-class iPhones —
            // VideoToolbox has no software AV1 decoder, so advertising it elsewhere would invite
            // a stream that can't decode; see AV1.swift). The host resolves the emitted codec
            // from these + the soft `preferredCodec`; `resolvedCodec` reflects what it chose.
            var videoCodecs = PunktfunkConnection.codecH264 | PunktfunkConnection.codecHEVC
            if AV1.hardwareDecodeSupported { videoCodecs |= PunktfunkConnection.codecAV1 }
            // PyroWave (wired LAN) is a pure opt-in: picking it in the codec setting both
            // advertises the bit and prefers it — the host never auto-selects it, and the
            // picker only offers it when the Metal decode probe passed (simdgroup floor ≈ A13;
            // every M-series Mac and the ATV 4K gen 3 pass). The decoder self-configures from
            // the per-frame sequence header (4:2:0/4:4:4, SDR/PQ — design/pyrowave-444-hdr.md),
            // so the session keeps the user's HDR/10-bit/4:4:4 caps exactly like HEVC/AV1.
            if pyroWave { videoCodecs |= PunktfunkConnection.codecPyroWave }
            // Cursor channel (remote-desktop-sweep M2, macOS): sessions STARTING in the desktop
            // mouse model advertise local cursor rendering — the host then stops compositing
            // the pointer and forwards shape/state, which StreamView draws as the real
            // NSCursor. Capture-mode sessions keep today's composited pointer.
            #if os(macOS)
            let presentCaps: UInt8 =
                (MouseInputMode(rawValue: effective.mouseMode) ?? .capture) == .desktop ? 0x01 : 0
            #else
            // iOS/tvOS run the stage-4 deadline presenter, whose link thread feeds
            // reportPhase — advertise the vsync-aware presenter (0x02, CLIENT_CAP_PHASE_LOCK).
            // macOS stays without it: the stage-2 arrival presenter has no latch grid.
            let presentCaps: UInt8 = 0x02
            #endif
            // "Keep host audio playing": the host taps its default playback device instead of
            // parking it on a silent endpoint, so the speakers on the host PC stay live. Pure
            // REQUEST — no host-cap echo — so an older host simply goes quiet as it always did.
            let clientCaps =
                presentCaps
                | (effective.keepHostAudio ? PunktfunkConnection.clientCapKeepHostAudio : 0)
            let result = Result { try PunktfunkConnection(
                host: host.address, port: host.port,
                width: width, height: height, refreshHz: hz,
                pinSHA256: pin, identity: identity, compositor: compositor,
                gamepad: gamepad, bitrateKbps: bitrateKbps, videoCaps: videoCaps,
                audioChannels: audioChannels,
                audioRateHz: audioRateHz, audioBits: audioBits,
                videoCodecs: videoCodecs, preferredCodec: preferredCodec,
                clientCaps: clientCaps,
                videoFit: VideoFit(name: effective.videoFit).wire,
                launchID: launchID,
                // Delegated approval: the host holds this connect open until the operator approves
                // it (~180 s) — outwait that window so a slow approval still lands here. Normal
                // connects keep the snappy default.
                timeoutMs: requestAccess ? 185_000 : 10_000,
                settings: effective) }
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
                    let landed = "connected \(host.displayName) "
                        + "mode=\(conn.width)x\(conn.height)@\(conn.refreshHz) "
                        + "codec=\(conn.videoCodec) bitrate=\(conn.resolvedBitrateKbps)kbps "
                        + "depth=\(conn.bitDepth) chroma=\(conn.isChroma444 ? "444" : "420") hdr=\(conn.isHDR) "
                        + "audio=\(conn.resolvedAudioChannels)ch/\(conn.resolvedAudioRateHz)Hz/\(conn.resolvedAudioBits)bit "
                        + "shard=\(conn.shardPayload) compositor=\(conn.resolvedCompositor.rawValue) "
                        + "gamepad=\(conn.resolvedGamepad.rawValue) mgmt=\(conn.hostMgmtPort)"
                    sessionLog.info("\(landed, privacy: .public)")
                    if pin != nil || autoTrust || requestAccess {
                        // requestAccess: the operator approved this device on the host, so the
                        // session is trusted — stream directly (the caller pins it as paired).
                        self.connection = conn
                        self.noteTouchFallback(conn)
                        self.startStatsTimer()
                        self.beginStreaming()
                    } else if allowTofu {
                        // Host advertised pair=optional — offer the reduced-security TOFU prompt
                        // over the live (blurred) stream (rule 3a).
                        self.connection = conn
                        self.noteTouchFallback(conn)
                        self.startStatsTimer()
                        self.phase = .awaitingTrust(fingerprint: conn.hostFingerprint)
                    } else {
                        // Unpinned and TOFU not permitted (rule 3b): never let this silently
                        // become trustable. Drop the connection; the caller routes to pairing.
                        Task.detached { conn.close() } // joins Rust threads — off-main
                        self.phase = .idle
                        self.activeHost = nil
                        self.revealStream() // no stream is coming; the hold must not outlive the dial
                        self.errorMessage = "\(host.displayName) is not paired yet. "
                            + "Pair with its PIN before streaming."
                    }
                case .failure(let error):
                    sessionLog.warning(
                        "connect \(host.displayName, privacy: .public) failed: \(String(describing: error), privacy: .public)")
                    self.phase = .idle
                    self.activeHost = nil
                    // The launch hold is an OPAQUE cover; only `revealStream` drops it, and
                    // otherwise nothing here does. It would sit over the home screen until the
                    // next session, and on tvOS it makes the host grid unfocusable behind it.
                    self.revealStream()
                    if case PunktfunkClientError.rejected(let rejection) = error {
                        // The host answered and stated its reason (declined / approval timed
                        // out / busy / versions differ) — show that, and never wake-retry a
                        // host that is demonstrably awake.
                        self.errorMessage = "\(host.displayName): \(rejection.userMessage)"
                    } else if let onUnreachable, !requestAccess {
                        // The caller owns recovery (wake-and-retry) — no error alert here; its
                        // own overlay explains what's happening.
                        onUnreachable()
                    } else if requestAccess {
                        // The delegated-approval connect ended without being admitted: the
                        // operator didn't approve it before the host's park window elapsed (or
                        // the host was unreachable).
                        self.errorMessage = "\(host.displayName) didn't let this device in. "
                            + "Approve it in the host's web console (port 47992 → Pairing), then "
                            + "request access again — the request expires after a few minutes."
                    } else {
                        self.errorMessage = pin != nil
                            ? "Couldn't reach \(host.displayName) — it may be asleep, or its "
                                + "identity changed since you paired. Pair with it again from "
                                + "its host card."
                            : "Couldn't reach \(host.displayName) — it may be asleep, or not "
                                + "paired yet. Wake it, or pair with it from its host card."
                    }
                }
            }
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
        backgroundTimer?.cancel()
        backgroundTimer = timer
        timer.resume()
    }

    /// Return to foreground: cancel the timeout, resume mic + video, and force a clean re-anchor —
    /// request a fresh IDR (infinite GOP: it won't come on its own) and let the pump's freeze gate
    /// withhold the concealed frames until it lands (it auto-arms on the resumed frame-index gap).
    func exitBackground() {
        guard isBackgrounded else { return }
        isBackgrounded = false
        backgroundDeadline = nil
        backgroundTimer?.cancel()
        backgroundTimer = nil
        applyMicMute() // back to the user's own choice — which may well still be "muted"
        if let conn = connection {
            conn.setVideoDropped(false)
            conn.requestKeyframe()
        }
    }

    // MARK: - Microphone mute (in-stream, per session)

    /// Whether this session has a mic uplink there is any point in muting: the mic must be on in
    /// the session's RESOLVED settings (a preset can turn it on or off), the platform must have
    /// an app-accessible input at all, and the OS must not have refused us one. Drives whether the
    /// mute control is offered — a live-looking mute button over a session that sends no
    /// microphone would be a lie. Same three conditions `SessionAudio` starts an uplink on
    /// (`.notDetermined` counts: the prompt is pending and a grant starts the uplink mid-session).
    var micAvailable: Bool {
        #if os(tvOS)
        return false // no app-accessible microphone — SessionAudio never opens an uplink either
        #else
        // The session's grants must include MIC (per-client access §7 — hide the mic UI when
        // ungranted; a mute button over a mic the host drops would be a lie twice over).
        guard settings.micEnabled, connection?.canUseMic != false else { return false }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized, .notDetermined: return true
        default: return false // denied / restricted — there is no uplink to mute
        }
        #endif
    }

    /// Flip the user's mute. The in-stream surfaces (HUD button, Stream menu, ⌃⌥⇧A while
    /// captured, the iOS mic disc) all land here.
    func toggleMicMute() {
        setMicMuted(!micMuted)
    }

    /// Set the user's mute directly (the badge's tap-to-unmute). Ignored when the session has no
    /// microphone, so a stale surface can't leave a phantom "muted" badge over a session that was
    /// never sending anything.
    func setMicMuted(_ muted: Bool) {
        guard micAvailable, micMuted != muted else { return }
        micMuted = muted
        applyMicMute()
    }

    /// A forwarded controller has a gyro this session cannot carry (see
    /// `GamepadCapture.onMotionUnreachable`). Show it briefly, then let it go.
    ///
    /// Last pad wins, and its timer restarts: two such pads are the same one fact to a player, and
    /// a second hint appearing under a still-visible first would only read as a stutter.
    /// Raise `touchFallbackNotice` when the passthrough touch model meets a host without touch
    /// injection — the same fallback `StreamLayerUIView` applies to the fingers themselves.
    private func noteTouchFallback(_ conn: PunktfunkConnection) {
        #if os(iOS)
        guard TouchInputMode.current(conn.settings) == .touch, !conn.hostSupportsTouch else { return }
        touchFallbackNotice = true
        touchHintTimer?.cancel()
        touchHintTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.motionHintSeconds))
            guard !Task.isCancelled else { return }
            self?.touchFallbackNotice = false
        }
        #endif
    }

    private func noteMotionUnreachable(_ kind: PunktfunkConnection.GamepadType) {
        motionUnreachableKind = kind
        motionHintTimer?.cancel()
        motionHintTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.motionHintSeconds))
            guard !Task.isCancelled else { return }
            self?.motionUnreachableKind = nil
        }
    }

    /// The SC2 passthrough's claim/release edges (`Sc2Capture.onPhaseChange`, delivered on
    /// main). A claim shows the badge briefly — motion-hint style; a release drops it at
    /// once, because a badge still saying "passing through" over a released slot would be
    /// exactly the silent lie the badge exists to prevent.
    private func noteSc2Phase(_ phase: Sc2Capture.Phase) {
        sc2HintTimer?.cancel()
        switch phase {
        case .captured:
            sc2CapturedHint = true
            sc2HintTimer = Task { [weak self] in
                try? await Task.sleep(for: .seconds(Self.motionHintSeconds))
                guard !Task.isCancelled else { return }
                self?.sc2CapturedHint = false
            }
        case .released:
            sc2CapturedHint = false
        }
    }

    /// Push the EFFECTIVE mute — the user's choice OR the background keep-alive's privacy mute —
    /// onto the audio engine. The two reasons are composed here and nowhere else: whichever one
    /// changed, the other still holds, so returning from the background can't un-mute a user who
    /// muted mid-stream, and a user unmuting while backgrounded (Live Activity, another window)
    /// doesn't open the mic behind their back.
    private func applyMicMute() {
        audio?.setMicMuted(micMuted || isBackgrounded)
    }

    // MARK: - Per-client access (chip state + expiry warnings)

    /// Refresh the published access state from the connection's LIVE grants + countdown —
    /// called by the 1 Hz stats tick, which is also what makes a mid-session `AccessUpdate`
    /// (a console edit) reach the chip and the capture gates within a second. The equality
    /// guards keep a full-and-permanent session (every old host) from publishing anything.
    private func updateAccessState() {
        guard let conn = connection else { return }
        let grants = conn.accessGrants
        let level = PunktfunkConnection.AccessLevel(grants: grants)
        let remaining = conn.accessExpiresInSeconds
        if accessLevel != level { accessLevel = level }
        if accessRemainingSecs != remaining { accessRemainingSecs = remaining }
        let limited = level != .fullControl || remaining != 0
        if accessLimited != limited { accessLimited = limited }
        // A mid-session edit that removed BOTH input classes releases an engaged capture:
        // holding a frozen cursor and swallowed keys over input the host now drops is
        // exactly the "keyboard does nothing and nobody says why" failure §7 exists to
        // prevent. (Engage is gated at the stream views; this is the live-revoke half.)
        if mouseCaptured,
           grants & (PunktfunkConnection.grantPointer | PunktfunkConnection.grantKeyboard) == 0 {
            NotificationCenter.default.post(name: .punktfunkReleaseCapture, object: nil)
        }
        // The T−5 m / T−1 m warning toasts (§7). Derived from the countdown CROSSING the
        // marks rather than from the AccessUpdate messages alone: the host's warnings
        // re-anchor the same countdown, so this shows them when they arrive AND still fires
        // on plain clock progress if a warning datagram never lands. One shot each; an edit
        // that extends the deadline back above a mark re-arms it.
        guard remaining != 0 else { return }
        if remaining > 300 {
            accessWarned5m = false
            accessWarned1m = false
        } else if remaining > 60 {
            accessWarned1m = false
            if !accessWarned5m {
                accessWarned5m = true
                showAccessWarning("Access ends in \(Self.accessCountdown(remaining))")
            }
        } else if !accessWarned1m {
            accessWarned1m = true
            accessWarned5m = true
            showAccessWarning("Access ends in under a minute")
        }
    }

    /// Put one warning toast up for a few seconds (the motion hint's pattern: last one wins,
    /// its timer restarts, teardown cancels a pending clear).
    private func showAccessWarning(_ text: String) {
        accessWarning = text
        accessWarningTimer?.cancel()
        accessWarningTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.motionHintSeconds))
            guard !Task.isCancelled else { return }
            self?.accessWarning = nil
        }
    }

    /// Same transient contract as `showAccessWarning`, on its own timer so neither hides the other.
    private func showLaunchNotice(_ text: String) {
        launchNoticeShown = text
        launchNotice = text
        launchNoticeTimer?.cancel()
        launchNoticeTimer = Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.launchNoticeSeconds))
            guard !Task.isCancelled else { return }
            self?.launchNotice = nil
        }
    }

    /// "1 h 58 m" / "12 m" / "45 s" — the countdown wording the chip and the warnings share.
    static func accessCountdown(_ secs: UInt32) -> String {
        let s = Int(secs)
        if s >= 3600 { return "\(s / 3600) h \((s % 3600) / 60) m" }
        if s >= 60 { return "\(s / 60) m" }
        return "\(s) s"
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

    /// Tear the session down. `deliberate` (the default) means a user-initiated quit — signal
    /// `disconnectQuit()` so the host skips the keep-alive linger; `sessionEnded()` (a host-ended /
    /// dropped session) passes `false` to leave the linger intact.
    func disconnect(deliberate: Bool = true) {
        if connection != nil {
            let line = "disconnect \(activeHost?.displayName ?? "-") deliberate=\(deliberate) phase=\(phase)"
            sessionLog.info("\(line, privacy: .public)")
        }
        statsTimer?.invalidate()
        statsTimer = nil
        // What this host has up is about to change, so the cards must ask again rather than wait
        // out the cache's TTL naming the game the user just left.
        if let host = activeHost { NowPlayingStore.shared.invalidate(host) }
        // No-op when this session never reached `.streaming` (a refused/aborted connect).
        displaySleepGuard.release()
        // Drop any armed background keep-alive (incl. the timeout that just fired us).
        backgroundTimer?.cancel()
        backgroundTimer = nil
        isBackgrounded = false
        backgroundDeadline = nil
        // The mic mute is per-session and never persisted: the next stream starts live (if the
        // mic is enabled), rather than silently carrying a mute nobody remembers making.
        micMuted = false
        // Cancel before clearing: a pending clear firing into a torn-down session would be
        // harmless but pointless, and leaving the hint set would carry it into the next stream.
        motionHintTimer?.cancel()
        motionHintTimer = nil
        motionUnreachableKind = nil
        touchHintTimer?.cancel()
        touchHintTimer = nil
        touchFallbackNotice = false
        // Access state is per-session: back to the invisible full-and-permanent default, and
        // no warning latch may carry into the next stream (same discipline as the mic mute).
        accessWarningTimer?.cancel()
        accessWarningTimer = nil
        accessWarning = nil
        launchNoticeTimer?.cancel()
        launchNoticeTimer = nil
        launchNotice = nil
        launchNoticeShown = nil
        accessLevel = .fullControl
        accessRemainingSecs = 0
        accessLimited = false
        accessWarned5m = false
        accessWarned1m = false
        let audio = self.audio
        self.audio = nil
        // The virtual pad's slot goes the same way, while the connection is still up.
        virtualPad?.close()
        virtualPad = nil
        virtualPadShown = false
        // Gamepad capture is main-actor (releases held buttons on the wire while the
        // connection is still up); the feedback drain joins off-main like audio.
        gamepadCapture?.stop()
        gamepadCapture = nil
        // Android's teardown order: unhook the hidRaw sink first (no raw replay onto a dying
        // capture), then the capture — its stop sends gamepadRemove and frees the wire index
        // while the connection is still up. The feedback drain joins off-main below.
        gamepadFeedback?.setHidRawSink(nil)
        sc2Capture?.stop()
        sc2Capture = nil
        // The stop path CANNOT rely on the capture's `.released` edge: `sc2Capture = nil`
        // above deallocates it before its main-queue release hop runs, so the weakly-held
        // callback is already gone. Clear the badge directly — same cancel-before-clear
        // discipline as the motion hint above, and same reason: a "passing through" badge
        // carried into the next stream would be a lie about a session that no longer exists.
        sc2HintTimer?.cancel()
        sc2HintTimer = nil
        sc2CapturedHint = false
        #if os(tvOS)
        remotePointer?.stop() // releases any held click while the connection is still up
        remotePointer = nil
        #endif
        let feedback = gamepadFeedback
        gamepadFeedback = nil
        #if !os(tvOS)
        let clipboard = clipboardSync
        clipboardSync = nil
        #endif
        clipboardEnabled = false
        clipboardReason = 0
        if let conn = connection {
            // Drain-thread teardown waits the pullers out and close() waits out in-flight
            // polls + joins the Rust worker threads — keep all of it off the main actor,
            // in this order (no poll left on any plane when the handle is freed).
            Task.detached {
                audio?.stop()
                feedback?.stop()
                #if !os(tvOS)
                // Disables sync on the wire while the connection is still up — and on iOS pulls a
                // host offer the user has not pasted yet down to real bytes, which needs that
                // connection, so it must stay ahead of the close below.
                clipboard?.stop()
                #endif
                // Deliberate user quit → tell the host to skip the keep-alive linger (must precede close).
                if deliberate { conn.disconnectQuit() }
                conn.close()
            }
        } else {
            Task.detached {
                audio?.stop()
                feedback?.stop()
                #if !os(tvOS)
                clipboard?.stop()
                #endif
            }
        }
        connection = nil
        activeHost = nil
        // A user-ended stream that STARTED on a shelf goes back to it — the same rule a game
        // exit follows, because "I'm done with this game" arrives both ways. Gated on having
        // actually streamed, so a refused or cancelled dial still ends where it was raised
        // from. `sessionEnded` has already set this for the paths it owns; it disconnects
        // non-deliberately, so the two can never both fire.
        if deliberate, phase == .streaming, let shelf = launchedShelf {
            returnToLibrary = shelf
        }
        // Read by `sessionEnded` BEFORE it calls us, so clearing here can't rob it of the answer.
        launchedTitleID = nil
        launchedShelf = nil
        revealStream()
        phase = .idle
        hudLines = []
        // Drop the previous session's grant too — the shared box outlives the session, and a new
        // link may never come up (a non-deadline rung has none at all).
        PresentLinkInfo.shared.clear()
        mouseCaptured = false
        resizing = false
        resizeIndicator = ResizeIndicator() // no stale target/timer into the next session
    }

    /// Called (via the main actor) when the pump hits end-of-session.
    func sessionEnded() {
        guard let conn = connection else { return }
        let name = activeHost?.displayName ?? "host"
        // WHY it ended, asked while the connection is still up — `disconnect` tears it down.
        let reason = conn.sessionEndReason
        // A typed mid-session rejection outranks the coarse reason: an access-expiry close
        // (per-client access §4) files under `.hostError` there, and "ended with an error"
        // is the wrong sentence for "your access expired".
        let rejection = conn.endRejection
        // The host's own words when it sent any: it can name the monitor it was told to
        // capture and no longer has, which our generic line cannot.
        let said = conn.endRejectionMessage
        // Where a game exit sends us: back into the library this session started from, so the
        // next title is a tap away. A plain desktop connect has no shelf, and so no way back.
        let host = activeHost
        // The SHELF is what says this came from the library, not the launch id: a Resume off a
        // shelf launches nothing and still has somewhere to go back to.
        let shelf = launchedShelf
        let endLine = "session ended by \(name) reason=\(reason) "
            + "rejection=\(rejection.map { String(describing: $0) } ?? "-")"
        sessionLog.info("\(endLine, privacy: .public)")
        disconnect(deliberate: false) // host/network ended it — keep the linger for a reconnect
        if let rejection {
            // The host's sentence, else the shared typed-rejection wording
            // ("Your access to this host has expired…").
            errorMessage = "\(name): \(said ?? rejection.userMessage)"
            return
        }
        switch reason {
        case .gameExited:
            // The player quit their own game. Not a failure, and they are probably after the next
            // title — so no banner, and back to the library it came from.
            if host != nil, let shelf {
                returnToLibrary = shelf
            }
        case .hostEnded, .local:
            // Someone asked for this: an operator "End" on the host, or our own close racing in.
            // Say it plainly, without the error framing.
            errorMessage = "\(name) ended the session"
        case .hostError:
            errorMessage = "\(name) ended the session with an error"
        case .lost:
            errorMessage = "Lost the connection to \(name)"
        case .none:
            // No verdict (an older core, or the close raced the read): keep the wording this path
            // has always used rather than inventing one.
            errorMessage = "Session ended by \(name)"
        }
    }

    /// Resize overlay START (main actor — from the Match-window follower's `onResizeTarget`): the
    /// window began differing from the live mode, so a `Reconfigure` toward `(width, height)` is
    /// imminent. Show the blur+spinner immediately, before the debounced request even leaves.
    func resizeTargeted(width: UInt32, height: UInt32) {
        resizeIndicator.steering(
            width: width, height: height, now: ProcessInfo.processInfo.systemUptime)
        resizing = resizeIndicator.active
    }

    /// Resize overlay END (main actor — hopped from the decode pump's `onDecodedSize`): a new-mode
    /// IDR decoded at `(width, height)`. Clears the overlay only when that matches the size we're
    /// steering to (a same-size loss-recovery IDR, or the initial connect IDR, is a no-op).
    func resizeDecoded(width: Int, height: Int) {
        resizeIndicator.decoded(width: UInt32(max(width, 0)), height: UInt32(max(height, 0)))
        resizing = resizeIndicator.active
    }

    /// Drop the launch hold and let the stream through.
    func revealStream() {
        launchWatch?.cancel()
        launchWatch = nil
        launchHold = nil
        launchWindowWait = false
    }

    /// Poll the host once a second for the launched title's state, and reveal when it has
    /// answered — or when it never will. Same lane and identity as the shelf's Resume badge.
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
        let port = connection.map(\.hostMgmtPort).flatMap { $0 > 0 ? $0 : nil } ?? host.effectiveMgmtPort
        guard let identity = (try? ClientIdentityStore.shared.load())?.identity else {
            revealStream()
            return
        }
        let began = Date()
        launchWatch?.cancel()
        launchWatch = Task { [weak self] in
            while !Task.isCancelled {
                let games = await LibraryClient.running(
                    address: host.address, port: port,
                    certPEM: identity.certPEM, keyPEM: identity.keyPEM,
                    hostFingerprint: host.pinnedSHA256)
                let game = games.first { $0.appID == hold.id }
                let state = game?.state
                let elapsed = Date().timeIntervalSince(began)
                let windowWait = state == "running" && game?.awaitingWindow == true
                let done: Bool
                switch state {
                case "launching": done = elapsed >= launchHoldMax
                // A Proton prefix or a splash can sit behind a running process for a minute.
                case "running" where windowWait: done = elapsed >= launchHoldMax
                // window, running, exited, untracked, grace: the host has said all it will.
                case .some: done = true
                case nil: done = elapsed >= launchNoLease
                }
                if done {
                    self?.revealStream()
                    return
                }
                self?.launchWindowWait = windowWait
                try? await Task.sleep(nanoseconds: NSEC_PER_SEC)
            }
        }
    }

    private func beginStreaming() {
        guard let conn = connection else { return }
        // Input capture itself is owned by StreamView (engaged by the captureEnabled
        // flip this phase change causes, released/re-engaged by the user from there).
        phase = .streaming
        watchLaunch()
        displaySleepGuard.acquire()
        // Audio starts with streaming, not during the trust prompt — no host sound (or
        // mic uplink!) before the user trusted the host. Devices and the mic switch come from the
        // session's resolved settings ("" = system default), so a preset that turns the mic on
        // for work calls applies to the uplink too.
        let audio = SessionAudio(connection: conn)
        audio.start(
            speakerUID: settings.speakerUID,
            micUID: settings.micUID,
            micChannel: settings.micChannel,
            // Deny-at-setup for an ungranted mic (per-client access §5): no MIC bit, no
            // uplink at all — a capture the host would only drop is pure privacy downside.
            micEnabled: settings.micEnabled && conn.canUseMic,
            echoCancel: settings.echoCancel,
            // The A/V sync reference: `endToEnd` is capture→on-glass, the one figure that says
            // where the picture actually IS, and the audio ring steers its depth to land with it.
            // The same meter object the presenter writes per presented frame, so audio reads the
            // video plane's own measurement rather than a second estimate of it — and under the
            // stage-1 fallback presenter, which stamps nothing, it stays empty and the loop
            // correctly declines to correct.
            videoLatency: endToEnd)
        self.audio = audio
        // Gamepads: forward every controller GamepadManager selected — each on its own wire pad
        // index (a pin forwards only one, Automatic forwards all) — and render the host's feedback
        // back to the pad it's addressed to (rumble always; lightbar/player-LEDs/adaptive-triggers
        // when a pad's virtual device is a DualSense). Same trust gate as audio — nothing is
        // forwarded during the trust prompt.
        // `gamepadForwarding` off means the host gets this device's pads from somewhere else
        // (USB passthrough, or a pad plugged into the host) — capture still runs, and still
        // watches for the escape chord, but puts nothing on the wire.
        // System-button routing: whether raw guide/share presses ride the wire, and whether
        // hold-Select arms as the alternate guide route (auto = on everywhere but macOS —
        // iOS reserves the physical Home press, tvOS never delivers it).
        let capture = GamepadCapture(
            connection: conn, manager: .shared, forwarding: settings.gamepadForwarding,
            systemForward: settings.systemButtonsForward,
            guideGesture: settings.guideGestureEnabled)
        // The cross-client escape chord (hold L1+R1+Start+Select 1.5 s) — on tvOS the only
        // controller way out of a stream (B/Menu is swallowed during sessions; see ContentView).
        capture.onDisconnectRequest = { [weak self] in self?.disconnect() }
        // A pad with a gyro that this session cannot carry — say so once, briefly, and name the
        // setting that fixes it. Already main-actor (GamepadCapture fires it there).
        capture.onMotionUnreachable = { [weak self] kind in self?.noteMotionUnreachable(kind) }
        capture.onRingChord = { [weak self] pad in self?.onRingChord?(pad) }
        capture.onRingNav = { [weak self] nav in self?.onRingNav?(nav) }
        capture.start()
        gamepadCapture = capture
        let feedback = GamepadFeedback(connection: conn, manager: .shared)
        feedback.start()
        gamepadFeedback = feedback
        capture.onOwnershipChange = { [weak feedback] owned in feedback?.setSilenced(!owned) }
        // Steam Controller 2 as-is passthrough (opt-in): capture an OS-paired SC2's vendor GATT
        // service and forward its raw reports — the host mirrors a real 28DE:1302 that its
        // Steam drives directly, and Steam's rumble/settings writes come back through the
        // feedback drain's hidRaw sink onto the physical controller. Gated like Android
        // (StreamScreen's `sc2Capture && gamepadForwarding`); started after GamepadFeedback so
        // the sink's drain is already up. The wire slot is claimed lazily on the first state
        // report, so an absent controller costs nothing but the 2 s acquisition poll.
        // Also grant-gated (per-client access §7, the clipboard's "not asking keeps the UI
        // honest" rule): in a controller-excluded session the connection would silently drop
        // the arrival and every report — holding the BLE radio, claiming a wire slot, and
        // showing a "passing through" badge for a passthrough that cannot happen. A grant
        // added mid-session engages on the next stream, exactly like the clipboard.
        if settings.sc2Capture, settings.gamepadForwarding, conn.canSendGamepad {
            let sc2 = Sc2Capture(connection: conn, manager: .shared)
            // The same escape-chord contract as GamepadCapture — a captured SC2's raw feed
            // bypasses GC entirely, so it brings its own way out of the stream.
            sc2.onDisconnectRequest = { [weak self] in self?.disconnect() }
            // The ring's pad path, the same two hooks GamepadCapture gets above.
            sc2.onRingChord = { [weak self] in self?.onRingChord?(nil) }
            sc2.onRingNav = { [weak self] nav in self?.onRingNav?(nav) }
            // Claim/release → the bottom-stack badge (the capture's only UI surface).
            sc2.onPhaseChange = { [weak self] phase in self?.noteSc2Phase(phase) }
            feedback.setHidRawSink(sc2.onHidRaw)
            sc2.start()
            sc2Capture = sc2
        }
        #if !os(tvOS)
        // Shared clipboard: opt-in per host AND host-advertised (older hosts / operator-disabled
        // hosts never see a ClipControl) AND granted to this device (per-client access §5 —
        // without the bit the host would refuse with CLIP_REASON_NOT_PERMITTED anyway; not
        // asking keeps the UI honest). Same trust gate as audio — nothing is announced
        // during the trust prompt.
        if activeHost?.clipboardSync == true, conn.hostSupportsClipboard, conn.canUseClipboard {
            startClipboardSync(conn)
        }
        #endif
        #if os(tvOS)
        let pointer = SiriRemotePointer(connection: conn)
        pointer.onDisconnectRequest = { [weak self] in self?.disconnect() }
        // The remote's short Back is the ring's opener on tvOS — the same hook the pad chord
        // uses, so the view wires one closure for both.
        pointer.onShortBack = { [weak self] in self?.onRingChord?(nil) }
        pointer.onRingNav = { [weak self] nav in self?.onRingNav?(nav) }
        pointer.start()
        remotePointer = pointer
        #endif
    }

    #if !os(tvOS)
    /// Create + start the session's clipboard bridge and route its host acks into the published
    /// UI state. `ClipboardSync.start()` sends the enable; the host's `.state` answer flips
    /// `clipboardEnabled` (or leaves it false with a `clipboardReason` the UI can explain).
    private func startClipboardSync(_ conn: PunktfunkConnection) {
        let sync = ClipboardSync(connection: conn)
        sync.onState = { [weak self] enabled, _, reason in
            Task { @MainActor in
                self?.clipboardEnabled = enabled
                self?.clipboardReason = reason
            }
        }
        sync.start()
        clipboardSync = sync
    }
    #endif

    /// Flip clipboard sync mid-session (the Stream menu). Off → on requires the host cap; on →
    /// off tears the bridge down (off-main — the drain join must not block the main actor) and
    /// tells the host, which drops any selection we own there. No-op on tvOS or while idle.
    func toggleClipboardSync() {
        #if !os(tvOS)
        guard let conn = connection, phase == .streaming else { return }
        if let sync = clipboardSync {
            clipboardSync = nil
            clipboardEnabled = false
            clipboardReason = 0
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
                self.resizing = self.resizeIndicator.active
                // Access chip + expiry warnings: the same tick that drives every other live
                // readout also walks the countdown and picks up mid-session grant edits.
                self.updateAccessState()
                guard let conn = self.connection else { return }
                if let notice = conn.launchNotice, notice != self.launchNoticeShown {
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
                    let text = conn.hudLines(tier: .detailed, advanced: true, facts: self.hudFacts())
                        .map(\.text).joined(separator: " | ")
                    statsLog.info("\(text, privacy: .public)")
                    if statsToStdout { print("pf.stats \(text)") }
                }
            }
        }
        // .common so the HUD keeps updating during window drags / menu tracking.
        RunLoop.main.add(timer, forMode: .common)
        statsTimer = timer
    }

    /// Re-format the overlay from the last drained window: each second, and at once on a tier
    /// change so a cycle never waits for the next tick.
    func renderHud() {
        guard let conn = connection else {
            hudLines = []
            return
        }
        let advanced = UserDefaults.standard.bool(forKey: DefaultsKey.advancedStats)
        hudLines = conn.hudLines(tier: statsVerbosity, advanced: advanced, facts: hudFacts())
    }

    /// What only the app knows: the floor policy (macOS presents straight to the display, so
    /// nothing is shaved there), this app's audio ring, the preset, and the Apple-only lines.
    private func hudFacts() -> PunktfunkConnection.HudFacts {
        var f = PunktfunkConnection.HudFacts()
        #if os(macOS)
        f.shaveOsFloor = false
        #else
        f.shaveOsFloor = true
        #endif
        if let a = audio?.stats {
            f.audioBufferMs = UInt32(clamping: a.bufferMS)
            f.avOffsetMs = Int32(clamping: a.avOffsetMS)
        }
        f.preset = settings.presetName
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
