// Input capture → punktfunk/1 datagrams.
//
// macOS takes mouse motion and buttons from StreamLayerView's NSEvent monitor under cursor
// disassociation, as the mouse's own counts before macOS acceleration: the host applies its
// pointer curve once. GCMouse there can stay silent and never sees a trackpad, so its motion
// and button handlers are not installed. iOS keeps GCMouse, raw under pointer lock.
// GCKeyboard gives HID keycodes, mapped to the Windows VK space the host's tables consume.
// Fractional motion carries per axis so slow, sub-pixel moves survive the integer wire.
//
// GC delivers only while the app is active, so everything held is released on
// didResignActive, on stop() and on each release of capture. `forwarding` follows
// StreamLayerView's capture state: while released, nothing reaches the host and keys travel
// the responder chain. GC handlers and notifications fire on the main queue, so the state
// here needs no locking.
//
// GCMouse.current and GCKeyboard.coalesced have one handler slot each, so one InputCapture
// is live per process. `activeCapture` keeps a stale capture's stop() off a newer one's handlers.

#if os(macOS)
import AppKit
#endif
#if canImport(UIKit)
import UIKit
#endif
import Foundation
import GameController
import PunktfunkCore
import PunktfunkShared
import os

/// Diagnostic logging for the input path. Off by default (input is high-rate); set
/// PUNKTFUNK_INPUT_DEBUG=1 in the environment to surface whether relative motion + buttons
/// are actually being SENT to the host without needing host-side logs. Motion is throttled
/// to once per second (see `motionDebugTick`); buttons log every transition.
private let inputLog = ClientLog(category: "input")
private let inputDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_INPUT_DEBUG"] == "1"

@MainActor
public final class InputCapture {
    private static weak var activeCapture: InputCapture?

    private let connection: PunktfunkConnection
    private var observers: [NSObjectProtocol] = []
    private var mice: [GCMouse] = []
    private var keyboards: [GCKeyboard] = []
    #if os(macOS)
    private var keyEventMonitor: Any?
    /// This capture holds `SystemHotKeys` off (see `holdSystemShortcuts`).
    private var holdsSystemShortcuts = false
    #endif

    // Main-queue-only state (see header comment).
    private var residualX: Float = 0
    private var residualY: Float = 0
    /// Scroll quantization and open-axis tracking for every pointer scroll path — wheel,
    /// trackpad, GCMouse — shared so a source switch on one axis cancels the previous source.
    private var scrollCapture = ScrollCapture()
    private var pressedVKs: Set<UInt32> = []
    private var pressedButtons: Set<UInt32> = []
    /// One-shot: the left click that engaged capture belongs to the local UI — GC sees
    /// it at the HID layer regardless, so its press AND release are dropped here.
    private var suppressedButton: UInt32?
    /// Whether the suppressed button's press has already been eaten — see `sendButton`.
    private var suppressedDownSeen = false

    /// Throttle for the PUNKTFUNK_INPUT_DEBUG motion counter (motion is high-rate — we log
    /// a rolling count + the last delta once per second, never per event). Main-queue only.
    private var motionDebugCount = 0
    private var motionDebugTick = Date.distantPast

    /// One-shot twin of `suppressedButton` for the ⌘⎋ toggle: the physical Esc also
    /// reaches GCKeyboard, racing the NSEvent monitor — latched here so it can't type
    /// an Escape into the host in either toggle direction.
    private var suppressedVK: UInt32?
    /// Physical ⌘ keys currently held (tracked even while released — the ⌘⎋ toggle and
    /// its Esc suppression need it in both states).
    private var cmdKeysDown: Set<UInt32> = []

    #if os(macOS)
    /// Windows VKs the ⌘-chord passthrough sent DOWN (see the keyDown monitor). AppKit keeps a
    /// keyUp from the responder chain while Command is held, so the monitor sends these releases
    /// itself (`takeRelease`). Whatever is still here when the last ⌘ comes up is released then
    /// (`flushCommandChord`), so a chord key never outlives its ⌘ on the host.
    private var commandChordVKs: Set<UInt32> = []
    /// The Caps Lock state the host was last sent, `nil` before the first. See `capsLockTap`.
    private var capsLockOn: Bool?

    #endif

    #if !os(macOS)
    /// The key currently auto-repeating, and the timer driving it. iOS/tvOS only — see
    /// `startAutoRepeat`. Main-queue only, like every other field here.
    private var autoRepeatVK: UInt32?
    private var autoRepeatTimer: Timer?
    #endif

    /// The keyboard's shortcuts (design/client-shortcuts.md §4), fed every physical key in both
    /// capture states. Built at `start`, when `onShortcut` is known.
    private var keyEngine = Shortcuts.Engine([], device: .keyboard)
    /// Physical keys the engine counts as down (Windows VKs, both sides of each modifier).
    private var shortcutKeys: Set<UInt32> = []
    /// Keys whose press fired a shortcut: their repeats and their release stay off the host.
    private var firedVKs: Set<UInt32> = []

    /// While true, mouse/keyboard flow to the host and key NSEvents are swallowed
    /// locally; while false the user is interacting with the local UI (dragging the
    /// window, clicking the HUD) and nothing is forwarded. Main-queue only.
    public private(set) var forwarding = false

    /// iPad pointer routing (the StreamViewController mirrors the scene's live pointer-lock
    /// state into this). GCMouse only delivers relative deltas + buttons while the scene is
    /// LOCKED, so this is true then and the GCMouse handlers forward. When the scene can't
    /// lock (Stage Manager, not frontmost, iPhone) the iPad routes the mouse through UIKit's
    /// pointer path as ABSOLUTE moves (`sendMouseAbs`) instead — so this goes false, gating
    /// GCMouse off and enabling the absolute path, the two never double-sending. Moot on
    /// macOS (no GCMouse handlers installed; `sendMouseAbs` is never called there). Main-queue.
    public var gcMouseForwarding = false

    /// Fired on ⌘⎋ (the capture toggle — detected here so it works in both states; the
    /// event itself is swallowed). Main queue.
    public var onToggleCapture: (() -> Void)?


