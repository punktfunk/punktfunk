// Per-session presenter stack shared by the Apple stream views.
//
// Explicit VideoToolbox decode is the default. iOS presents through CAMetalDisplayLink, tvOS
// sends decoded IOSurfaces to its video plane, and macOS presents a CAMetalLayer on arrival.
// The latency intent uses each platform's shortest path; smoothness retains an explicit FIFO.
// Stage-1's compressed AVSampleBufferDisplayLayer remains a diagnostic/fallback.
//
// Views own capture, geometry, and ordinary display links; this type owns presenter lifecycle.
// Start, layout, stop, and ordinary display-link ticks run on the main thread.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import PunktfunkShared
import QuartzCore
#if os(tvOS) || os(iOS)
import UIKit
#endif

private let presentLog = ClientLog(category: "present")

/// Weak-target wrapper for CADisplayLink. The link retains its target, so targeting a view or
/// presenter directly makes a `owner → link → owner` cycle that only `invalidate()` breaks — if a
/// teardown is ever missed the owner leaks and keeps ticking. The proxy is what the link retains;
/// the handler closure captures the owner `[weak]`, so the owner can deallocate and its `deinit`
/// invalidate the link.
public final class DisplayLinkProxy: NSObject {
    private let onTick: (CADisplayLink) -> Void
    public init(_ onTick: @escaping (CADisplayLink) -> Void) { self.onTick = onTick }
    @objc public func tick(_ link: CADisplayLink) { onTick(link) }
}

/// What a wedge report gets (the pipeline's own relink came first, `LinkStallPolicy`). When
/// tvOS re-plumbs the display, the layer on screen never shows a frame again, and a fresh one
/// shows only once the display finishes, about five seconds on: so one fresh layer, then hold
/// with the link armed. A rebuild during that wait starts it over. A clean run longer than
/// `resetAfter` earns the next wedge a fresh layer again. Internal for tests.
struct WedgePolicy {
    enum Action { case rebuild, hold }
    let resetAfter: CFTimeInterval
    private(set) var wedges = 0
    private var last: CFTimeInterval = -.infinity
    init(resetAfter: CFTimeInterval = 30) { self.resetAfter = resetAfter }
    mutating func next(now: CFTimeInterval) -> Action {
        if now - last > resetAfter { wedges = 0 }
        last = now
        wedges += 1
        return wedges == 1 ? .rebuild : .hold
    }
}

/// Selects a session's presentation mechanism.
///
/// Stage-2/3/4 share explicit decode and the Metal renderer, using arrival, on-glass-gated, or
/// CAMetalDisplayLink pacing. `decoded` keeps explicit decode but sends its IOSurface directly to
/// the tvOS video renderer. Stage-1 sends compressed samples to that renderer and remains a debug
/// diagnostic because its internal decoder cannot participate in recovery or decode metering.
///
/// Internal rather than private for resolution tests.
enum PresenterChoice: Equatable {
    case stage1
    case stage2
    case stage3
    case stage4
    case decoded

    /// Resolve from the `PUNKTFUNK_PRESENTER` env override (A/B without touching settings) first,
    /// then the persisted `DefaultsKey.presenter` setting; anything unknown (or an empty env var)
    /// falls back to the platform default. `allowStage1` is false in release builds, where a
    /// leftover DEBUG "stage1" value silently maps to the default rather than reviving the
    /// freeze-prone fallback.
    static func resolve(setting: String?, env: String?, allowStage1: Bool) -> PresenterChoice {
        explicit(setting: setting, env: env, allowStage1: allowStage1) ?? platformDefault
    }

