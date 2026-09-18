// The quick-action ring (design/touch-client-overlay.md §2): six round glass buttons on a circle
// under the fingers plus a centre "More" that opens the sheet — the complete catalogue with
// values. The two-finger twist drives the opening frame by frame (`RingState.handle`); the exit
// disc opens it at the corner. Closed, it leaves the view hierarchy entirely (tenet 1: any layer
// above the stream costs a refresh of display latency on iOS). tvOS carries the same ring,
// opened by a short Back on the remote or `Select+A` and driven by the pad (§2.5, §2.6); it
// has no twist, no tap and no drag, so those paths are iOS-only below.
//
// macOS carries it too, opened by ⌃⌥⇧O, the Stream menu or `Select+A` — no twist there
// either, so it opens CENTRED (`openCentred`) rather than under a hand. A Mac has no touch screen
// and no software keyboard, so the touch-mode, virtual-controller and keyboard slots are dimmed
// the way tvOS dims its own: the mouse and the real keyboard already do that work.

#if os(iOS) || os(tvOS) || os(macOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI

/// The ring's open/closed state and what it is showing. One per session; the overlay reads it.
@MainActor
final class RingState: ObservableObject {
    /// 0 closed … 1 open — driven by the twist until `committed`.
    @Published var progress: CGFloat = 0
    @Published var committed = false
    /// The wind-in after a close: the overlay stays mounted while the discs ease back.
    @Published var closing = false
    @Published var clockwise = true
    /// Stream-view points; the ring is centred here, clamped so it stays on screen.
    @Published var centre = CGPoint.zero
    /// Opened with no point of its own (a chord, a menu item): the overlay puts it in the middle
    /// of the stage instead, so it never depends on a pointer that may be locked or off-window.
    @Published var centred = false
    @Published var sheet = false
    /// A destructive slot awaiting its second press (the slot id).
    @Published var armed: String?
    /// The label under the ring: a slot's name, why it is unavailable, or "tap again".
    @Published var hint: String?
    @Published var lastTouch = Date()
    /// The mode the Welcome carried, captured at first open — the Resolution row's first chip.
    var native: (w: UInt32, h: UInt32, hz: UInt32)?
    /// The wire pad whose `Select+A` opened the ring; `nil` for any other open.
    var opener: UInt32?
    /// The pad's highlight: a slot 0…5, or 6 for the centre (the initial one — `Select+A`
    /// then A opens the sheet in two presses). `nil` until a pad moves it.
    @Published var highlight: Int?
    /// The sheet row the pad is on.
    @Published var sheetCursor = 0
    /// The in-flight wind-in, so a second close cannot clear `closing` under the first.
    private var closeTask: Task<Void, Never>?
    /// Pad presses awaiting the overlay (`RingOverlay` drains them); `navSeq` makes each an event.
    ///
    /// A QUEUE, not a slot: capture emits several of these from one poll — a stick crossing a
    /// sector boundary in the same frame as an A press — and SwiftUI coalesces the `navSeq` bumps
    /// into one update, so a single slot delivered only the last of them. The dropped ones were
    /// the presses: the highlight moved and nothing fired, and the same race ate B, so the ring
    /// refused to close.
    var pendingNav: [RingNav] = []
    @Published var navSeq = 0

    func nav(_ n: RingNav) {
        pendingNav.append(n)
        navSeq &+= 1
    }

    // Haptic triggers: `.sensoryFeedback` fires on change, so each is a counter. The vocabulary
    // is one tick when the twist arms, a thump at commit, a tap per press, a firm "no" on a
    // dimmed button, and a warning when a destructive slot arms.
    @Published var armTick = 0
    @Published var commitTick = 0
    @Published var pressTick = 0
    @Published var refuseTick = 0
    @Published var warnTick = 0
    private var twistArmed = false

    var visible: Bool { committed || progress > 0 || closing }

    func handle(_ event: DialEvent) {
        switch event {
        case .turn(let p, let cw, let at):
            guard !committed else { return }
            if !twistArmed {
                twistArmed = true
                armTick &+= 1
            }
            progress = p
            clockwise = cw
            centre = at
        case .commit:
            commit()
        case .cancel:
            // Lifted short of commit, or wound back after one: the ring winds back in.
            close()
        case .open(let at):
            // The edge pull, which arrives already decided — there is no turn behind it.
            openAt(at)
        }
    }

    func commit() {
        committed = true
        progress = 1
        commitTick &+= 1
        touch()
    }

    func openAt(_ c: CGPoint) {
        centre = c
        centred = false
        commit()
    }

    /// Open in the middle of the stage — the keyboard, menu and pad routes, which carry no point.
    func openCentred() {
        centred = true
        commit()
    }

    /// A chord, a menu item or a remote's Back: open it in the middle of the stage, or close the
    /// one already up. The routes that carry no point are also the routes with no other way out.
    func toggleCentred() {
        guard !committed else { return close() }
        pressTick &+= 1
        openCentred()
    }

