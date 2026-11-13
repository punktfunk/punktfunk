// Match-window resize follower (design/midstream-resolution-resize.md D1/D2, client C3).
//
// The presenting view feeds this its PHYSICAL-PIXEL size on every layout; it debounces to
// resize-end, spaces requests ≥ 1 s apart, and asks the connection to switch the host's virtual
// display + encoder to match (`PunktfunkConnection.requestMode`) — so a windowed macOS session or
// an iPad Stage Manager / Split View scene streams native-resolution pixels instead of scaling.
// Above 1x render scale the window's own pixels ride along as the view. The decode/present side
// needs nothing: VideoToolbox recreates its session on the keyframe-derived format-description
// change (the first new-mode AU is an IDR with fresh parameter sets).
//
// The trigger discipline is the shared cross-client one (mirrors the session binary's
// `resize_decision`): debounce to resize-end; ≥ 1 s between requests; skip a size and view equal to
// the live ones; and skip an ask equal to the last one sent — which both stops re-asking a rejected
// size and keeps a host-side rollback (accepted, rebuild failed, corrective ack restored the old
// mode) from looping request → rollback → request. The size is `RenderScale.apply`'s: rounded
// DOWN to even (the host rejects odd dimensions), ≥ 320×200, within the codec ceiling. A clamped
// mode still moves its view.

import Foundation
import PunktfunkShared

/// The pure, side-effect-free core of the Match-window trigger — so the skip discipline is
/// unit-tested without a live connection or a UI (`MatchWindowTests`).
public enum MatchWindow {
    /// Whether to request `target` now (the debounce has already settled; spacing is the caller's
    /// timer): `nil` to skip — equal to the live mode and view, or to the last ask sent (a
    /// rejected size / a host rollback must not loop). `target` is expected host-valid already
    /// (`RenderScale.apply`).
    public static func request(
        target: Ask, current: Ask, lastRequested: Ask?
    ) -> Ask? {
        if target == current || target == lastRequested { return nil }
        return target
    }

    /// A mode ask: its size and the view it is shown at, `0×0` for the mode itself.
    public struct Ask: Equatable, Sendable {
        public var width: UInt32
        public var height: UInt32
        public var viewWidth: UInt32 = 0
        public var viewHeight: UInt32 = 0

        public init(
            _ size: (width: UInt32, height: UInt32),
            view: (width: UInt32, height: UInt32)? = nil
        ) {
            width = size.width
            height = size.height
            viewWidth = view?.width ?? 0
            viewHeight = view?.height ?? 0
        }
    }
}

/// A Match-window resize toward a mode, with the view it is shown at above 1x render scale.
public typealias ResizeTarget = (
    _ width: UInt32, _ height: UInt32, _ view: (width: UInt32, height: UInt32)?
) -> Void

/// Owns the debounce timer + serialization state and drives `PunktfunkConnection.requestMode` from
/// the stream view's layout callbacks. Main-actor: the views feed it on the main thread and it reads
/// the connection's live mode there. Enabled per session from the `matchWindow` setting.
@MainActor
public final class MatchWindowFollower {
    private weak var connection: PunktfunkConnection?
    private let debounce: TimeInterval
    private let minSpacing: TimeInterval
    /// Render-scale multiplier applied to the live window size before it's requested, so the
    /// match-window path supersamples/undersamples exactly like the fixed-mode connect. 1.0 = the
    /// window's native pixels. `maxDimension` is the codec's per-axis ceiling.
    private let renderScale: Double
    private let maxDimension: Int
    private var enabled: Bool

    private var work: DispatchWorkItem?
    private var pendingSize: (width: Int, height: Int)?
    private var lastRequested: MatchWindow.Ask?
    /// The view the host scales the live mode to, as this side last asked it.
    private var liveView: (width: UInt32, height: UInt32)?
    /// `systemUptime` of the last request, so a wall-clock step never stretches the spacing.
    private var lastRequestAt: TimeInterval?
    /// The last size we reported via [`onResizeTarget`] — dedups the per-layout stream of a drag so
    /// the UI is notified once per distinct target, and reset to `nil` when the window is back in
    /// sync with the live mode (so a later resize re-reports).
    private var lastSteered: (width: UInt32, height: UInt32)?

    /// Fired (on the main actor) the instant the window starts differing from the live mode — i.e.
    /// a resize is under way and a `Reconfigure` for `(width, height)`, shown at `view`, is
    /// imminent. Drives the resize overlay's INSTANT feedback (blur + spinner) BEFORE the debounced
    /// request leaves; the overlay clears when a decoded frame reaches the mode or the view (or on
    /// a timeout). Deduped per target.
    public var onResizeTarget: ResizeTarget?

    /// The refresh to ask for along with a size; nil keeps the live one. A foldable's two screens
    /// can run at different rates, and a session streaming the native rate follows the screen.
    public var refreshHz: (() -> UInt32?)?

