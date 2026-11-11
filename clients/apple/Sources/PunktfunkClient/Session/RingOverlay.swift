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

#if os(iOS) || os(visionOS) || os(tvOS) || os(macOS)
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
    /// The mode this session's Welcome carried, set as it starts streaming: the Resolution row's
    /// Native. Nil in the editor, which takes the live mode.
    var native: RingNav.Mode?
    /// The sheet's last mode request, which the next one steps on from until the host switches.
    /// Kept until the ring closes.
    var asked: RingNav.ModeAsk?
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

    /// A pad chord: on tvOS and macOS it toggles the ring, since neither has a finger to tap the
    /// scrim with; iOS opens only, the twist that opened it also winds it back. `pad` is the
    /// wire pad that asked.
    func chordToggle(pad: UInt32?) {
        #if os(tvOS) || os(macOS)
        toggleCentred()
        #else
        pressTick &+= 1
        openCentred()
        #endif
        if committed { opener = pad }
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
        asked = nil
    }

    func touch() { lastTouch = Date() }

    /// The pad's part of this state, as `RingNav.step` reads and leaves it.
    var navState: RingNav.State {
        get { RingNav.State(highlight: highlight, sheet: sheet, cursor: sheetCursor, armed: armed) }
        set {
            highlight = newValue.highlight
            sheet = newValue.sheet
            sheetCursor = newValue.cursor
            armed = newValue.armed
        }
    }
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
    /// One synthetic system-button tap (a `GamepadWire` bit) on a wire pad, `nil` for the
    /// first forwarded one.
    var tapPadButton: (UInt32, UInt32?) -> Void
    /// Controller mouse: the pointer grant it needs, the wire pads it acts on (a bit per pad,
    /// `0` = none), the mode they share, and the step to the next one.
    var pointerGranted: () -> Bool
    var padMouseTarget: () -> UInt16
    var padMouseMode: () -> PunktfunkConnection.PadMouseMode
    var cyclePadMouse: () -> Void
    var currentMode: () -> (w: UInt32, h: UInt32, hz: UInt32)
    var requestMode: (UInt32, UInt32, UInt32) -> Void
    var scrollInverted: () -> Bool = { false }
    var toggleScrollInversion: () -> Void = {}
    /// The game this device launched that this stream plays (`RunningGame.streamedHere`); nil
    /// offers no End game.
    var streamedGame: () -> RunningGame? = { nil }
    /// End that game on the host, then the stream.
    var endGame: () -> Void = {}
    /// Controller type: the type every forwarded pad declares, whether one is forwarded, and the
    /// step to the next type (`GamepadType.nextInRing`) for this stream.
    var padType: () -> PunktfunkConnection.GamepadType = { .auto }
    var padTypeAvailable: () -> Bool = { false }
    var cyclePadType: () -> Void = {}
    /// Release the captured mouse and keyboard, or capture them again.
    var toggleCapture: () -> Void = {}
    var toggleMouseMode: () -> Void = {}
    var toggleFullscreen: () -> Void = {}
    var minimize: () -> Void = {}
    /// Clipboard sharing: whether this host and grant allow it, and the switch.
    var clipboardAvailable: () -> Bool = { false }
    var toggleClipboard: () -> Void = {}
    /// A shortcut held past its hold: a key combination's keys, or a system button on the
    /// pad that fired it, down on the host until the shortcut lifts.
    var holdKeys: ([String], Bool) -> Void = { _, _ in }
    var holdPadButton: (UInt32, Bool, UInt32?) -> Void = { _, _, _ in }
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
    /// A pad outline drawn in place of `icon` (`GamepadType.mark`).
    var mark: Image? = nil
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
private let noTouchScreenReason = String(localized: L10n.appleRingNoTouchTv)
#elseif os(macOS)
private let noTouchScreenReason = String(localized: L10n.appleRingNoTouchMac)
#endif

/// Why the two system-button slots are dimmed: they ride the wire pad, like the virtual one.
private let padOffReason = String(localized: L10n.ringPadNotForwarded)