    func close() {
        if committed {
            // An open ring winds in over ~120 ms before it unmounts; a cancel short of commit
            // leaves at once (`progress` alone held it). Held and cancelled, because two closes
            // inside the wind (a scrim tap, then the idle timeout) would otherwise let the first
            // task clear the flag mid-wind and pop the discs out.
            closing = true
            closeTask?.cancel()
            closeTask = Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(140))
                guard !Task.isCancelled else { return }
                self?.closing = false
            }
        }
        committed = false
        progress = 0
        sheet = false
        armed = nil
        hint = nil
        highlight = nil
        twistArmed = false
        opener = nil
    }

    func touch() { lastTouch = Date() }
}

/// What the ring can do this session — the shell's live state and commands behind each slot.
struct RingActions {
    var endStream: () -> Void
    var disconnectLinger: () -> Void
    var touchMode: () -> TouchInputMode
    var cycleTouchMode: () -> Void
    var keyboard: () -> Void
    var stats: () -> StatsVerbosity
    var cycleStats: () -> Void
    var micAvailable: () -> Bool
    var micMuted: () -> Bool
    var toggleMic: () -> Void
    var hostActions: () -> [HostAction]
    var invokeHost: (HostAction) -> Void
    var sendShortcut: ([String]) -> Void
    /// The virtual controller (§4): whether its input can reach the host, whether it is up, and the toggle.
    var padAvailable: () -> Bool
    var padShown: () -> Bool
    var togglePad: () -> Void
    /// One synthetic system-button tap on the host's pad (a `GamepadWire` bit).
    var tapPadButton: (UInt32) -> Void
    /// Controller mouse: the pointer grant it needs, the wire pads it acts on (a bit per pad,
    /// `0` = none), whether they are all on, and the toggle.
    var pointerGranted: () -> Bool
    var padMouseTarget: () -> UInt16
    var padMouseOn: () -> Bool
    var togglePadMouse: () -> Void
    var currentMode: () -> (w: UInt32, h: UInt32, hz: UInt32)
    var requestMode: (UInt32, UInt32, UInt32) -> Void
}

/// The editor's hooks (design §3.3): a tap on a slot picks its action instead of firing it, and
/// a disc dragged onto another slot swaps the two. Nil in-stream.
struct RingEditing {
    var pick: (Int) -> Void
    var swap: (Int, Int) -> Void
}

/// One button as the ring draws it: glyph or keycap chip, its state, and why it is dimmed.
struct SlotSpec {
    var id: String
    var label: String
    var icon: String? = nil
    /// A shortcut's chord, drawn as a stacked keycap (`ChordKeycap`).
    var keys: [String]? = nil
    var enabled = true
    var reason = ""
    /// Destructive: two presses.
    var armed = false
    /// A toggle leaves the ring open so the new state is visible (D6).
    var toggle = false
    var state = ""
}

#if os(tvOS)
/// Why the touch slots are dimmed on a device with no touch screen.
private let noTouchScreenReason = "Apple TV has no touch screen"
#elseif os(macOS)
private let noTouchScreenReason = "A Mac has no touch screen"
#endif

/// Why the two system-button slots are dimmed: they ride the wire pad, like the virtual one.
private let padOffReason = "Controller input is not forwarded this session"