    /// `debounce` = quiet time after the last size event before requesting (Win32 gets
    /// `WM_EXITSIZEMOVE` for free; we debounce). `minSpacing` = floor between accepted requests
    /// (a full host pipeline rebuild each). Defaults match the other clients.
    public init(
        connection: PunktfunkConnection,
        enabled: Bool,
        renderScale: Double = 1.0,
        maxDimension: Int = 8192,
        debounce: TimeInterval = 0.4,
        minSpacing: TimeInterval = 1.0
    ) {
        self.connection = connection
        self.liveView = connection.connectView
        self.enabled = enabled
        self.renderScale = RenderScale.sanitize(renderScale)
        self.maxDimension = maxDimension
        self.debounce = debounce
        self.minSpacing = minSpacing
    }

    /// The host-valid mode for a live window size: the window's physical pixels × the render scale,
    /// aspect-preserved, even, and clamped to the codec ceiling — the match-window twin of
    /// `EffectiveSettings.streamMode`.
    private func targetMode(widthPx: Int, heightPx: Int) -> (width: UInt32, height: UInt32) {
        RenderScale.apply(
            baseWidth: widthPx, baseHeight: heightPx,
            scale: renderScale, maxDimension: maxDimension)
    }

    /// The window's own pixels as the view beside `targetMode` above 1x; nil at 1x and below,
    /// which also clears a view asked for before.
    private func targetView(widthPx: Int, heightPx: Int) -> (width: UInt32, height: UInt32)? {
        RenderScale.view(
            baseWidth: widthPx, baseHeight: heightPx,
            scale: renderScale, maxDimension: maxDimension)
    }

    /// Turn following on/off live. Off cancels a pending request; on forgets the last request, since
    /// something else (an external monitor) may have moved the mode away from it meanwhile.
    public func setEnabled(_ on: Bool) {
        enabled = on
        if on {
            lastRequested = nil
        } else {
            work?.cancel()
            work = nil
            pendingSize = nil
            lastSteered = nil
        }
    }

    /// Feed the presenting view's current PHYSICAL-PIXEL size (its `bounds` × the backing/display
    /// scale). Called from every layout pass; coalesced by the debounce so a drag-resize sends one
    /// request at its end, never one per frame.
    public func noteSize(widthPx: Int, heightPx: Int) {
        guard enabled else { return }
        pendingSize = (widthPx, heightPx)
        schedule()
        reportSteering(widthPx: widthPx, heightPx: heightPx)
    }

    /// Report the resize overlay's START signal (deduped): the moment the normalized window size
    /// differs from the live mode we're steering toward a new size. No connection / no negotiated
    /// mode yet → nothing to compare against, skip.
    private func reportSteering(widthPx: Int, heightPx: Int) {
        guard let connection else { return }
        let target = targetMode(widthPx: widthPx, heightPx: heightPx)
        let mode = connection.currentMode()
        guard mode.width > 0, mode.height > 0 else { return }
        if target.width == mode.width, target.height == mode.height {
            lastSteered = nil // back in sync — a later change re-reports
            return
        }
        if lastSteered?.width == target.width, lastSteered?.height == target.height { return }
        lastSteered = target
        onResizeTarget?(
            target.width, target.height, targetView(widthPx: widthPx, heightPx: heightPx))
    }

    private func schedule() {
        work?.cancel()
        let item = DispatchWorkItem { [weak self] in self?.fire() }
        work = item
        DispatchQueue.main.asyncAfter(deadline: .now() + debounce, execute: item)
    }

    private func fire() {
        guard enabled, let connection, let size = pendingSize else { return }
        // ≥ 1 s spacing: a request went out recently → re-arm the debounce and retry later rather
        // than fire early (keeps at most ~one request outstanding — the accept ack round-trips in
        // milliseconds, ahead of the host's rebuild).
        if let last = lastRequestAt, ProcessInfo.processInfo.systemUptime - last < minSpacing {
            schedule()
            return
        }
        let target = targetMode(widthPx: size.width, heightPx: size.height)
        let view = targetView(widthPx: size.width, heightPx: size.height)
        let mode = connection.currentMode()
        pendingSize = nil
        guard let req = MatchWindow.request(
            target: MatchWindow.Ask(target, view: view),
            current: MatchWindow.Ask((mode.width, mode.height), view: liveView),
            lastRequested: lastRequested
        ) else { return }
        // Match window follows size. The rate rides along only where the screen sets it.
        connection.requestMode(
            width: req.width, height: req.height, refreshHz: refreshHz?() ?? mode.refreshHz,
            view: view)
        lastRequested = req
        liveView = view
        lastRequestAt = ProcessInfo.processInfo.systemUptime
    }
}