    #if os(macOS)
    /// Whether a key event belongs to this capture's window. Every capture's key monitor sees
    /// every key the app receives, so one that swallowed a chord for another window would
    /// leave that window's stream without it. nil = every event.
    public var ownsEvent: ((NSEvent) -> Bool)?
    #endif

    /// A keyboard shortcut fired (by default the ⌃⌥⇧ letters, Ctrl+Alt+Shift elsewhere), in
    /// either capture state; the key is swallowed. The only dispatch of a bound press: the macOS
    /// monitor runs before the Stream menu, which carries no key equivalents elsewhere. Main queue.
    public var onShortcut: ((Shortcuts.Action, Shortcuts.Phase) -> Void)?

    /// Fired on ⌃⌘F (macOS) — toggle the streaming window in/out of fullscreen. Detected in the
    /// monitor only WHILE FORWARDING with `inhibit_shortcuts` off: a captured stream view swallows
    /// keys, so the Stream menu's identical ⌃⌘F equivalent never reaches it. With the setting on,
    /// ⌃⌘F is the host's like any ⌘ chord; released, the menu handles it. Main queue.
    public var onToggleFullscreen: (() -> Void)?


    /// The engine's id for a physical key: either side of a modifier is one key.
    nonisolated static func shortcutKey(_ vk: UInt32) -> UInt32 {
        switch vk {
        case 0xA0, 0xA1: return 0x10 // Shift
        case 0xA2, 0xA3: return 0x11 // Control
        case 0xA4, 0xA5: return 0x12 // Option = Alt
        case 0x5C: return 0x5B // either ⌘
        default: return vk
        }
    }

    /// One physical key through the keyboard's shortcut engine. True when a shortcut took it:
    /// its press fired one, so the press and its release stay off the host. Modifiers always
    /// pass; a firing chord lifts the ones the host holds.
    private func shortcutTakes(_ vk: UInt32, down: Bool) -> Bool {
        let id = Self.shortcutKey(vk)
        let otherSide = shortcutKeys.contains { $0 != vk && Self.shortcutKey($0) == id }
        if down { shortcutKeys.insert(vk) } else { shortcutKeys.remove(vk) }
        var fired = false
        if !otherSide {
            for out in keyEngine.step(down ? .press(id) : .release(id), at: .now) {
                switch out {
                case .lift(let ids):
                    for held in pressedVKs where ids.contains(Self.shortcutKey(held)) {
                        pressedVKs.remove(held)
                        emitKey(held, down: false)
                    }
                case .fire(let action, let phase):
                    fired = true
                    onShortcut?(action, phase)
                case .forward:
                    break
                }
            }
        }
        if down, fired { firedVKs.insert(vk) }
        return down ? fired : firedVKs.remove(vk) != nil
    }

    /// The keyboard engine forgets every key: GC delivers nothing while inactive, so a key
    /// released in another app would otherwise stay down here.
    private func forgetShortcutKeys() {
        _ = keyEngine.flush()
        shortcutKeys.removeAll()
        firedVKs.removeAll()
    }

    /// Fired when a newer InputCapture takes the process-global GC handler slots (the
    /// singletons hold ONE handler each): the preempted owner must drop its capture
    /// state — its handlers are gone, so it would otherwise sit "captured" with dead
    /// input. Main queue.
    public var onPreempted: (() -> Void)?

    public init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    /// Gate the forwarding without detaching the GC handlers. `suppressClick` marks the
    /// transition as click-driven: that click's press/release are not forwarded. Every
    /// transition to false flushes held keys/buttons host-side.
    public func setForwarding(_ on: Bool, suppressClick: Bool = false) {
        if on {
            forwarding = true
            suppressedButton = suppressClick ? 1 : nil
            suppressedDownSeen = false
            #if os(macOS)
            holdSystemShortcuts()
            #endif
        } else if forwarding {
            #if os(macOS)
            releaseSystemShortcuts()
            #endif
            releaseAll()
            forwarding = false
            suppressedButton = nil
            suppressedDownSeen = false
        }
    }

    /// The engage click is over (its NSEvent mouseUp processed) — stop suppressing.
    /// Backstop for the GC-vs-NSEvent ordering where both halves of the click landed
    /// before mouseDown armed the latch, which would otherwise eat the next real click.
    public func endClickSuppression() {
        suppressedButton = nil
        suppressedDownSeen = false
    }