func spec(_ slot: SlotId, _ cfg: OverlayConfig, _ a: RingActions) -> SlotSpec {
    switch slot {
    case .endStream:
        return SlotSpec(id: "end_stream", label: "End stream", icon: "xmark", armed: true)
    case .disconnectLinger:
        return SlotSpec(id: "disconnect_linger", label: "Disconnect, keep the game running",
                        icon: "rectangle.portrait.and.arrow.right")
    case .touchMode:
        #if os(tvOS) || os(macOS)
        return SlotSpec(id: "touch_mode", label: "Touch mode", icon: "hand.tap",
                        enabled: false, reason: noTouchScreenReason)
        #else
        let m = a.touchMode()
        let icon: String
        switch m {
        case .trackpad: icon = "hand.point.up.left"
        case .pointer: icon = "cursorarrow"
        case .touch: icon = "hand.tap"
        }
        return SlotSpec(id: "touch_mode", label: "Touch mode", icon: icon, toggle: true,
                        state: m.rawValue.capitalized)
        #endif
    case .keyboard:
        #if os(macOS)
        // No software keyboard on a Mac, and the real one already reaches the host — a slot that
        // opened nothing would read as a broken button rather than as a platform that has one.
        return SlotSpec(id: "keyboard", label: "Keyboard", icon: "keyboard",
                        enabled: false, reason: "Use this Mac's keyboard")
        #else
        return SlotSpec(id: "keyboard", label: "Keyboard", icon: "keyboard")
        #endif
    case .stats:
        return SlotSpec(id: "stats", label: "Statistics", icon: "chart.bar", toggle: true,
                        state: a.stats().label)
    case .mic:
        return SlotSpec(id: "mic", label: "Microphone", icon: a.micMuted() ? "mic.slash.fill" : "mic.fill",
                        enabled: a.micAvailable(), reason: "No microphone is running this session",
                        toggle: true, state: a.micMuted() ? "Muted" : "On")
    case .pad:
        #if os(tvOS) || os(macOS)
        return SlotSpec(id: "pad", label: "Virtual controller", icon: "gamecontroller",
                        enabled: false, reason: noTouchScreenReason)
        #else
        return SlotSpec(id: "pad", label: "Virtual controller", icon: "gamecontroller",
                        enabled: a.padAvailable(), reason: "Controller input is not forwarded this session",
                        toggle: true, state: a.padShown() ? "On" : "Off")
        #endif
    case .sendText:
        return SlotSpec(id: "send_text", label: "Send text", icon: "textformat",
                        enabled: false, reason: "Use the keyboard on this device")
    case .guide:
        return SlotSpec(id: "guide", label: "Guide button", icon: "house",
                        enabled: a.padAvailable(), reason: padOffReason)
    case .qam:
        return SlotSpec(id: "qam", label: "Quick access menu", icon: "sidebar.right",
                        enabled: a.padAvailable(), reason: padOffReason)
    case .padMouse:
        return SlotSpec(id: "pad_mouse", label: "Controller mouse", icon: "computermouse",
                        enabled: a.pointerGranted() && a.padMouseTarget() != 0,
                        reason: a.pointerGranted() ? "No controller is connected"
                            : "This host only allows controller input",
                        toggle: true, state: a.padMouseOn() ? "On" : "Off")
    case .host(let id):
        let act = a.hostActions().first { $0.id == id }
        // Three power actions, three glyphs — the same icon on all three made them one button.
        let icon: String
        switch id {
        case "power.sleep": icon = "moon.zzz.fill"
        case "power.reboot": icon = "arrow.clockwise"
        case "power.shutdown": icon = "power"
        default: icon = "bolt.fill"
        }
        return SlotSpec(id: "host:\(id)", label: act?.label ?? id, icon: icon,
                        enabled: act?.available == true,
                        reason: act?.unavailableReason ?? "This host doesn't offer it",
                        armed: act?.danger ?? true)
    case .shortcut(let id):
        let s = cfg.shortcut(id)
        let keys = s?.keys ?? []
        let known = !keys.isEmpty && keys.allSatisfy { keyVk($0) != nil }
        return SlotSpec(id: "shortcut:\(id)", label: (s?.label.isEmpty == false ? s!.label : chordChip(keys)),
                        keys: keys, enabled: known, reason: "A key in this chord is unknown")
    }
}

/// The second-press prompt for a destructive slot, in the words of the input that reaches it.
#if os(iOS)
private let againHint = "Tap again"
#else
private let againHint = "Press again"
#endif

private let ringRadius: CGFloat = 120
private let slotSize: CGFloat = 56
private let centreSize: CGFloat = 64
/// Button k lags the previous one by this much of the twist, so the ring visibly unwinds.
private let slotLag: CGFloat = 0.06

/// The ring and its sheet. Sits above the stream view; a tap on the scrim outside closes it.
struct RingOverlay: View {
    @ObservedObject var state: RingState
    let cfg: OverlayConfig
    let actions: RingActions
    /// Set by the settings editor; nil in-stream.
    var editing: RingEditing? = nil
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// The disc under an editing drag and how far it has been carried.
    @State private var drag: (k: Int, offset: CGSize)?
    /// Editing: which slot each disc is drawn in. Identity except while a swap plays: the two
    /// discs travel to each other's slots on a spring, then the blob is written and this
    /// snaps back to identity with the contents swapped — nothing visible moves at that point.
    @State private var order = Array(0..<OverlayConfig.ringSlots)
    /// False for the first frame, so a ring opened by a disc or `Select+A` springs from the
    /// centre instead of appearing in place.
    @State private var appeared = false

    /// What places the discs: the twist (frame by frame, no animation), the open (each disc on
    /// its own spring), or the wind-in. `-1` is the unrendered first frame.
    private var phase: Int {
        if !appeared { return -1 }
        if state.committed { return 1 }
        return state.closing ? 2 : 0
    }

    /// Disc `k` (6 = centre) at 0 (centre, hidden) … 1 (its ring position). Under the twist,
    /// button k lags the previous one by `slotLag` so the ring visibly unwinds; the centre
    /// arrives last.
    private func discQ(_ k: Int) -> CGFloat {
        switch phase {
        case 1: return 1
        case 0:
            let last: CGFloat = k == 6 ? 6 : 5
            return min(max((state.progress - CGFloat(k) * slotLag) / (1 - last * slotLag), 0), 1)
        default: return 0
        }
    }