/// The controller-mouse slot's state; the Rust and Android rings say the same.
func padMouseState(_ mode: PunktfunkConnection.PadMouseMode) -> String {
    switch mode {
    case .off: return String(localized: L10n.ringPadMouseOff)
    case .touchpad: return String(localized: L10n.ringPadMouseTouchpad)
    case .full: return String(localized: L10n.ringPadMouseFull)
    }
}

/// The Touch mode slot's state word.
private func touchModeState(_ mode: TouchInputMode) -> String {
    switch mode {
    case .trackpad: return String(localized: L10n.playerTouchModeTrackpad)
    case .pointer: return String(localized: L10n.playerTouchModePointer)
    case .touch: return String(localized: L10n.playerTouchModeTouch)
    case .off: return String(localized: L10n.playerTouchModeOff)
    }
}

func spec(_ slot: SlotId, _ cfg: OverlayConfig, _ a: RingActions) -> SlotSpec {
    switch slot {
    case .endStream:
        return SlotSpec(id: "end_stream", label: String(localized: L10n.ringEndStream), icon: "xmark",
                        armed: true)
    case .endGame:
        return SlotSpec(id: "end_game", label: String(localized: L10n.ringEndGame), icon: "xmark.circle",
                        enabled: a.streamedGame() != nil,
                        reason: String(localized: L10n.ringEndGameUnavailable), armed: true)
    case .disconnectLinger:
        return SlotSpec(id: "disconnect_linger", label: String(localized: L10n.ringDisconnectLinger),
                        icon: "rectangle.portrait.and.arrow.right")
    case .touchMode:
        #if os(tvOS) || os(macOS)
        return SlotSpec(id: "touch_mode", label: String(localized: L10n.ringTouchMode), icon: "hand.tap",
                        enabled: false, reason: noTouchScreenReason)
        #else
        let m = a.touchMode()
        let icon: String
        switch m {
        case .trackpad: icon = "hand.point.up.left"
        case .pointer: icon = "cursorarrow"
        case .touch: icon = "hand.tap"
        case .off: icon = "hand.raised.slash"
        }
        return SlotSpec(id: "touch_mode", label: String(localized: L10n.ringTouchMode), icon: icon,
                        toggle: true, state: touchModeState(m))
        #endif
    case .keyboard:
        #if os(macOS)
        // No software keyboard on a Mac, and the real one already reaches the host — a slot that
        // opened nothing would read as a broken button rather than as a platform that has one.
        return SlotSpec(id: "keyboard", label: String(localized: L10n.ringKeyboard), icon: "keyboard",
                        enabled: false, reason: String(localized: L10n.appleRingKeyboardMac))
        #else
        return SlotSpec(id: "keyboard", label: String(localized: L10n.ringKeyboard), icon: "keyboard")
        #endif
    case .stats:
        return SlotSpec(id: "stats", label: String(localized: L10n.ringStats), icon: "chart.bar",
                        toggle: true, state: a.stats().label)
    case .mic:
        return SlotSpec(id: "mic", label: String(localized: L10n.ringMic),
                        icon: a.micMuted() ? "mic.slash.fill" : "mic.fill",
                        enabled: a.micAvailable(), reason: String(localized: L10n.ringMicUnavailable),
                        toggle: true,
                        state: a.micMuted() ? String(localized: L10n.ringMicMuted)
                            : String(localized: L10n.ringMicOn))
    case .pad:
        #if os(tvOS) || os(macOS)
        return SlotSpec(id: "pad", label: String(localized: L10n.ringPad), icon: "gamecontroller",
                        enabled: false, reason: noTouchScreenReason)
        #else
        return SlotSpec(id: "pad", label: String(localized: L10n.ringPad), icon: "gamecontroller",
                        enabled: a.padAvailable(), reason: String(localized: L10n.ringPadNotForwarded),
                        toggle: true,
                        state: a.padShown() ? String(localized: L10n.ringPadOn)
                            : String(localized: L10n.ringPadOff))
        #endif
    case .sendText:
        return SlotSpec(id: "send_text", label: String(localized: L10n.ringSendText), icon: "textformat",
                        enabled: false, reason: String(localized: L10n.ringSendTextUnavailable))
    case .guide:
        return SlotSpec(id: "guide", label: String(localized: L10n.ringGuide), icon: "house",
                        enabled: a.padAvailable(), reason: padOffReason)
    case .qam:
        return SlotSpec(id: "qam", label: String(localized: L10n.ringQam), icon: "sidebar.right",
                        enabled: a.padAvailable(), reason: padOffReason)
    case .padMouse:
        return SlotSpec(id: "pad_mouse", label: String(localized: L10n.ringPadMouse), icon: "computermouse",
                        enabled: a.pointerGranted() && a.padMouseTarget() != 0,
                        reason: a.pointerGranted() ? String(localized: L10n.ringNoController)
                            : String(localized: L10n.ringControllerOnly),
                        toggle: true, state: padMouseState(a.padMouseMode()))
    case .padType:
        // Automatic keeps a plain controller; a picked type shows its outline.
        let type = a.padType()
        return SlotSpec(id: "pad_type", label: String(localized: L10n.ringPadType), icon: "dpad",
                        mark: type.mark,
                        enabled: a.padTypeAvailable(), reason: String(localized: L10n.ringNoController),
                        toggle: true, state: type.ringLabel)
    case .streamMute:
        return SlotSpec(id: "stream_mute", label: String(localized: L10n.ringStreamMute),
                        icon: "speaker.wave.2",
                        enabled: false, reason: String(localized: L10n.appleRingStreamMuteUnavailable))
    case .swapScreens:
        return SlotSpec(id: "swap_screens", label: String(localized: L10n.ringSwapScreens),
                        icon: "arrow.up.arrow.down",
                        enabled: false, reason: String(localized: L10n.ringSwapScreensUnavailable))
    case .releaseInput:
        return SlotSpec(id: slot.id, label: slot.label(), icon: "lock.open",
                        enabled: slot.runsHere, reason: String(localized: L10n.ringNoPointerCapture))
    case .mouseMode:
        return SlotSpec(id: slot.id, label: slot.label(), icon: "cursorarrow.rays",
                        enabled: slot.runsHere, reason: String(localized: L10n.appleRingMacOnly))
    case .fullscreen:
        return SlotSpec(id: slot.id, label: slot.label(), icon: "arrow.up.left.and.arrow.down.right",
                        enabled: slot.runsHere, reason: String(localized: L10n.appleRingMacOnly))
    case .minimize:
        return SlotSpec(id: slot.id, label: slot.label(), icon: "minus.square",
                        enabled: slot.runsHere, reason: String(localized: L10n.appleRingMacOnly))
    case .clipboard:
        return SlotSpec(id: slot.id, label: slot.label(), icon: "doc.on.clipboard",
                        enabled: slot.runsHere && a.clipboardAvailable(),
                        reason: String(localized: slot.runsHere ? L10n.ringHostNotOffered
                            : L10n.ringClipboardUnavailable))
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
                        reason: act?.unavailableReason ?? String(localized: L10n.ringHostNotOffered),
                        armed: act?.danger ?? true)
    case .shortcut(let id):
        let s = cfg.shortcut(id)
        let keys = s?.keys ?? []
        let known = !keys.isEmpty && keys.allSatisfy { keyVk($0) != nil }
        return SlotSpec(id: "shortcut:\(id)", label: (s?.label.isEmpty == false ? s!.label : chordChip(keys)),
                        keys: keys, enabled: known, reason: String(localized: L10n.ringChordUnknown))
    }
}

