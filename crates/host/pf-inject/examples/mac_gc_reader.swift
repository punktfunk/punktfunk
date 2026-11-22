// The reader `macos-hid-input-check.sh` runs `mac_input_live hid` against: one floating window
// that logs what AppKit and GameController each receive, so the check can tell a virtual HID
// device's input from posted events.
//
// Build: `swiftc -O -o mac_gc_reader mac_gc_reader.swift`. Its lines:
// - `READY display= key= front= scroll=` once it is key and topmost, `STATE key= front=` on
//   each change: what `mac_input_live` waits for;
// - `from=post|other KEY` and `MOUSE` per AppKit event: `post` is its parent's CGEvent;
// - `GC mouse connected vendor=`, `GCMOUSE vendor= moved` and `left pressed=`, `GCKEY code=
//   pressed=`, from GameController.
// It exits when stdin closes.
import AppKit
import GameController

setvbuf(stdout, nil, _IOLBF, 0)
let t0 = ProcessInfo.processInfo.systemUptime
func out(_ s: String) {
    print(String(format: "%7.3f ", ProcessInfo.processInfo.systemUptime - t0) + s)
}

/// Logs every key and mouse event taken off the queue.
final class App: NSApplication {
    override func nextEvent(matching mask: NSEvent.EventTypeMask, until expiration: Date?,
                            inMode mode: RunLoop.Mode, dequeue deqFlag: Bool) -> NSEvent? {
        let e = super.nextEvent(matching: mask, until: expiration, inMode: mode, dequeue: deqFlag)
        if let e, deqFlag, let cg = e.cgEvent {
            let src = cg.getIntegerValueField(.eventSourceUnixProcessID)
            let from = src == Int64(getppid()) ? "post" : "other"
            switch e.type {
            case .keyDown, .keyUp:
                out(String(format: "from=%@ KEY %@ code=0x%02x", from,
                           e.type == .keyDown ? "down" : "up", e.keyCode))
            case .mouseMoved, .leftMouseDragged, .leftMouseDown, .leftMouseUp,
                 .rightMouseDown, .rightMouseUp:
                out(String(format: "from=%@ MOUSE t=%lu at=%.1f,%.1f d=%lld,%lld", from,
                           e.type.rawValue, cg.location.x, cg.location.y,
                           cg.getIntegerValueField(.mouseEventDeltaX),
                           cg.getIntegerValueField(.mouseEventDeltaY)))
            default:
                break
            }
        }
        return e
    }
}

let app = App.shared
app.setActivationPolicy(.regular)
let primaryH = NSScreen.screens[0].frame.height
let vf = (NSScreen.main ?? NSScreen.screens[0]).visibleFrame
let W: CGFloat = 900, H: CGFloat = 640
let win = NSWindow(
    contentRect: NSRect(x: vf.midX - W / 2, y: vf.midY - H / 2, width: W, height: H),
    styleMask: [.titled], backing: .buffered, defer: false)
win.title = "pf GameController reader"
win.level = .floating
win.acceptsMouseMovedEvents = true
let field = NSTextField(frame: NSRect(x: 20, y: H - 50, width: W - 40, height: 30))
win.contentView?.addSubview(field)

func hookMouse(_ m: GCMouse) {
    let vendor = m.vendorName ?? "-"
    out("GC mouse connected vendor=\(vendor)")
    m.mouseInput?.mouseMovedHandler = { _, dx, dy in
        out(String(format: "GCMOUSE vendor=%@ moved dx=%.1f dy=%.1f", vendor, dx, dy))
    }
    m.mouseInput?.leftButton.pressedChangedHandler = { _, _, p in
        out("GCMOUSE vendor=\(vendor) left pressed=\(p)")
    }
}
func hookKeyboard(_ k: GCKeyboard) {
    out("GC keyboard connected vendor=\(k.vendorName ?? "-")")
    k.keyboardInput?.keyChangedHandler = { _, _, code, p in
        out("GCKEY code=\(code.rawValue) pressed=\(p)")
    }
}
NotificationCenter.default.addObserver(forName: .GCMouseDidConnect, object: nil, queue: .main) {
    if let m = $0.object as? GCMouse { hookMouse(m) }
}
NotificationCenter.default.addObserver(forName: .GCKeyboardDidConnect, object: nil, queue: .main) {
    if let k = $0.object as? GCKeyboard { hookKeyboard(k) }
}
GCMouse.mice().forEach(hookMouse)
if let k = GCKeyboard.coalesced { hookKeyboard(k) }

/// Key, and topmost at the centre and the four inset corners of the window.
func state() -> (Bool, Bool) {
    let r = win.frame.insetBy(dx: 12, dy: 12)
    let points = [NSPoint(x: r.midX, y: r.midY), NSPoint(x: r.minX, y: r.minY),
                  NSPoint(x: r.maxX, y: r.minY), NSPoint(x: r.minX, y: r.maxY - 30),
                  NSPoint(x: r.maxX, y: r.maxY - 30)]
    let front = points.allSatisfy {
        NSWindow.windowNumber(at: $0, belowWindowWithWindowNumber: 0) == win.windowNumber
    }
    return (win.isKeyWindow, front)
}

var ready = false
var lastState = ""
let started = Date()
Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { _ in
    let (key, front) = state()
    let s = "STATE key=\(key ? 1 : 0) front=\(front ? 1 : 0)"
    if s != lastState { lastState = s; out(s) }
    if !ready && ((key && front) || Date().timeIntervalSince(started) > 3) {
        ready = true
        let id = (win.screen?.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value ?? 0
        let c = win.convertToScreen(win.contentLayoutRect)
        out(String(format: "READY pid=%d display=%u key=%d front=%d scroll=%.1f,%.1f,%.1f,%.1f",
                   getpid(), id, key ? 1 : 0, front ? 1 : 0,
                   c.minX, primaryH - c.maxY, c.width, c.height))
    }
}

DispatchQueue.global().async {
    while readLine() != nil {}
    DispatchQueue.main.async { exit(0) }
}

win.makeKeyAndOrderFront(nil)
win.makeFirstResponder(field)
app.activate(ignoringOtherApps: true)
app.run()