    /// Begin forwarding the current (and future) mouse/keyboard to the host. Steals the
    /// global GC handler slots from any previous capture (one live capture per process),
    /// notifying it via `onPreempted` so its owner releases its capture state.
    public func start() {
        if let previous = Self.activeCapture, previous !== self {
            // Drop the previous owner's device lists first: its stop() must not be able
            // to nil out the handler slots this capture is about to claim.
            previous.mice.removeAll()
            previous.keyboards.removeAll()
            previous.onPreempted?()
        }
        Self.activeCapture = self
        keyEngine = Shortcuts.Engine(
            connection.settings.resolvedShortcuts.runnable(dial: onShortcut != nil), device: .keyboard)
        forgetShortcutKeys()
        // Attach EVERY connected mouse, not just GCMouse.current. With two pointing devices (e.g.
        // the iPad's own Magic Keyboard trackpad AND a Universal Control "V-UC Automouse"), only one
        // is `current` at a time; attaching just that one left the OTHER device's motion handler
        // uninstalled, so moving it did nothing. Each GCMouse delivers its own deltas through its own
        // handler, so handling all of them lets either device drive. New arrivals are caught by the
        // GCMouseDidConnect observer below.
        for mouse in GCMouse.mice() { attach(mouse: mouse) }
        if let keyboard = GCKeyboard.coalesced { attach(keyboard: keyboard) }
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCMouseDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            if let m = n.object as? GCMouse { MainActor.assumeIsolated { self?.attach(mouse: m) } }
        })
        #if os(iOS) || os(visionOS)
        // The mouse can become the *current* one after it connected (and after our start()
        // already ran) — re-attach on that too so a launch-time race doesn't leave the iOS
        // GCMouse path without handlers. attach() is idempotent (dedupes by identity).
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCMouseDidBecomeCurrent, object: nil, queue: .main
        ) { [weak self] n in
            if let m = n.object as? GCMouse { MainActor.assumeIsolated { self?.attach(mouse: m) } }
        })
        #endif
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            if let k = n.object as? GCKeyboard {
                MainActor.assumeIsolated { self?.attach(keyboard: k) }
            }
        })
        #if !os(macOS)
        // A device that drops mid-press never sends its releases, and the repeat ticker would
        // keep typing a held key.
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCMouseDidDisconnect, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.releaseMouseButtons() }
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidDisconnect, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.releaseAll()
                self?.forgetShortcutKeys()
            }
        })
        #endif
        // Focus loss: GC stops delivering, so release everything still held host-side.
        #if os(macOS)
        let resignActive = NSApplication.didResignActiveNotification
        #else
        let resignActive = UIApplication.willResignActiveNotification
        #endif
        observers.append(NotificationCenter.default.addObserver(
            forName: resignActive, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.releaseAll()
                self?.forgetShortcutKeys()
            }
        })
        // Runs before any menu key equivalent and StreamLayerView's keyDown, the host's only key
        // path on macOS, so an event it swallows must be handled or forwarded right here.
        // ⌘⎋ is the client's in both states, then the keyboard shortcuts. Every other ⌘ chord,
        // ⌃⌘F included, is the host's while captured (`forwardsCommandChord`).
        #if os(macOS)
        keyEventMonitor = NSEvent.addLocalMonitorForEvents(
            matching: [.keyDown, .keyUp, .flagsChanged]
        ) { [weak self] event in
            guard let self, self.ownsEvent?(event) ?? true else { return event }
            // Modifiers only feed the shortcut engine; StreamLayerView still sends them.
            if event.type == .flagsChanged {
                if let (vk, down) = Self.resolveModifier(
                    keyCode: event.keyCode, rawFlags: UInt(event.modifierFlags.rawValue),
                    isDown: { self.shortcutKeys.contains($0) }) {
                    _ = self.shortcutTakes(vk, down: down)
                }
                return event
            }
            if event.type == .keyUp {
                if let vk = Self.keyCodeToVK[event.keyCode], self.shortcutTakes(vk, down: false) {
                    return nil
                }
                guard let vk = Self.takeRelease(
                    event, forwarding: self.forwarding, pressedVKs: self.pressedVKs,
                    chordVKs: &self.commandChordVKs)
                else { return event }
                self.sendKey(vk, down: false)
                return nil // The monitor owns this release; the responder must not send it again
            }
            let flags = Self.chordFlags(event)
            // A held chord auto-repeats: swallow the repeats and act on the first press only.
            @MainActor func take(_ vk: UInt32, _ action: (() -> Void)?) -> NSEvent? {
                if !event.isARepeat { self.suppressedVK = vk; action?() }
                return nil
            }
            if event.keyCode == 53 /* Esc */, flags == .command {
                return take(0x1B, self.onToggleCapture) // VK_ESC
            }
            // A modifier let go while another window was key never reached the engine.
            for (flag, vks) in Self.modifierVKs where !event.modifierFlags.contains(flag) {
                for vk in vks where self.shortcutKeys.contains(vk) { _ = self.shortcutTakes(vk, down: false) }
            }
            // Swallowed, so a fired shortcut never beeps; a held one swallows its repeats.
            if let vk = Self.keyCodeToVK[event.keyCode],
               event.isARepeat ? self.firedVKs.contains(vk) : self.shortcutTakes(vk, down: true) {
                return nil
            }
            // Every OTHER ⌘ chord is the HOST's while captured, or the menu takes ⌘Q first. It is
            // sent from here, since returning nil also skips StreamLayerView's keyDown; a chord
            // with no host VK is swallowed. The ⌘ itself already went out as a flagsChanged.
            if self.forwarding, flags.contains(.command), Self.forwardsCommandChord(
                keyCode: event.keyCode, flags: flags, forwarding: self.forwarding,
                inhibitShortcuts: self.connection.settings.inhibitShortcuts
            ) {
                if let vk = Self.keyCodeToVK[event.keyCode] { self.sendCommandChordKey(vk) }
                return nil
            }
            // Captured with `inhibit_shortcuts` off, ⌃⌘F is the client's. The captured view swallows
            // the menu's identical equivalent; the F is latched so its keyUp can't type into the
            // host. keyCode 3 = kVK_ANSI_F (layout-independent).
            if self.forwarding, flags == [.control, .command], event.keyCode == 3 /* F */ {
                return take(0x46, self.onToggleFullscreen) // VK_F
            }
            return event
        }
        #endif
    }

    /// Take the global handler slots back from the newer capture that preempted this one, so
    /// the stream the player turned back to owns the keyboard and mouse again. No-op while this
    /// capture holds them. `stop` leaves a newer owner's handlers alone.
    public func reclaim() {
        guard Self.activeCapture !== self else { return }
        stop()
        start()
    }

    public func stop() {
        releaseAll()
        forgetShortcutKeys()
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        #if os(macOS)
        if let monitor = keyEventMonitor {
            NSEvent.removeMonitor(monitor)
            keyEventMonitor = nil
        }
        releaseSystemShortcuts()
        #endif
        // Don't clobber the handlers if a newer capture has taken the global devices.
        if Self.activeCapture === self || Self.activeCapture == nil {
            for mouse in mice {
                guard let input = mouse.mouseInput else { continue }
                input.mouseMovedHandler = nil
                input.leftButton.pressedChangedHandler = nil
                input.rightButton?.pressedChangedHandler = nil
                input.middleButton?.pressedChangedHandler = nil
                input.auxiliaryButtons?.forEach { $0.pressedChangedHandler = nil }
                input.scroll.valueChangedHandler = nil
            }
            for keyboard in keyboards {
                keyboard.keyboardInput?.keyChangedHandler = nil
            }
            Self.activeCapture = nil
        }
        mice.removeAll()
        keyboards.removeAll()
        #if !os(macOS)
        stopAutoRepeat()
        #endif
    }

    isolated deinit { stop() }

    /// Send release events for everything currently held, and drop the motion residuals
    /// and modifier/latch tracking (GC delivers nothing while inactive, so a ⌘ released
    /// in another app would otherwise stay "held" here forever — hijacking Esc).
    private func releaseAll() {
        #if !os(macOS)
        stopAutoRepeat() // before the releases below, so the ticker can't outlive the key-up
        #endif
        cmdKeysDown.removeAll()
        // The shortcut engine keeps its keys: Release input itself lands here, with its
        // modifiers still held. A lost device forgets them (`forgetShortcutKeys`).
        suppressedVK = nil
        #if os(macOS)
        commandChordVKs.removeAll() // their releases are in `pressedVKs`, flushed just below
        #endif
        for vk in pressedVKs {
            emitKey(vk, down: false)
        }
        for button in pressedButtons {
            connection.send(.mouseButton(button, down: false))
        }
        for event in scrollCapture.cancelAll() {
            connection.send(event) // axes close BEFORE `forwarding` drops below
        }
        pressedVKs.removeAll()
        pressedButtons.removeAll()
        residualX = 0
        residualY = 0
    }

    #if !os(macOS)
    /// Windows VKs that must never auto-repeat: the modifiers (a held Shift is a state, not a
    /// stream of presses) and the lock keys (each repeat would toggle the light again).
    private static let noAutoRepeatVKs: Set<UInt32> = [
        0x10, 0xA0, 0xA1, // Shift, LShift, RShift
        0x11, 0xA2, 0xA3, // Control, LControl, RControl
        0x12, 0xA4, 0xA5, // Alt, LAlt, RAlt
        0x5B, 0x5C, // LWin, RWin
        0x14, 0x90, 0x91, // CapsLock, NumLock, ScrollLock
    ]

    /// One repeat of `vk`. Stops the moment the key is no longer held — a release that raced
    /// the timer, or a releaseAll from a blur, must not keep typing into the host.
    private func repeatKey(_ vk: UInt32) {
        guard autoRepeatVK == vk, forwarding, pressedVKs.contains(vk) else {
            stopAutoRepeat()
            return
        }
        emitKey(vk, down: true)
    }

    /// Start (or hand over) the held-key auto-repeat, matching a real keyboard's delay-then-rate.
    ///
    /// GameController reports a key ONCE on press and once on release — it has no repeat channel —
    /// and the host injects exactly what it is told, so nothing downstream ever turns a held key
    /// into a stream of presses. macOS is unaffected: its NSEvent path already receives the
    /// window server's own repeats and forwards them.
    ///
    /// Only the newest key repeats, which is what a hardware keyboard does — pressing a second key
    /// takes the repeat over from the first. Timings are the iOS/macOS defaults (~0.5 s delay,
    /// ~25 Hz); `.common` keeps it running while a scroll or gesture is tracking.
    private func startAutoRepeat(_ vk: UInt32) {
        guard !Self.noAutoRepeatVKs.contains(vk) else { return }
        stopAutoRepeat()
        autoRepeatVK = vk
        // Both timers fire on the main run loop.
        let timer = Timer(timeInterval: 0.5, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.autoRepeatVK == vk else { return }
                let ticker = Timer(timeInterval: 0.04, repeats: true) { [weak self] _ in
                    MainActor.assumeIsolated { self?.repeatKey(vk) }
                }
                self.autoRepeatTimer = ticker
                RunLoop.main.add(ticker, forMode: .common)
            }
        }
        autoRepeatTimer = timer
        RunLoop.main.add(timer, forMode: .common)
    }

    private func stopAutoRepeat() {
        autoRepeatTimer?.invalidate()
        autoRepeatTimer = nil
        autoRepeatVK = nil
    }
    #endif

    /// The single wire boundary for a key event. Every `.key` send funnels through here so the
    /// active location-based modifier layout is applied in exactly one place while all internal
    /// press/release bookkeeping (`pressedVKs`, `cmdKeysDown`, `resolveModifier`'s `isDown`) stays on
    /// the physical VK. The layout is the session's. Non-modifier VKs pass through untouched.
    private func emitKey(_ vk: UInt32, down: Bool) {
        let layout = ModifierLayout(rawValue: connection.settings.modifierLayout) ?? .mac
        connection.send(.key(Self.applyModifierLayout(vk, layout), down: down))
    }

    /// Release any held MOUSE buttons host-side, leaving keyboard state untouched. Used when
    /// the iPad pointer lock drops while a GCMouse button is held: by then the GCMouse release
    /// handler is gated off (`gcMouseForwarding` is false), so it can't deliver the release
    /// itself and the button would otherwise stick until the next `releaseAll` (blur / stop).
    public func releaseMouseButtons() {
        for button in pressedButtons {
            connection.send(.mouseButton(button, down: false))
        }
        pressedButtons.removeAll()
    }

    private func sendButton(_ button: UInt32, pressed: Bool) {
        guard forwarding else { return }
        if button == suppressedButton {
            // The latch is worth exactly ONE press and its release. A second press means the
            // release never reached us — the pointer lock can be granted between the engaging
            // click's down and its up, gating off the handler that would have carried the up —
            // and staying armed would swallow this genuine click too.
            if pressed, suppressedDownSeen {
                suppressedButton = nil
                suppressedDownSeen = false
            }
        }
        if button == suppressedButton {
            if pressed { suppressedDownSeen = true }
            if !pressed { // capture click over — stop suppressing
                suppressedButton = nil
                suppressedDownSeen = false
            }
            if inputDebug {
                inputLog.debug(
                    "button \(button, privacy: .public) \(pressed ? "down" : "up", privacy: .public) SUPPRESSED (engage click)")
            }
            return
        }
        if pressed {
            pressedButtons.insert(button)
        } else {
            pressedButtons.remove(button)
        }
        if inputDebug {
            inputLog.debug(
                "button \(button, privacy: .public) \(pressed ? "down" : "up", privacy: .public) sent")
        }
        connection.send(.mouseButton(button, down: pressed))
    }

    /// NSEvent button path (macOS): StreamLayerView's local mouse monitor routes physical
    /// button transitions here so they go through the same `suppressedButton` engage-click
    /// latch and `pressedButtons` release-on-blur set as the (iOS) GCMouse path. Wire ids:
    /// 1=left 2=middle 3=right 4=X1 5=X2.
    public func sendMouseButton(_ button: UInt32, pressed: Bool) {
        sendButton(button, pressed: pressed)
    }

    #if os(macOS)
    /// NSEvent key path (macOS): StreamLayerView's keyDown/keyUp/flagsChanged route Windows
    /// VKs here while captured. Mirrors `sendButton` — gated by `forwarding`, honours the
    /// ⌘⎋ toggle's `suppressedVK` latch, and tracks into `pressedVKs` so releaseAll()/blur
    /// flushes anything still held (a flagsChanged up can be missed on focus change). macOS
    /// has no GCKeyboard send (that path is iOS-only now), so this is the single key source.
    public func sendKey(_ vk: UInt32, down: Bool) {
        guard forwarding else { return }
        // The ⌘⎋ toggle's Esc is latched here (see the keyDown monitor) so it never types
        // an Escape into the host — clear the latch on its release, in front of the send.
        if vk == suppressedVK {
            if !down { suppressedVK = nil }
            if inputDebug {
                inputLog.debug(
                    "key \(vk, privacy: .public) \(down ? "down" : "up", privacy: .public) SUPPRESSED (⌘⎋ toggle)")
            }
            return
        }
        if down {
            pressedVKs.insert(vk)
        } else {
            pressedVKs.remove(vk)
        }
        if inputDebug {
            inputLog.debug(
                "key \(vk, privacy: .public) \(down ? "down" : "up", privacy: .public) sent")
        }
        emitKey(vk, down: down)
    }

    /// NSEvent modifier path (macOS): modifier keys never fire keyDown/keyUp — they arrive
    /// as flagsChanged, which carries no down-vs-up. `keyCode` names the key that changed
    /// (kVK_Control & co., already L/R-specific); `resolveModifier` recovers the direction
    /// from the flags. Fed `event.keyCode` + `UInt(event.modifierFlags.rawValue)` — LOW 16
    /// bits intact, they carry the device-dependent L/R bits (the .deviceIndependentFlagsMask
    /// the ⌘⎋ monitor uses deliberately strips exactly these — do NOT pre-mask here).
    /// Caps Lock carries a lock state instead, and goes out as a tap (`capsLockTap`).
    public func handleFlagsChanged(keyCode: UInt16, rawFlags: UInt) {
        if inputDebug {
            inputLog.debug(
                "flagsChanged keyCode \(keyCode, privacy: .public) flags 0x\(String(rawFlags, radix: 16), privacy: .public) forwarding \(self.forwarding, privacy: .public)")
        }
        guard forwarding else { return }
        if let caps = Self.capsLockTap(keyCode: keyCode, rawFlags: rawFlags, was: capsLockOn) {
            capsLockOn = caps.on
            if caps.tap {
                sendKey(0x14, down: true) // VK_CAPITAL
                sendKey(0x14, down: false)
            }
            return
        }
        guard let (vk, down) = Self.resolveModifier(
            keyCode: keyCode, rawFlags: rawFlags, isDown: { pressedVKs.contains($0) })
        else { return } // Fn / unknown — nothing the host consumes on this path
        // Keep cmdKeysDown in step (the ⌘⎋ toggle + Esc suppression read it); sendKey
        // adds the VK to pressedVKs so releaseAll/blur flushes a held modifier cleanly.
        if vk == 0x5B || vk == 0x5C {
            if down {
                cmdKeysDown.insert(vk)
            } else {
                cmdKeysDown.remove(vk)
                // Last ⌘ up: release the chord keys still held, BEFORE the ⌘'s own release goes
                // out, so the host never sees the letter outlive the modifier it was pressed with.
                if cmdKeysDown.isEmpty { flushCommandChord() }
            }
        }
        sendKey(vk, down: down)
    }

    /// Resolve one flagsChanged transition to (Windows VK, down). The changed key is
    /// `keyCode`; the direction comes from the flags. The device-dependent L/R bits (LOW
    /// 16 bits, NX_DEVICE*KEYMASK) disambiguate the two same-class keys, but some
    /// keyboards ship flagsChanged WITHOUT them — only the device-independent class
    /// bit (NX_CONTROLMASK & co.) is set. A pure diff of the device bits silently drops
    /// those keys (seen live: Control never forwarded), so this is keyCode-driven with the
    /// flags as evidence: class bit clear → the key went up; device bits present → they
    /// say which side is held now; class bit set with NO device bits → flip the held state
    /// we track (`isDown`, from pressedVKs — SDL ships the same fallback). Each keyCode
    /// maps to the L/R modifier VK `hidToVK` already emits, so the host needs no change.
    /// Returns nil for Fn and Caps Lock; Caps Lock goes through `capsLockTap`.
    nonisolated static func resolveModifier(
        keyCode: UInt16, rawFlags: UInt, isDown: (UInt32) -> Bool
    ) -> (vk: UInt32, down: Bool)? {
        guard let mod = modifierBits[keyCode] else { return nil }
        let down: Bool
        if rawFlags & mod.classMask == 0 {
            down = false
        } else if rawFlags & (mod.deviceBit | mod.siblingBit) != 0 {
            down = rawFlags & mod.deviceBit != 0
        } else {
            down = !isDown(mod.vk)
        }
        return (mod.vk, down)
    }

    /// Caps Lock (kVK_CapsLock) reaches flagsChanged once per toggle, as the `.capsLock` bit, never
    /// as a press and a release. A host toggles its lock on a press, so a state that differs from
    /// `was`, the one the host was last sent, is one tap. Nil for any other key.
    nonisolated static func capsLockTap(
        keyCode: UInt16, rawFlags: UInt, was: Bool?
    ) -> (on: Bool, tap: Bool)? {
        guard keyCode == 57 else { return nil }
        let on = rawFlags & NSEvent.ModifierFlags.capsLock.rawValue != 0
        return (on, on != was)
    }

    // MARK: - ⌘ chord passthrough

    /// Each chord modifier's flag and the Windows VKs of its two keys.
    nonisolated static let modifierVKs: [(NSEvent.ModifierFlags, [UInt32])] = [
        (.control, [0xA2, 0xA3]), (.option, [0xA4, 0xA5]), (.shift, [0xA0, 0xA1]), (.command, [0x5B, 0x5C]),
    ]

    /// The four modifiers a client chord is ever spelled with, isolated from the incidental bits
    /// `deviceIndependentFlagsMask` also carries: Caps Lock, and the `.function`/`.numericPad`
    /// pair every arrow and F-key sets. Equality against the raw masked flags meant a chord
    /// stopped being recognized the moment Caps Lock was on — ⌘⎋ and ⌃⌥⇧Q, both escape hatches,
    /// included. That was survivable while the monitor claimed six chords; it is not, now that it
    /// swallows every ⌘ chord there is.
    nonisolated static let chordFlagMask: NSEvent.ModifierFlags = [.command, .control, .option, .shift]

    /// One event's chord modifiers (see `chordFlagMask`).
    nonisolated static func chordFlags(_ event: NSEvent) -> NSEvent.ModifierFlags {
        event.modifierFlags.intersection(chordFlagMask)
    }

    /// Does this keyDown get taken off AppKit and forwarded to the host instead? Only while input
    /// is actually captured, only with the cross-client `inhibit_shortcuts` on — and never ⌘⎋,
    /// the way out of capture. ⌃⌘F goes to the host too; ⌘⎋, then the menu, leaves fullscreen.
    ///
    /// The mouse model is NOT a condition, and re-adding it is the trap: `inhibit_shortcuts`
    /// applies in both models here exactly as it does on the SDL clients, whose keyboard grab
    /// reads `inhibit` and never `desktop` (`pf-presenter`'s `apply_capture`). A remote desktop
    /// needs the host's ⌘ chords too, and under the desktop model the unlocked pointer clicking
    /// another window is the always-available way back.
    nonisolated static func forwardsCommandChord(
        keyCode: UInt16, flags: NSEvent.ModifierFlags,
        forwarding: Bool, inhibitShortcuts: Bool
    ) -> Bool {
        guard forwarding, inhibitShortcuts else { return false }
        guard flags.contains(.command) else { return false }
        return !(keyCode == 53 && flags == .command) // ⌘⎋
    }

    /// Forward one key of a ⌘ chord the monitor just took off AppKit, remembering it so its
    /// release can be synthesized (see `commandChordVKs`).
    private func sendCommandChordKey(_ vk: UInt32) {
        commandChordVKs.insert(vk)
        sendKey(vk, down: true)
    }

    /// The keyUp the key monitor sends itself; nil leaves it to the responder chain. AppKit keeps
    /// a keyUp from `keyUp(with:)` while ⌘ is held, so the monitor takes any held key released
    /// under ⌘, including one pressed before ⌘ went down. A key the ⌘-chord passthrough pressed
    /// is taken whatever the modifiers are by its release.
    nonisolated static func takeRelease(
        _ event: NSEvent, forwarding: Bool, pressedVKs: Set<UInt32>, chordVKs: inout Set<UInt32>
    ) -> UInt32? {
        guard forwarding, event.type == .keyUp, let vk = keyCodeToVK[event.keyCode]
        else { return nil }
        if chordVKs.remove(vk) != nil { return vk }
        return chordFlags(event).contains(.command) && pressedVKs.contains(vk) ? vk : nil
    }

    /// Release whatever the ⌘-chord passthrough sent down and is still held — called when the last
    /// physical ⌘ comes up, so a chord key never outlives its ⌘. A release the key monitor took
    /// has already removed its VK.
    private func flushCommandChord() {
        // Same cause, different victim: a one-shot latch whose key-up never arrived goes on to eat
        // the NEXT press of that key (⌃⌘F's F, ⌘⎋'s Esc). Once ⌘ is up, a pending latch is stale.
        suppressedVK = nil
        guard !commandChordVKs.isEmpty else { return }
        for vk in commandChordVKs where pressedVKs.contains(vk) {
            pressedVKs.remove(vk)
            emitKey(vk, down: false)
            if inputDebug {
                inputLog.debug("key \(vk, privacy: .public) up SYNTHESIZED (⌘ chord release)")
            }
        }
        commandChordVKs.removeAll()
    }

    // MARK: - System shortcuts

    /// The other half of `inhibit_shortcuts` on macOS. The keyDown monitor above claims the ⌘
    /// chords that REACH the app; ⌘Space, ⌘Tab, Mission Control and other apps' global shortcuts
    /// never do — WindowServer dispatches them first. While captured, `SystemHotKeys` switches that
    /// dispatch off, so those keys arrive as ordinary key events and take the same path to the host.
    private func holdSystemShortcuts() {
        guard !holdsSystemShortcuts, connection.settings.inhibitShortcuts else { return }
        holdsSystemShortcuts = true
        SystemHotKeys.hold()
    }

    private func releaseSystemShortcuts() {
        guard holdsSystemShortcuts else { return }
        holdsSystemShortcuts = false
        SystemHotKeys.release()
    }
    #endif

    /// A preempted capture keeps its connect observers; the guard stops a reconnect there from
    /// taking the global handler slots back from the live one.
    private func attach(mouse: GCMouse) {
        guard Self.activeCapture === self,
              let input = mouse.mouseInput,
              !mice.contains(where: { $0 === mouse }) // re-delivered on wake — attach once
        else { return }
        mice.append(mouse)
        // macOS drives motion + buttons from NSEvent (StreamLayerView's local monitor →
        // sendMotion/sendMouseButton) because GCMouse's handlers proved unreliable there;
        // installing them too would double-send. iOS keeps GCMouse (raw deltas under
        // pointer lock). See the file header.
        #if !os(macOS)
        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let self, self.forwarding, self.gcMouseForwarding else { return }
            // GC gives +y up; the host expects screen-space (+y down).
            let fx = dx + self.residualX
            let fy = -dy + self.residualY
            let ix = fx.rounded(.towardZero)
            let iy = fy.rounded(.towardZero)
            self.residualX = fx - ix
            self.residualY = fy - iy
            if ix != 0 || iy != 0 {
                self.connection.send(.mouseMove(dx: Int32(ix), dy: Int32(iy)))
                if inputDebug {
                    self.motionDebugCount += 1
                    let now = Date()
                    if now.timeIntervalSince(self.motionDebugTick) >= 1 {
                        inputLog.debug(
                            "motion forwarded: \(self.motionDebugCount, privacy: .public) events, last dx \(Int(ix), privacy: .public) dy \(Int(iy), privacy: .public)")
                        self.motionDebugCount = 0
                        self.motionDebugTick = now
                    }
                }
            }
        }
        // Buttons take the GCMouse path only while the scene is pointer-locked; when it
        // isn't, the UIKit indirect-pointer path carries them (gcMouseForwarding gates here
        // so the two can't double-send).
        input.leftButton.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self, self.gcMouseForwarding else { return }
            self.sendButton(1, pressed: pressed)
        }
        input.rightButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self, self.gcMouseForwarding else { return }
            self.sendButton(3, pressed: pressed)
        }
        input.middleButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self, self.gcMouseForwarding else { return }
            self.sendButton(2, pressed: pressed)
        }
        // First two side buttons → GameStream X1/X2.
        if let aux = input.auxiliaryButtons {
            for (i, button) in aux.prefix(2).enumerated() {
                button.pressedChangedHandler = { [weak self] _, _, pressed in
                    guard let self, self.gcMouseForwarding else { return }
                    self.sendButton(UInt32(4 + i), pressed: pressed)
                }
            }
        }
        // Scroll WHEEL, tvOS only: raw detents, one per notch (×120 → v120), the vertical wheel on
        // x (+x = down) and the horizontal on y (+y = right), as SDL reads it. iOS takes every scroll
        // from the stream view's pan recognizers, locked or not: right axes, Natural Scrolling
        // applied. Both would send every notch twice.
        #if os(tvOS)
        input.scroll.valueChangedHandler = { [weak self] _, x, y in
            guard let self, self.forwarding, self.gcMouseForwarding else { return }
            self.sendScroll( // a real wheel: counted detents, not distance
                dx: y * 120, dy: -x * 120,
                source: PUNKTFUNK_SCROLL_SOURCE_WHEEL, phase: PUNKTFUNK_SCROLL_PHASE_NONE)
        }
        #endif
        #endif
    }

    /// Forward relative mouse motion (macOS), fed by StreamLayerView's NSEvent monitor while
    /// the cursor is disassociated: the mouse's own counts, +y down as the host expects, so
    /// unlike the iOS GCMouse path they are not y-negated. Fractional remainders accumulate
    /// so slow, sub-pixel motion isn't truncated away.
    public func sendMotion(dx: Float, dy: Float) {
        guard forwarding else { return }
        let fx = dx + residualX
        let fy = dy + residualY
        let ix = fx.rounded(.towardZero)
        let iy = fy.rounded(.towardZero)
        residualX = fx - ix
        residualY = fy - iy
        guard ix != 0 || iy != 0 else { return }
        connection.send(.mouseMove(dx: Int32(ix), dy: Int32(iy)))
        if inputDebug {
            // High-rate — log a rolling count + the last delta once per second, not per event.
            motionDebugCount += 1
            let now = Date()
            if now.timeIntervalSince(motionDebugTick) >= 1 {
                inputLog.debug(
                    "motion forwarded: \(self.motionDebugCount, privacy: .public) events, last dx \(Int(ix), privacy: .public) dy \(Int(iy), privacy: .public)")
                motionDebugCount = 0
                motionDebugTick = now
            }
        }
    }

    /// Forward an ABSOLUTE cursor position (iPad pointer fallback). Fed by the iOS stream
    /// view's hover / indirect-pointer path when the scene can't pointer-lock: the host
    /// places its cursor at this client-surface pixel — the same letterbox mapping the touch
    /// path uses. Gated by `forwarding` AND `!gcMouseForwarding` (the relative GCMouse path
    /// owns motion while locked), so absolute and relative motion never both fire. No residual
    /// accumulation — the value is absolute, not a delta.
    public func sendMouseAbs(
        x: Int32, y: Int32, surfaceWidth: UInt32, surfaceHeight: UInt32, screen: UInt8 = 0
    ) {
        guard forwarding, !gcMouseForwarding else { return }
        connection.send(.mouseMoveAbs(
            x: x, y: y, surfaceWidth: surfaceWidth, surfaceHeight: surfaceHeight, screen: screen))
    }

    /// Forward a scroll sample as normalized `InputKind::Scroll` events — positive = up /
    /// right, `dx`/`dy` in `source`'s unit (v120 for a counted wheel, DIP for a surface's
    /// measured distance) and `phase` the platform's gesture boundary. Fed by
    /// StreamLayerView.scrollWheel, the iOS pan recognizers, and the GCMouse wheel. The
    /// unsent fraction rides in `scrollCapture`; inversion happens once at the connection's
    /// outbound seam (`setInvertScroll`), never here.
    public func sendScroll(
        dx: Float, dy: Float, source: PunktfunkScrollSource, phase: PunktfunkScrollPhase
    ) {
        guard forwarding else { return }
        for event in scrollCapture.event(
            dx: Double(dx), dy: Double(dy), source: source, phase: phase)
        {
            connection.send(event)
        }
    }

    private func attach(keyboard: GCKeyboard) {
        guard Self.activeCapture === self, !keyboards.contains(where: { $0 === keyboard })
        else { return }
        keyboards.append(keyboard)
        // macOS sends keys from NSEvent (StreamLayerView's keyDown/keyUp/flagsChanged →
        // sendKey/handleFlagsChanged) because GCKeyboard delivery proved unreliable there —
        // the same GameController quirk that killed GCMouse motion (fixed in e414ec0).
        // Installing this handler too would double-send, so on macOS we leave the keyboard
        // tracked (for stop()'s cleanup) but attach no send handler: NSEvent is the only
        // key path. iOS keeps the GCKeyboard path (and detects the ⌘⎋ toggle from the HID
        // stream, since there's no NSEvent monitor there). See the file header.
        #if !os(macOS)
        keyboard.keyboardInput?.keyChangedHandler = { [weak self] _, _, keyCode, pressed in
            guard let self, let vk = Self.hidToVK[keyCode.rawValue] else { return }
            if vk == 0x5B || vk == 0x5C { // physical ⌘ state, tracked in both states
                if pressed {
                    self.cmdKeysDown.insert(vk)
                } else {
                    self.cmdKeysDown.remove(vk)
                }
            }
            // The ⌘⎋ toggle's Esc — checked before the forwarding gate, because in the
            // engage direction forwarding is already true when this fires.
            if vk == self.suppressedVK {
                if !pressed { self.suppressedVK = nil }
                return
            }
            #if os(iOS) || os(visionOS)
            // No NSEvent monitor here — the toggle combo is detected from the HID
            // stream itself.
            if pressed, vk == 0x1B, !self.cmdKeysDown.isEmpty {
                self.suppressedVK = 0x1B
                self.onToggleCapture?()
                return
            }
            // The keyboard shortcuts, in both states: a modifier held before capture engaged
            // still counts.
            if self.shortcutTakes(vk, down: pressed) { return }
            #endif
            guard self.forwarding else { return }
            // Release direction of the toggle: GC's Esc-down can beat the NSEvent
            // monitor — never type Esc into the host while ⌘ is held (⌘⎋ is reserved).
            if vk == 0x1B, !self.cmdKeysDown.isEmpty {
                return
            }
            if pressed {
                self.pressedVKs.insert(vk)
            } else {
                self.pressedVKs.remove(vk)
            }
            self.emitKey(vk, down: pressed)
            // GC has no repeat channel, so a held key becomes a repeat here (see startAutoRepeat).
            // A release only cancels the repeat if it is THIS key's — releasing an older key while
            // a newer one is still held must leave the newer one repeating.
            if pressed {
                self.startAutoRepeat(vk)
            } else if self.autoRepeatVK == vk {
                self.stopAutoRepeat()
            }
        }
        #endif
    }
}