/// What a slot does once it may fire: the dial's press and a shortcut share it. The ring closes
/// first wherever the action takes the screen, the pointer or the stream. `pad` is the wire pad
/// that asked; `nil` falls back to the dial's opener.
@MainActor
func perform(_ slot: SlotId, _ cfg: OverlayConfig, _ a: RingActions, _ ring: RingState, pad: UInt32? = nil) {
    switch slot {
    case .endStream: ring.close(); a.endStream()
    case .endGame: ring.close(); a.endGame()
    case .disconnectLinger: ring.close(); a.disconnectLinger()
    case .touchMode: a.cycleTouchMode()
    case .keyboard: ring.close(); a.keyboard()
    case .stats: a.cycleStats()
    case .mic: a.toggleMic()
    case .pad: a.togglePad()
    case .sendText, .streamMute, .swapScreens: break
    // The host's own overlay is taking the screen: close first, like End stream.
    case .guide, .qam:
        let target = pad ?? ring.opener
        ring.close()
        a.tapPadButton(slot == .guide ? GamepadWire.guide : GamepadWire.misc1, target)
    case .padMouse: a.cyclePadMouse()
    case .padType: a.cyclePadType()
    case .releaseInput: ring.close(); a.toggleCapture()
    case .mouseMode: ring.close(); a.toggleMouseMode()
    case .fullscreen: ring.close(); a.toggleFullscreen()
    case .minimize: ring.close(); a.minimize()
    case .clipboard: a.toggleClipboard()
    case .host(let id):
        if let act = a.hostActions().first(where: { $0.id == id }) {
            ring.close()
            a.invokeHost(act)
        }
    case .shortcut(let id):
        if let sc = cfg.shortcut(id) {
            ring.close()
            a.sendShortcut(sc.keys)
        }
    }
}