    /// The open is a bouncier spring per disc, the centre first and each slot 25 ms behind
    /// the last, from wherever the twist left it. Reduce Motion keeps the plain fade.
    private func discAnimation(_ k: Int) -> Animation? {
        switch phase {
        case 1:
            if reduceMotion { return .easeOut(duration: 0.12) }
            let order = k == 6 ? 0 : k + 1
            return .spring(response: 0.4, dampingFraction: 0.62).delay(Double(order) * 0.025)
        case 2: return .easeIn(duration: 0.12)
        default: return nil
        }
    }

    private var scrimAnimation: Animation? {
        switch phase {
        case 1: return reduceMotion ? .easeOut(duration: 0.12) : .spring(response: 0.35, dampingFraction: 0.6)
        case 2: return .easeIn(duration: 0.12)
        default: return nil
        }
    }

    var body: some View {
        GeometryReader { geo in
            let margin = ringRadius + slotSize / 2 + 16
            // Clamped so the whole ring stays on screen; a stage narrower than two margins
            // (the editor's) centres it instead of pinning it to one side.
            let cx = state.centred || geo.size.width < 2 * margin
                ? geo.size.width / 2
                : min(max(state.centre.x, margin), geo.size.width - margin)
            let cy = state.centred || geo.size.height < 2 * margin
                ? geo.size.height / 2
                : min(max(state.centre.y, margin), geo.size.height - margin)
            ZStack {
                // The scrim: a tap outside closes the ring, and nothing reaches the stream
                // while it is open. The editor has no stream under it and draws none.
                Color.black.opacity(editing == nil ? 0.18 * (phase == 1 ? 1 : phase == 0 ? state.progress : 0) : 0)
                    .contentShape(Rectangle())
                    #if os(iOS) || os(macOS)
                    .onTapGesture {
                        if state.sheet { state.sheet = false } else { state.close() }
                    }
                    #endif
                    // In the editor the backdrop under the ring owns the twist and the tap.
                    .allowsHitTesting(editing == nil)
                    .animation(scrimAnimation, value: phase)
                // The discs stay in the tree at 0 so each can spring from the centre; the
                // overlay itself unmounts once closed (tenet 1).
                ForEach(0..<OverlayConfig.ringSlots, id: \.self) { k in
                    let q = discQ(k)
                    // Slot k sits at 12, 2, 4… o'clock and travels out along a short
                    // spiral that turns the way the hand turns. `order` redirects a disc to
                    // another slot while a swap plays.
                    let turn: CGFloat = state.clockwise ? -40 : 40
                    let deg = -90 + 60 * CGFloat(order[k]) + (1 - q) * turn
                    let rad = deg * .pi / 180
                    let slot = cfg.ring[k]
                    let s = slot.map { spec($0, cfg, actions) }
                    slotButton(s, size: slotSize, scale: 0.6 + 0.4 * q, alpha: q,
                               armed: s != nil && state.armed == s?.id,
                               highlighted: state.highlight == k) {
                        if let editing {
                            editing.pick(k)
                        } else if let slot, let s {
                            fire(s, slot)
                        }
                    }
                    .offset(drag?.k == k ? drag?.offset ?? .zero : .zero)
                    .position(x: cx + ringRadius * q * cos(rad), y: cy + ringRadius * q * sin(rad))
                    .allowsHitTesting(q > 0)
                    #if os(iOS) || os(macOS)
                    // Editing: one gesture owns the disc, so a drag never also fires the tap.
                    // In-stream the mask leaves the Button alone.
                    .highPriorityGesture(slotDrag(k), including: editing == nil ? .subviews : .all)
                    #endif
                    .animation(discAnimation(k), value: phase)
                    .animation(.spring(response: 0.35, dampingFraction: 0.82), value: order)
                }
                // The centre opens the sheet. In the editor it is not editable, so it sits
                // dimmed and inert rather than offering a preview nobody asked for.
                let cq = discQ(6)
                slotButton(SlotSpec(id: "more", label: "More", icon: "ellipsis"),
                           size: centreSize, scale: 0.6 + 0.4 * cq,
                           alpha: cq * (editing == nil ? 1 : 0.45), armed: false,
                           highlighted: state.highlight == 6) {
                    state.touch()
                    state.pressTick &+= 1
                    state.sheet = true
                }
                .position(x: cx, y: cy)
                .allowsHitTesting(cq > 0 && editing == nil)
                .animation(discAnimation(6), value: phase)
                // The label under the ring: a hint, else the highlighted slot's name.
                let label: String? = state.hint ?? state.highlight.flatMap { h in
                    h == 6 ? "More" : cfg.ring[h].map { spec($0, cfg, actions).label }
                }
                if let hint = label {
                    Text(hint)
                        .font(.geist(13, .medium, relativeTo: .caption))
                        .foregroundStyle(.white.opacity(0.9))
                        .padding(.horizontal, 14).padding(.vertical, 8)
                        .glassBackground(Capsule())
                        .position(x: cx, y: cy + ringRadius + slotSize)
                }
                if state.sheet {
                    RingSheet(state: state, rows: sheetRows())
                        .frame(maxHeight: geo.size.height * 0.6)
                        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottom)
                        .padding(.bottom, 16)
                        .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
            .environment(\.colorScheme, .dark) // reads over any frame
        }
        .ignoresSafeArea()
        .onAppear {
            appeared = true
            if state.native == nil { state.native = actions.currentMode() }
        }
        .onChange(of: state.navSeq) { _, _ in
            let pending = state.pendingNav
            state.pendingNav = []
            for n in pending { handleNav(n) }
        }
        .animation(reduceMotion ? .easeOut(duration: 0.12) : .smooth(duration: 0.25), value: state.sheet)
        // Idle: the exit disc's 8 s rule, for the same latency reason — unless the sheet is up.
        .task(id: "\(state.lastTouch.timeIntervalSinceReferenceDate)-\(state.sheet)") {
            guard state.committed, !state.sheet, editing == nil else { return }
            try? await Task.sleep(for: .seconds(8))
            if !Task.isCancelled { state.close() }
        }
        // An armed slot and a hint both time out.
        .task(id: "\(state.armed ?? "")-\(state.hint ?? "")-\(state.lastTouch.timeIntervalSinceReferenceDate)") {
            guard state.armed != nil || state.hint != nil else { return }
            try? await Task.sleep(for: .seconds(2))
            if !Task.isCancelled {
                state.armed = nil
                state.hint = nil
            }
        }
        #if os(iOS)
        .sensoryFeedback(.selection, trigger: state.armTick)
        .sensoryFeedback(.impact(weight: .medium), trigger: state.commitTick)
        .sensoryFeedback(.impact(weight: .light), trigger: state.pressTick)
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: state.refuseTick)
        .sensoryFeedback(.warning, trigger: state.warnTick)
        #endif
    }

