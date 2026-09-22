// The shared console's C ABI as Swift sees it (`clients/apple/native/src/console.rs`). One class
// owns the handle; every call but the model pushes belongs to the thread that built it, which is
// the main thread — `ConsoleView` drives frames from the display link there.
//
// The JSON on this boundary is the console's own model (`pf_console_ui::bridge`), the same the
// Android client speaks, so there is no Swift mirror type to drift.

import Foundation
import Metal
import PunktfunkCore

@MainActor
public final class ConsoleBridge {
    private var handle: OpaquePointer?

    /// What the host pushes. The raw values are the header's `PUNKTFUNK_CONSOLE_PUSH_*`.
    public enum Push: UInt8 {
        case hosts = 0
        case pair = 1
        case wake = 2
        case notice = 3
        case speed = 4
        case libraryBegin = 5
        case libraryPhase = 6
        case libraryGames = 7
        case libraryCached = 8
        case libraryRunning = 9
        case libraryStale = 10
        case settings = 11
        case presets = 12
        case knownHosts = 13
        case pads = 14
        case navigate = 15
    }

    /// A discrete menu event, as the shell numbers them.
    public enum Menu: UInt8 {
        case up = 0, down = 1, left = 2, right = 3
        case confirm = 4, back = 5, secondary = 6, tertiary = 7
        case jumpBack = 8, jumpForward = 9
    }

    /// Which device the event came from — it picks the glyph legend.
    public enum Source: UInt8 {
        case keys = 0
        case pad = 1
    }

    /// Touch and mouse, in texture pixels.
    public enum Pointer: UInt8 {
        case move = 0
        case down = 1
        case up = 2
        case back = 3
        case wheel = 4
        case cancel = 5
        /// A finger on the glass: the shell defers it so a swipe scrolls instead of acting.
        case touchDown = 6
    }

    /// The keys the console understands; anything else stays the app's.
    public enum Key: UInt8 {
        case left = 0, right = 1, up = 2, down = 3
        case `return` = 4, space = 5, escape = 6, backspace = 7
        case pageUp = 8, pageDown = 9, tab = 10, y = 11, x = 12
    }

    /// Where the session the console asked for stands.
    public enum Phase: UInt8 {
        case connecting = 0, streaming = 1, failed = 2, ended = 3, reconnecting = 4
    }

    /// `nil` when the options JSON or Skia's Metal context fails; the log says which.
    public init?(options: String, device: MTLDevice, queue: MTLCommandQueue) {
        guard
            let handle = punktfunk_console_new(
                options, Unmanaged.passUnretained(device).toOpaque(),
                Unmanaged.passUnretained(queue).toOpaque())
        else { return nil }
        self.handle = handle
    }

    deinit {
        // `free` belongs to this thread, and a @MainActor class is only ever released on it.
        MainActor.assumeIsolated { punktfunk_console_free(handle) }
    }

    /// Draw into `texture` and submit. `false` = nothing drawn (idle, or the texture was
    /// refused): present nothing this tick.
    public func frame(
        texture: MTLTexture, width: Int, height: Int, insets: PunktfunkInsets, scale: Double
    ) -> Bool {
        punktfunk_console_frame(
            handle, Unmanaged.passUnretained(texture).toOpaque(), UInt32(width), UInt32(height),
            insets, scale)
    }

    /// Whether a Back would leave the console rather than pop a screen. A TV host binds the
    /// Menu button only while this is false, so at the root the press reaches the system.
    public var atRoot: Bool { punktfunk_console_at_root(handle) }

    /// `false` = the shell let the press go: it belongs to the system (tvOS Menu at the root).
    @discardableResult
    public func menu(_ event: Menu, from source: Source) -> Bool {
        punktfunk_console_menu(handle, event.rawValue, source.rawValue)
    }

    @discardableResult
    public func pointer(_ kind: Pointer, x: Float, y: Float, wheel: Float = 0) -> Bool {
        punktfunk_console_pointer(handle, kind.rawValue, x, y, wheel)
    }

    @discardableResult
    public func key(_ key: Key, shift: Bool = false, repeated: Bool = false) -> Bool {
        punktfunk_console_key(handle, key.rawValue, shift, repeated)
    }

    public func text(_ text: String) {
        punktfunk_console_text(handle, text)
    }

    public func phase(_ phase: Phase, message: String = "") {
        punktfunk_console_phase(handle, phase.rawValue, message)
    }

    public func push(_ kind: Push, _ json: String) {
        punktfunk_console_push(handle, kind.rawValue, json)
    }

    /// One title's poster, encoded; the shell decodes it at the size it draws.
    public func art(id: String, bytes: Data) {
        bytes.withUnsafeBytes { raw in
            punktfunk_console_art(
                handle, id, raw.bindMemory(to: UInt8.self).baseAddress, UInt(raw.count))
        }
    }

    /// The next event the shell raised, or nil when there is none.
    public func nextEvent() -> String? {
        guard let raw = punktfunk_console_next_event(handle) else { return nil }
        defer { punktfunk_console_string_free(raw) }
        return String(cString: raw)
    }

    /// Every `ConsoleCmd` queued since the last call, as a JSON array.
    public func drainCmds() -> String {
        guard let raw = punktfunk_console_drain_cmds(handle) else { return "[]" }
        defer { punktfunk_console_string_free(raw) }
        return String(cString: raw)
    }
}