/// The second-press prompt in the sheet's value column.
private var againHint: String { String(localized: L10n.ringPressAgain) }

/// The ring's question for a destructive slot: one whole message per slot, chosen by id.
func confirmHint(_ s: SlotSpec) -> String {
    switch s.id {
    case "end_stream": return String(localized: L10n.ringEndStreamConfirm)
    case "end_game": return String(localized: L10n.ringEndGameConfirm)
    case "host:power.sleep": return String(localized: L10n.ringHostSleepConfirm)
    case "host:power.reboot": return String(localized: L10n.ringHostRebootConfirm)
    case "host:power.shutdown": return String(localized: L10n.ringHostShutdownConfirm)
    default: return L10n.ringHostActionConfirm(action: s.label)
    }
}

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
    #if os(visionOS)
    /// On the Vision Pro's screen the ring is an attachment over the picture, with no scrim.
    @Environment(\.inAttachment) private var inAttachment
    #else
    private let inAttachment = false
    #endif
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
                // while it is open. The editor has no stream under it and draws none; an
                // attachment on the Vision Pro's screen draws none either (a square would show),
                // the picture closes the ring there.
                Color.black.opacity(
                    editing == nil && !inAttachment
                        ? 0.18 * (phase == 1 ? 1 : phase == 0 ? state.progress : 0) : 0)
                    .contentShape(Rectangle())
                    #if os(iOS) || os(visionOS) || os(macOS)
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
                        } else if slot != nil {
                            fire(k)
                        }
                    }
                    .offset(drag?.k == k ? drag?.offset ?? .zero : .zero)
                    .position(x: cx + ringRadius * q * cos(rad), y: cy + ringRadius * q * sin(rad))
                    .allowsHitTesting(q > 0)
                    #if os(iOS) || os(visionOS) || os(macOS)
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
                slotButton(SlotSpec(id: "more", label: String(localized: L10n.ringMore), icon: "ellipsis"),
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
                    h == 6 ? String(localized: L10n.ringMore) : cfg.ring[h].map { spec($0, cfg, actions).label }
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
        .onAppear { appeared = true }
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
        #if os(iOS) || os(visionOS)
        .sensoryFeedback(.selection, trigger: state.armTick)
        .sensoryFeedback(.impact(weight: .medium), trigger: state.commitTick)
        .sensoryFeedback(.impact(weight: .light), trigger: state.pressTick)
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: state.refuseTick)
        .sensoryFeedback(.warning, trigger: state.warnTick)
        #endif
    }

    /// A pad or remote press (design §2.6), decided by `RingNav.step` over the discs, or the
    /// sheet's rows while it is up.
    private func handleNav(_ n: RingNav) {
        state.touch()
        let rows = state.sheet ? sheetRows() : []
        let facts = state.sheet ? rows.map(\.nav) : ringRows()
        apply(RingNav.step(state: state.navState, event: n, rows: facts), rows)
    }

    /// A finger on disc `k`: the pad's A on that disc.
    private func fire(_ k: Int) {
        state.touch()
        apply(RingNav.fire(state: state.navState, slot: k, rows: ringRows()), [])
    }

    /// The six discs as `RingNav` reads them; nil for an empty slot.
    private func ringRows() -> [RingNav.Row?] {
        cfg.ring.map { slot in
            slot.map {
                let s = spec($0, cfg, actions)
                return RingNav.Row(id: s.id, enabled: s.enabled, twoPress: s.armed)
            }
        }
    }

    /// Takes a step's state, then performs its effects in order. A toggle that ran leaves the
    /// ring open with its new state as the hint.
    private func apply(_ step: (state: RingNav.State, effects: [RingNav.Effect]), _ rows: [SheetRowSpec]) {
        state.navState = step.state
        for effect in step.effects {
            switch effect {
            case .haptic(.press): state.pressTick &+= 1
            case .haptic(.refuse): state.refuseTick &+= 1
            case .haptic(.warn): state.warnTick &+= 1
            case .explain(let k): state.hint = cfg.ring[k].map { spec($0, cfg, actions).reason }
            case .ask(let k): state.hint = cfg.ring[k].map { confirmHint(spec($0, cfg, actions)) }
            case .fire(let k):
                guard let slot = cfg.ring[k] else { break }
                state.hint = nil
                let toggle = spec(slot, cfg, actions).toggle
                perform(slot, cfg, actions, state)
                if toggle {
                    let after = spec(slot, cfg, actions)
                    state.hint = "\(after.label): \(after.state)"
                }
            case .adjust(let dir): rows[safe: state.sheetCursor]?.adjust?(dir)
            case .tap: rows[safe: state.sheetCursor]?.tap()
            case .close: state.close()
            }
        }
    }

    #if os(iOS) || os(visionOS) || os(macOS)
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
            } else if let mark = s?.mark {
                mark.resizable().scaledToFit().frame(width: size * 0.5, height: size * 0.5)
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
        .accessibilityLabel(s?.label ?? String(localized: L10n.ringSlotEmpty))
        .accessibilityValue(
            armed ? String(localized: L10n.ringArmedA11y)
                : (s?.enabled == false ? (s?.reason ?? "") : (s?.state ?? "")))
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

    var nav: RingNav.Row { RingNav.Row(enabled: enabled, adjustable: adjust != nil) }

    static func scrollInversion(_ actions: RingActions) -> SheetRowSpec {
        let granted = actions.pointerGranted()
        return SheetRowSpec(label: String(localized: L10n.ringSheetInvertScroll),
                            value: granted
                                ? String(localized: actions.scrollInverted() ? L10n.ringScrollInvertOn
                                    : L10n.ringScrollInvertOff)
                                : String(localized: L10n.ringPointerNotAllowed),
                            enabled: granted) {
            if actions.pointerGranted() { actions.toggleScrollInversion() }
        }
    }
}

