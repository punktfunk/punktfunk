// Controller discovery + selection, app-lifetime. One GamepadManager (`.shared`) watches
// GCController connect/disconnect from launch, so the Settings page shows live controller
// state without a session, and the session components (GamepadCapture / GamepadFeedback)
// follow `forwarded` — every forwarded controller is streamed to the host, each on its own
// wire pad index (pf-client-core parity; up to `GamepadWire.maxPads`).
//
// Selection (mirrors pf-client-core's `forwarded_ids` + slot model): with no pin, EVERY
// extended controller is forwarded — each assigned a stable lowest-free pad index held for
// its forwarded lifetime, so a disconnect frees only its own index and never renumbers the
// others. A pin (Settings, persisted under DefaultsKey.gamepadID) forwards ONLY that one pad
// — an explicit single-player choice. `active` stays the single "primary" pad (the pinned
// one, else the most recently connected extended gamepad) that the Settings / launcher / menu
// UI reads. GCController has no stable hardware serial, so the pin is a fingerprint of
// vendorName|productCategory (+ a connect-order suffix for twins); identical twin controllers
// may swap a pin across reconnects, which the Settings footer documents.
//
// A singleton (not a SwiftUI environment object) because macOS shows Settings in its own
// `Settings{}` scene — there is no common ancestor view to inject from.

import Combine
import Foundation
import GameController
import PunktfunkShared

@MainActor
public final class GamepadManager: ObservableObject {
    public static let shared = GamepadManager()

    /// One detected controller, decorated for the Settings UI.
    public struct DiscoveredController: Identifiable, Equatable {
        /// Stable-ish fingerprint: `vendorName|productCategory` (+ `#n` for twins).
        public let id: String
        /// User-facing name (the vendor string, e.g. "DualSense Wireless Controller").
        public let name: String
        public let productCategory: String
        /// The full extended profile exists — only these are forwardable.
        public let isExtended: Bool
        /// The virtual-pad type a physical match resolves to under `.auto`: DualSense →
        /// `.dualSense`, DualShock 4 → `.dualShock4`, an Xbox pad → `.xboxOne`, anything
        /// else → `.xbox360`. (`.auto` is never stored here.)
        public let kind: PunktfunkConnection.GamepadType
        public let hasLight: Bool
        public let hasHaptics: Bool
        /// This controller has a GYROSCOPE — not merely a `GCMotion`. The distinction is the whole
        /// point: an X-Box pad exposes a `GCMotion` that reports gravity and nothing else, so
        /// `motion != nil` is true for a controller with no angular rate to give. Read
        /// `hasRotationRate`, which is GameController's own answer to the question we mean.
        public let hasMotion: Bool
        public let hasAdaptiveTriggers: Bool
        /// Specifically a DualSense (incl. the Edge — same feedback surface) — gates the
        /// DualSense-only feedback (adaptive triggers, player LEDs) and the PlayStation glyph
        /// in Settings.
        public var isDualSense: Bool { kind == .dualSense || kind == .dualSenseEdge }
        /// A PlayStation pad with a touchpad + motion (DualSense family OR DualShock 4) — gates
        /// rich-input CAPTURE (touchpad contacts + gyro/accel on plane 0xCC).
        public var hasTouchpadAndMotion: Bool {
            kind == .dualSense || kind == .dualSenseEdge || kind == .dualShock4
        }
        /// 0...1, nil when the controller doesn't report a battery (e.g. wired).
        public let batteryLevel: Float?
        public let isCharging: Bool
        public let controller: GCController

        public static func == (l: DiscoveredController, r: DiscoveredController) -> Bool {
            l.id == r.id && l.controller === r.controller
                && l.batteryLevel == r.batteryLevel && l.isCharging == r.isCharging
        }
    }

    /// Every detected controller, in connect order (Settings lists these).
    @Published public private(set) var controllers: [DiscoveredController] = []

    /// The single "primary" controller — the pinned one, else the most recently connected
    /// extended gamepad; nil when none qualifies. The Settings / launcher / menu UI and the
    /// connect-time `resolveType` read this; the streaming input path uses `forwarded`.
    @Published public private(set) var active: DiscoveredController?

