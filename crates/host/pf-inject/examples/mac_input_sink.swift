// The event sink `mac_input_live` drives: one floating window with a text field and a scroll
// view, logging to stdout every input event it receives, in CG global points.
//
// Build: `swiftc -O -o mac_input_sink mac_input_sink.swift`. Run it only through
// `mac_input_live`, which reads its lines:
// - `READY display= key= front= content= scroll= field=` once it is key and topmost;
// - `STATE key= front=` on each change, and `FOREIGN` for input its parent did not post;
// - `KEY`, `FLAGS`, `MOUSE`, `SCROLL`, `PROX`, `GESTURE` per event (`src=` is the posting pid;
//   a pen's `MOUSE` adds its tablet fields), `VIEW` per scroll view move, and `FIELD` with the
//   text field's contents when stdin closes.
// Responsive scrolling is off so every scroll event, phases included, reaches the main thread.
import AppKit

setvbuf(stdout, nil, _IOLBF, 0)
let t0 = ProcessInfo.processInfo.systemUptime
func out(_ s: String) {
    print(String(format: "%7.3f ", ProcessInfo.processInfo.systemUptime - t0) + s)
}

final class Doc: NSView {
    override var isFlipped: Bool { true }
    override class var isCompatibleWithResponsiveScrolling: Bool { false }
}

/// Logs each scroll and gesture event the view handles; a gesture's events need not pass the
/// queue.
final class Scroll: NSScrollView {
    override func scrollWheel(with e: NSEvent) {
        logEvent(e, via: "view")
        super.scrollWheel(with: e)
    }
    override func magnify(with e: NSEvent) { logEvent(e, via: "view") }
    override func rotate(with e: NSEvent) { logEvent(e, via: "view") }
}

/// Logs every event taken off the queue, tracking loops included.
final class App: NSApplication {
    override func nextEvent(matching mask: NSEvent.EventTypeMask, until expiration: Date?,
                            inMode mode: RunLoop.Mode, dequeue deqFlag: Bool) -> NSEvent? {
        let e = super.nextEvent(matching: mask, until: expiration, inMode: mode, dequeue: deqFlag)
        if let e, deqFlag { logEvent(e, via: "queue") }
        return e
    }
}

let app = App.shared
app.setActivationPolicy(.regular)
let primaryH = NSScreen.screens[0].frame.height
let screen = NSScreen.main ?? NSScreen.screens[0]
let W: CGFloat = 900, H: CGFloat = 640
let vf = screen.visibleFrame
let win = NSWindow(
    contentRect: NSRect(x: vf.midX - W / 2, y: vf.midY - H / 2, width: W, height: H),
    styleMask: [.titled], backing: .buffered, defer: false)
win.title = "pf input sink"
win.level = .floating
win.acceptsMouseMovedEvents = true
let content = NSView(frame: NSRect(x: 0, y: 0, width: W, height: H))
win.contentView = content
let field = NSTextField(frame: NSRect(x: 20, y: H - 50, width: W - 40, height: 30))
content.addSubview(field)
let scroll = Scroll(frame: NSRect(x: 20, y: 20, width: W - 40, height: H - 90))
scroll.hasVerticalScroller = true
scroll.hasHorizontalScroller = true
let doc = Doc(frame: NSRect(x: 0, y: 0, width: 8000, height: 8000))
scroll.documentView = doc
content.addSubview(scroll)
doc.scroll(NSPoint(x: 4000, y: 4000))
scroll.contentView.postsBoundsChangedNotifications = true
var lastOrigin = scroll.contentView.bounds.origin
NotificationCenter.default.addObserver(
    forName: NSView.boundsDidChangeNotification, object: scroll.contentView, queue: nil
) { _ in
    let o = scroll.contentView.bounds.origin
    out(String(format: "VIEW origin=%.1f,%.1f d=%.1f,%.1f", o.x, o.y, o.x - lastOrigin.x, o.y - lastOrigin.y))
    lastOrigin = o
}

/// A view's frame in CG global points (top-left origin, y down).
func cgRect(_ v: NSView) -> String {
    let r = win.convertToScreen(v.convert(v.bounds, to: nil))
    return String(format: "%.1f,%.1f,%.1f,%.1f", r.minX, primaryH - r.maxY, r.width, r.height)
}

var ready = false

func flagsHex(_ e: NSEvent) -> String { String(format: "0x%llx", e.cgEvent?.flags.rawValue ?? 0) }

