// A session's further screens on a Mac: a borderless window on each display the host built a
// screen for, each with its own presenter and decoder. The host draws the pointer into their
// pictures, so the local one hides over them. The windows never take key: the keyboard and the
// main window's capture stay where they are.

#if os(macOS)
import AppKit
import AVFoundation
import PunktfunkCore
import PunktfunkShared

private let screensLog = ClientLog(category: "screens")

/// One further screen's picture, and the pointer over it as absolute input on that screen.
final class ScreenStreamView: NSView {
    let screen: UInt8
    private let displayLayer = AVSampleBufferDisplayLayer()
    private let presenter = SessionPresenter()
    private var connection: PunktfunkConnection?
    /// The decoded picture's size: the pointer maps onto it. nil before the first frame.
    private var contentSize: (width: UInt32, height: UInt32)?
    /// A pointer move as a pixel of screen `screen`'s picture, with the picture's size.
    var onPointer: ((_ screen: UInt8, _ x: Int32, _ y: Int32, _ width: UInt32, _ height: UInt32) -> Void)?
    var onButton: ((_ button: UInt32, _ down: Bool) -> Void)?
    /// A scroll here; the host scrolls where the last move put its pointer.
    var onScroll: ((NSEvent) -> Void)?
    private static let noCursor = NSCursor(
        image: NSImage(size: NSSize(width: 1, height: 1)), hotSpot: .zero)

    init(screen: UInt8) {
        self.screen = screen
        super.init(frame: .zero)
        displayLayer.videoGravity = .resizeAspect
        displayLayer.backgroundColor = NSColor.black.cgColor
        layer = displayLayer // layer-hosting: assign before wantsLayer
        wantsLayer = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Present screen `screen` of `connection`; `onEnded` runs on the pump thread when its lane
    /// closes.
    func start(connection: PunktfunkConnection, onEnded: @escaping @Sendable () -> Void) {
        self.connection = connection
        presenter.start(
            connection: connection, baseLayer: displayLayer, endToEndMeter: nil,
            makeDisplayLink: { [unowned self] in self.displayLink(target: $0, selector: $1) },
            onFrame: nil, onSessionEnd: onEnded,
            onDecodedSize: { [weak self] w, h in
                DispatchQueue.main.async { self?.noteContentSize(width: w, height: h) }
            },
            adaptiveSync: { [weak self] in
                StreamLayerView.isAdaptiveSync(self?.window?.screen ?? NSScreen.main)
            },
            screen: screen)
        layoutPresenter()
    }

    func stop() {
        presenter.stop()
        connection = nil
    }

    private func noteContentSize(width: Int, height: Int) {
        guard width > 0, height > 0 else { return }
        contentSize = (UInt32(width), UInt32(height))
        presenter.setContentSize(CGSize(width: width, height: height))
        layoutPresenter()
    }

    private func layoutPresenter() {
        presenter.setPanel(StreamLayerView.panelInfo(window?.screen))
        presenter.layout(in: bounds, contentsScale: window?.backingScaleFactor ?? 1)
        displayLayer.videoGravity = SessionPresenter.gravity(
            VideoFit(name: connection?.settings.videoFit))
    }

    override func layout() {
        super.layout()
        layoutPresenter()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        layoutPresenter()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        layoutPresenter()
    }

    // MARK: - Pointer

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        // `.activeAlways`: this window never becomes key, and moves still reach it.
        addTrackingArea(NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .cursorUpdate, .activeAlways, .inVisibleRect,
                      .enabledDuringMouseDrag],
            owner: self))
    }

    override func cursorUpdate(with event: NSEvent) { Self.noCursor.set() }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseMoved(with event: NSEvent) { point(event) }
    override func mouseDragged(with event: NSEvent) { point(event) }
    override func rightMouseDragged(with event: NSEvent) { point(event) }
    override func otherMouseDragged(with event: NSEvent) { point(event) }
    override func mouseDown(with event: NSEvent) { press(event, down: true) }
    override func mouseUp(with event: NSEvent) { press(event, down: false) }
    override func rightMouseDown(with event: NSEvent) { press(event, down: true) }
    override func rightMouseUp(with event: NSEvent) { press(event, down: false) }
    override func otherMouseDown(with event: NSEvent) { press(event, down: true) }
    override func otherMouseUp(with event: NSEvent) { press(event, down: false) }
    override func scrollWheel(with event: NSEvent) { onScroll?(event) }

    private func point(_ event: NSEvent) {
        guard let connection, let window, let content = contentSize,
              let p = StreamLayerView.picturePoint(
                  convert(event.locationInWindow, from: nil), bounds: bounds,
                  scale: window.backingScaleFactor, content: content,
                  fit: VideoFit(name: connection.settings.videoFit))
        else { return }
        onPointer?(screen, p.x, p.y, content.width, content.height)
    }

    /// A press lands where it was pressed.
    private func press(_ event: NSEvent, down: Bool) {
        if down { point(event) }
        onButton?(StreamLayerView.wireButton(for: event), down)
    }
}

