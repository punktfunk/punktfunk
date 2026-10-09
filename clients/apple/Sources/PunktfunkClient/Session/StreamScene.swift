// The live stream and everything drawn over it: the stats HUD, the badges, the touch discs, the
// virtual controller and the quick-action ring. ContentView mounts one per connection under the
// trust prompt; the ring's slots reach the session through `SessionModel.ringActions`.

import PunktfunkKit
import SwiftUI

struct StreamScene: View {
    @ObservedObject var model: SessionModel
    @ObservedObject var ring: RingState
    let captureEnabled: Bool
    /// The tier the overlay shows, resolved by ContentView.
    let statsVerbosity: StatsVerbosity
    /// The stats-OFF tier's touch-exit disc window. The disc must LEAVE the hierarchy so nothing
    /// composites over the metal layer; ContentView opens the window at each session start.
    @Binding var showTouchExit: Bool
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.statsScalePct) private var statsScalePct = 100

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

    var body: some View {
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
                    // No `.id`: a tier change keeps the StreamHUDView identity, so its glass card
                    // morphs to the new tier. The `.transition` fires only on off↔on, a scale-up
                    // from the HUD's corner. The ZStack is the stable host `.animation` watches.
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
                        RingOverlay(state: ring, cfg: ringConfig, actions: model.ringActions(conn, ring: ring))
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
    #else
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
}

extension SessionModel {
    /// The session's live state and commands behind each ring slot.
    func ringActions(_ conn: PunktfunkConnection, ring: RingState) -> RingActions {
        RingActions(
            endStream: { [weak self] in self?.disconnect() },
            disconnectLinger: { [weak self] in self?.disconnect(deliberate: false) },
            touchMode: { TouchInputMode.current(conn.settings) },
            cycleTouchMode: {
                // Passthrough is skipped toward a host that drops contacts (§5.4).
                let order = TouchInputMode.allCases.filter { $0 != .touch || conn.hostSupportsTouch }
                let i = order.firstIndex(of: TouchInputMode.current(conn.settings)) ?? 0
                TouchInputMode.sessionOverride = order[(i + 1) % order.count]
            },
            keyboard: { NotificationCenter.default.post(name: .punktfunkToggleSoftKeyboard, object: conn) },
            stats: { self.statsVerbosity },
            cycleStats: { self.cycleStats() },
            micAvailable: { self.micAvailable },
            micMuted: { self.micMuted },
            toggleMic: { self.toggleMicMute() },
            hostActions: { self.activeHost.map { HostPowerStore.shared.actions(for: $0) } ?? [] },
            invokeHost: { action in
                guard let host = self.activeHost else { return }
                Task { _ = await HostPowerStore.shared.invoke(action, on: host) }
            },
            sendShortcut: { keys in
                let vks = keys.compactMap(keyVk)
                guard vks.count == keys.count, !vks.isEmpty else { return }
                for vk in vks { conn.send(.key(vk, down: true)) }
                for vk in vks.reversed() { conn.send(.key(vk, down: false)) }
            },
            padAvailable: { self.virtualPadAvailable },
            padShown: { self.virtualPadShown },
            togglePad: { self.toggleVirtualPad() },
            tapPadButton: { bit in self.tapPadButton(bit) },
            pointerGranted: { conn.canSendPointer },
            padMouseTarget: { [ring] in padMouseTarget(ring, conn) },
            padMouseMode: { [ring] in conn.padMouseMode(padMouseTarget(ring, conn)) },
            cyclePadMouse: { [ring] in conn.cyclePadMouse(padMouseTarget(ring, conn)) },
            currentMode: {
                let m = conn.currentMode()
                return (m.width, m.height, m.refreshHz)
            },
            requestMode: { w, h, hz in conn.requestMode(width: w, height: h, refreshHz: hz) },
            scrollInverted: { self.settings.invertScroll },
            toggleScrollInversion: { self.setInvertScroll(!self.settings.invertScroll) },
            streamedGame: { self.streamedGame },
            endGame: { [weak self] in self?.endStreamedGame() },
            padType: { self.padType },
            padTypeAvailable: { self.padTypeAvailable },
            cyclePadType: { [weak self] in self?.cyclePadType() })
    }
}

/// The wire pads the controller-mouse toggle acts on: the ring's opener, else every live pad.
@MainActor private func padMouseTarget(_ ring: RingState, _ conn: PunktfunkConnection) -> UInt16 {
    guard let pad = ring.opener else { return conn.livePads }
    return pad < 16 ? 1 << pad : 0
}