    /// The pad (design §2.6): the left stick AIMS — its sector is the highlight, so the ring
    /// follows the thumb — while the D-pad steps, Right clockwise, Left anticlockwise, Up to 12
    /// o'clock, Down to 6; Y returns the highlight to the centre, A fires it (the centre opens
    /// the sheet), B closes. In the sheet, Up/Down walk the rows and Left/Right adjust one.
    private func handleNav(_ n: RingNav) {
        state.touch()
        if state.sheet {
            let rows = sheetRows()
            switch n {
            // A sheet is a list, not a dial: the six sectors fold onto its four directions.
            case .sector(let k):
                if let k { handleNav(k == 0 ? .up : k == 3 ? .down : k < 3 ? .right : .left) }
            case .up: state.sheetCursor = max(state.sheetCursor - 1, 0); state.pressTick &+= 1
            case .down: state.sheetCursor = min(state.sheetCursor + 1, max(rows.count - 1, 0)); state.pressTick &+= 1
            case .left, .right:
                if let adjust = rows[safe: state.sheetCursor]?.adjust {
                    adjust(n == .left ? -1 : 1)
                    state.pressTick &+= 1
                } else {
                    state.refuseTick &+= 1
                }
            case .confirm:
                if let row = rows[safe: state.sheetCursor] {
                    if row.enabled { state.pressTick &+= 1 } else { state.refuseTick &+= 1 }
                    row.tap()
                }
            case .back: state.sheet = false; state.pressTick &+= 1
            case .centre: break
            }
            return
        }
        let h = state.highlight ?? 6
        switch n {
        // The weapon-wheel idiom: the thumb's sector is the slot, neutral is the centre.
        case .sector(let k):
            let next = k ?? 6
            guard state.highlight != next else { return }
            state.highlight = next
            state.pressTick &+= 1
        case .right: state.highlight = h >= 6 ? 0 : (h + 1) % 6; state.pressTick &+= 1
        case .left: state.highlight = h >= 6 ? 5 : (h + 5) % 6; state.pressTick &+= 1
        case .up: state.highlight = 0; state.pressTick &+= 1
        case .down: state.highlight = 3; state.pressTick &+= 1
        case .centre: state.highlight = 6; state.pressTick &+= 1
        case .confirm:
            if h >= 6 {
                state.pressTick &+= 1
                state.sheetCursor = 0
                state.sheet = true
            } else if let slot = cfg.ring[h] {
                fire(spec(slot, cfg, actions), slot)
            } else {
                state.refuseTick &+= 1
            }
        case .back: state.close()
        }
    }