extension Collection {
    fileprivate subscript(safe i: Index) -> Element? { indices.contains(i) ? self[i] : nil }
}

extension RingOverlay {
    /// Depth two: the complete catalogue in a fixed order (D2). The mode rows step through
    /// `RingNav.nextMode` and `.nextRefresh`.
    fileprivate func sheetRows() -> [SheetRowSpec] {
        let a = actions
        let live = a.currentMode()
        let mode = RingNav.Mode(width: live.w, height: live.h, hz: live.hz)
        let native = state.native ?? mode
        let resLabel: String = {
            if mode.width == native.width && mode.height == native.height {
                return L10n.ringSheetNative(width: String(mode.width), height: String(mode.height))
            }
            return RingNav.resolutions.first { $0.width == mode.width && $0.height == mode.height }?.name
                ?? "\(mode.width)×\(mode.height)"
        }()
        func request(_ next: (RingNav.Mode) -> RingNav.Mode) {
            let target = next(RingNav.origin(live: mode, asked: state.asked))
            state.asked = RingNav.ModeAsk(from: mode, to: target)
            a.requestMode(target.width, target.height, target.hz)
        }
        func adjustRes(_ dir: Int) { request { RingNav.nextMode(mode: $0, native: native, dir: dir) } }
        func adjustHz(_ dir: Int) { request { RingNav.nextRefresh(mode: $0, native: native, dir: dir) } }
        var rows: [SheetRowSpec] = []
        rows.append(SheetRowSpec(header: String(localized: L10n.ringGroupSession),
                                 label: String(localized: L10n.ringEndStream),
                                 value: state.armed == "end_stream" ? againHint : "") { [state] in
            if state.armed == "end_stream" { state.close(); a.endStream() } else { state.warnTick &+= 1; state.armed = "end_stream" }
        })
        if a.streamedGame() != nil {
            rows.append(SheetRowSpec(label: String(localized: L10n.ringEndGame),
                                     value: state.armed == "end_game" ? againHint : "") { [state] in
                if state.armed == "end_game" { state.close(); a.endGame() } else { state.warnTick &+= 1; state.armed = "end_game" }
            })
        }
        rows.append(SheetRowSpec(label: String(localized: L10n.ringDisconnectLinger)) { [state] in
            state.close()
            a.disconnectLinger()
        })
        rows.append(SheetRowSpec(header: String(localized: L10n.ringGroupResolution),
                                 label: String(localized: L10n.ringSheetResolution),
                                 value: resLabel, adjust: adjustRes) { adjustRes(1) })
        rows.append(SheetRowSpec(label: String(localized: L10n.ringSheetRefresh),
                                 value: L10n.commonHz(value: String(mode.hz)), adjust: adjustHz) { adjustHz(1) })
        let tm = spec(.touchMode, cfg, a)
        rows.append(SheetRowSpec(header: String(localized: L10n.ringGroupInput), label: tm.label,
                                 value: tm.enabled ? tm.state : tm.reason,
                                 enabled: tm.enabled) {
            if tm.enabled { a.cycleTouchMode() }
        })
        rows.append(.scrollInversion(a))
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
                let opener = state.opener
                state.close()
                a.tapPadButton(bit, opener)
            })
        }
        let pm = spec(.padMouse, cfg, a)
        rows.append(SheetRowSpec(label: pm.label, value: pm.enabled ? pm.state : pm.reason, enabled: pm.enabled) {
            if pm.enabled { a.cyclePadMouse() }
        })
        let pt = spec(.padType, cfg, a)
        rows.append(SheetRowSpec(label: pt.label, value: pt.enabled ? pt.state : pt.reason, enabled: pt.enabled) {
            if pt.enabled { a.cyclePadType() }
        })
        rows.append(SheetRowSpec(header: String(localized: L10n.ringGroupView),
                                 label: String(localized: L10n.ringStats),
                                 value: a.stats().label) { a.cycleStats() })
        let mic = spec(.mic, cfg, a)
        rows.append(SheetRowSpec(header: String(localized: L10n.ringGroupAudio), label: mic.label,
                                 value: mic.enabled ? mic.state : mic.reason,
                                 enabled: mic.enabled) { if mic.enabled { a.toggleMic() } })
        for (i, act) in a.hostActions().enumerated() {
            let id = "host:\(act.id)"
            let value = !act.available ? (act.unavailableReason ?? "") : (state.armed == id ? againHint : "")
            rows.append(SheetRowSpec(header: i == 0 ? String(localized: L10n.ringGroupHost) : nil,
                                     label: act.label, value: value,
                                     enabled: act.available) { [state] in
                guard act.available else { state.refuseTick &+= 1; return }
                if act.danger, state.armed != id { state.warnTick &+= 1; state.armed = id } else { state.close(); a.invokeHost(act) }
            })
        }
        for (i, sc) in cfg.shortcuts.enumerated() {
            let ok = !sc.keys.isEmpty && sc.keys.allSatisfy { keyVk($0) != nil }
            rows.append(SheetRowSpec(header: i == 0 ? String(localized: L10n.ringGroupShortcuts) : nil,
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