    /// Resolve an explicit diagnostic override, or nil for no supported selection.
    ///
    /// Stage-2 remains a faithful arrival-pacing A/B. Stage-4 is limited to iOS/tvOS because the
    /// macOS path has separate synchronization constraints. `decoded` (the video-plane A/B) needs
    /// iOS/tvOS 17.4 for its displayed-IOSurface metering. Stage-1 additionally requires the
    /// caller's release-build gate.
    static func explicit(setting: String?, env: String?, allowStage1: Bool) -> PresenterChoice? {
        let raw = env.flatMap { $0.isEmpty ? nil : $0 } ?? setting
        switch raw {
        case "stage1": return allowStage1 ? .stage1 : nil
        case "stage2": return .stage2
        case "stage3": return .stage3
        case "stage4":
            #if os(macOS)
            return nil
            #else
            return .stage4
            #endif
        case "decoded":
            #if os(tvOS) || os(iOS)
            if #available(iOS 17.4, tvOS 17.4, *) { return .decoded }
            return nil
            #else
            return nil
            #endif
        default: return nil
        }
    }

    /// iOS and tvOS use deadline-paced Metal, macOS arrival-paced Metal.
    ///
    /// The deadline link presents each drawable as late as its deadline allows
    /// (`DeadlinePacer`). Filmed at 240 fps on a 60 Hz Apple TV, a late pick answered controller
    /// presses 9 ms sooner than the decoded video plane, whose displayed-surface metric reads
    /// about a refresh early. `decoded` stays an explicit A/B on tvOS 17.4+.
    static var platformDefault: PresenterChoice {
        #if os(macOS)
        return .stage2
        #else
        return .stage4
        #endif
    }
}

/// The user's presentation INTENT — what replaced the visible stage picker in the 2026-07
/// rebuild (design/apple-presentation-rebuild.md). Two intents, one engine per platform:
///
/// - `.latency` (the default): no deliberate app buffer. Metal uses a newest-wins slot; tvOS's
///   decoded path submits directly and drops on renderer backpressure. Jitter becomes a repeat or
///   drop rather than standing latency.
/// - `.smooth(buffer:)`: a small deliberate jitter buffer (`FrameStore.fifo`) evens the present
///   cadence at the cost of `buffer` refresh intervals of added display latency — which the HUD
///   SHOWS (only the OS floor is shaved, never the user's chosen buffer). `buffer` ∈ 1…3;
///   the "Automatic" setting (stored 0) currently maps to 2.
///
/// Mechanism stays internal: intents map onto `PresentPacing`/`FrameStore.Policy` per platform
/// in `SessionPresenter.start`; the stage ladder survives only as the PUNKTFUNK_PRESENTER debug
/// env lever. Internal (not private) for unit tests.
enum PresentPriority: Equatable {
    case latency
    case smooth(buffer: Int)

    /// Resolve from the persisted settings: `DefaultsKey.presentPriority` ("latency" default;
    /// anything but "smooth" — unset, garbage, a synced unknown future value — falls back to
    /// latency) and `DefaultsKey.smoothBuffer` (0/out-of-range = Automatic = 2).
    static func resolve(setting: String?, bufferSetting: Int?) -> PresentPriority {
        guard setting == "smooth" else { return .latency }
        let raw = bufferSetting ?? 0
        return .smooth(buffer: (1...3).contains(raw) ? raw : 2)
    }

    /// The frame hand-off policy this intent runs (see `FrameStore`).
    var storePolicy: FrameStorePolicy {
        switch self {
        case .latency: return .newestWins
        case .smooth(let buffer): return .fifo(capacity: buffer)
        }
    }
}

@MainActor
final class SessionPresenter {
    /// Map a presenter choice and codec to its execution path.
    ///
    /// The decoded tvOS path requires a CVPixelBuffer, so PyroWave retains deadline-paced Metal.
    /// Stage-3 and stage-4 map directly to glass and deadline pacing; stage-2 is arrival for every
    /// codec. Glass gating waits for the previous frame's on-glass callback, about a refresh per
    /// frame.
    nonisolated static func pacing(for choice: PresenterChoice, codec: VideoCodec) -> PresentPacing {
        if choice == .decoded { return codec == .pyrowave ? .deadline : .decoded }
        if choice == .stage4 { return .deadline }
        if choice == .stage3 { return .glass }
        return .arrival
    }

    /// The decoded sink is zero-buffer and validated for biplanar 4:2:0. Preserve deadline Metal
    /// for Smoothness and 4:4:4 rather than ignoring intent or risking an unsupported video plane.
    nonisolated static func effectivePacing(
        _ selected: PresentPacing, priority: PresentPriority, videoLayerCompatible: Bool = true
    ) -> PresentPacing {
        if selected == .decoded, priority != .latency || !videoLayerCompatible { return .deadline }
        return selected
    }