    /// The controllers forwarded to the host this session, in wire-pad-index preference order
    /// (pf-client-core's `forwarded_ids`): a pin forwards ONLY the pinned pad; Automatic forwards
    /// every extended controller. GamepadCapture opens a slot per entry and GamepadFeedback routes
    /// feedback back to it, each on the index from `padIndex(for:)`.
    @Published public private(set) var forwarded: [DiscoveredController] = []

    /// Stable wire pad index (0..<`GamepadWire.maxPads`) per forwarded controller, keyed by
    /// GCController identity. Lowest-free, held while the controller stays forwarded — a
    /// disconnect frees only its own index so the others never renumber (pf-client-core's
    /// `lowest_free_index`). Recomputed by `assignPadIndices` whenever `forwarded` changes.
    private var padIndexByController: [ObjectIdentifier: UInt8] = [:]

    /// Wire pad indices reserved by EXTERNAL (non-GameController) captures — today the Steam
    /// Controller 2 BLE passthrough (`Sc2Capture`), whose device GameController never surfaces
    /// and so can never appear in the identity-keyed table above. Sharing ONE allocator
    /// (`takenIndices` feeds both `assignPadIndices` and `reserveExternalPadIndex`) is what
    /// makes a GC pad and an external capture unable to collide on an index.
    private var externalIndices: Set<UInt8> = []

    /// The kind of the last controller that was actually attached — persisted under
    /// `DefaultsKey.lastGamepadKind` and deliberately NEVER cleared on disconnect. Button
    /// legends read it (through `GamepadGlyphs`) whenever `active` is nil, so a DualSense user's
    /// ✕/◯ hints don't turn into A/B the moment the pad sleeps. `.auto` = nothing has ever been
    /// seen on this device (⇒ neutral glyphs).
    ///
    /// @Published so the legends re-render when a pad of a different family arrives; the screens
    /// already observe this object for `active`.
    @Published public private(set) var lastKnownKind: PunktfunkConnection.GamepadType

    /// A Steam Controller 2 is delivering state reports to `Sc2MenuPad` — iOS only, where
    /// GameController surfaces no such device and `active` therefore stays nil with the pad in
    /// the user's hands. Published because it is half of `uiPadConnected`; the pad's BUTTONS are
    /// deliberately not published (`sc2MenuState` is polled instead — a 66 Hz @Published would
    /// re-render every observing screen for every report).
    @Published public private(set) var sc2MenuAttached = false

    /// A controller the gamepad UI can be driven by: an extended GameController pad, or an SC2
    /// only `Sc2MenuPad` can see. What `GamepadUIEnvironment.isActive` wants for
    /// `gamepadConnected` — `active` alone would leave an iOS SC2 owner on the touch UI with a
    /// controller attached, and `GamepadMenuInput` reads the same two sources.
    public var uiPadConnected: Bool { active != nil || sc2MenuAttached }

    /// A stream's `GamepadCapture` owns the physical controllers, so menus leave the pad alone.
    public internal(set) var streamOwnsPads = false

    /// The user's pinned controller fingerprint ("" = automatic). Persisted; updating it
    /// reselects immediately, so a Settings Picker can bind straight to this.
    @Published public var preferredID: String {
        didSet {
            UserDefaults.standard.set(preferredID, forKey: Self.preferredKey)
            reselect()
        }
    }

    private static let preferredKey = DefaultsKey.gamepadID
    private static let lastKindKey = DefaultsKey.lastGamepadKind
    /// Connect order (identity-keyed) — drives both twin de-dup suffixes and auto-pick.
    private var connectOrder: [ObjectIdentifier] = []
    private var observers: [NSObjectProtocol] = []