    private func fire(_ s: SlotSpec, _ slot: SlotId) {
        state.touch()
        guard s.enabled else {
            state.refuseTick &+= 1
            state.armed = nil
            state.hint = s.reason
            return
        }
        if s.armed, state.armed != s.id {
            state.warnTick &+= 1
            state.armed = s.id
            state.hint = "\(s.label)? \(againHint)"
            return
        }
        state.pressTick &+= 1
        state.armed = nil
        state.hint = nil
        switch slot {
        case .endStream: state.close(); actions.endStream()
        case .disconnectLinger: state.close(); actions.disconnectLinger()
        case .touchMode: actions.cycleTouchMode()
        case .keyboard: state.close(); actions.keyboard()
        case .stats: actions.cycleStats()
        case .mic: actions.toggleMic()
        case .pad: actions.togglePad()
        case .sendText: break
        // The host's own overlay is taking the screen: close first, like End stream.
        case .guide: state.close(); actions.tapPadButton(GamepadWire.guide)
        case .qam: state.close(); actions.tapPadButton(GamepadWire.misc1)
        case .padMouse: actions.togglePadMouse()
        case .host(let id):
            if let act = actions.hostActions().first(where: { $0.id == id }) {
                state.close()
                actions.invokeHost(act)
            }
        case .shortcut(let id):
            if let sc = cfg.shortcut(id) {
                state.close()
                actions.sendShortcut(sc.keys)
            }
        }
        if s.toggle {
            let after = spec(slot, cfg, actions)
            state.hint = "\(after.label): \(after.state)"
        }
    }

    #if os(iOS) || os(macOS)
    /// Editing: the disc's one gesture. A touch that stays put is the pick; one carried onto
    /// another slot swaps the two (§3.3); released near the centre or over its own slot it
    /// springs home and nothing changes. Masked off in-stream (see the call site).
    private func slotDrag(_ k: Int) -> some Gesture {
        DragGesture(minimumDistance: 0)
            .onChanged { v in
                guard editing != nil, state.committed else { return }
                let moved = hypot(v.translation.width, v.translation.height) > 8
                if moved || drag != nil { drag = (k, v.translation) }
            }
            .onEnded { v in
                defer {
                    withAnimation(.spring(response: 0.35, dampingFraction: 0.7)) { drag = nil }
                }
                guard let editing, state.committed else { return }
                if hypot(v.translation.width, v.translation.height) <= 8 {
                    editing.pick(k)
                    return
                }
                let rad = (-90 + 60 * CGFloat(k)) * .pi / 180
                let dx = ringRadius * cos(rad) + v.translation.width
                let dy = ringRadius * sin(rad) + v.translation.height
                guard hypot(dx, dy) > ringRadius / 2 else { return }
                let deg = atan2(dy, dx) * 180 / .pi + 90
                let target = ((Int((deg / 60).rounded()) % 6) + 6) % 6
                guard target != k, order == Array(0..<OverlayConfig.ringSlots) else { return }
                // The two discs travel to each other's slots (the dragged one from wherever it
                // was released); once they land the blob is written and `order` resets in a
                // transaction with animations off, so the swap of contents draws nothing.
                withAnimation(.spring(response: 0.35, dampingFraction: 0.82)) {
                    order.swapAt(k, target)
                }
                // Past the spring's settle: a reset while the discs still move jumped them
                // the last few points as the contents swapped, which read as a flash.
                Task { @MainActor in
                    try? await Task.sleep(for: .milliseconds(650))
                    var t = Transaction()
                    t.disablesAnimations = true
                    withTransaction(t) {
                        order = Array(0..<OverlayConfig.ringSlots)
                        editing.swap(k, target)
                    }
                }
            }
    }
    #endif

    /// One round glass button — the exit disc's primitive, at ring size. On tvOS it is a plain
    /// disc, not a Button: in-stream every press stays on the GameController path (the pad's
    /// and the remote's `RingNav`), and a focusable Button only parked the engine's idle focus
    /// on one slot, drawing the system platter behind it next to the pad's own highlight.
    private func slotButton(_ s: SlotSpec?, size: CGFloat, scale: CGFloat, alpha: CGFloat, armed: Bool,
                            highlighted: Bool = false, action: @escaping () -> Void) -> some View {
        let face = Group {
            if let keys = s?.keys {
                ChordKeycap(keys: keys)
            } else {
                Image(systemName: s?.icon ?? "circle.dashed")
                    .font(.system(size: size * 0.4, weight: .semibold))
            }
        }
        .foregroundStyle(armed ? Color.red : (s?.enabled ?? false ? Color.white : Color.white.opacity(0.35)))
        .frame(width: size, height: size)
        .glassBackground(Circle(), interactive: true)
        .overlay(Circle().strokeBorder(
            Color.white.opacity(highlighted ? 0.85 : (armed ? 0.6 : 0.18)),
            lineWidth: highlighted ? 2 : 1))
        .contentShape(Circle())
        return Group {
            #if os(tvOS)
            face
            #else
            Button(action: action) { face }
                .buttonStyle(.plain)
                // An empty slot is inert in-stream and a pick target in the editor.
                .disabled(s == nil && editing == nil)
            #endif
        }
        .scaleEffect(scale)
        .opacity(alpha)
        .accessibilityLabel(s?.label ?? "Empty slot")
        .accessibilityValue(
            armed ? "armed — press again" : (s?.enabled == false ? (s?.reason ?? "") : (s?.state ?? "")))
    }
}