    /// The glass gate's in-flight present budget (`PresentGate` capacity): 1 everywhere.
    ///
    /// Depth 1 is the only depth that works. The 2026-07 depth-2 experiment (one flip scanning
    /// out + one queued, predicted ~5–8 ms at 120 Hz) REGRESSED the iPad Pro's display stage to
    /// 22–28 ms vs depth 1's 14: any second gate slot becomes a STANDING queue — a burst fills
    /// it, and with presents and latches then running at the same rate the occupancy never
    /// returns to zero, so every frame permanently rides one extra refresh per slot. A bounded
    /// FIFO can cap the queue but nothing ever drains it. Low latency needs a path that cannot
    /// build that queue—deadline pacing or tvOS's decoded video plane—not a deeper gate.
    ///
    #if os(macOS)
    /// Adaptive-refresh latency sessions choose immediate sparse presents or one dense present per
    /// display-link target: on arrival, a 120 fps stream on a ProMotion panel loses about a tenth
    /// to a fifth of its frames, PyroWave included. Smoothness and the other presenters keep their
    /// own pacing mechanisms.
    nonisolated static func adaptiveSlotPaced(
        adaptiveSync: Bool, priority: PresentPriority, pacing: PresentPacing
    ) -> Bool {
        adaptiveSync && priority == .latency && pacing == .arrival
    }
    #endif

    /// `PUNKTFUNK_GATE_DEPTH` (1…3) still overrides on iOS/tvOS so the standing-queue ladder
    /// stays reproducible on-device; macOS is pinned to 1, env ignored — a deeper gate only builds
    /// a standing queue (see above). Internal (not private) for unit tests.
    nonisolated static func gateDepth(env: String?) -> Int {
        #if os(macOS)
        return 1
        #else
        if let env, let depth = Int(env), (1...3).contains(depth) { return depth }
        return 1
        #endif
    }

    /// The display-link frame-rate hint for a stream of `hz` (see `syncFrameRate`). iOS/tvOS
    /// keep a 120 ceiling (mandatory, or ProMotion caps the link at 60) with a 24/30 Hz VRR
    /// floor. macOS spans 24…`hz` with VRR on and fixes min = max at `hz` with it off.
    /// Internal (not private) for unit tests.
    nonisolated static func frameRateRange(hz: Float, allowVRR: Bool) -> CAFrameRateRange {
        #if os(macOS)
        let minimum = allowVRR ? min(hz, 24) : hz
        return CAFrameRateRange(minimum: minimum, maximum: hz, preferred: hz)
        #else
        let floor = allowVRR ? min(hz, 24) : min(hz, 30)
        return CAFrameRateRange(minimum: floor, maximum: max(hz, 120), preferred: hz)
        #endif
    }

    #if os(visionOS)
    /// The curved screen while a volume shows the picture.
    private(set) var curve: CurveTarget?

    /// Draw into `target`, or back into the layer with nil. Costs one IDR. Main thread.
    func setCurve(_ target: CurveTarget?) {
        guard target !== curve else { return }
        curve = target
        restartPresentation()
    }
    #endif

    private var pump: StreamPump?
    private var stage2: Stage2Pipeline?
    private var stage2Link: CADisplayLink?
    /// iOS: runs `pushHeadroom` twice a second. Headroom follows brightness and posts no change
    /// notification; the linear HDR shaders roll highlights onto it.
    private var headroomTimer: Timer?
    #if os(iOS)
    /// Stage the EDR headroom of the screen the app is on, current and potential.
    private static func pushHeadroom(to pipeline: Stage2Pipeline?) {
        let screen = NativeDisplay.screen
        pipeline?.setDisplayHeadroom(
            screen.currentEDRHeadroom, potential: screen.potentialEDRHeadroom)
    }
    #endif
    private var panel = PanelInfo(minHz: 0, maxHz: 0)

