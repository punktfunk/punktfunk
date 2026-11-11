// The live stream and everything drawn over it: the stats HUD, the badges, the touch discs, the
// virtual controller and the quick-action ring. ContentView mounts one per connection under the
// trust prompt; the ring's slots reach the session through `SessionModel.ringActions`.

#if os(visionOS)
import GameController
#endif
import PunktfunkKit
import SwiftUI

private let foldLog = ClientLog(category: "display")

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
    #if os(visionOS)
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow
    /// The session's picture: the presenter draws here from the first frame, flat or curved.
    @State private var picture = CurveTarget()
    #endif

    var body: some View {
        Group {
            if let conn = model.connection {
                #if os(iOS)
                // A half-open fold splits the session at its hinge. The picture keeps the upright
                // half as one container, so its fit, gestures, HUD and ring need no second layout.
                GeometryReader { geo in
                    let fold = foldRead(geo)
                    VStack(spacing: 0) {
                        picture(conn, padOverPicture: fold.split == nil)
                            .frame(height: fold.split?.video)
                        if let split = fold.split {
                            Color.black.frame(height: split.hinge)
                            flatHalf(conn)
                        }
                    }
                    .onChange(of: fold.trace, initial: true) { _, trace in
                        foldLog.info("fold \(trace, privacy: .public)")
                    }
                }
                #if DEBUG
                // A capture starts with the virtual controller up.
                .task {
                    if ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_PAD"] != nil,
                       !model.virtualPadShown { model.toggleVirtualPad() }
                }
                #endif
                #else
                picture(conn, padOverPicture: true)
                #endif
            }
        }
    }

    /// The live stream and everything drawn over it. `padOverPicture` is false while the virtual
    /// controller has the flat half of a fold to itself. On visionOS a pad press goes to the game,
    /// or to the ring through GamepadCapture, wherever the eyes rest; it never pinches the window.
    @ViewBuilder
    private func picture(_ conn: PunktfunkConnection, padOverPicture: Bool) -> some View {
        let placement = HUDPlacement(rawValue: hudPlacement) ?? .topTrailing
        StreamView(
            connection: conn,
            captureEnabled: captureEnabled,
            overlayShown: captureEnabled && statsVerbosity != .off,
            onCaptureChange: { [weak model] captured in
                model?.mouseCaptured = captured
                #if os(visionOS)
                // The window that takes the keyboard takes the controllers too.
                if captured { model?.claimControllers() }
                #endif
            },
            onDisconnectRequest: { [weak model] in model?.disconnect() },
            onShortcut: { [weak model, ring] action, phase in
                model?.runShortcut(action, phase, pad: nil, ring: ring)
            },
            onDial: dialSink,
            onFrame: { [meter = model.meter, queue = model.clientQueue] au in
                meter.note(byteCount: au.byteCount)
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
            onResizeTarget: { [weak model] w, h, view in
                model?.resizeTargeted(width: w, height: h, view: view)
            },
            // Resize overlay END — the coded dims of each new-mode IDR, reported from the
            // decode pump thread; hop to the main actor to clear the overlay.
            onDecodedSize: { [weak model] w, h in
                Task { @MainActor in model?.resizeDecoded(width: w, height: h) }
            },
            endToEndMeter: model.endToEnd
        )
        #if os(visionOS)
        // One path for the whole session: the presenter always draws the texture, which shows
        // flat here or bent on the curve. The stream view under it keeps the window's input.
        .curve(picture)
        .overlay {
            if CurveStage.shared.curved(conn) {
                ScreenPlaceholder(endStream: { [weak model] in model?.disconnect() })
            } else {
                FlatScreen(target: picture)
            }
        }
        // A streaming session follows the stored stop: a curved one moves the picture to the
        // curve, Flat brings it back here.
        .onChange(
            of: model.phase == .streaming ? ScreenCurve(name: screen) : nil, initial: true
        ) { _, wanted in
            CurveStage.shared.register(conn, model: model, ring: ring, target: picture)
            if wanted?.radius != nil {
                CurveStage.shared.enter(conn)
            } else {
                CurveStage.shared.leave(conn)
            }
        }
        // The curve has its own volume, opened and closed with it.
        .onChange(of: CurveStage.shared.curved(conn)) { _, curved in
            guard let id = CurveStage.shared.session(for: conn)?.id else { return }
            if curved {
                openWindow(id: CurveScene.sceneID, value: id)
            } else {
                dismissWindow(id: CurveScene.sceneID, value: id)
            }
        }
        .onDisappear { CurveStage.shared.ended(conn) }
        // Above the window, where the curve's volume hangs its controls too.
        .ornament(attachmentAnchor: .scene(.top), contentAlignment: .bottom) {
            StreamOrnament(quickActions: { ring.toggleCentred() })
        }
        #endif
        .overlay(alignment: placement.alignment) {
            // No `.id`: a tier change keeps the StreamHUDView identity, so its glass card
            // morphs to the new tier. The `.transition` fires only on off↔on, a scale-up
            // from the HUD's corner. The ZStack is the stable host `.animation` watches.
            ZStack {
                if captureEnabled && statsVerbosity != .off && overlaysHere(conn) {
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
            .overFlatPicture()
        }
        .overlay(alignment: .bottom) {
            StreamBadgeStack(
                model: model, captureEnabled: captureEnabled && overlaysHere(conn),
                statsVerbosity: statsVerbosity)
                .overFlatPicture()
        }
        #if os(iOS) || os(visionOS)
        // A corner disc opens the ring in compact, and for 8 s in off (an overlay above the
        // stream costs ~a refresh of latency). It takes the top corner the HUD leaves free, and
        // leaves while the virtual controller, which carries its own ring button, is up.
        .overlay(alignment: placement == .topLeading ? .topTrailing : .topLeading) {
            if captureEnabled, overlaysHere(conn), !model.virtualPadShown,
               statsVerbosity == .compact || (statsVerbosity == .off && showTouchExit) {
                HStack(spacing: 10) {
                    // End stream is a slot inside, behind a two-press arm. The overlay clamps
                    // the far x onto the trailing edge, so the ring opens at the disc.
                    Button {
                        ring.pressTick &+= 1
                        ring.openAt(CGPoint(
                            x: placement == .topLeading ? CGFloat.greatestFiniteMagnitude : 30, y: 30))
                    } label: { touchDisc("ellipsis.circle") }
                        .buttonStyle(.plain)
                        .accessibilityLabel(L10n.appleQuickActionsA11y)
                    // Off and compact carry no HUD mute button, so this disc is a touch-only
                    // user's only way to mute. Absent, not greyed, with no mic.
                    if model.micAvailable {
                        Button { model.toggleMicMute() } label: {
                            touchDisc(model.micMuted ? "mic.slash.fill" : "mic.fill")
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel(
                            model.micMuted ? L10n.appleUnmuteMicrophone : L10n.appleMuteMicrophone)
                    }
                }
                .padding(12)
                .overFlatPicture()
                .transition(.opacity)
                // Keyed on the tier: a jump from compact straight to off starts the timer too.
                .task(id: statsVerbosity) {
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
            if padOverPicture, captureEnabled, overlaysHere(conn), model.virtualPadShown,
                   let pad = model.virtualPad {
                VirtualPadLayer(config: OverlayConfig.parse(model.settings.overlayActions).pad,
                                wire: pad, openRing: { [ring] at in ring.openAt(at) })
                    .overFlatPicture()
            }
        }
        #endif
        // The quick-action ring, over the virtual controller: opened by the iOS twist or
        // disc, the pad's ring button, the remote's Back, the Mac's chord or the Vision
        // Pro's ornament. Mounted only while open — a closed overlay costs nothing.
        .overlay {
            if captureEnabled, ring.visible, overlaysHere(conn) {
                RingOverlay(state: ring, cfg: model.ringConfig, actions: model.ringActions(conn, ring: ring))
                    .overFlatPicture()
            }
        }
        .onChange(of: ring.committed) { _, open in
            model.setRingOpen(open)
            #if os(macOS) || os(iOS)
            // The Mac's pointer is grabbed while streaming and an iPad's mouse is locked,
            // so the stream layer hands it back for as long as the ring is up (nothing
            // else can click a button above the video) and takes it again on close.
            NotificationCenter.default.post(
                name: .punktfunkRingOpen, object: NSNumber(value: open))
            #endif
        }
        #if os(visionOS)
        .handlesGameControllerEvents(matching: .gamepad)
        #endif
    }

    #if os(iOS)
    /// The flat half of a half-open fold: the virtual controller while it is up, else a
    /// trackpad. The ring opens centred on the picture, never down here.
    @ViewBuilder
    private func flatHalf(_ conn: PunktfunkConnection) -> some View {
        ZStack {
            Color.black
            if captureEnabled, model.virtualPadShown, let pad = model.virtualPad {
                VirtualPadLayer(config: OverlayConfig.parse(model.settings.overlayActions).pad,
                                wire: pad, openRing: { [ring] _ in ring.toggleCentred() })
            } else if captureEnabled, conn.canSendPointer {
                TrackpadSurface(connection: conn, enabled: captureEnabled, onDial: dialSink)
                    .background(
                        Color.white.opacity(0.06),
                        in: RoundedRectangle(cornerRadius: 28, style: .continuous))
                    .overlay {
                        Label("Trackpad", systemImage: "hand.point.up.left")
                            .font(.geist(13, relativeTo: .footnote))
                            .foregroundStyle(.white.opacity(0.45))
                            .allowsHitTesting(false)
                    }
                    .padding(12)
            }
        }
    }

    /// The tabletop split for this container, read off the system's fold region: nil on a flat,
    /// shut or book-held device, and before iOS 27.1. `trace` is the log's line for it.
    private func foldRead(_ geo: GeometryProxy) -> (split: FoldSplit?, trace: String) {
        #if DEBUG
        // A capture stands a fold up without a hinge: the fraction of the height it sits at.
        if let at = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_FOLD"].flatMap(Double.init) {
            let hinge = CGRect(x: 0, y: geo.size.height * at - 20, width: geo.size.width, height: 40)
            return (FoldSplit(hinge: hinge, in: geo.size), "capture at \(at)")
        }
        #endif
        guard #available(iOS 27.1, *) else { return (nil, "unsupported") }
        let folds = geo.reservedRegions(kind: .division, options: [.includeInactive])
        let split = folds.first { $0.isActive }
            .flatMap { FoldSplit(hinge: $0.frame, in: geo.size) }
        let seen = folds.map { "active=\($0.isActive) frame=\($0.frame)" }
            .joined(separator: " | ")
        let cut = split.map { "\($0.video)+\($0.hinge)" } ?? "none"
        return (split, "\(seen.isEmpty ? "none" : seen) split=\(cut)")
    }
    #endif

    #if os(iOS) || os(visionOS)
    /// The two-finger twist → the ring. Nil on the platforms without a twist.
    private var dialSink: ((DialEvent) -> Void)? { { [ring] event in ring.handle(event) } }
    #else
    private var dialSink: ((DialEvent) -> Void)? { nil }
    #endif

    /// The picture shows in this window, so its overlays draw here; on a curve they float
    /// there instead.
    private func overlaysHere(_ conn: PunktfunkConnection) -> Bool {
        #if os(visionOS)
        return !CurveStage.shared.curved(conn)
        #else
        return true
        #endif
    }

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
    /// The ring this platform draws. macOS takes the DESKTOP default (no soft keyboard, no
    /// on-screen pad), tvOS the TV one (no touch, keyboard or microphone slot), iOS the touch
    /// one; a configured blob overrides each.
    var ringConfig: OverlayConfig {
        #if os(macOS)
        OverlayConfig.parse(settings.overlayActions, platform: .desktop)
        #elseif os(tvOS)
        OverlayConfig.parse(settings.overlayActions, platform: .tv)
        #else
        OverlayConfig.parse(settings.overlayActions)
        #endif
    }

    /// A shortcut fired (design/client-shortcuts.md §4) on wire pad `pad`, `nil` for the keyboard
    /// or a captured Steam Controller. It runs as the dial's slot would, at once: only the host's
    /// danger actions and End game ask for a second press. A held key-like action stays down on
    /// the host until its `.up`.
    func runShortcut(_ action: Shortcuts.Action, _ phase: Shortcuts.Phase, pad: UInt32?, ring: RingState) {
        guard let conn = connection, self.phase == .streaming else { return }
        guard case .slot(let slot) = action else {
            if action == .ring { pad == nil ? ring.toggleCentred() : ring.chordToggle(pad: pad) }
            return
        }
        let a = ringActions(conn, ring: ring)
        let cfg = ringConfig
        if phase == .down || phase == .up {
            switch slot {
            case .guide: a.holdPadButton(GamepadWire.guide, phase == .down, pad)
            case .qam: a.holdPadButton(GamepadWire.misc1, phase == .down, pad)
            case .shortcut(let id): a.holdKeys(cfg.shortcut(id)?.keys ?? [], phase == .down)
            default: break
            }
            return
        }
        let s = spec(slot, cfg, a)
        guard s.enabled else { return }
        if s.armed, slot != .endStream, !confirmShortcut(s.id, prompt: confirmHint(s)) { return }
        perform(slot, cfg, a, ring, pad: pad)
    }

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
            tapPadButton: { bit, pad in self.tapPadButton(bit, pad: pad) },
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
            cyclePadType: { [weak self] in self?.cyclePadType() },
            toggleCapture: { streamSlot(.releaseInput, conn) },
            toggleMouseMode: { streamSlot(.mouseMode, conn) },
            toggleFullscreen: {
                NotificationCenter.default.post(name: .punktfunkToggleFullscreen, object: nil)
            },
            minimize: { streamSlot(.minimize, conn) },
            clipboardAvailable: { conn.hostSupportsClipboard && conn.canUseClipboard },
            toggleClipboard: { [weak self] in self?.toggleClipboardSync() },
            holdKeys: { keys, down in
                let vks = keys.compactMap(keyVk)
                guard vks.count == keys.count else { return }
                for vk in down ? vks : vks.reversed() { conn.send(.key(vk, down: down)) }
            },
            holdPadButton: { [weak self] bit, down, pad in self?.holdPadButton(bit, down: down, pad: pad) })
    }
}

extension View {
    /// Just in front of a Vision Pro window's flat picture, which RealityKit draws a millimetre out
    /// from the window: 2D at the window's own depth would sit behind it. Elsewhere unchanged.
    @ViewBuilder func overFlatPicture() -> some View {
        #if os(visionOS)
        offset(z: FlatScreen.overlayLift)
        #else
        self
        #endif
    }
}

/// Hand a slot only the stream view runs to this session's view.
@MainActor private func streamSlot(_ slot: SlotId, _ conn: PunktfunkConnection) {
    NotificationCenter.default.post(name: .punktfunkStreamSlot, object: conn, userInfo: ["slot": slot.id])
}

/// The wire pads the controller-mouse toggle acts on: the ring's opener, else every live pad.
@MainActor private func padMouseTarget(_ ring: RingState, _ conn: PunktfunkConnection) -> UInt16 {
    guard let pad = ring.opener else { return conn.livePads }
    return pad < 16 ? 1 << pad : 0
}