#if os(macOS)
// Private CoreGraphics calls, linked like any other symbol — the switch UTM (Mac App Store), SDL
// and Parsec use. C ABI: UInt32 arguments, a CGError (Int32) back.
@_silgen_name("CGSMainConnectionID") private func CGSMainConnectionID() -> UInt32
@_silgen_name("CGSSetGlobalHotKeyOperatingMode")
private func CGSSetGlobalHotKeyOperatingMode(_ connection: UInt32, _ mode: UInt32) -> Int32
@_silgen_name("CGSGetGlobalHotKeyOperatingMode")
private func CGSGetGlobalHotKeyOperatingMode(_ connection: UInt32, _ mode: UnsafeMutablePointer<UInt32>) -> Int32

/// WindowServer's global shortcuts — ⌘Tab, ⌘Space, Mission Control, Force Quit, other apps' hot
/// keys — switched off while any capture holds them. Off, they reach the frontmost app as ordinary
/// key events; no permission is involved. Process-wide, and macOS restores them if the process dies.
///
/// They come back on while the main thread has not answered for 2 s: a hung app must not keep
/// ⌥⌘⎋ from the user. Everything runs on `queue`, so the watchdog works with main stuck.
enum SystemHotKeys {
    private static let queue = DispatchQueue(label: "io.unom.punktfunk.system-hot-keys")
    // Queue-only state.
    private static var holders = 0
    private static var off = false
    private static var lastAnswer = DispatchTime.now()
    private static var pinging = false
    private static var watchdog: DispatchSourceTimer?