/// A chord on a disc: the modifiers as the compact glyphs Apple keyboards print (⌃ ⌥ ⇧ ⌘),
/// the key large under them, shrinking to fit. One legend line ("Ctrl+Shift+Esc") ran past
/// the disc's edge. The editor previews a chord with this same view.
struct ChordKeycap: View {
    let keys: [String]

    var body: some View {
        let mods = keys.dropLast().map(modGlyph).joined()
        let key = keys.last.map(keyLegend) ?? ""
        VStack(spacing: -1) {
            if !mods.isEmpty {
                Text(mods).font(.system(size: 11, weight: .semibold))
            }
            Text(key)
                .font(.geistFixed(key.count > 3 ? 11 : 15, .semibold))
                .lineLimit(1)
                .minimumScaleFactor(0.6)
        }
        .frame(width: 42)
    }

    private func modGlyph(_ k: String) -> String {
        switch k.lowercased() {
        case "ctrl", "control": return "⌃"
        case "alt", "option": return "⌥"
        case "shift": return "⇧"
        case "win", "cmd", "super", "meta": return "⌘"
        default: return keyLegend(k)
        }
    }
}

/// One row of the sheet as data, so a finger and a pad drive the same list.
private struct SheetRowSpec {
    var header: String? = nil
    var label: String
    var value = ""
    var enabled = true
    /// Left/Right on a pad: cycle a value (the resolution rows); a tap cycles forward.
    var adjust: ((Int) -> Void)? = nil
    var tap: () -> Void
}

private let resPresets: [(String, UInt32, UInt32)] = [("1440p", 2560, 1440), ("1080p", 1920, 1080), ("720p", 1280, 720)]
private let hzPresets: [UInt32] = [120, 60]

extension Collection {
    fileprivate subscript(safe i: Index) -> Element? { indices.contains(i) ? self[i] : nil }
}