    private init() {
        preferredID = UserDefaults.standard.string(forKey: Self.preferredKey) ?? ""
        // Stored as an Int (what UserDefaults round-trips losslessly) and validated back into a
        // real case: a value written by a NEWER client — a pad family this build has no case for
        // — must fall back to the neutral glyphs, not trap on an invalid raw value.
        lastKnownKind = (UserDefaults.standard.object(forKey: Self.lastKindKey) as? Int)
            .flatMap { UInt32(exactly: $0) }
            .flatMap(PunktfunkConnection.GamepadType.init(rawValue:)) ?? .auto
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCControllerDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            MainActor.assumeIsolated {
                guard let self, let c = n.object as? GCController else { return }
                self.noteConnected(c)
            }
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCControllerDidDisconnect, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.rebuild() }
        })
        for c in GCController.controllers() { connectOrder.append(ObjectIdentifier(c)) }
        rebuild()
        #if os(iOS) || os(visionOS)
        // Both switches behind the menu pad live in UserDefaults and only this app writes them,
        // so one notification covers a Settings toggle of either without a call site to forget.
        observers.append(NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.syncSc2Menu() }
        })
        syncSc2Menu()
        #endif
    }

    /// Re-read battery levels etc. (the notifications only fire on connect/disconnect) —
    /// Settings calls this on appear.
    public func refresh() {
        rebuild()
    }

    /// Scan for nearby wireless controllers while the Settings page is visible.
    public func startDiscovery() {
        GCController.startWirelessControllerDiscovery()
    }

    public func stopDiscovery() {
        GCController.stopWirelessControllerDiscovery()
    }

    /// The user's controller-type choice AS CHOSEN (not resolved) for the session being dialed —
    /// adopted by `resolveType` and read back by `declaredKind(for:)`. `.auto` = detect per pad.
    public private(set) var typeSetting: PunktfunkConnection.GamepadType = .auto

    /// The kind to DECLARE to the host for one forwarded controller (its `GamepadArrival`).
    /// An explicit setting wins for every pad — the handshake's session default alone does NOT
    /// stick, because a current host honors the per-pad arrival over it (punktfunk-host's
    /// `Pads::set_kind`), so a client that declared only the detected kind here would silently
    /// undo the user's choice. `.auto` keeps per-pad detection, which is what makes a mixed
    /// session (pad 0 a DualSense, pad 1 an Xbox pad) honest.
    public func declaredKind(
        for controller: DiscoveredController
    ) -> PunktfunkConnection.GamepadType {
        Self.declaredKind(setting: typeSetting, detected: controller.kind)
    }

    /// The pure fold behind `declaredKind(for:)` (pf-client-core's `declared_kind`).
    nonisolated static func declaredKind(
        setting: PunktfunkConnection.GamepadType,
        detected: PunktfunkConnection.GamepadType
    ) -> PunktfunkConnection.GamepadType {
        setting == .auto ? detected : setting
    }

    /// Connect-time resolution of the user's controller-type setting: an explicit choice
    /// wins; `.auto` matches the virtual pad to the active physical controller (DualSense →
    /// DualSense, DualShock 4 → DualShock 4, an Xbox pad → Xbox One, anything else → Xbox
    /// 360); no controller at all defers to the host. Called once per dial with the RAW setting,
    /// which it also adopts for `declaredKind(for:)` so the handshake default and every pad's
    /// arrival can never disagree about an explicit choice.
    public func resolveType(
        setting: PunktfunkConnection.GamepadType
    ) -> PunktfunkConnection.GamepadType {
        typeSetting = setting
        guard setting == .auto else { return setting }
        // Refresh from the LIVE controller list first. `active` is otherwise only populated by the
        // async `.GCControllerDidConnect` notification, so at connect time it can still be nil even
        // with a DualSense attached — which would send `.auto` and the host would create an Xbox 360
        // pad. `rebuild()` re-reads `GCController.controllers()` synchronously, closing that race.
        rebuild()
        guard let active else { return .auto }
        return active.kind
    }

    private func noteConnected(_ c: GCController) {
        let key = ObjectIdentifier(c)
        connectOrder.removeAll { $0 == key }
        connectOrder.append(key)
        rebuild()
    }

    /// While the SC2 passthrough owns the physical hardware, its GameController shadow must not
    /// ALSO be forwarded: macOS surfaces a captured Steam Controller 2 / Puck as an ordinary
    /// controller (on-glass 2026-08-31, vendorName "Steam Controller Puck"), and forwarding both
    /// hands the host the same pad twice. Held only while `Sc2Capture` has a claimed wire slot
    /// (`syncShadowSuppression`), never for its lifetime: an SC2 the capture cannot open must
    /// keep the ordinary path rather than be forwarded on neither.
    /// COUNTED, not a flag: a second SC2 the capture never claimed would otherwise be dropped
    /// here as well and forwarded on neither plane.
    var steamController2Claims = 0 {
        didSet { if steamController2Claims != oldValue { rebuild() } }
    }

    /// Until then every SC2 twin is held back, whatever the claim count: a capture about to claim
    /// the pad must beat its twin to the wire, or the host builds the twin as an Xbox pad first.
    private var steamController2HoldUntil = Date.distantPast

    /// Hold every SC2 twin back for `seconds` (`Sc2Capture.holdTwins`). A twin the capture never
    /// claims rejoins the ordinary path when the hold ends.
    func holdSteamController2Twins(for seconds: TimeInterval) {
        steamController2HoldUntil = Date().addingTimeInterval(seconds)
        rebuild()
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds + 0.05) { [weak self] in
            MainActor.assumeIsolated { self?.rebuild() }
        }
    }

    /// Whether a GameController device is the SC2 family's shadow. Keyed on the measured
    /// vendorName; GameController surfaced no Valve device before the SC2 family, so the prefix
    /// only matches hardware `Sc2Capture` captures.
    private static func isSteamController2(_ c: GCController) -> Bool {
        (c.vendorName ?? "").hasPrefix("Steam Controller")
    }

    private func rebuild() {
        let present = GCController.controllers()
        connectOrder.removeAll { key in !present.contains { ObjectIdentifier($0) == key } }
        for c in present where !connectOrder.contains(ObjectIdentifier(c)) {
            connectOrder.append(ObjectIdentifier(c))
        }
        // In connect order, fingerprinting twins by their position among same-named pads.
        let ordered = connectOrder.compactMap { key in
            present.first { ObjectIdentifier($0) == key }
        }
        var seen: [String: Int] = [:]
        controllers = ordered.map { c in
            let base = "\(c.vendorName ?? "Controller")|\(c.productCategory)"
            let n = (seen[base] ?? 0) + 1
            seen[base] = n
            return Self.describe(c, id: n == 1 ? base : "\(base)#\(n)")
        }
        reselect()
    }

    private func reselect() {
        // Suppress at most as many twins as the capture actually holds. They cannot be matched to
        // their captured device from here, so the most recently connected ones are dropped, and a
        // pad beyond the claim count keeps the ordinary path. A hold drops them all.
        var remaining = steamController2HoldUntil > Date() ? Int.max : steamController2Claims
        var suppressed: Set<ObjectIdentifier> = []
        for entry in controllers.reversed() where remaining > 0 {
            guard Self.isSteamController2(entry.controller) else { continue }
            suppressed.insert(ObjectIdentifier(entry.controller))
            remaining -= 1
        }
        let candidates = controllers.filter {
            $0.isExtended && !suppressed.contains(ObjectIdentifier($0.controller))
        }
        // The pin wins when present; otherwise the most recently connected extended pad
        // (list is in connect order). A stale pin falls back to automatic.
        let pinned = candidates.last { $0.id == preferredID }
        active = pinned ?? candidates.last
        // Remember the family for the legends (see `lastKnownKind`). Only ever WRITTEN, never
        // cleared: `active` going nil is precisely the moment the memory has to survive, and a
        // pad whose `kind` is genuinely unknown never becomes active in the first place.
        if let active, active.kind != lastKnownKind {
            lastKnownKind = active.kind
            UserDefaults.standard.set(Int(active.kind.rawValue), forKey: Self.lastKindKey)
        }
        // Forwarded set (pf-client-core's `forwarded_ids`): a pin forwards ONLY the pinned pad
        // (explicit single-player); Automatic forwards every extended controller in connect order
        // (oldest→newest), so a game's player numbers are stable across hot-plug churn.
        let next = pinned.map { [$0] } ?? candidates
        // Update the pad-index assignment BEFORE publishing `forwarded`: @Published emits in
        // `willSet`, so GamepadCapture/GamepadFeedback reconcile against `padIndex(for:)` the
        // instant this assignment lands — a stale map here would skip a newly-forwarded pad.
        assignPadIndices(for: next)
        forwarded = next
    }

    /// Assign each forwarded controller a stable wire pad index (lowest-free, held while it stays
    /// forwarded) — mirrors pf-client-core's slot model, where a disconnect frees only its own
    /// index and the others keep theirs. A controller already holding an index keeps it across the
    /// churn; a slot beyond `GamepadWire.maxPads` goes unassigned (that pad is not forwarded).
    private func assignPadIndices(for next: [DiscoveredController]) {
        let live = Set(next.map { ObjectIdentifier($0.controller) })
        padIndexByController = padIndexByController.filter { live.contains($0.key) }
        for dc in next {
            let key = ObjectIdentifier(dc.controller)
            guard padIndexByController[key] == nil,
                  let free = Self.lowestFreeIndex(takenIndices()) else { continue }
            padIndexByController[key] = free
        }
    }

    /// Every index currently in use — the GC table's plus the external reservations. The one
    /// set both allocation paths consult.
    private func takenIndices() -> Set<UInt8> {
        Set(padIndexByController.values).union(externalIndices)
    }

    /// Reserve the lowest free wire pad index for an external (non-GameController) capture —
    /// `Sc2Capture` claims through here on its first state report. Held until
    /// `releaseExternalPadIndex(_:)`; nil when all `GamepadWire.maxPads` indices are taken (the
    /// caller drops reports until one frees).
    public func reserveExternalPadIndex() -> UInt8? {
        guard let free = Self.lowestFreeIndex(takenIndices()) else { return nil }
        externalIndices.insert(free)
        return free
    }

    /// Hand an external reservation back (link drop / capture stop). Idempotent.
    public func releaseExternalPadIndex(_ index: UInt8) {
        externalIndices.remove(index)
    }

    // MARK: - Steam Controller 2 (menu)

    #if os(iOS) || os(visionOS)
    /// The app-lifetime SC2 reader — see `Sc2MenuPad`. Nil whenever the switches below say
    /// nothing could use it, so the radio and its permission prompt cost nothing by default.
    private var sc2Menu: Sc2MenuPad?
    /// A streaming `Sc2Capture` holds the physical pad (`holdSc2Hardware`).
    private var sc2HardwareHeld = false
    #endif

    /// The live SC2 menu state for `GamepadMenuInput`'s poll — nil off iOS, and whenever no pad
    /// is delivering. Polled rather than published: see `sc2MenuAttached`.
    var sc2MenuState: Sc2Device.State? {
        #if os(iOS) || os(visionOS)
        return sc2Menu?.snapshot
        #else
        return nil
        #endif
    }

    /// A streaming `Sc2Capture` is taking the pad (`true`) or handing it back. Two centrals
    /// subscribed to one peripheral would double-feed it, so the menu pad stands down for the
    /// stream and re-acquires after it. Only iOS has a menu pad to move.
    func holdSc2Hardware(_ held: Bool) {
        #if os(iOS) || os(visionOS)
        guard held != sc2HardwareHeld else { return }
        sc2HardwareHeld = held
        syncSc2Menu()
        #endif
    }

    #if os(iOS) || os(visionOS)
    /// Start or stop the menu pad from the two switches that decide whether anything could use
    /// it — the SC2 passthrough opt-in, which is what pays for the Bluetooth prompt, and the
    /// gamepad UI switch — plus `holdSc2Hardware`. Runs on every defaults write, so it stays an
    /// idempotent compare.
    private func syncSc2Menu() {
        let defaults = UserDefaults.standard
        // `gamepadUIEnabled` defaults to ON, which `bool(forKey:)` alone cannot express.
        let uiEnabled = (defaults.object(forKey: DefaultsKey.gamepadUIEnabled) as? Bool) ?? true
        let wanted = defaults.bool(forKey: DefaultsKey.sc2Capture) && uiEnabled && !sc2HardwareHeld
        guard wanted != (sc2Menu != nil) else { return }
        guard wanted else {
            sc2Menu?.stop()
            sc2Menu = nil
            sc2MenuAttached = false
            return
        }
        let pad = Sc2MenuPad()
        pad.onAttachChange = { [weak self] attached in self?.sc2MenuAttached = attached }
        pad.start()
        sc2Menu = pad
    }
    #endif

    /// The lowest wire pad index not already taken, or nil when all `GamepadWire.maxPads` are in
    /// use (pf-client-core's `lowest_free_index`).
    private static func lowestFreeIndex(_ taken: Set<UInt8>) -> UInt8? {
        (0..<UInt8(GamepadWire.maxPads)).first { !taken.contains($0) }
    }

    /// The wire pad index a forwarded controller streams on, or nil when it isn't forwarded.
    public func padIndex(for controller: DiscoveredController) -> UInt8? {
        padIndexByController[ObjectIdentifier(controller.controller)]
    }

    /// Drop every pad-index assignment and recompute from the current forwarded set — called when
    /// a streaming session begins so the assignment starts fresh (a controller pinned before the
    /// session forwards as pad 0, not whatever index it held for the Settings list). pf-client-core
    /// assigns indices at slot-open time; this reproduces that session-scoped start.
    public func resetForwardingAssignment() {
        padIndexByController.removeAll()
        reselect()
    }

    private static func describe(_ c: GCController, id: String) -> DiscoveredController {
        let extended = c.extendedGamepad
        let kind = padKind(extended, productCategory: c.productCategory)
        return DiscoveredController(
            id: id,
            name: c.vendorName ?? c.productCategory,
            productCategory: c.productCategory,
            isExtended: extended != nil,
            kind: kind,
            hasLight: c.light != nil,
            hasHaptics: c.haptics != nil,
            // `hasRotationRate`, not `motion != nil` — see the property. The settings row shows a
            // gyroscope badge off this, and promising a gyro an X-Box pad does not have is the
            // same lie as streaming its non-existent rotation to the host.
            hasMotion: c.motion?.hasRotationRate ?? false,
            // GCDualSenseGamepad's triggers are GCDualSenseAdaptiveTrigger by declaration (the
            // Edge included); the DualShock 4 has none.
            hasAdaptiveTriggers: kind == .dualSense || kind == .dualSenseEdge,
            batteryLevel: c.battery.flatMap { $0.batteryLevel >= 0 ? $0.batteryLevel : nil },
            isCharging: c.battery?.batteryState == .charging,
            controller: c)
    }

    /// Resolve a physical controller's matching virtual-pad type from its GameController
    /// subclass (+ the product-category string where the subclass is shared). Detection order
    /// (all are `: GCExtendedGamepad`): DualSense family first (the Edge is a
    /// `GCDualSenseGamepad` too — its distinct product category splits it out), then
    /// DualShock 4, any Xbox pad, then Nintendo Switch pads by category (GameController has no
    /// dedicated subclass for them). A non-extended / absent profile falls back to `.xbox360`
    /// (it's never forwarded anyway).
    private static func padKind(
        _ extended: GCExtendedGamepad?,
        productCategory: String
    ) -> PunktfunkConnection.GamepadType {
        guard let extended else { return .xbox360 }
        let category = productCategory.lowercased()
        // Deployment floor (macOS 14 / iOS 17 / tvOS 17) clears every introduction version
        // here, so no `@available` guard is needed — matching the unguarded
        // `GCDualSenseGamepad` use elsewhere in the package.
        if extended is GCDualSenseGamepad {
            return category.contains("edge") ? .dualSenseEdge : .dualSense
        }
        if extended is GCDualShockGamepad { return .dualShock4 }
        if extended is GCXboxGamepad { return .xboxOne }
        // Nintendo Switch Pro Controller / a paired Joy-Con set (a full pad surface). Single
        // Joy-Cons ("Joy-Con (L)" / "(R)") stay on the Xbox 360 fallback — half a pad.
        if category.contains("switch pro") || category.contains("joy-con (l/r)") {
            return .switchPro
        }
        return .xbox360
    }
}
