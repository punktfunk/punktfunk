// Hosts grid ⇄ trust prompt ⇄ live stream. ContentView owns the session model, host store and
// LAN discovery, and switches between the home grid (HomeView) and the live session. The connect,
// trust and wake flow (ConnectFlow), the grid + cards (HomeView/HostCards), the trust prompt
// (TrustCardView) and the HUD with its badges (StreamHUDView) live in their own files.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

struct ContentView: View {
    @StateObject private var model = SessionModel()
    @ObservedObject private var store = HostStore.shared
    /// The settings-preset catalog (design/client-settings-profiles.md §4.2) — read at every
    /// connect to resolve the session's `EffectiveSettings`, and edited by the settings surface.
    @ObservedObject private var presets = PresetStore.shared
    @StateObject private var discovery = HostDiscovery()
    // The dev auto-connect hook (DEBUG-only — see `autoConnectIfAsked`) writes these three, so
    // they stay observed here; every OTHER stream setting reaches a session through
    // `EffectiveSettings`, resolved once per connect.
    @AppStorage(DefaultsKey.streamWidth) private var width = 0
    @AppStorage(DefaultsKey.streamHeight) private var height = 0
    @AppStorage(DefaultsKey.streamHz) private var hz = 0
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) private var fullscreenWhileStreaming = true
    @AppStorage(DefaultsKey.fullscreenAlways) private var fullscreenAlways = false
    // The raw string is what @AppStorage observes (so cycles from any surface re-render this
    // view); the absent-key default runs the legacy-hudEnabled migration once per init.
    @AppStorage(DefaultsKey.statsVerbosity) private var statsVerbosityRaw
        = StatsVerbosity.current.rawValue
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.statsScalePct) private var statsScalePct = 100
    /// The tier the overlay actually shows: the live session's (its preset's, then whatever the
    /// ⌃⌥⇧S/three-finger cycle moved it to) while streaming, the persisted global otherwise.
    private var statsVerbosity: StatsVerbosity {
        model.connection != nil
            ? model.statsVerbosity
            : (StatsVerbosity(rawValue: statsVerbosityRaw) ?? .normal)
    }
    /// Fullscreen-while-streaming is presetable (a Game preset goes fullscreen, a Work one
    /// doesn't), so a live session obeys ITS value and the host list obeys the global.
    private var fullscreenForSession: Bool {
        model.connection != nil ? model.settings.fullscreenWhileStreaming : fullscreenWhileStreaming
    }
    /// The window is in a fullscreen THIS app drove it into (FullscreenController owns it).
    @State private var appDrivenFullscreen = false
    @State private var showAddHost = false
    /// A `punktfunk://` deep link (widget / Siri / Shortcuts) couldn't be honored — unknown host, or
    /// a live session is already up. Surfaced as an informational alert (distinct from the
    /// "Connection failed" one, which is for actual connect errors).
    @State private var deepLinkNotice: String?
    /// A `punktfunk://` deep link that named a saved host by something GUESSABLE — its display
    /// name, its address, or the `host=` recovery parameter — instead of by its stable record id.
    /// Anything that can open a URL can guess "Gaming PC", so the link's action waits for this
    /// confirmation; a link that names the id (every shortcut this app emits) still runs on its own.
    struct DeepLinkConfirm {
        /// Which question an answer belongs to: a newer link replaces the one on screen.
        let id = UUID()
        let host: StoredHost
        let launch: String?
        let preset: PresetSelection
        /// A `browse` link: open the host's library instead of dialing it.
        let browse: Bool
        /// The link's `as=`: the host profile to play as, this connect only.
        var asProfile: String?

        var actionTitle: String { browse ? "Open Library" : "Connect" }
        var message: String {
            let asked = browse
                ? "open \(host.displayName)'s game library"
                : "connect to \(host.displayName)"
                    + (launch.map { " and launch \u{201C}\($0)\u{201D}" } ?? "")
            return "A link asked to \(asked). It names the host by its label or address, which "
                + "anything that can open a link could guess — a shortcut made in Punktfunk names "
                + "the host's id and opens without asking."
        }
    }
    @State private var deepLinkConfirm: DeepLinkConfirm?
    /// The console could not be built on this device (`ConsoleHomeView.onFailed`).
    @State private var consoleFailed = false
    #if os(iOS)
    /// Owns the Live Activity for the running session (Lock Screen / Dynamic Island). Driven from
    /// the session model's published state below; iPhone/iPad only.
    @State private var liveActivity = SessionActivityController()
    #endif
    @State private var pairingTarget: StoredHost?
    /// A fresh `pair=required`/unknown host the user tapped: drives the choice between no-PIN
    /// delegated approval ("Request Access") and the SPAKE2 PIN ceremony (rule 3b).
    @State private var approvalChoice: ApprovalRequest?
    /// A delegated-approval connect is in flight (host parks it until the operator approves):
    /// drives the cancelable "Waiting for approval" prompt and the pin-as-paired on success.
    @State private var awaitingApproval: ApprovalRequest?
    /// The profile picker a connect waits on.
    @State private var profileAsk: ProfileAsk?
    /// The wait for a profile's seat to come up, before the dial.
    @State private var seatWait: SeatWait?
    @State private var speedTestTarget: StoredHost?
    @State private var libraryTarget: LibraryTarget?
    #if os(iOS) || os(visionOS) || os(tvOS)
    /// The touch and TV UIs' tab. A written `libraryTarget` lands on the Library tab.
    @State private var touchTab: TouchTab = .hosts
    #endif
    @AppStorage(DefaultsKey.libraryShelf) private var libraryShelfID = ""
    #if os(macOS)
    /// The Mac's source-list selection. A written `libraryTarget` opens the Library row on it.
    @State private var macDestination: MacDestination = .hosts
    /// What a host window hands over: this window streams, browses, wakes and pairs for it.
    @ObservedObject private var hostRouter = MacHostRouter.shared
    @Environment(\.openWindow) private var openWindow
    /// `.key` while this window is the one in front.
    @Environment(\.controlActiveState) private var controlActiveState
    #endif
    /// Wakes a sleeping host and waits for it to come back online before connecting (drives the
    /// "Waking…" phase of the connect overlay). Available on every platform now that the iOS/tvOS
    /// multicast entitlement is granted (see PunktfunkConnection.wakeOnLANAvailable).
    @StateObject private var waker = HostWaker()
    #if os(macOS)
    /// Whether the hosting window is native-fullscreen right now (reported by
    /// FullscreenController). Holds the error alert back while a fullscreen we drove leaves.
    @State private var isFullscreen = false
    /// The fullscreen edge and ownership, outliving the controller views SwiftUI rebuilds.
    @State private var fullscreenEdge = FullscreenController.Edge()
    #endif
    #if os(iOS) || os(visionOS)
    /// The stats-OFF tier's touch-exit disc window (see the overlay in `stream(captureEnabled:)`
    /// — the disc must LEAVE the hierarchy so nothing composites over the metal layer).
    @State private var showTouchExit = false
    #endif
    /// The quick-action ring (design/touch-client-overlay.md §2), one per session. iOS opens it
    /// with the two-finger twist or the exit disc, tvOS with a short Back on the remote, macOS
    /// with ⌃⌥⇧O or the Stream menu, an iPad keyboard with ⌃⌥⇧O; a pad opens it with `Select+A`
    /// on all three (§2.5, §2.6).
    @StateObject private var ring = RingState()

    /// The ring this platform draws. macOS takes the DESKTOP default (no soft keyboard, no
    /// on-screen pad), tvOS the TV one (no touch, keyboard or microphone slot), iOS the touch
    /// one; a configured blob overrides each.
    private var ringConfig: OverlayConfig {
        #if os(macOS)
        OverlayConfig.parse(model.settings.overlayActions, platform: .desktop)
        #elseif os(tvOS)
        OverlayConfig.parse(model.settings.overlayActions, platform: .tv)
        #else
        OverlayConfig.parse(model.settings.overlayActions)
        #endif
    }
    #if !os(macOS)
    @State private var showSettings = false
    #endif
    // A connected controller (+ the Settings toggle) swaps the whole home screen for the console
    // (ConsoleHomeView) instead of retrofitting HomeView's touch/desktop UI — see `home` below.
    // On tvOS the same screens are focus-engine-driven, so the Siri Remote keeps working;
    // with no (extended) controller attached tvOS falls back to HomeView as before.
    @ObservedObject private var gamepadManager = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    /// When the switch above takes over — "connected" (default) or "always". See
    /// `GamepadUIEnvironment`.
    @AppStorage(DefaultsKey.gamepadUIMode) private var gamepadUIMode =
        GamepadUIEnvironment.modeWhenConnected
    /// Auto-wake on connect (Settings → General). On (default): a dial to an offline saved host
    /// fires Wake-on-LAN up front and falls into the "Waking…" wait if the dial fails. Off: connects
    /// go straight through with no wake. The explicit "Wake Host" action is unaffected either way.
    @AppStorage(DefaultsKey.autoWake) private var autoWakeEnabled = true
    /// Where a bare launch opens (Settings → Library). Library (the default) opens the default
    /// host's shelf; Stream also dials its desktop. Resolved once per process by
    /// `applyStartScreen`, never on foregrounding — see `startApplied`.
    @AppStorage(DefaultsKey.startIn) private var startInRaw = StartIn.hosts.stored
    /// Which host that is, when several are paired. Empty until somebody picks one; with exactly
    /// one paired host the default is derived and this stays empty.
    @AppStorage(DefaultsKey.defaultHost) private var defaultHostID = ""
    /// The start screen is a once-per-process decision, so a second window never re-runs it. Set
    /// by `applyStartScreen` and by `handleDeepLink`, so whichever fires first on a cold start
    /// wins and the other stands down.
    @MainActor private static var startApplied = false
    /// Background keep-alive (Settings → General, iOS-only). Default OFF (today's freeze-on-background
    /// is the default). When on, backgrounding a live session keeps audio + the connection alive and
    /// drops video, auto-disconnecting after `backgroundTimeoutMinutes`.
    @AppStorage(DefaultsKey.backgroundKeepAlive) private var backgroundKeepAlive = false
    @AppStorage(DefaultsKey.backgroundTimeoutMinutes) private var backgroundTimeoutMinutes = 10
    /// scenePhase drives the keep-alive: use THIS, not the willResignActive observers — resign-active
    /// also fires for Control Center / app-switcher peeks, where the disconnect timer must not start.
    @Environment(\.scenePhase) private var scenePhase

    /// While the console fronts the app and no stream is up, the console draws every screen:
    /// connect, wake, pairing, the approval wait, a failed dial, its launch hold. The app's own
    /// alerts and sheets would be a second interface over it — and on a TV, a focus trap the pad
    /// cannot reach. Once the stream shows, the app owns the screen again, trust card included.
    private var consoleOwnsScreen: Bool {
        gamepadUIActive
            && (model.phase == .idle || model.phase == .connecting || consoleHoldsStream)
    }

    /// The console's launch hold covers the stream it dialled. A trust card is the stream
    /// view's, so the hold gives way to it.
    private var consoleHoldsStream: Bool {
        guard model.consoleHold else { return false }
        if case .awaitingTrust = model.phase { return false }
        return true
    }

    /// A console that could not be built hands the screen back to this app's own UI.
    private var gamepadUIActive: Bool {
        !consoleFailed
            && GamepadUIEnvironment.isActive(
                gamepadConnected: gamepadManager.uiPadConnected, enabledSetting: gamepadUIEnabled,
                mode: gamepadUIMode)
    }

    // The body is split in two — `driven` (the screen plus its lifecycle drivers and sheets) and
    // the prompt chain below. Not a style choice: as ONE expression this blew Swift's
    // type-checker budget on the iOS slice ("unable to type-check in reasonable time"), which
    // macOS builds never reveal. Keep new modifiers on whichever half is shorter.
    var body: some View {
        driven
            // Fresh pair=required / unknown host: the two ways in. An alert, since iOS 26 draws a
            // confirmation dialog as a narrow popover that squeezes this message. "Request Access"
            // is the no-PIN approval path, "Pair with PIN…" the SPAKE2 ceremony. The follow-on
            // presentation waits a tick so this alert is fully dismissed first.
            .alert(
                "Pairing required",
                isPresented: approvalChoicePresented,
                presenting: approvalChoice
            ) { req in
                Button("Request Access") {
                    DispatchQueue.main.async { flow.requestAccess(req) }
                }
                Button("Pair with PIN…") {
                    DispatchQueue.main.async { pairingTarget = req.host }
                }
                Button("Cancel", role: .cancel) {}
            } message: { req in
                Text("\(req.host.displayName) requires pairing. Request access and approve this "
                    + "device in the host's web console (port 47992 → Pairing) — no PIN needed. Or "
                    + "pair with the 4-digit PIN it can display.")
            }
            // One "Connection failed" surface for every home screen (touch grid, gamepad launcher)
            // and platform — SessionModel funnels all connect/session errors into `errorMessage`.
            .alert("Connection failed", isPresented: connectionErrorPresented) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(model.errorMessage ?? "")
            }
            // The delegated-approval wait: the host holds the connection open until the operator
            // approves it. Cancel returns the UI at once; the in-flight connect is left to time out
            // and its late result is discarded by SessionModel's connect guard (disconnect resets
            // the phase/host it checks).
            .alert(
                "Waiting for approval",
                isPresented: awaitingApprovalPresented,
                presenting: awaitingApproval
            ) { _ in
                Button("Cancel", role: .cancel) { model.disconnect() }
            } message: { req in
                Text("Approve \u{201C}\(DeviceName.current)\u{201D} in \(req.host.displayName)'s "
                    + "web console (port 47992 → Pairing). This device connects automatically once "
                    + "you approve it — no need to reconnect.")
            }
            // Who is playing: shown by a connect when its host lists several profiles.
            .sheet(item: $profileAsk) { ProfilePickerView(ask: $0) }
            // The seat of the picked profile is starting; the dial follows once it is up.
            .sheet(item: $seatWait) { SeatWaitView(wait: $0) }
            // Informational deep-link outcome (unknown host, a refused preset, already
            // streaming). Not an error.
            .alert("Can't open", isPresented: deepLinkNoticePresented) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(deepLinkNotice ?? "")
            }
            // A link that named a saved host by a guessable reference: the dial (or the library)
            // happens on the user's word rather than on the link's.
            .alert(
                "Open this link?",
                isPresented: deepLinkConfirmPresented,
                presenting: deepLinkConfirm
            ) { confirm in
                Button(confirm.actionTitle) { runDeepLinkConfirm(confirm) }
                Button("Cancel", role: .cancel) {}
            } message: { confirm in
                Text(confirm.message)
            }
    }

    /// The confirmed link's action: exactly what a `.known` (id-referenced) link would have done,
    /// one tap later.
    private func runDeepLinkConfirm(_ confirm: DeepLinkConfirm) {
        deepLinkConfirm = nil
        if confirm.browse {
            libraryTarget = LibraryTarget(host: confirm.host, preset: confirm.preset)
        } else {
            flow.connect(
                confirm.host, launchID: confirm.launch, preset: confirm.preset,
                profile: .ask(link: confirm.asProfile))
        }
    }

    private var driven: some View {
        ZStack {
            // The stream view's structural identity MUST be stable across the
            // awaiting-trust → streaming transition: recreating it restarts the pump,
            // which has then already missed the opening IDR (infinite GOP — no other
            // keyframe ever comes) and decodes nothing. So: one branch per connection,
            // trust prompt as an overlay.
            if model.connection != nil {
                sessionView
            }
            // The console stays mounted over the stream while its launch hold is up.
            if model.connection == nil || consoleHoldsStream {
                home
            }
        }
        // The launch hold rides OVER this switch, because it starts before it: the cover leaves
        // its shelf tile at the tap, while `home` is still up, and the swap to `sessionView`
        // happens behind it. Mounting is instant (the view fades its own backdrop in over the
        // shelf — that fade IS the transition); only the reveal fades out.
        .overlay {
            if let hold = model.launchHold {
                LaunchHoldView(
                    entry: hold.entry, host: model.activeHost,
                    connecting: model.connection == nil, windowWait: model.launchWindowWait,
                    download: model.launchDownload,
                    sourceRect: hold.sourceRect,
                    onShow: { model.revealStream() })
                    // Its own view per launch — a reused one keeps the last flight's state.
                    .id(hold.seq)
                    .transition(.asymmetric(insertion: .identity, removal: .opacity))
            }
        }
        .animation(.easeInOut(duration: 0.3), value: model.launchHold)
        .onAppear {
            DemoMode.resume(in: store)
            autoConnectIfAsked()
            // An intent that ran before this window subscribed. Ahead of the start screen,
            // which stands down for a link.
            if let link = DeepLinkInbox.takePending() { handleDeepLink(link) }
            applyStartScreen()
            #if os(iOS)
            SessionActivityController.sweepOrphans() // end any Activity a prior killed launch left
            #endif
        }
        // Deep links (widget quick-launch, Siri/Shortcuts): route into the SAME connect path a card
        // tap uses, so trust policy / WoL / the approval sheet all come along. Never starts a
        // parallel session — this drives the one `model` ContentView owns.
        .onOpenURL { handleDeepLink($0) }
        // A Settings change to the stored tier moves a live session too.
        .onChange(of: statsVerbosityRaw) { _, raw in
            model.setStatsVerbosity(StatsVerbosity(rawValue: raw) ?? .normal)
        }
        // The in-stream cycle (⌃⌥⇧S, the three-finger tap, a pad chord) moves only the session
        // it names, never the stored tier.
        .onReceive(NotificationCenter.default.publisher(for: .punktfunkStatsCycled)) { note in
            guard let conn = model.connection, note.object == nil || note.object as AnyObject === conn
            else { return }
            model.cycleStats()
        }
        #if os(iOS) || os(visionOS) || os(tvOS)
        // Coming back to the app re-arms the LAN browse. The home's `onAppear`/`onDisappear` do
        // NOT fire across background/foreground, and a browse the system suspended while we were
        // away does not resume on its own — so the host grid came back empty and stayed empty
        // until the app was relaunched. No-op unless the browse is already running (mid-session
        // the home has deliberately torn it down).
        //
        // Mobile only: macOS never suspends the process, and its `scenePhase` flips on every
        // window focus change — re-arming there would rebuild the browser each time you alt-tab.
        // A Mac browse that genuinely breaks is caught by `HostDiscovery`'s own sweep instead.
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { discovery.refreshIfRunning() }
        }
        #endif
        #if os(iOS) || os(visionOS) || os(tvOS)
        // Backgrounding driver. Only .background/.active matter; .inactive (a transient peek) is
        // ignored so neither branch fires for a Control-Center pull.
        //
        // Backgrounding MUST end the session one way or the other: the app keeps running while
        // streaming (the `audio` background mode plus a live audio session), so its QUIC connection
        // keeps answering the host's keep-alives with the user long gone — the host has no way to
        // tell that apart from someone watching, and the session survived indefinitely. Either hold
        // it under the opt-in keep-alive (bounded by that path's own auto-disconnect timer) or end
        // it here.
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background:
                applyBackgroundPolicy()
                // A kill from the background runs no teardown.
                presets.flush()
            case .active:
                model.exitBackground()
            default:
                break
            }
        }
        // A dial in flight when the user swiped home can land before suspension: same rule.
        .onChange(of: model.phase) { _, phase in
            if phase == .streaming, scenePhase == .background { applyBackgroundPolicy() }
        }
        #endif
        #if os(iOS)
        // Live Activity lifecycle, driven from the model's published state. iPhone/iPad only —
        // ActivityKit (and so `liveActivity`) does not exist on tvOS, which is why this stays in its
        // own os(iOS) block rather than riding the backgrounding driver's.
        .onChange(of: model.phase) { _, phase in
            switch phase {
            case .streaming:
                if let host = model.activeHost {
                    liveActivity.begin(
                        hostID: host.id, hostName: host.displayName,
                        launchTitle: nil, // no live foreground-app title mid-session (v1)
                        modeLine: currentModeLine(), startedAt: Date())
                }
            case .idle:
                liveActivity.end()
            default:
                break
            }
        }
        .onChange(of: model.isBackgrounded) { _, backgrounded in
            liveActivity.update {
                $0.stage = backgrounded ? .background : .streaming
                $0.backgroundDeadline = model.backgroundDeadline
            }
        }
        // The Live Activity's / Shortcuts' End button runs EndStreamIntent in-process, which posts
        // this — tear the session down deliberately (quit-close the host). iOS-only along with
        // the intent itself (LiveActivityIntent is ActivityKit's world).
        .onReceive(NotificationCenter.default.publisher(for: .punktfunkEndActiveSession)) { _ in
            model.disconnect(deliberate: true)
        }
        #endif
        // Connect App Intent (Siri/Shortcuts/Spotlight): route its punktfunk:// URL through the
        // same handler a widget tap uses. NOT iOS-gated — the Connect intent compiles on macOS and
        // tvOS too, and an intent that posts to nobody would be a shortcut that silently does
        // nothing.
        .onReceive(NotificationCenter.default.publisher(for: .punktfunkOpenDeepLink)) { note in
            guard let link = note.object as? NSURL else { return }
            #if os(macOS)
            // Every window hears it: the front one takes it now, another only if none did.
            let wait: TimeInterval = controlActiveState == .key ? 0 : 0.25
            DispatchQueue.main.asyncAfter(deadline: .now() + wait) {
                guard DeepLinkInbox.take(link) else { return }
                handleDeepLink(link as URL)
            }
            #else
            guard DeepLinkInbox.take(link) else { return }
            handleDeepLink(link as URL)
            #endif
        }
        .onChange(of: model.phase) { _, phase in
            switch phase {
            case .streaming:
                #if os(iOS) || os(visionOS)
                showTouchExit = true // the off-tier exit disc's 8 s window, per session start
                #endif
                ring.close()
                ring.native = nil // this session's Welcome mode, captured at its first open
                // Host-action slots are pre-fetched here, never when the ring opens (§3.1).
                if let host = model.activeHost { HostPowerStore.shared.refresh(host) }
                // `Select+A` on a pad opens the ring in the middle of the stage; while it is up
                // the pad belongs to it. On tvOS the remote's short Back arrives here too, and on
                // macOS the chord and the menu item do — on both, a second press closes it again,
                // because on neither is there a finger to tap the scrim with. iOS opens only: the
                // twist that opened it also winds it back.
                model.onRingChord = { [ring] pad in
                    #if os(tvOS) || os(macOS)
                    ring.toggleCentred()
                    #else
                    ring.pressTick &+= 1
                    ring.openCentred()
                    #endif
                    if ring.committed { ring.opener = pad }
                }
                model.onRingNav = { [ring] nav in ring.nav(nav) }
                // A session actually started — remember it on the card ("Connected … ago"
                // plus the accent ring on the most recent host).
                guard let host = model.activeHost else { break }
                // Delegated approval just succeeded: the operator let this device in, so pin the
                // host's observed fingerprint and remember it as paired — future connects are then
                // silent (rule 1), exactly like after a PIN/TOFU success. Dismisses the wait prompt.
                let approvedFingerprint = awaitingApproval?.host.id == host.id
                    ? model.connection?.hostFingerprint : nil
                if awaitingApproval?.host.id == host.id { awaitingApproval = nil }
                // The session's Welcome names the library's port without an mDNS advert, so a
                // host reached by address over a VPN has one too. 0 is not advertised.
                let liveMgmtPort = model.connection?.hostMgmtPort
                let store = store
                // On the next run-loop turn: a store write inside `.onChange` publishes from
                // within a view update.
                DispatchQueue.main.async {
                    store.markConnected(
                        host.id, mgmtPort: liveMgmtPort, fingerprint: approvedFingerprint)
                }
            case .idle:
                // The delegated-approval connect failed, timed out, or was cancelled — drop the
                // wait prompt (SessionModel surfaces any error via `errorMessage`).
                if awaitingApproval != nil { awaitingApproval = nil }
            default:
                break
            }
        }
        .onDisappear { // window closed mid-session or mid-wake (Cmd+N spawns more)
            waker.cancel() // its onOnline would dial for a window that is gone
            model.disconnect()
        }
        // Expose the session to the Scene-level Stream menu (Disconnect ⌃⌥⇧D works even when
        // the HUD is hidden). tvOS has no such menu.
        #if !os(tvOS)
        .focusedSceneValue(\.sessionFocus, SessionFocus(
            isStreaming: model.connection != nil,
            // Host cap AND this device's CLIPBOARD grant (per-client access §7) — an
            // ungranted session's menu item greys out instead of inviting a refused enable.
            clipboardAvailable: model.connection.map {
                $0.hostSupportsClipboard && $0.canUseClipboard
            } == true,
            clipboardOn: model.clipboardEnabled,
            toggleClipboard: { model.toggleClipboardSync() },
            micAvailable: model.micAvailable,
            micMuted: model.micMuted,
            toggleMicMute: { model.toggleMicMute() },
            cycleStats: { model.cycleStats() },
            toggleQuickActions: { if model.phase == .streaming { ring.toggleCentred() } },
            disconnect: { model.disconnect() }))
        // ⌃⌥⇧A fired while input was CAPTURED (InputCapture's chord path posts it — the menu's
        // identical equivalent can't reach a captured stream). Same toggle either way. It names
        // its session, so a second window's stream keeps its own mic state.
        .onReceive(NotificationCenter.default.publisher(for: .punktfunkToggleMicMute)) { note in
            guard note.object as AnyObject === model.connection else { return }
            model.toggleMicMute()
        }
        #endif
        #if os(macOS)
        // Fullscreen from launch under Always, else only while a session is up (incl. the trust
        // prompt over the blurred stream). The session's mode picks native or panel fullscreen
        // and hides the title bar.
        .background(FullscreenController(
            active: fullscreenAlways || (fullscreenForSession && model.connection != nil),
            stream: model.connection.map { CGSize(width: Int($0.width), height: Int($0.height)) },
            captured: model.mouseCaptured,
            isFullscreen: $isFullscreen, appDriven: $appDrivenFullscreen, edge: fullscreenEdge))
        #endif
        // A game launched from the library just exited, so the session ended on purpose: put the
        // player back in that host's library rather than on host selection. Set on the outer Group
        // (like the sheets below) so it survives the streaming → home transition the disconnect
        // drives, and consumed here — the model hands the host over once and we clear it, so a
        // later manual dismiss of the library can't be undone by a stale value.
        .onChange(of: model.returnToLibrary) { _, shelf in
            guard let shelf else { return }
            model.returnToLibrary = nil
            libraryTarget = shelf
        }
        #if os(macOS)
        .onChange(of: hostRouter.pending) { _, request in
            if request != nil { takeHostRequest() }
        }
        .onAppear {
            hostRouter.mainWindows += 1
            takeHostRequest()
        }
        .onDisappear { hostRouter.mainWindows -= 1 }
        // The controllers follow the front window: its stream takes them, or its menus do.
        .onChange(of: controlActiveState) { _, state in
            guard state == .key else { return }
            if model.phase == .streaming {
                model.claimControllers()
            } else {
                GamepadCapture.releaseControllers()
            }
        }
        #endif
        // On the outer Group so the sheet survives the trust-prompt → home transition
        // (the "Pair with PIN instead" path disconnects first — the host's accept loop
        // is sequential, a pairing connection would queue behind the live session).
        #if !os(tvOS)
        // The touch UI's pairing sheet. The console pairs on its own screen, so the sheet hides
        // while the console owns the screen (see `touchPairingTarget`).
        .sheet(item: touchPairingTarget) { host in
            PairSheet(host: host) { fingerprint in handlePaired(host, fingerprint: fingerprint) }
        }
        #endif
    }

    // Presentation flags for the prompt chain, extracted from their `.alert`/`.confirmationDialog`
    // calls so each manual get/set Binding type-checks on its own instead of inflating the body's
    // budget (inline, they tip SwiftUI's per-expression limit — see the split sections idiom).

    private var deepLinkNoticePresented: Binding<Bool> {
        Binding(
            get: { deepLinkNotice != nil && !consoleOwnsScreen },
            set: { if !$0 { deepLinkNotice = nil } })
    }

    /// Down while the console is up: it asks the same question in a way a pad can answer.
    private var deepLinkConfirmPresented: Binding<Bool> {
        Binding(
            get: { deepLinkConfirm != nil && !consoleOwnsScreen },
            set: { if !$0 { deepLinkConfirm = nil } })
    }

    #if os(iOS) || os(visionOS) || os(tvOS)
    /// In the touch and TV UIs a shelf is a tab, not a presentation: a written `libraryTarget`
    /// becomes the Library tab's shelf and clears, so the gamepad shell never inherits it as an
    /// open layer.
    private func showShelfInTab() {
        guard !gamepadUIActive, let shelf = libraryTarget else { return }
        libraryShelfID = shelf.id
        touchTab = .library
        libraryTarget = nil
    }
    #endif

    #if os(macOS)
    /// On the Mac a shelf is the Library row's pick, not a presentation: a written `libraryTarget`
    /// becomes that pick, selects the row and clears. In gamepad mode the console takes it.
    private func showShelfInSidebar() {
        guard !gamepadUIActive, let shelf = libraryTarget else { return }
        libraryShelfID = shelf.id
        macDestination = .library
        libraryTarget = nil
    }

    /// Run a host window's pending request here, unless another main window took it first. A
    /// connect or a pairing needs an idle window: a busy one leaves it to an idle one, then opens
    /// a window of its own for it.
    private func takeHostRequest() {
        guard let request = hostRouter.pending else { return }
        switch request {
        case .connect, .pair:
            guard !model.isBusy else {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) {
                    if hostRouter.claimOpening(request) { openWindow(id: PunktfunkClientApp.mainSceneID) }
                }
                return
            }
        case .browse, .wake:
            break
        }
        _ = hostRouter.take()
        func saved(_ id: StoredHost.ID) -> StoredHost? { store.hosts.first { $0.id == id } }
        switch request {
        case .connect(let id, let selection):
            if let host = saved(id) { flow.connect(host, preset: selection) }
        case .browse(let id):
            if let host = saved(id) { libraryTarget = LibraryTarget(host: host) }
        case .wake(let id):
            if let host = saved(id) { flow.wakeOnly(host) }
        case .pair(let id):
            if let host = saved(id) { pairingTarget = host }
        }
    }
    #endif

    /// The pairing sheet's item. On iOS it hides while the gamepad shell presents the pair screen
    /// in place — the same proxy the library uses, and for the same reason: every writer keeps
    /// writing `pairingTarget`, and whichever presentation the current mode owns picks it up.
    /// macOS has no shell, so the sheet stays and switches its CONTENT by mode instead.
    private var touchPairingTarget: Binding<StoredHost?> {
        #if os(macOS)
        Binding(get: { consoleOwnsScreen ? nil : pairingTarget }, set: { pairingTarget = $0 })
        #else
        Binding(
            get: { gamepadUIActive ? nil : pairingTarget },
            set: { pairingTarget = $0 })
        #endif
    }

    private var approvalChoicePresented: Binding<Bool> {
        Binding(
            get: { approvalChoice != nil && !consoleOwnsScreen },
            set: { if !$0 { approvalChoice = nil } })
    }

    private var awaitingApprovalPresented: Binding<Bool> {
        Binding(
            get: { awaitingApproval != nil && !consoleOwnsScreen },
            set: { if !$0 { awaitingApproval = nil } })
    }

    /// Whether the "Connection failed" alert is ready to be shown at all (see the macOS
    /// deferral below).
    private var connectionErrorReady: Bool {
        guard model.errorMessage != nil else { return false }
        #if os(macOS)
        // Hold the alert while a fullscreen we drove is still leaving: a sheet on a fullscreen
        // window makes AppKit drop `-toggleFullScreen:` and strands it there. Under Always, or in
        // a fullscreen the user chose, nothing leaves, so holding it would swallow the error.
        if appDrivenFullscreen && isFullscreen && !fullscreenAlways { return false }
        #endif
        return true
    }

    private var connectionErrorPresented: Binding<Bool> {
        Binding(
            get: { connectionErrorReady && !consoleOwnsScreen },
            set: { if !$0 { model.errorMessage = nil } })
    }

    #if os(iOS) || os(visionOS)
    /// The Live Activity mode line, e.g. "2560×1440 @120 · HEVC · HDR", from the live connection.
    private func currentModeLine() -> String {
        guard let c = model.connection else { return "" }
        let codec: String
        switch c.videoCodec {
        case .h264: codec = "H.264"
        case .hevc: codec = "HEVC"
        case .av1: codec = "AV1"
        case .pyrowave: codec = "PyroWave"
        }
        var line = "\(c.width)×\(c.height)"
        if c.refreshHz > 0 { line += " @\(c.refreshHz)" }
        line += " · \(codec)"
        if c.isHDR { line += " · HDR" }
        return line
    }
    #endif

    /// Route a `punktfunk://` deep link into the existing connect path — the whole §2 grammar
    /// (design/client-deep-links.md): a stable id, a unique host name or an `addr[:port]`, with
    /// `fp`/`host` recovery parameters and a one-off `preset`.
    ///
    /// The security posture is the parser's plus four rules that live here, and none of them
    /// bends: a URL never pairs and never trusts on its own (an unknown host becomes a
    /// confirmation, not a connect), never dials on a GUESSABLE reference (only the stable record
    /// id connects unattended — a label or an address becomes a confirmation), never preempts a
    /// live session (same host → focus, different host → say so; NEVER tear one down on a
    /// background tap), and carries only references — a preset it can't honor refuses with a
    /// notice rather than streaming with the wrong settings.
    #if os(iOS) || os(visionOS) || os(tvOS)
    /// Hold a streaming session under the opt-in keep-alive, or end it.
    private func applyBackgroundPolicy() {
        guard model.phase == .streaming else { return }
        if backgroundKeepAlive {
            model.enterBackground(timeoutMinutes: backgroundTimeoutMinutes)
        } else {
            // Not deliberate: the user may come straight back, so let the host linger the
            // display for a fast reconnect instead of tearing it down.
            model.disconnect(deliberate: false)
        }
    }
    #endif

    private func handleDeepLink(_ url: URL) {
        // Explicit intent beats the start-screen policy, and the two race on a cold start:
        // `.onOpenURL` and `.onAppear` have no guaranteed order. Claiming the once-per-process
        // slot here is symmetric with `applyStartScreen`, so either order is benign — if the
        // start already opened a shelf, the link's own rules take over from there.
        Self.startApplied = true
        let link: DeepLink
        do {
            link = try DeepLink(url: url)
        } catch DeepLinkError.notOurScheme {
            return // not ours — ignore it silently rather than warning about someone else's URL
        } catch {
            deepLinkNotice = (error as? DeepLinkError)?.message
                ?? "That link is malformed and was ignored."
            return
        }
        switch link.route {
        case .connect:
            break
        case .browse:
            // The reserved library route, now real: open the host's game library without starting
            // a session. `launch=`/`preset=` are meaningless on a browse (nothing streams until a
            // title is picked, and that connect resolves its own preset) — ignored, not refused,
            // per the unknown-parameter rule.
            openLibrary(from: link)
            return
        case .wake:
            // Still reserved: saying so beats silently connecting instead. (Shortcuts users have
            // the Wake Host intent, which never round-trips through a URL.)
            deepLinkNotice = "Punktfunk links can't do “wake” yet."
            return
        }
        let session = DeepLinkRouter.SessionState(
            isIdle: model.phase == .idle,
            activeHostID: model.activeHost?.id,
            activeHostName: model.activeHost?.displayName)
        switch DeepLinkRouter.resolve(
            link: link, hosts: store.hosts, catalog: presets.catalog,
            session: session, browse: false
        ) {
        case .notice(let text):
            deepLinkNotice = text
        case .alreadyHere:
            break // deep-linked to the host we're already on — nothing to do
        case .confirm(let host, let selection):
            deepLinkConfirm = DeepLinkConfirm(
                host: host, launch: link.launch, preset: selection, browse: false,
                asProfile: link.asProfile)
        case .proceed(let host, let selection):
            flow.connect(
                host, launchID: link.launch, preset: selection,
                profile: .ask(link: link.asProfile))
        }
    }

    /// `punktfunk://browse/<host-ref>` — jump into a host's game library. Drives the SAME
    /// `libraryTarget` every internal surface writes, so the link lands in whichever presentation
    /// the current mode owns: the gamepad console's in-place library screen, the touch cover, the
    /// macOS sheet, or tvOS's cover. Connect's posture minus the connect itself: a pin conflict
    /// refuses, a live session is never preempted (same host → the open already foregrounded the
    /// app, which is all "focus it" can mean mid-stream; different host → say so), and an unsaved
    /// host can't be browsed — the library fetch rides the paired mTLS identity, so there is
    /// nothing to show before the host is saved (the notice says what to do instead).
    private func openLibrary(from link: DeepLink) {
        let session = DeepLinkRouter.SessionState(
            isIdle: model.phase == .idle,
            activeHostID: model.activeHost?.id,
            activeHostName: model.activeHost?.displayName)
        switch DeepLinkRouter.resolve(
            link: link, hosts: store.hosts, catalog: presets.catalog,
            session: session, browse: true
        ) {
        case .notice(let text):
            deepLinkNotice = text
        case .alreadyHere:
            break // browsing the host we're already streaming — nothing to do
        case .confirm(let host, let selection):
            deepLinkConfirm = DeepLinkConfirm(
                host: host, launch: nil, preset: selection, browse: true)
        case .proceed(let host, let selection):
            libraryTarget = LibraryTarget(host: host, preset: selection)
        }
    }

    private var home: some View {
        // The full-screen connect takeover rides over BOTH home UIs (and the pre-connect window is
        // still `home`, so it covers the whole dial → wake → online → connect sequence): instant
        // "Connecting…" feedback on any dial, flowing seamlessly into the "Waking…" wait if the host
        // turns out to be asleep.
        homeBase
            #if os(tvOS)
            // The focus engine only enters the takeover once nothing under it can hold focus;
            // then Menu reaches the overlay's `.onExitCommand` instead of the launcher — or the app.
            // Never while the console owns the screen: disabling its view would stop the very
            // input it draws the connect card for.
            .disabled(
                !consoleOwnsScreen
                    && (connectingOverlayName != nil || waker.waking != nil
                        || model.launchHold != nil))
            #endif
            .overlay {
                // The console draws its own dial, wake wait and launch hold.
                if !consoleOwnsScreen {
                    ConnectOverlay(
                        connectingHostName: connectingOverlayName,
                        waker: waker,
                        onCancelConnect: { model.disconnect() })
                }
            }
    }

    /// The host label for the connect takeover's "Connecting…" phase — a plain dial in flight. Nil
    /// during the delegated-approval wait (that has its own "Waiting for approval" prompt, so the
    /// takeover must not stack over it) and, of course, when idle or streaming.
    private var connectingOverlayName: String? {
        // A launch dial has the launch hold instead — that says which GAME is coming, and stacking
        // "Connecting to <host>" on top of it would be two takeovers for one act.
        guard awaitingApproval == nil, model.launchHold == nil,
              model.phase == .connecting, let host = model.activeHost
        else { return nil }
        return host.displayName
    }

    @ViewBuilder private var homeBase: some View {
        #if os(macOS)
        Group {
            if gamepadUIActive {
                console
            } else {
                MacShellView(
                    store: store, selection: $macDestination,
                    hosts: HomeView(
                        store: store, model: model, discovery: discovery,
                        showAddHost: $showAddHost, pairingTarget: $pairingTarget,
                        speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
                        connect: { flow.connect($0, preset: $1) },
                        connectDiscovered: flow.connectDiscovered,
                        onPaired: handlePaired, onLaunchTitle: launchTitle,
                        onConnectShelf: connectFromShelf, wake: { flow.wakeOnly($0) }),
                    onLaunch: launchTitle, onConnectShelf: connectFromShelf,
                    onConnectHost: { flow.connect($0, preset: .inherit, fromLibrary: true) })
                // On appear too: `returnToLibrary` writes the shelf while the stream is still up.
                .onAppear(perform: showShelfInSidebar)
                .onChange(of: libraryTarget) { _, _ in showShelfInSidebar() }
                .modifier(HomePresence(store: store, discovery: discovery))
            }
        }
        #else
        Group {
            if gamepadUIActive {
                console
            } else {
                touchTabs
                    // On appear too: `returnToLibrary` writes the shelf while the stream is still up.
                    .onAppear(perform: showShelfInTab)
                    .onChange(of: libraryTarget) { _, _ in showShelfInTab() }
                    .modifier(HomePresence(store: store, discovery: discovery))
            }
        }
        #endif
    }

    /// The shared console (`design/console-ui-element-layer.md`): home, library, settings,
    /// pairing and the host menu are all drawn by it, so this branch mounts nothing else. A
    /// shelf written while it is up is a navigation inside it, not a presentation over it.
    private var console: some View {
        ConsoleHomeView(
            store: store, model: model, discovery: discovery, waker: waker,
            entry: $libraryTarget, notice: $deepLinkNotice, pairing: $pairingTarget,
            linkConfirm: $deepLinkConfirm, runLink: runDeepLinkConfirm,
            onFailed: { consoleFailed = true }, onPaired: handlePaired,
            connect: { flow.connect($0, preset: $1, profile: .send($2)) },
            connectDiscovered: flow.connectDiscovered,
            requestAccess: flow.consoleRequestAccess,
            requestAccessDiscovered: flow.requestAccessDiscovered,
            launchTitle: { launch($0, $1, profile: .send($2)) }, connectShelf: connectFromShelf,
            wakeOnly: { flow.wakeOnly($0) })
    }

    #if !os(macOS)
    /// The host list of the touch and remote UIs.
    private var touchHome: some View {
        HomeView(
            store: store, model: model, discovery: discovery,
            showAddHost: $showAddHost, pairingTarget: $pairingTarget,
            speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
            showSettings: $showSettings,
            connect: { flow.connect($0, preset: $1) }, connectDiscovered: flow.connectDiscovered,
            onPaired: handlePaired, onLaunchTitle: launchTitle,
            onConnectShelf: connectFromShelf, wake: { flow.wakeOnly($0) })
    }
    #endif

    #if os(iOS) || os(visionOS) || os(tvOS)
    #if os(iOS) || os(visionOS)
    /// Hosts and Library. On iPadOS 18 the tab bar turns into a sidebar at a tap, as iPad apps do;
    /// iOS 17 keeps the plain tab bar.
    @ViewBuilder private var touchTabs: some View {
        if #available(iOS 18, *) {
            TabView(selection: $touchTab) {
                Tab("Hosts", systemImage: "desktopcomputer", value: TouchTab.hosts) { touchHome }
                Tab("Library", systemImage: "square.grid.2x2", value: TouchTab.library) {
                    libraryTab
                }
            }
            .tabViewStyle(.sidebarAdaptable)
        } else {
            TabView(selection: $touchTab) {
                touchHome
                    .tabItem { Label("Hosts", systemImage: "desktopcomputer") }
                    .tag(TouchTab.hosts)
                libraryTab
                    .tabItem { Label("Library", systemImage: "square.grid.2x2") }
                    .tag(TouchTab.library)
            }
        }
    }
    #else
    /// Hosts, Library and Settings as the TV's top tab bar: one swipe up reaches it from anywhere,
    /// and Menu inside a tab returns to it.
    private var touchTabs: some View {
        TabView(selection: $touchTab) {
            touchHome
                .tabItem { TouchTab.hostsLabel }
                .tag(TouchTab.hosts)
            libraryTab
                .tabItem { Label("Library", systemImage: "square.grid.2x2") }
                .tag(TouchTab.library)
            NavigationStack { SettingsView() }
                .tvPushSlide()
                .tabItem { Label("Settings", systemImage: "gearshape") }
                .tag(TouchTab.settings)
        }
    }
    #endif

    private var libraryTab: some View {
        LibraryTabView(
            store: store, onLaunch: launchTitle, onConnectShelf: connectFromShelf,
            onConnectHost: { flow.connect($0, preset: .inherit, fromLibrary: true) },
            showHosts: { touchTab = .hosts })
    }
    #endif

    // MARK: - Session

    private var sessionView: some View {
        let pendingFingerprint: Data? = {
            if case .awaitingTrust(let fp) = model.phase { return fp }
            return nil
        }()
        return ZStack {
            stream(captureEnabled: pendingFingerprint == nil && !consoleHoldsStream)
                // Blur the live stream during the trust prompt (heavy) and during a resize (lighter
                // — the deliberate "hold on" while the host rebuilds its pipeline and the decoder
                // re-inits on the new-mode IDR). Only the resize blur animates; the trust blur snaps
                // as before (its own overlay handles the transition).
                .blur(radius: pendingFingerprint != nil ? 32 : (model.resizing ? 16 : 0))
                .animation(.easeInOut(duration: 0.22), value: model.resizing)
                .overlay {
                    if pendingFingerprint != nil {
                        Color.black.opacity(0.45)
                    }
                }
                // The resize spinner rides over the (blurred) stream; suppressed under the trust
                // prompt, which owns the screen. It never hit-tests, so window-drag resizes keep
                // steering and the next click still reaches the stream. Mounted ONLY while a
                // resize is live: resident structure above the CAMetalLayer is what the stage-4
                // direct-to-display hunt is eliminating — composited presents reach glass a full
                // refresh later. The enter/exit fade rides the call-site transition + the
                // .animation(value: resizing) below (the view's internal `if active` fade can't
                // run when the whole view unmounts).
                .overlay {
                    if pendingFingerprint == nil, model.resizing {
                        ResizeIndicatorView(active: true)
                            .transition(.opacity.combined(with: .scale(scale: 0.92)))
                    }
                }
                .animation(.easeInOut(duration: 0.22), value: model.resizing)
            if let fp = pendingFingerprint {
                TrustCardView(
                    fingerprint: fp,
                    hostName: model.activeHost?.displayName ?? "host",
                    onCancel: { model.rejectTrust() },
                    onTrust: {
                        if let fp = model.confirmTrust(), let host = model.activeHost {
                            store.pin(host.id, fingerprint: fp)
                        }
                    },
                    onPairInstead: {
                        let host = model.activeHost
                        model.rejectTrust()
                        pairingTarget = host
                    })
            }
        }
        #if os(macOS)
        .frame(minWidth: 640, minHeight: 360)
        .background(Color.black)
        // Edge-to-edge: FullscreenController hides the title bar for a session, and the panel
        // fullscreen covers the camera housing on purpose (a thin top-centre strip occluded).
        .ignoresSafeArea()
        #elseif os(iOS) || os(visionOS)
        // Streaming is immersive: edge-to-edge under the status bar and home
        // indicator, both hidden for the session (they return with the hosts grid).
        .background(Color.black)
        .ignoresSafeArea()
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        #else
        .background(Color.black)
        .ignoresSafeArea()
        // SWALLOW Menu/B during a session — a game controller's B button ALSO surfaces as this
        // UIKit menu press, so the old instant-disconnect here ended the session on every B
        // press in gameplay. The button still reaches the host via GamepadCapture; the
        // DELIBERATE exits are holding the remote's Back ≥ 1 s (SiriRemotePointer) and holding
        // L1+R1+Start+Select ≥ 1.5 s on a pad (GamepadCapture's escape chord), both surfaced by
        // the start-of-stream banner. The empty handler is what keeps the press from bubbling
        // out and suspending the app.
        .onExitCommand {}
        #endif
    }

    private func stream(captureEnabled: Bool) -> some View {
        let placement = HUDPlacement(rawValue: hudPlacement) ?? .topTrailing
        return Group {
            if let conn = model.connection {
                StreamView(
                    connection: conn,
                    captureEnabled: captureEnabled,
                    onCaptureChange: { [weak model] captured in
                        model?.mouseCaptured = captured
                        #if os(visionOS)
                        // The window that takes the keyboard takes the controllers too.
                        if captured { model?.claimControllers() }
                        #endif
                    },
                    onDisconnectRequest: { [weak model] in
                        model?.disconnect() // the captured-state ⌃⌥⇧D combo
                    },
                    onDial: dialSink,
                    onFrame: { [meter = model.meter, queue = model.clientQueue] au in
                        meter.note(byteCount: au.data.count)
                        // Receipt and the host split are the core's; the client-queue wait
                        // (receipt → pull, both client-local) is Apple's own overlay line.
                        queue.record(
                            ptsNs: UInt64(bitPattern: au.receivedNs), atNs: au.pulledNs,
                            offsetNs: 0)
                    },
                    onSessionEnd: { [weak model] in
                        Task { @MainActor in model?.sessionEnded() }
                    },
                    // Resize overlay START — the follower is main-actor, so this drives the blur
                    // + spinner synchronously the instant the window differs from the live mode.
                    onResizeTarget: { [weak model] w, h in
                        model?.resizeTargeted(width: w, height: h)
                    },
                    // Resize overlay END — the coded dims of each new-mode IDR, reported from the
                    // decode pump thread; hop to the main actor to clear the overlay.
                    onDecodedSize: { [weak model] w, h in
                        Task { @MainActor in model?.resizeDecoded(width: w, height: h) }
                    },
                    endToEndMeter: model.endToEnd
                )
                #if os(visionOS)
                .theater(TheaterStage.shared.renderers(for: conn))
                .overlay {
                    if TheaterStage.shared.renderers(for: conn) != nil { InTheaterPlaceholder() }
                }
                .ornament(attachmentAnchor: .scene(.bottom), contentAlignment: .top) {
                    StreamOrnament(connection: conn, quickActions: { ring.toggleCentred() })
                }
                #endif
                .overlay(alignment: placement.alignment) {
                    // The stats overlay MORPHS between tiers and SCALES UP on enter. With no `.id`, a
                    // verbosity change keeps the same StreamHUDView identity, so its one shared glass
                    // card animates its frame/shape to the new tier (a morph) instead of cross-fading a
                    // fresh card in. The `.transition` therefore fires only on the off↔on boundary — a
                    // scale-up (0.8→1) from the HUD's own corner. The ZStack is the stable host the
                    // `.animation` watches as the child enters/leaves and morphs.
                    ZStack {
                        if captureEnabled && statsVerbosity != .off {
                            StreamHUDView(
                                model: model, connection: conn, placement: placement,
                                verbosity: statsVerbosity,
                                scale: Double(min(max(statsScalePct, 75), 200)) / 100)
                                .transition(
                                    .scale(scale: 0.8, anchor: placement.unitPoint)
                                        .combined(with: .opacity))
                        }
                    }
                    .animation(.smooth(duration: 0.28), value: statsVerbosity)
                }
                .overlay(alignment: .bottom) {
                    StreamBadgeStack(
                        model: model, captureEnabled: captureEnabled, statsVerbosity: statsVerbosity)
                }
                #if os(iOS) || os(visionOS)
                // Touch has no menu or ⌘D: while the HUD shows no Disconnect (compact, off) a
                // corner disc opens the ring. Off drops it after 8 s, since any overlay above the
                // stream costs ~a refresh of latency; compact composites a pill anyway. The
                // virtual controller carries its own ring button, so the discs leave while it is up.
                .overlay(alignment: .topLeading) {
                    if captureEnabled, !model.virtualPadShown,
                       statsVerbosity == .compact || (statsVerbosity == .off && showTouchExit) {
                        HStack(spacing: 10) {
                            // Opens the quick-action ring (End stream is a slot inside, behind
                            // a two-press arm), keeping the disc's fade rules.
                            Button {
                                ring.pressTick &+= 1
                                ring.openAt(CGPoint(x: 30, y: 30))
                            } label: { touchDisc("ellipsis.circle") }
                                .buttonStyle(.plain)
                                .accessibilityLabel("Quick actions")
                            // The mic toggle rides the same discs, for the same reason: in these
                            // tiers the HUD carries no buttons (compact is a stat pill, off is
                            // nothing), so this is a touch-only user's ONLY way to mute. Absent —
                            // not greyed — when the session sends no microphone at all.
                            if model.micAvailable {
                                Button { model.toggleMicMute() } label: {
                                    touchDisc(model.micMuted ? "mic.slash.fill" : "mic.fill")
                                }
                                .buttonStyle(.plain)
                                .accessibilityLabel(
                                    model.micMuted ? "Unmute microphone" : "Mute microphone")
                            }
                        }
                        .padding(12)
                        .transition(.opacity)
                        .task {
                            guard statsVerbosity == .off else { return }
                            try? await Task.sleep(for: .seconds(8))
                            withAnimation(.easeOut(duration: 0.6)) { showTouchExit = false }
                        }
                    }
                }
                // The virtual controller: above the stream's touch surface, so its controls take
                // their fingers first and every other finger falls through; below the ring, whose
                // scrim owns every finger while it is up. Mounted only while shown (tenet 1).
                .overlay {
                    if captureEnabled, model.virtualPadShown, let pad = model.virtualPad {
                        VirtualPadLayer(config: OverlayConfig.parse(model.settings.overlayActions).pad,
                                        wire: pad, openRing: { [ring] at in ring.openAt(at) })
                    }
                }
                #endif
                // The quick-action ring, over the virtual controller: opened by the iOS twist or
                // disc, the pad's ring button, the remote's Back, the Mac's chord or the Vision
                // Pro's ornament. Mounted only while open — a closed overlay costs nothing.
                .overlay {
                    if captureEnabled, ring.visible {
                        RingOverlay(state: ring, cfg: ringConfig, actions: ringActions(conn))
                    }
                }
                .onChange(of: ring.committed) { _, open in
                    model.setRingOpen(open)
                    #if os(macOS)
                    // The Mac's pointer is grabbed while streaming, so the stream layer hands it
                    // back for as long as the ring is up (nothing else can click a button above
                    // the video) and takes it again on close.
                    NotificationCenter.default.post(
                        name: .punktfunkRingOpen, object: NSNumber(value: open))
                    #endif
                }
                #if !os(tvOS)
                // ⌃⌥⇧O from InputCapture: the Mac's while captured, the iPad's in both states. It
                // names its session; the Stream menu's item goes through `sessionFocus` instead.
                .onReceive(NotificationCenter.default.publisher(
                    for: .punktfunkToggleQuickActions
                )) { note in
                    guard captureEnabled, model.phase == .streaming,
                          note.object as AnyObject === conn else { return }
                    ring.toggleCentred()
                }
                #endif
            }
        }
    }

    #if os(iOS) || os(visionOS)
    /// The two-finger twist → the ring. Nil on the platforms without a twist.
    private var dialSink: ((DialEvent) -> Void)? { { [ring] event in ring.handle(event) } }
    #endif

    #if os(iOS) || os(visionOS) || os(tvOS) || os(macOS)
    /// The session's live state and commands behind each ring slot.
    private func ringActions(_ conn: PunktfunkConnection) -> RingActions {
        RingActions(
            endStream: { [weak model] in model?.disconnect() },
            disconnectLinger: { [weak model] in model?.disconnect(deliberate: false) },
            touchMode: { TouchInputMode.current(conn.settings) },
            cycleTouchMode: {
                // Passthrough is skipped toward a host that drops contacts (§5.4).
                let order = TouchInputMode.allCases.filter { $0 != .touch || conn.hostSupportsTouch }
                let i = order.firstIndex(of: TouchInputMode.current(conn.settings)) ?? 0
                TouchInputMode.sessionOverride = order[(i + 1) % order.count]
            },
            keyboard: { NotificationCenter.default.post(name: .punktfunkToggleSoftKeyboard, object: conn) },
            stats: { [model] in model.statsVerbosity },
            cycleStats: { [model] in model.cycleStats() },
            micAvailable: { [model] in model.micAvailable },
            micMuted: { [model] in model.micMuted },
            toggleMic: { [model] in model.toggleMicMute() },
            hostActions: { [model] in model.activeHost.map { HostPowerStore.shared.actions(for: $0) } ?? [] },
            invokeHost: { [model] action in
                guard let host = model.activeHost else { return }
                Task { _ = await HostPowerStore.shared.invoke(action, on: host) }
            },
            sendShortcut: { keys in
                let vks = keys.compactMap(keyVk)
                guard vks.count == keys.count, !vks.isEmpty else { return }
                for vk in vks { conn.send(.key(vk, down: true)) }
                for vk in vks.reversed() { conn.send(.key(vk, down: false)) }
            },
            padAvailable: { [model] in model.virtualPadAvailable },
            padShown: { [model] in model.virtualPadShown },
            togglePad: { [model] in model.toggleVirtualPad() },
            tapPadButton: { [model] bit in model.tapPadButton(bit) },
            pointerGranted: { conn.canSendPointer },
            padMouseTarget: { [ring] in Self.padMouseTarget(ring, conn) },
            padMouseMode: { [ring] in conn.padMouseMode(Self.padMouseTarget(ring, conn)) },
            cyclePadMouse: { [ring] in conn.cyclePadMouse(Self.padMouseTarget(ring, conn)) },
            currentMode: {
                let m = conn.currentMode()
                return (m.width, m.height, m.refreshHz)
            },
            requestMode: { w, h, hz in conn.requestMode(width: w, height: h, refreshHz: hz) },
            scrollInverted: { [model] in model.settings.invertScroll },
            toggleScrollInversion: { [model] in model.setInvertScroll(!model.settings.invertScroll) },
            streamedGame: { [model] in model.streamedGame },
            endGame: { [weak model] in model?.endStreamedGame() },
            padType: { [model] in model.padType },
            padTypeAvailable: { [model] in model.padTypeAvailable },
            cyclePadType: { [weak model] in model?.cyclePadType() })
    }
    #endif
    #if os(iOS) || os(visionOS) || os(tvOS) || os(macOS)
    /// The wire pads the controller-mouse toggle acts on: the ring's opener, else every live pad.
    private static func padMouseTarget(_ ring: RingState, _ conn: PunktfunkConnection) -> UInt16 {
        guard let pad = ring.opener else { return conn.livePads }
        return pad < 16 ? 1 << pad : 0
    }
    #endif
    #if !os(iOS) && !os(visionOS)
    private var dialSink: ((DialEvent) -> Void)? { nil }
    #endif

    #if os(iOS) || os(visionOS)
    /// One touch-control disc: an SF Symbol on a floating glass disc over the frame (26+,
    /// material fallback), sized as a comfortable tap target. `interactive`: the disc IS the tap
    /// target, so the glass reacts to press, and the hit region is matched to the visible disc so
    /// every tap triggers that press highlight.
    private func touchDisc(_ symbol: String) -> some View {
        Image(systemName: symbol)
            .font(.headline.weight(.semibold))
            .frame(width: 36, height: 36)
            .glassBackground(Circle(), interactive: true)
            .contentShape(Circle())
    }
    #endif

    // The two `shortcutHintText` strings that used to live here — one per platform, told once per
    // session by the banner above — are now `ShortcutsCatalog.groups`, which both About pages
    // render. The mic line is still conditional there for the same reason it was here: teaching a
    // shortcut for a microphone that isn't on would be a lie.

    // MARK: - Connect

    /// The connect, trust and wake flow over this window's model, stores and prompts.
    private var flow: ConnectFlow {
        ConnectFlow(
            model: model, store: store, presets: presets, discovery: discovery, waker: waker,
            autoWake: $autoWakeEnabled, approvalChoice: $approvalChoice,
            awaitingApproval: $awaitingApproval, profileAsk: $profileAsk, seatWait: $seatWait)
    }

    /// A title picked on a library shelf: dial its host, booting straight into that title — with
    /// the shelf's preset. A pinned card's shelf carries its card's preset as the one-off, so a
    /// launch made there streams with the preset the card promises; the host's own shelf carries
    /// `.inherit` and the binding decides, exactly as a plain card tap does.
    private func launchTitle(_ shelf: LibraryTarget, _ id: String) {
        launch(shelf, id, profile: .ask())
    }

    private func launch(_ shelf: LibraryTarget, _ id: String, profile: ConnectFlow.ProfileChoice) {
        libraryTarget = nil
        flow.connect(shelf.host, launchID: id, preset: shelf.preset, profile: profile)
    }

    /// A shelf's own Connect / Resume: dial its host launching NOTHING. The host is already
    /// showing whatever is up, and asking it to launch the game it is running is how a second
    /// copy starts — so this is also the only way back into a launch the host cannot track.
    ///
    /// `fromLibrary` is what makes the session remember the shelf: quitting the game (or the
    /// session) comes back here rather than to the host list.
    private func connectFromShelf(_ shelf: LibraryTarget) {
        libraryTarget = nil
        flow.connect(shelf.host, preset: shelf.preset, fromLibrary: true)
    }

    /// Pairing ceremony succeeded — pin the host and connect. The guard backstops a stale
    /// ceremony surfacing after dismissal (PairSheet also self-discards those).
    private func handlePaired(_ host: StoredHost, fingerprint: Data) {
        guard pairingTarget?.id == host.id else { return }
        store.pin(host.id, fingerprint: fingerprint)
        var pinned = host
        pinned.pinnedSHA256 = fingerprint
        flow.connect(pinned)
    }

    // MARK: - First-run + dev hooks

    /// PUNKTFUNK_AUTOCONNECT=host[:port] connects immediately (trust-on-first-use,
    /// auto-confirmed — dev only) at the saved or PUNKTFUNK_MODE=WxHxHz mode, without
    /// touching the saved host list. PUNKTFUNK_COMPOSITOR=kwin|gamescope|… overrides the
    /// compositor preference and PUNKTFUNK_REMOTE_GAMEPAD=xbox360|dualsense the virtual
    /// pad type (same names as the host env knobs). (IPv4/hostname only.)
    ///
    /// DEBUG-ONLY, and compiled out of a release build: it streams to whatever host an
    /// environment variable names with the trust prompt auto-confirmed, which is a dev lever
    /// (`swift run`, the shot harness), never something a shipped app should answer to.
    /// Open where the Start in setting says, once per process. Library shows the default host's
    /// shelf; Stream also dials its desktop, one attempt, with the shelf underneath to cancel
    /// onto. Never on foregrounding and never after a session ends — a policy that reconnected
    /// every time a stream stopped would loop on a host that keeps ending them.
    ///
    /// Silent when anything else already owns the launch: a deep link (`startApplied`), the DEBUG
    /// auto-connect or any live session (`phase`), a library already open, or a confirmation
    /// waiting for an answer.
    private func applyStartScreen() {
        guard !Self.startApplied, model.phase == .idle, libraryTarget == nil, deepLinkConfirm == nil
        else { return }
        Self.startApplied = true
        let start = StartScreen.resolve(
            startIn: startInRaw, defaultHost: defaultHostID, hosts: store.hosts)
        guard let host = start.host else { return }
        libraryTarget = LibraryTarget(host: host)
        // The connect overlay rides over home, so cancelling leaves the shelf underneath.
        if case .stream = start { flow.connect(host) }
    }

    private func autoConnectIfAsked() {
        #if DEBUG
        guard let target = ProcessInfo.processInfo.environment["PUNKTFUNK_AUTOCONNECT"],
              !target.isEmpty, model.phase == .idle
        else { return }
        // `demo`: add the demo address as Add Host does, then stream from the saved record.
        if target == "demo" {
            store.add(StoredHost(name: "", address: DemoMode.address))
            if let demo = store.hosts.first(where: DemoMode.isDemo) { flow.connect(demo) }
            return
        }
        let parts = target.split(separator: ":")
        var host = StoredHost(name: "", address: String(parts[0]))
        if parts.count == 2, let p = UInt16(parts[1]) { host.port = p }
        if let mode = ProcessInfo.processInfo.environment["PUNKTFUNK_MODE"] {
            let dims = mode.split(separator: "x").compactMap { Int($0) }
            if dims.count == 3 {
                width = dims[0]
                height = dims[1]
                hz = dims[2]
            }
        }
        // The dev levers layer over the globals (no host record, so no binding to resolve).
        var effective = EffectiveSettings(defaults: .standard)
        if let name = ProcessInfo.processInfo.environment["PUNKTFUNK_COMPOSITOR"],
           let c = PunktfunkConnection.Compositor(name: name) {
            effective.compositor = Int(c.rawValue)
        }
        var pad = GamepadManager.shared.resolveType(
            setting: PunktfunkConnection.GamepadType(
                rawValue: UInt32(clamping: effective.gamepadType)) ?? .auto)
        if let name = ProcessInfo.processInfo.environment["PUNKTFUNK_REMOTE_GAMEPAD"],
           let g = PunktfunkConnection.GamepadType(name: name) {
            // Back through resolveType so the lever is adopted as the session's setting: the
            // per-pad arrivals declare it too, which is what the host actually builds from.
            pad = GamepadManager.shared.resolveType(setting: g)
        }
        if let kbps = ProcessInfo.processInfo.environment["PUNKTFUNK_BITRATE_KBPS"],
           let v = Int(kbps) {
            effective.bitrateKbps = v
        }
        model.connect(to: host, effective: effective, gamepad: pad, autoTrust: true)
        #endif
    }
}