    /// Called from main — which is therefore alive right now.
    static func hold() {
        queue.async {
            holders += 1
            lastAnswer = .now()
            update()
        }
    }

    static func release() {
        queue.async {
            holders = max(0, holders - 1)
            update()
        }
    }

    /// WindowServer's own view, for the tests.
    static var isOff: Bool {
        var mode: UInt32 = 0
        return CGSGetGlobalHotKeyOperatingMode(CGSMainConnectionID(), &mode) == 0 && mode == 1
    }

    /// Off only while held AND the main thread answers.
    static func shouldBeOff(holders: Int, mainSilentFor seconds: Double) -> Bool {
        holders > 0 && seconds < 2
    }

    private static func update() {
        let silent = Double(DispatchTime.now().uptimeNanoseconds - lastAnswer.uptimeNanoseconds) / 1e9
        let want = shouldBeOff(holders: holders, mainSilentFor: silent)
        if want != off {
            off = want // set even on failure: one log line, not one per tick
            let err = CGSSetGlobalHotKeyOperatingMode(CGSMainConnectionID(), want ? 1 : 0)
            if err != 0 { inputLog.error("system shortcuts: hot key mode \(want ? 1 : 0) refused (\(err))") }
        }
        if holders > 0, watchdog == nil {
            let timer = DispatchSource.makeTimerSource(queue: queue)
            timer.schedule(deadline: .now() + 0.5, repeating: 0.5)
            timer.setEventHandler {
                ping()
                update()
            }
            timer.resume()
            watchdog = timer
        } else if holders == 0 {
            watchdog?.cancel()
            watchdog = nil
        }
    }

    /// One main-thread round trip at a time; its answer refreshes `lastAnswer`.
    private static func ping() {
        guard !pinging else { return }
        pinging = true
        DispatchQueue.main.async {
            queue.async {
                pinging = false
                lastAnswer = .now()
                update()
            }
        }
    }
}
#endif
