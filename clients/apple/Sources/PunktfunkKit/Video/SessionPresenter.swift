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
#if os(tvOS)
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
    /// macOS path has separate synchronization constraints. `decoded` is tvOS-only because its
    /// latency win and displayed-IOSurface metering are validated there. Stage-1 additionally
    /// requires the caller's release-build gate.
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
            #if os(tvOS)
            if #available(tvOS 17.4, *) { return .decoded }
            return nil
            #else
            return nil
            #endif
        default: return nil
        }
    }

    /// iOS uses deadline-paced Metal, tvOS the decoded video plane, and macOS arrival-paced Metal.
    ///
    /// A fixed-rate Apple TV vends CAMetalDisplayLink drawables about two refreshes before glass.
    /// Passing VideoToolbox's decoded IOSurface directly to AVSampleBufferVideoRenderer removes
    /// that reservation and the Metal FIFO: the measured display stage falls from 28–32 ms to
    /// about 10–12 ms at 60 fps. tvOS before 17.4 retains deadline pacing because it cannot query
    /// the displayed IOSurface for metrics; PyroWave, 4:4:4, and Smoothness retain Metal.
    static var platformDefault: PresenterChoice {
        #if os(iOS)
        return .stage4
        #elseif os(tvOS)
        if #available(tvOS 17.4, *) { return .decoded }
        return .stage4
        #else
        return .stage2
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

final class SessionPresenter {
    /// Map a presenter choice and codec to its execution path.
    ///
    /// The decoded tvOS path requires a CVPixelBuffer, so PyroWave retains deadline-paced Metal.
    /// Stage-3 and stage-4 map directly to glass and deadline pacing. On macOS, default PyroWave
    /// uses glass gating to coalesce its bursty, near-instant decode output; an explicit stage-2
    /// selection remains a faithful arrival-pacing comparison. Other stage-2 sessions use arrival.
    static func pacing(
        for choice: PresenterChoice, explicit: PresenterChoice?, codec: VideoCodec
    ) -> PresentPacing {
        if choice == .decoded { return codec == .pyrowave ? .deadline : .decoded }
        if choice == .stage4 { return .deadline }
        if choice == .stage3 { return .glass }
        #if os(macOS)
        if explicit == nil, codec == .pyrowave { return .glass }
        #endif
        return .arrival
    }

    /// The decoded sink is zero-buffer and validated for biplanar 4:2:0. Preserve deadline Metal
    /// for Smoothness and 4:4:4 rather than ignoring intent or risking an unsupported video plane.
    static func effectivePacing(
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
    /// Resolve the windowed (composited) present MECHANISM for this session — the DCP
    /// swapID-panic mitigation picker (see `WindowedPresentMode`). The
    /// `PUNKTFUNK_WINDOWED_PRESENT=async|transaction|surface` env lever wins (dev A/B);
    /// otherwise the user's safe-present setting: ON/unset → `.transaction` (the validated
    /// mitigation), OFF → `.async` (the fast pre-mitigation path — the panic returns on
    /// affected high-refresh setups; the Settings caption says so). `.surface` is currently
    /// env-only (prototype — HDR-composite verification owed). Fullscreen always presents
    /// async regardless (`setComposited`). Internal (not private) for unit tests.
    static func windowedPresentMode(setting: Bool?, env: String?) -> WindowedPresentMode {
        if let env, let mode = WindowedPresentMode(rawValue: env) { return mode }
        return (setting ?? true) ? .transaction : .async
    }

    /// Adaptive-refresh latency sessions choose immediate sparse presents or one dense present per
    /// display-link target. Smoothness and the other presenters keep their own pacing mechanisms.
    static func adaptiveSlotPaced(
        adaptiveSync: Bool, priority: PresentPriority, pacing: PresentPacing
    ) -> Bool {
        adaptiveSync && priority == .latency && pacing == .arrival
    }
    #endif

    /// `PUNKTFUNK_GATE_DEPTH` (1…3) still overrides on iOS/tvOS so the standing-queue ladder
    /// stays reproducible on-device; macOS is pinned to 1, env ignored — a deeper gate only builds
    /// a standing queue (see above), and macOS glass pacing exists for PyroWave smoothness
    /// (see `pacing`), where depth 1 is the point. Internal (not private) for unit tests.
    static func gateDepth(env: String?) -> Int {
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
    static func frameRateRange(hz: Float, allowVRR: Bool) -> CAFrameRateRange {
        #if os(macOS)
        let minimum = allowVRR ? min(hz, 24) : hz
        return CAFrameRateRange(minimum: minimum, maximum: hz, preferred: hz)
        #else
        let floor = allowVRR ? min(hz, 24) : min(hz, 30)
        return CAFrameRateRange(minimum: floor, maximum: max(hz, 120), preferred: hz)
        #endif
    }

    private var pump: StreamPump?
    private var stage2: Stage2Pipeline?
    private var stage2Link: CADisplayLink?
    private var metalLayer: CAMetalLayer?
    #if os(macOS)
    /// The windowed present MECHANISM this session runs while composited (resolved once per
    /// session in `start` — the user's safe-present setting + the PUNKTFUNK_WINDOWED_PRESENT
    /// dev override) and the routing last pushed to the pipeline — see `setComposited` (the DCP
    /// swapID-panic mitigation). Main-thread only, like all of this.
    private var windowedMode: WindowedPresentMode = .transaction
    private var windowedPresentApplied: WindowedPresentMode = .async
    /// The windowed `surface` present target (sibling above `metalLayer`, transparent while
    /// unused) — installed whenever stage-2 runs so a mechanism flip never has to mutate the
    /// layer tree mid-session.
    private var surfaceLayer: CALayer?
    #endif
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

    /// Start the resolved presenter for `connection`.
    ///
    /// Stage-1 sends compressed samples to `baseLayer`; tvOS's decoded path sends it VideoToolbox
    /// output. Metal paths leave that layer idle and overlay their own CAMetalLayer. The supplied
    /// display-link factory tracks the hosting display for ordinary pacing and decoded-frame
    /// metering; deadline pacing owns a CAMetalDisplayLink instead.
    ///
    /// Call `layout(in:contentsScale:)` after start so any Metal sublayer has valid geometry.
    /// `adaptiveSync` is the hosting screen's fixed-vs-adaptive verdict on macOS.
    func start(
        connection: PunktfunkConnection,
        baseLayer: AVSampleBufferDisplayLayer,
        endToEndMeter: LatencyMeter?,
        makeDisplayLink: @escaping (AnyObject, Selector) -> CADisplayLink,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil,
        onFrameHDR: (@Sendable (Bool) -> Void)? = nil,
        adaptiveSync: Bool = false
    ) {
        stop()
        self.connection = connection
        self.baseLayer = baseLayer
        restart = { [weak self] layer in
            self?.start(
                connection: connection, baseLayer: layer, endToEndMeter: endToEndMeter,
                makeDisplayLink: makeDisplayLink,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize,
                onFrameHDR: onFrameHDR, adaptiveSync: adaptiveSync)
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
        let selectedPacing = Self.pacing(
            for: choice, explicit: explicit, codec: connection.videoCodec)
        let priority = PresentPriority.resolve(
            setting: connection.settings.presentPriority,
            bufferSetting: connection.settings.smoothBuffer)
        // Direct video presentation is the zero-buffer latency path. A user who asks for a
        // smoothness buffer keeps the existing deadline engine, where FrameStore owns that buffer.
        let pacing = Self.effectivePacing(
            selectedPacing, priority: priority, videoLayerCompatible: !connection.isChroma444)
        #if os(macOS)
        let vsyncPaced = priority != .latency && pacing == .arrival
        let adaptiveSlotPaced = Self.adaptiveSlotPaced(
            adaptiveSync: adaptiveSync, priority: priority, pacing: pacing)
        #else
        let vsyncPaced = false
        let adaptiveSlotPaced = false
        #endif
        if choice != .stage1,
           let pipeline = Stage2Pipeline(
               endToEndMeter: endToEndMeter,
               displayLayer: pacing == .decoded ? baseLayer : nil,
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
            #if os(macOS)
            windowedPresentApplied = .async
            // Resolve THIS session's windowed mechanism once (setting + dev env lever) —
            // `setComposited` routes between it and fullscreen-async from every layout.
            windowedMode = Self.windowedPresentMode(
                setting: connection.settings.windowedSafePresent,
                env: ProcessInfo.processInfo.environment["PUNKTFUNK_WINDOWED_PRESENT"])
            // The surface present target sits ABOVE the metal layer: transparent (nil contents)
            // unless the surface mechanism actually presents, covering it while it does.
            baseLayer.addSublayer(pipeline.surfaceLayer)
            surfaceLayer = pipeline.surfaceLayer
            #endif
            stage2 = pipeline
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
            pipeline.start(
                connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd,
                onDecodedSize: onDecodedSize, onFrameHDR: onFrameHDR)
        } else {
            let pump = StreamPump()
            pump.start(
                connection: connection, layer: baseLayer, endToEndMeter: endToEndMeter,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize)
            self.pump = pump
        }
    }

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
    /// Pen-proximity panel-rate boost pass-through (Stage2Pipeline.setInteractionBoost):
    /// deadline pacing only — under arrival/glass the staged hint feeds no link, so this
    /// is a no-op there. MAIN thread.
    func setInteractionBoost(_ on: Bool) {
        stage2?.setInteractionBoost(on)
    }

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
    static func gravity(_ fit: VideoFit) -> AVLayerVideoGravity {
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
        #if os(macOS)
        // The surface present target mirrors the metal layer's geometry exactly — its IOSurfaces
        // are sized to the same snapped pixel rect, so the contents composite is a 1:1 blit too.
        surfaceLayer?.contentsScale = contentsScale
        surfaceLayer?.frame = snapped
        #endif
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
        stage2?.setDisplayHeadroom(UIScreen.main.currentEDRHeadroom)
        #endif
    }

    /// Record the decoded frame's real dimensions (the view hops the pump's `onDecodedSize` to main
    /// and calls this) so `layout` aspect-fits to what's actually on screen instead of the possibly-
    /// stale `currentMode()`. Only stores — the caller re-runs `layout` right after, because a
    /// resize-END produces no bounds change to trigger one. No-op on a zero/unchanged size.
    func setContentSize(_ size: CGSize) {
        guard size.width > 0, size.height > 0, size != contentSize else { return }
        contentSize = size
    }

    #if os(macOS)
    /// Route presents for the window's composited state (MAIN thread — the view pushes it on
    /// every layout, which fullscreen transitions always trigger). A COMPOSITED (windowed)
    /// session presents through this session's resolved mitigation mechanism (`windowedMode` —
    /// transactional by default, see `windowedPresentMode`) instead of the async image queue —
    /// the DCP "mismatched swapID's" kernel-panic mitigation (see `MetalVideoPresenter`; the
    /// async-swap race survives glass pacing, so pacing alone was not enough). ALL codecs:
    /// PyroWave hit it 2026-07-18 and windowed HEVC hit the same 240 Hz Mac Studio 2026-07-21 —
    /// it is the async image queue itself, not any codec or present rate. Fullscreen keeps the
    /// async path (direct scanout, lowest latency, no panic there). The full HDR/EDR render
    /// path is preserved in every mechanism.
    func setComposited(_ composited: Bool) {
        guard let stage2 else { return }
        let mode: WindowedPresentMode = composited ? windowedMode : .async
        guard mode != windowedPresentApplied else { return }
        let wasSurface = windowedPresentApplied == .surface
        windowedPresentApplied = mode
        stage2.setWindowedPresent(mode)
        if wasSurface {
            // Uncover the metal layer NOW (its last drawable is still attached, so fullscreen
            // entry shows the previous frame until the next present — no black flash).
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            surfaceLayer?.contents = nil
            CATransaction.commit()
        }
    }
    #endif

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

    /// `onPresentWedged`'s cure, hopped to MAIN: a fresh pipeline, presenter and CAMetalLayer on
    /// the SAME connection — the new pump asks the host for one IDR. Main thread.
    private func rebuildPresentation() {
        guard restart != nil, baseLayer != nil else { return }
        presentLog.error("presenter wedged — rebuilding pipeline, presenter and layer")
        restartPresentation()
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
        pump?.stop()
        pump = nil
        stage2Link?.invalidate()
        stage2Link = nil
        stage2?.stop() // stops the pump (synchronous join) + drops the decode session
        stage2 = nil
        metalLayer?.removeFromSuperlayer()
        metalLayer = nil
        #if os(macOS)
        surfaceLayer?.removeFromSuperlayer()
        surfaceLayer = nil
        windowedPresentApplied = .async
        #endif
        connection = nil
    }

    deinit {
        // The owning view's stop() normally ran already; this covers a missed teardown so the
        // display link can't keep ticking a deallocated pipeline.
        stage2Link?.invalidate()
        stage2?.stop()
        pump?.stop()
    }
}
#endif