extension RingOverlay {
    /// Depth two: the complete catalogue in a fixed order (D2).
    fileprivate func sheetRows() -> [SheetRowSpec] {
        let a = actions
        let mode = a.currentMode()
        let native = state.native ?? mode
        let resLabel: String = {
            if mode.w == native.w && mode.h == native.h { return "Native (\(mode.w)×\(mode.h))" }
            return resPresets.first { $0.1 == mode.w && $0.2 == mode.h }?.0 ?? "\(mode.w)×\(mode.h)"
        }()
        func adjustRes(_ dir: Int) {
            let options = [(native.w, native.h)] + resPresets.map { ($0.1, $0.2) }
            let i = options.firstIndex { $0 == (mode.w, mode.h) } ?? 0
            let n = options.count
            let next = options[((i + dir) % n + n) % n]
            a.requestMode(next.0, next.1, mode.hz)
        }
        func adjustHz(_ dir: Int) {
            var options = [native.hz]
            for hz in hzPresets where !options.contains(hz) { options.append(hz) }
            let i = options.firstIndex(of: mode.hz) ?? 0
            let n = options.count
            a.requestMode(mode.w, mode.h, options[((i + dir) % n + n) % n])
        }
        var rows: [SheetRowSpec] = []
        rows.append(SheetRowSpec(header: "Session", label: "End stream",
                                 value: state.armed == "end_stream" ? againHint : "") { [state] in
            if state.armed == "end_stream" { state.close(); a.endStream() } else { state.warnTick &+= 1; state.armed = "end_stream" }
        })
        rows.append(SheetRowSpec(label: "Disconnect, keep the game running") { [state] in state.close(); a.disconnectLinger() })
        rows.append(SheetRowSpec(header: "Resolution", label: "Resolution", value: resLabel, adjust: adjustRes) { adjustRes(1) })
        rows.append(SheetRowSpec(label: "Refresh", value: "\(mode.hz) Hz", adjust: adjustHz) { adjustHz(1) })
        let tm = spec(.touchMode, cfg, a)
        rows.append(SheetRowSpec(header: "Input", label: tm.label,
                                 value: tm.enabled ? tm.state : tm.reason,
                                 enabled: tm.enabled) {
            if tm.enabled { a.cycleTouchMode() }
        })
        let kb = spec(.keyboard, cfg, a)
        rows.append(SheetRowSpec(label: kb.label, value: kb.enabled ? "" : kb.reason,
                                 enabled: kb.enabled) { [state] in
            guard kb.enabled else { return }
            state.close()
            a.keyboard()
        })
        let pad = spec(.pad, cfg, a)
        rows.append(SheetRowSpec(label: pad.label, value: pad.enabled ? pad.state : pad.reason, enabled: pad.enabled) {
            if pad.enabled { a.togglePad() }
        })
        for (slot, bit) in [(SlotId.guide, GamepadWire.guide), (SlotId.qam, GamepadWire.misc1)] {
            let sys = spec(slot, cfg, a)
            rows.append(SheetRowSpec(label: sys.label, value: sys.enabled ? "" : sys.reason,
                                     enabled: sys.enabled) { [state] in
                guard sys.enabled else { state.refuseTick &+= 1; return }
                state.close()
                a.tapPadButton(bit)
            })
        }
        let pm = spec(.padMouse, cfg, a)
        rows.append(SheetRowSpec(label: pm.label, value: pm.enabled ? pm.state : pm.reason, enabled: pm.enabled) {
            if pm.enabled { a.togglePadMouse() }
        })
        rows.append(SheetRowSpec(header: "View", label: "Statistics", value: a.stats().label) { a.cycleStats() })
        let mic = spec(.mic, cfg, a)
        rows.append(SheetRowSpec(header: "Audio", label: mic.label, value: mic.enabled ? mic.state : mic.reason,
                                 enabled: mic.enabled) { if mic.enabled { a.toggleMic() } })
        for (i, act) in a.hostActions().enumerated() {
            let id = "host:\(act.id)"
            let value = !act.available ? (act.unavailableReason ?? "") : (state.armed == id ? againHint : "")
            rows.append(SheetRowSpec(header: i == 0 ? "Host" : nil, label: act.label, value: value,
                                     enabled: act.available) { [state] in
                guard act.available else { state.refuseTick &+= 1; return }
                if act.danger, state.armed != id { state.warnTick &+= 1; state.armed = id } else { state.close(); a.invokeHost(act) }
            })
        }
        for (i, sc) in cfg.shortcuts.enumerated() {
            let ok = !sc.keys.isEmpty && sc.keys.allSatisfy { keyVk($0) != nil }
            rows.append(SheetRowSpec(header: i == 0 ? "Shortcuts" : nil,
                                     label: sc.label.isEmpty ? chordChip(sc.keys) : sc.label,
                                     value: chordChip(sc.keys), enabled: ok) { [state] in
                guard ok else { state.refuseTick &+= 1; return }
                state.close()
                a.sendShortcut(sc.keys)
            })
        }
        return rows
    }
}

/// The sheet as a scrollable glass panel; the pad's cursor row is tinted.
private struct RingSheet: View {
    @ObservedObject var state: RingState
    let rows: [SheetRowSpec]

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(rows.indices, id: \.self) { i in
                        let r = rows[i]
                        if let h = r.header { header(h) }
                        // tvOS: rows, not Buttons — the pad's cursor is the only navigation in-stream
                        // (see `slotButton`), and a Button's idle focus drew the platter on row 0.
                        Group {
                            #if os(tvOS)
                            row(r, at: i)
                            #else
                            Button {
                                state.touch()
                                state.sheetCursor = i
                                if r.enabled { state.pressTick &+= 1 } else { state.refuseTick &+= 1 }
                                r.tap()
                            } label: {
                                row(r, at: i)
                            }
                            .buttonStyle(.plain)
                            #endif
                        }
                        .opacity(r.enabled ? 1 : 0.45)
                        .id(i)
                    }
                }
                .padding(.vertical, 8)
            }
            // The pad walks rows the 60 % panel cannot show; the sheet follows its cursor.
            .onChange(of: state.sheetCursor) { _, i in
                withAnimation(.easeOut(duration: 0.15)) { proxy.scrollTo(i) }
            }
        }
        .frame(maxWidth: 520)
        .glassBackground(RoundedRectangle(cornerRadius: 16))
        .padding(.horizontal, 16)
    }

    private func row(_ r: SheetRowSpec, at i: Int) -> some View {
        HStack {
            Text(r.label).font(.geist(15, .regular)).foregroundStyle(.white)
            Spacer()
            if !r.value.isEmpty {
                Text(r.value).font(.geist(15, .regular)).foregroundStyle(.white.opacity(0.7))
            }
        }
        .padding(.horizontal, 20).padding(.vertical, 12)
        .background(state.sheetCursor == i ? Color.white.opacity(0.12) : Color.clear)
        .contentShape(Rectangle())
    }

    private func header(_ text: String) -> some View {
        Text(text)
            .font(.geist(12, .semibold, relativeTo: .caption))
            .foregroundStyle(.white.opacity(0.6))
            .padding(.horizontal, 20).padding(.top, 12).padding(.bottom, 4)
    }
}
#endif