    /// The hosting screen's refresh range and granularity, from the view on start and every
    /// layout (a window can move screens). Main thread. Diagnostics only.
    func setPanel(_ info: PanelInfo) {
        panel = info
        stage2?.setPanel(info)
    }
    private var metalLayer: CAMetalLayer?
    private var connection: PunktfunkConnection?
    /// Re-runs this session's `start` with its own arguments on the given base layer — the
    /// wedged-presenter cure (`rebuildPresentation`) and the move between screens (`move(to:)`).
    /// Set by `start`, cleared by `stop`. Main-thread only.
    private var restart: ((AVSampleBufferDisplayLayer) -> Void)?
    /// The base layer the running session presents into. Main-thread only.
    private var baseLayer: AVSampleBufferDisplayLayer?
    /// The last `layout` call, replayed after a rebuild so the new sublayer has a frame
    /// before its first present. Main-thread only.
    private var lastLayout: (bounds: CGRect, contentsScale: CGFloat)?
    /// The decoded frame's REAL pixel dimensions (ground truth, pushed by the view from the pump's
    /// `onDecodedSize` new-mode-IDR callback). Used for the aspect-fit in `layout` in preference to
    /// `connection.currentMode()`, which (a) lags a mid-stream resize — it only updates on the
    /// `Reconfigured` ack, and a resize-END produces no bounds change to re-run `layout` afterward —
    /// and (b) can disagree with what the host actually DELIVERED (Windows corrective-ack falls back
    /// to an advertised mode). The pixels we're drawing are the only correct aspect source; a wrong
    /// one here is the "black bars + stretched" resize artifact. nil until the first frame → `layout`
    /// falls back to `currentMode()`. Main-thread only.
    private var contentSize: CGSize?
    /// The screen verdict the running pipeline chose its pacing from, and its live source.
    private var builtAdaptive = false
    /// The window's Direct verdict (`setDirectPossible`), re-applied to each pipeline built.
    private var directPossible = true
    private var adaptiveSync: () -> Bool = { false }
    /// The window, scene and display around the layer, for the wedge line. Main thread.
    var surfaceState: (() -> String)?
    private var wedgePolicy = WedgePolicy()