/// The session's further screens: a window on each display the host built one for, kept in step
/// with the host's layout, and the ask made again as displays come and go.
@MainActor
final class ScreenWindows {
    private let connection: PunktfunkConnection
    /// The display each asked screen stands for, by screen index.
    private var displays: [UInt8: CGDirectDisplayID] = [:]
    private var windows: [UInt8: (window: NSWindow, view: ScreenStreamView)] = [:]
    /// The host layout the windows follow.
    private var generation: UInt32 = 0
    /// How often each screen's picture ended in this layout, and when it last did.
    private var failed: [UInt8: (count: Int, at: TimeInterval)] = [:]
    private var timer: Timer?
    private var observers: [NSObjectProtocol] = []
    /// Pointer input from a further screen's window; the owner forwards it or not.
    var onPointer: ((_ screen: UInt8, _ x: Int32, _ y: Int32, _ width: UInt32, _ height: UInt32) -> Void)?
    var onButton: ((_ button: UInt32, _ down: Bool) -> Void)?
    var onScroll: ((NSEvent) -> Void)?

    init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    /// Ask for a screen per display, `first` being the stream window's, then follow the host.
    /// Asks again when the displays change or the stream window moves to another one.
    func start(first: @escaping @MainActor () -> NSScreen?) {
        ask(first: first())
        // 10 Hz: a layout moves only when a screen is built or dropped.
        let timer = Timer(timeInterval: 0.1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.follow() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
        let center = NotificationCenter.default
        observers = [
            center.addObserver(
                forName: NSApplication.didChangeScreenParametersNotification, object: nil,
                queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated { self?.ask(first: first()) }
            },
            // Any window's move: only a new display under the stream window asks again.
            center.addObserver(
                forName: NSWindow.didChangeScreenNotification, object: nil, queue: .main
            ) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self, let screen = first(), screen.displayID != self.displays[0]
                    else { return }
                    self.ask(first: screen)
                }
            },
        ]
    }

    func stop() {
        timer?.invalidate()
        timer = nil
        observers.forEach(NotificationCenter.default.removeObserver)
        observers = []
        windows.keys.forEach(close)
        displays = [:]
    }

    /// The displays as screens now. A screen may stand for another display, so every window
    /// reopens with the host's answer, never from the layout the core holds now.
    private func ask(first: NSScreen?) {
        guard let first else { return }
        let mode = connection.currentMode()
        let settings = connection.settings
        let planned = NSScreen.sessionScreens(
            first: first, mode: (mode.width, mode.height, mode.refreshHz),
            scale: (settings.renderScale, RenderScale.maxDimension(codec: settings.codec)))
        displays = Dictionary(uniqueKeysWithValues: planned.map { ($0.record.index, $0.display) })
        windows.keys.forEach(close)
        failed = [:]
        generation = connection.screenLayout().generation
        connection.requestScreens(planned.map(\.record), views: planned.map(\.view))
    }

    /// Open a window for each screen the host's newest layout adds; close the ones it dropped.
    /// Between layouts, a window whose picture ended reopens 2 s on, three times at most.
    private func follow() {
        let (newest, layout) = connection.screenLayout()
        let fresh = newest != generation
        if fresh {
            generation = newest
            failed = [:]
            for index in windows.keys where !layout.contains(where: { $0.index == index }) {
                close(index)
            }
        }
        let now = ProcessInfo.processInfo.systemUptime
        for s in layout where s.index > 0 && windows[s.index] == nil {
            if !fresh {
                guard let f = failed[s.index], f.count <= 3, now - f.at >= 2 else { continue }
            }
            guard let id = displays[s.index],
                  let display = NSScreen.screens.first(where: { $0.displayID == id })
            else { continue }
            open(s.index, on: display)
        }
    }

    private func open(_ index: UInt8, on display: NSScreen) {
        let window = NSWindow(
            contentRect: display.frame, styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false // `windows` owns it
        window.title = "Punktfunk · \(Int(index) + 1)"
        window.backgroundColor = .black
        window.hasShadow = false
        window.isExcludedFromWindowsMenu = true
        window.collectionBehavior = [.fullScreenAuxiliary, .ignoresCycle]
        window.acceptsMouseMovedEvents = true
        let view = ScreenStreamView(screen: index)
        view.onPointer = { [weak self] in self?.onPointer?($0, $1, $2, $3, $4) }
        view.onButton = { [weak self] in self?.onButton?($0, $1) }
        view.onScroll = { [weak self] in self?.onScroll?($0) }
        window.contentView = view
        window.setFrame(display.frame, display: false)
        window.orderFront(nil)
        windows[index] = (window, view)
        view.start(connection: connection) { [weak self, weak view] in
            DispatchQueue.main.async { self?.ended(index, view) }
        }
    }

    /// Screen `index`'s picture ended: close its window if `view` still shows it, and count it.
    private func ended(_ index: UInt8, _ view: ScreenStreamView?) {
        guard let view, windows[index]?.view === view else { return }
        close(index)
        let count = (failed[index]?.count ?? 0) + 1
        failed[index] = (count, ProcessInfo.processInfo.systemUptime)
        if count == 4 { screensLog.warning("screen \(index) window left closed after 3 retries") }
    }

    private func close(_ index: UInt8) {
        guard let w = windows.removeValue(forKey: index) else { return }
        w.view.stop()
        w.window.orderOut(nil)
        w.window.close()
    }
}
#endif