func logEvent(_ e: NSEvent, via: String) {
    guard let cg = e.cgEvent else { return }
    let loc = cg.location
    let f = { (field: CGEventField) in cg.getIntegerValueField(field) }
    let src = f(.eventSourceUnixProcessID)
    // Scroll stays out: a scroll utility's event tap re-posts posted wheel events as its own.
    let input: Set<NSEvent.EventType> = [.keyDown, .keyUp, .flagsChanged, .mouseMoved,
        .leftMouseDragged, .rightMouseDragged, .otherMouseDragged, .leftMouseDown, .leftMouseUp,
        .rightMouseDown, .rightMouseUp, .otherMouseDown, .otherMouseUp]
    if ready && input.contains(e.type) && src != Int64(getppid()) {
        out("FOREIGN src=\(src) type=\(e.type.rawValue)")
    }
    switch e.type {
    case .keyDown, .keyUp:
        out("src=\(src) " + String(format: "KEY %@ code=0x%02x chars=%@ ign=%@ repeat=%d flags=%@",
                   e.type == .keyDown ? "down" : "up", e.keyCode,
                   (e.characters ?? "").debugDescription, (e.charactersIgnoringModifiers ?? "").debugDescription,
                   e.isARepeat ? 1 : 0, flagsHex(e)))
    case .flagsChanged:
        out("src=\(src) " + String(format: "FLAGS code=0x%02x flags=%@", e.keyCode, flagsHex(e)))
    case .mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged,
         .leftMouseDown, .leftMouseUp, .rightMouseDown, .rightMouseUp, .otherMouseDown, .otherMouseUp:
        // The tablet properties are valid only on a tablet-point event.
        let pen = e.subtype == .tabletPoint
            ? String(format: " TABLET p=%.3f tilt=%.3f,%.3f rot=%.1f bm=0x%lx abs=%ld,%ld dev=%lu",
                     e.pressure, e.tilt.x, e.tilt.y, e.rotation, e.buttonMask.rawValue,
                     e.absoluteX, e.absoluteY, e.deviceID)
            : ""
        out("src=\(src) via=\(via) " + String(format: "MOUSE t=%@ btn=%lld at=%.1f,%.1f d=%lld,%lld clicks=%lld flags=%@",
                   "\(e.type.rawValue)", f(.mouseEventButtonNumber), loc.x, loc.y,
                   f(.mouseEventDeltaX), f(.mouseEventDeltaY), f(.mouseEventClickState), flagsHex(e)) + pen)
    case .tabletProximity:
        out("src=\(src) via=\(via) " + String(format: "PROX enter=%d type=%lu caps=0x%lx dev=%lu vendor=0x%lx unique=%llu",
                   e.isEnteringProximity ? 1 : 0, e.pointingDeviceType.rawValue, e.capabilityMask,
                   e.deviceID, e.vendorPointingDeviceType, e.uniqueID))
    case .magnify, .rotate:
        out("src=\(src) via=\(via) " + String(format: "GESTURE t=%lu magnify=%.4f rotate=%.3f phase=%lu at=%.1f,%.1f",
                   e.type.rawValue, e.type == .magnify ? e.magnification : 0,
                   e.type == .rotate ? Double(e.rotation) : 0, e.phase.rawValue, loc.x, loc.y))
    case .gesture, .beginGesture, .endGesture, .smartMagnify, .swipe:
        out("src=\(src) via=\(via) GESTURE t=\(e.type.rawValue) phase=\(e.phase.rawValue)")
    case .scrollWheel:
        out("src=\(src) via=\(via) " + String(format: "SCROLL sdx=%.2f sdy=%.2f precise=%d phase=%lu momentum=%lu inverted=%d line=%lld,%lld pt=%lld,%lld cont=%lld",
                   e.scrollingDeltaX, e.scrollingDeltaY, e.hasPreciseScrollingDeltas ? 1 : 0,
                   e.phase.rawValue, e.momentumPhase.rawValue, e.isDirectionInvertedFromDevice ? 1 : 0,
                   f(.scrollWheelEventDeltaAxis1), f(.scrollWheelEventDeltaAxis2),
                   f(.scrollWheelEventPointDeltaAxis1), f(.scrollWheelEventPointDeltaAxis2),
                   f(.scrollWheelEventIsContinuous)))
    default:
        break
    }
}

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

var lastState = ""
let started = Date()
Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { _ in
    let (key, front) = state()
    let s = "STATE key=\(key ? 1 : 0) front=\(front ? 1 : 0)"
    if s != lastState { lastState = s; out(s) }
    if !ready && ((key && front) || Date().timeIntervalSince(started) > 3) {
        ready = true
        let id = (win.screen?.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value ?? 0
        out("READY pid=\(getpid()) display=\(id) key=\(key ? 1 : 0) front=\(front ? 1 : 0) content=\(cgRect(content)) scroll=\(cgRect(scroll)) field=\(cgRect(field))")
    }
}

DispatchQueue.global().async {
    while readLine() != nil {}
    DispatchQueue.main.async {
        let text = (field.currentEditor() as? NSTextView)?.string ?? field.stringValue
        out("FIELD \(text.debugDescription)")
        exit(0)
    }
}

win.makeKeyAndOrderFront(nil)
win.makeFirstResponder(field)
app.activate(ignoringOtherApps: true)
app.run()