    /// Start the resolved presenter for `connection`.
    ///
    /// Stage-1 sends compressed samples to `baseLayer`; tvOS's decoded path sends it VideoToolbox
    /// output. Metal paths leave that layer idle and overlay their own CAMetalLayer. The supplied
    /// display-link factory tracks the hosting display for ordinary pacing and decoded-frame
    /// metering; deadline pacing owns a CAMetalDisplayLink instead.
    ///
    /// Call `layout(in:contentsScale:)` after start so any Metal sublayer has valid geometry.
    /// `adaptiveSync` reads the hosting screen's fixed-vs-adaptive verdict on macOS, once per build.
    /// `screen` past 0 presents that further screen of the session, on Metal only.
    func start(
        connection: PunktfunkConnection,
        baseLayer: AVSampleBufferDisplayLayer,
        endToEndMeter: LatencyMeter?,
        makeDisplayLink: @escaping (AnyObject, Selector) -> CADisplayLink,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil,
        onFrameHDR: (@Sendable (Bool) -> Void)? = nil,
        adaptiveSync: @escaping () -> Bool = { false },
        screen: UInt8 = 0
    ) {
        stop()
        self.connection = connection
        self.baseLayer = baseLayer
        self.adaptiveSync = adaptiveSync
        builtAdaptive = adaptiveSync()
        restart = { [weak self] layer in
            guard let self else { return }
            // The Pencil reports proximity on its edges only, so the new pipeline is told here.
            let boost = interactionBoost
            start(
                connection: connection, baseLayer: layer, endToEndMeter: endToEndMeter,
                makeDisplayLink: makeDisplayLink,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize,
                onFrameHDR: onFrameHDR, adaptiveSync: adaptiveSync, screen: screen)
            setInteractionBoost(boost)
        }

        // Explicit decode stays default so loss recovery and decode metering survive. Presentation
        // is deadline Metal on iOS, the decoded video plane on tvOS, and arrival Metal on macOS.
        // PUNKTFUNK_PRESENTER keeps comparison paths reachable; the old persisted picker is ignored.
        // The user-facing latency/smoothness intent selects zero buffering or an explicit FIFO.
        // Stage-1 remains the automatic Metal-missing fallback and an environment-only release A/B.
        #if DEBUG
        let allowStage1 = true
        #else
        // Release accepts stage-1 only from an explicit process environment. A stale persisted
        // choice cannot revive its internal decoder, while device builds retain the video-plane A/B.
        let allowStage1 =
            ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENTER"] == "stage1"
        #endif
        let explicit = PresenterChoice.explicit(
            setting: nil, // the legacy DefaultsKey.presenter picker value is no longer read
            env: ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENTER"],
            allowStage1: allowStage1)
        let choice = explicit ?? PresenterChoice.platformDefault
        let selectedPacing = Self.pacing(for: choice, codec: connection.videoCodec)
        let priority = PresentPriority.resolve(
            setting: connection.settings.presentPriority,
            bufferSetting: connection.settings.smoothBuffer)
        // Direct video presentation is the zero-buffer latency path. A user who asks for a
        // smoothness buffer keeps the existing deadline engine, where FrameStore owns that buffer.
        let effective = Self.effectivePacing(
            selectedPacing, priority: priority, videoLayerCompatible: !connection.isChroma444)
        #if os(visionOS)
        // The curve's queue vends its own drawables: frames draw on arrival.
        let pacing = curve != nil ? PresentPacing.arrival : effective
        #else
        let pacing = effective
        #endif
        #if os(macOS)
        let vsyncPaced = priority != .latency && pacing == .arrival
        let adaptiveSlotPaced = Self.adaptiveSlotPaced(
            adaptiveSync: builtAdaptive, priority: priority, pacing: pacing)
        #else
        let vsyncPaced = false
        let adaptiveSlotPaced = false
        #endif
        if choice != .stage1,
           let pipeline = Stage2Pipeline(
               endToEndMeter: endToEndMeter,
               videoRenderer: pacing == .decoded ? baseLayer.sampleBufferRenderer : nil,
               pacing: pacing,
               gateDepth: Self.gateDepth(
                   env: ProcessInfo.processInfo.environment["PUNKTFUNK_GATE_DEPTH"]),
               storePolicy: priority.storePolicy,
               vsyncPaced: vsyncPaced,
               adaptiveSlotPaced: adaptiveSlotPaced) {
            pipeline.onPresentWedged = { [weak self] in
                DispatchQueue.main.async { self?.rebuildPresentation() }
            }
            let metal = pipeline.layer
            // Metal pacing overlays the idle video layer. The decoded path leaves the backing
            // video layer exposed and submits VideoToolbox output directly to its renderer.
            if pacing != .decoded {
                baseLayer.addSublayer(metal)
                metalLayer = metal
            }
            stage2 = pipeline
            pipeline.setDirectPossible(directPossible)
            #if os(visionOS)
            pipeline.setCurve(curve)
            #endif
            // The ordinary link supplies the vsync grid, retries transient Metal drawable misses,
            // and polls which decoded IOSurface reached glass. Frame arrival remains the render
            // trigger. Deadline pacing owns its drawable-vending CAMetalDisplayLink instead.
            if pacing != .deadline {
                let proxy = DisplayLinkProxy { [weak self] link in
                    self?.stage2?.renderTick(
                        targetMediaTime: link.targetTimestamp,
                        displayedMediaTime: link.timestamp,
                        period: link.targetTimestamp - link.timestamp)
                }
                let link = makeDisplayLink(proxy, #selector(DisplayLinkProxy.tick(_:)))
                link.add(to: .main, forMode: .common)
                stage2Link = link
            }
            syncFrameRate(hz: connection.currentMode().refreshHz)
            #if os(iOS)
            Self.pushHeadroom(to: pipeline)
            let timer = Timer(timeInterval: 0.5, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated { Self.pushHeadroom(to: self?.stage2) }
            }
            RunLoop.main.add(timer, forMode: .common)
            headroomTimer = timer
            #endif
            pipeline.setPanel(panel)
            pipeline.start(
                connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd,
                onDecodedSize: onDecodedSize, onFrameHDR: onFrameHDR, screen: screen)
        } else if screen == 0 {
            let pump = StreamPump()
            pump.start(
                connection: connection, layer: baseLayer, endToEndMeter: endToEndMeter,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize)
            self.pump = pump
        }
    }

    /// Pen-proximity panel-rate boost pass-through (Stage2Pipeline.setInteractionBoost):
    /// deadline pacing only — under arrival/glass the staged hint feeds no link, so this
    /// is a no-op there. Kept here so a rebuilt pipeline starts boosted. MAIN thread.
    func setInteractionBoost(_ on: Bool) {
        interactionBoost = on
        stage2?.setInteractionBoost(on)
    }

    private var interactionBoost = false

    /// Hint the display link with the stream's cadence. On iOS/tvOS a range is always required:
    /// without one, ProMotion devices cap CADisplayLink at 60 Hz (iPhones additionally need
    /// `CADisableMinimumFrameDurationOnPhone` in Info.plist), so a 120 fps stream would present
    /// at half rate with the ring silently dropping every other frame. `maximum` allows up to 120
    /// so the system MAY tick faster than a sub-120 stream (each extra tick is a near-free empty
    /// `renderTick`, and presenting on a denser grid shortens the decode→glass wait).
    ///
    /// The `allowVRR` setting (default on) widens that hint into a true variable-refresh request:
    /// `preferred` = the stream rate with a low floor, so a ProMotion / adaptive-sync display can
    /// drop its physical refresh to match the content. VRR off falls back to a fixed floor:
    /// iOS keeps 30 Hz; macOS pins the link at the stream rate (see `frameRateRange`).
    /// Re-applied from `layout` so a mid-session `Reconfigure` picks up a new refresh.
    private func syncFrameRate(hz: UInt32) {
        guard hz > 0 else { return }
        // Deadline pacing: the hint goes to the pipeline's CAMetalDisplayLink instead (staged;
        // applied from the link's own thread — see Stage2Pipeline.setFrameRateHint). A no-op
        // under arrival/glass pacing, where the CADisplayLink below is the one hinted link.
        stage2?.setFrameRateHint(hz: Float(hz))
        guard let link = stage2Link else { return }
        let range = Self.frameRateRange(
            hz: Float(hz), allowVRR: connection?.settings.allowVRR ?? true)
        if link.preferredFrameRateRange != range { link.preferredFrameRateRange = range }
        #if os(macOS)
        // The requested range and its readback on every sync (layout, fullscreen and screen
        // transitions re-apply it) — diagnostics only, the readback is the API's one clamp signal.
        if presentDebug {
            let actual = link.preferredFrameRateRange
            print(String(
                format: "pf-present-range requested=%.0f-%.0f@%.0f actual=%.0f-%.0f@%.0f",
                range.minimum, range.maximum, range.preferred ?? 0,
                actual.minimum, actual.maximum, actual.preferred ?? 0))
            fflush(stdout)
        }
        #endif
    }

    /// Refresh display timing and position a Metal presentation layer.
    ///
    /// Every explicit-decode path updates its display link after reconfiguration, including tvOS's
    /// decoded video plane. Metal paths then place their sublayer and size its drawable in backing
    /// pixels so the shader owns scaling. The video layer uses `videoGravity` (see
    /// `SessionPresenter.gravity(_:)`) and needs no geometry here.
    ///
    /// No-op before a connection starts and for stage-1 beyond the harmless timing update.
    /// The system scaler's closest match to `fit`, for the AVSampleBufferDisplayLayer paths
    /// (stage 1, tvOS's decoded plane). It does not snap like the placement does.
    nonisolated static func gravity(_ fit: VideoFit) -> AVLayerVideoGravity {
        switch fit {
        case .fit: return .resizeAspect
        case .crop: return .resizeAspectFill
        case .stretch: return .resize
        }
    }

    func layout(in bounds: CGRect, contentsScale: CGFloat) {
        lastLayout = (bounds, contentsScale)
        guard let connection else { return }
        let mode = connection.currentMode()
        syncFrameRate(hz: mode.refreshHz)
        guard let metalLayer else { return }
        // Frame size: the ACTUAL decoded dims when known (survives a lagging `currentMode()` and a
        // host that delivered a different size than requested), else the negotiated mode. The shader
        // stretches its source rect across the WHOLE drawable, so a stale size here is the
        // post-resize black-bars+stretch.
        let aspect: CGSize? = {
            if let c = contentSize, c.width > 0, c.height > 0 { return c }
            if mode.width > 0, mode.height > 0 {
                return CGSize(width: Int(mode.width), height: Int(mode.height))
            }
            return nil
        }()
        // Place the picture in BACKING PIXELS (`VideoPlacement`, the rule every client shares): a
        // metal sublayer whose frame doesn't land on whole device pixels is RESAMPLED by the
        // compositor — a uniform soft blur even when the drawable itself is 1:1 — so the layer takes
        // the placement's whole-pixel rect, and the drawable shows its `src` part of the frame.
        let scale = contentsScale > 0 ? contentsScale : 1
        let viewPx = (width: Int((bounds.width * scale).rounded()), height: Int((bounds.height * scale).rounded()))
        let placement = aspect.map {
            VideoFit(name: connection.settings.videoFit)
                .place(view: viewPx, frame: (Int($0.width), Int($0.height)))
        }
        let snapped: CGRect
        if let p = placement, !p.isEmpty, let frame = aspect {
            #if os(macOS)
            // AppKit layer geometry is bottom-left; the placement's rows count from the top.
            let yPx = viewPx.height - p.dstY - p.dstHeight
            #else
            let yPx = p.dstY
            #endif
            snapped = CGRect(
                x: bounds.minX + CGFloat(p.dstX) / scale, y: bounds.minY + CGFloat(yPx) / scale,
                width: CGFloat(p.dstWidth) / scale, height: CGFloat(p.dstHeight) / scale)
            stage2?.setSourceRect(CGRect(
                x: p.srcX / frame.width, y: p.srcY / frame.height,
                width: p.srcWidth / frame.width, height: p.srcHeight / frame.height))
        } else {
            snapped = CGRect(
                x: (bounds.origin.x * scale).rounded() / scale,
                y: (bounds.origin.y * scale).rounded() / scale,
                width: CGFloat(viewPx.width) / scale, height: CGFloat(viewPx.height) / scale)
            stage2?.setSourceRect(CGRect(x: 0, y: 0, width: 1, height: 1))
        }
        // No implicit resize animation; contentsScale tracks the view's backing/display scale.
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        metalLayer.contentsScale = contentsScale
        metalLayer.frame = snapped
        CATransaction.commit()
        // Hand the resulting pixel size to the render thread (it must not read layer geometry
        // cross-thread) — this is what the presenter sizes its drawable to. Uses the SNAPPED size so
        // the drawable's texel count equals the on-screen device-pixel count exactly (1 texel ↔ 1
        // device pixel); with the frame snapped, this equals the pre-snap rounded value, so the
        // decoded↔drawable 1:1 the log confirmed is preserved.
        stage2?.setDrawableTarget(CGSize(
            width: (snapped.width * scale).rounded(),
            height: (snapped.height * scale).rounded()))
        #if os(tvOS)
        // Push the display's live EDR headroom alongside: > 1 means the TV is composited in an
        // HDR mode (the session's AVDisplayManager request landed — see StreamViewIOS), and HDR
        // frames flip to PQ passthrough. The stream view also re-layouts on mode-switch/screen-
        // mode notifications, so a mid-session switch reaches here without a bounds change.
        stage2?.setDisplayHeadroom(
            UIScreen.main.currentEDRHeadroom, potential: UIScreen.main.potentialEDRHeadroom)
        #endif
    }

    #if os(tvOS)
    /// The HDMI output switched into (or out of) the session's HDR mode: HDR frames present as
    /// PQ passthrough while it holds. Main thread; the stream view watches the switch.
    func setHDROutput(_ on: Bool) {
        stage2?.setHDROutput(on)
    }
    #endif

    /// Record the decoded frame's real dimensions (the view hops the pump's `onDecodedSize` to main
    /// and calls this) so `layout` aspect-fits to what's actually on screen instead of the possibly-
    /// stale `currentMode()`. Only stores — the caller re-runs `layout` right after, because a
    /// resize-END produces no bounds change to trigger one. No-op on a zero/unchanged size.
    func setContentSize(_ size: CGSize) {
        guard size.width > 0, size.height > 0, size != contentSize else { return }
        contentSize = size
    }

    /// Rebuild the presentation on `layer`, keeping the connection: the picture moving between
    /// the phone and an attached monitor. The fresh display link binds to the screen `layer` is
    /// on. Costs one IDR; the caller re-runs `layout` for the new geometry. No-op before a session
    /// starts or when already on `layer`. Main thread.
    func move(to layer: AVSampleBufferDisplayLayer) {
        guard let restart, layer !== baseLayer else { return }
        let size = contentSize
        restart(layer)
        contentSize = size // the view drops the new pipeline's repeat of this size
    }

    /// The window moved to another screen. Pacing is chosen at build time, so rebuild (one IDR)
    /// only when the fixed-vs-adaptive verdict flipped. Main thread.
    func screenChanged() {
        guard restart != nil, adaptiveSync() != builtAdaptive else { return }
        restartPresentation()
    }

    /// Whether the window can scan out Direct — fullscreen, nothing of the app's own over the
    /// video. Kept for each pipeline this presenter builds. Main thread.
    func setDirectPossible(_ possible: Bool) {
        directPossible = possible
        stage2?.setDirectPossible(possible)
    }

    /// `onPresentWedged`'s cure, hopped to MAIN, by `WedgePolicy`: a fresh pipeline, presenter
    /// and CAMetalLayer on the SAME connection (the new pump asks the host for one IDR), or hold.
    /// The line names the window, scene and display around the layer. Main thread.
    private func rebuildPresentation() {
        guard restart != nil, baseLayer != nil else { return }
        let action = wedgePolicy.next(now: CACurrentMediaTime())
        let what = action == .rebuild
            ? "rebuilding pipeline, presenter and layer" : "holding, the link stays armed"
        let layer = metalLayer.map {
            "frame=\($0.frame) hidden=\($0.isHidden) opacity=\($0.opacity) drawable=\($0.drawableSize)"
        } ?? "-"
        let line = "presenter wedged ×\(wedgePolicy.wedges) — \(what); layer \(layer); "
            + (surfaceState?() ?? "-")
        presentLog.error("\(line, privacy: .public)")
        if action == .rebuild { restartPresentation() }
    }

    /// Re-run `start` on the live base layer and replay the last layout so the new sublayer has
    /// a frame before the first present. `start`→`stop` clears `contentSize`, and the view dedups
    /// the repeated decoded-size callback — so the known size is restored here, same as
    /// `move(to:)`, or layout falls back to a stale negotiated aspect. Main thread.
    private func restartPresentation() {
        guard let restart, let baseLayer else { return }
        let size = contentSize
        restart(baseLayer)
        contentSize = size
        if let lastLayout { layout(in: lastLayout.bounds, contentsScale: lastLayout.contentsScale) }
    }

    /// Stop the active pump/pipeline (≤ one poll timeout; stage-2 joins its pump) and detach the
    /// stage-2 layer + link. Does not close the connection — that stays with whoever owns it.
    /// Idempotent.
    func stop() {
        restart = nil
        baseLayer = nil
        contentSize = nil // a new session re-derives it from its first frame
        interactionBoost = false
        pump?.stop()
        pump = nil
        stage2Link?.invalidate()
        stage2Link = nil
        headroomTimer?.invalidate()
        headroomTimer = nil
        stage2?.stop() // stops the pump (synchronous join) + drops the decode session
        stage2 = nil
        metalLayer?.removeFromSuperlayer()
        metalLayer = nil
        connection = nil
    }

    isolated deinit {
        // The owning view's stop() normally ran already; this covers a missed teardown so the
        // display link can't keep ticking a deallocated pipeline.
        stage2Link?.invalidate()
        headroomTimer?.invalidate()
        stage2?.stop()
        pump?.stop()
    }
}
#endif
