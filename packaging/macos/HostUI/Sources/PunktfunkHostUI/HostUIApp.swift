// The app: an NSStatusItem whose button opens an NSPopover hosting PanelView, and the setup
// window. AppKit's lifecycle rather than SwiftUI's MenuBarExtra: MenuBarExtra has no way to
// open its panel from code, and SIGUSR1 (a second launch) must show it.
// For checks without input: PUNKTFUNK_HOST_UI_OPEN=panel|onboarding opens one at launch, and
// PUNKTFUNK_HOST_UI_STEP=<step> (welcome, screen, done, …) picks the setup page.
import AppKit
import SwiftUI

@MainActor
public final class HostUIApp: NSObject, NSApplicationDelegate, NSPopoverDelegate, NSWindowDelegate {
    private let store: HostStore
    private var statusItem: NSStatusItem?
    private let popover = NSPopover()
    private var setupWindow: NSWindow?
    private var onboarding: Onboarding?
    private var signals: DispatchSourceSignal?
    private static var shared: HostUIApp?

    static let shownKey = "OnboardingShown"

    init(store: HostStore) {
        self.store = store
    }

    /// Owns the main thread from here on; the process ends through `exit`, never a return.
    public static func run(_ store: HostStore) -> Never {
        let app = NSApplication.shared
        let delegate = HostUIApp(store: store)
        shared = delegate
        app.delegate = delegate
        app.setActivationPolicy(.accessory)
        app.mainMenu = mainMenu()
        app.run()
        exit(0)
    }

    public func applicationDidFinishLaunching(_ notification: Notification) {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        item.button?.target = self
        item.button?.action = #selector(togglePanel)
        statusItem = item
        popover.behavior = .transient
        popover.delegate = self
        let host = NSHostingController(rootView: PanelView(store: store))
        host.sizingOptions = .preferredContentSize
        popover.contentViewController = host
        store.onChange = { [weak self] in self?.refreshGlyph() }
        store.onShowOnboarding = { [weak self] in self?.showSetup(firstLaunch: false) }
        refreshGlyph()
        store.start()
        watchSIGUSR1()

        let firstLaunch = !UserDefaults.standard.bool(forKey: Self.shownKey)
        switch ProcessInfo.processInfo.environment["PUNKTFUNK_HOST_UI_OPEN"] {
        // The status item is not on screen until a moment after launch.
        case "panel": DispatchQueue.main.asyncAfter(deadline: .now() + 1) { self.showPanel() }
        case "onboarding": showSetup(firstLaunch: firstLaunch)
        default:
            if store.api != nil, firstLaunch || store.grants.missingRequired || !store.inApplications {
                showSetup(firstLaunch: firstLaunch)
            }
        }
    }

    /// A quit from outside (the Dock, macOS's "Quit & Reopen", logout) goes through the host's
    /// SIGTERM handler, which exits once sessions end and displays are restored.
    public func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard store.context.hostExecutable != nil else { return .terminateNow }
        kill(getpid(), SIGTERM)
        return .terminateLater
    }

    private func refreshGlyph() {
        guard let button = statusItem?.button else { return }
        button.image = Glyph.image(store.glyph)
        button.toolTip = store.state.headline
        button.setAccessibilityLabel(store.state.headline)
    }

    /// The host ignores SIGUSR1 before it loads the UI; a dispatch source still sees it.
    private func watchSIGUSR1() {
        signal(SIGUSR1, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: SIGUSR1, queue: .main)
        source.setEventHandler { [weak self] in
            MainActor.assumeIsolated {
                guard let self else { return }
                if let window = self.setupWindow, window.isVisible { self.bringToFront(window) } else { self.showPanel() }
            }
        }
        source.resume()
        signals = source
    }

    @objc private func togglePanel() {
        if popover.isShown { popover.performClose(nil) } else { showPanel() }
    }

    func showPanel() {
        guard let button = statusItem?.button, !popover.isShown else { return }
        store.panelOpen = true
        NSApp.activate()
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        popover.contentViewController?.view.window?.makeKey()
    }

    public func popoverDidClose(_ notification: Notification) {
        store.panelOpen = false
        store.confirm = nil
    }

    func showSetup(firstLaunch: Bool) {
        popover.performClose(nil)
        if let window = setupWindow {
            bringToFront(window)
            return
        }
        let model = Onboarding(store: store, firstLaunch: firstLaunch)
        if let page = ProcessInfo.processInfo.environment["PUNKTFUNK_HOST_UI_STEP"],
           let i = model.steps.firstIndex(where: { "\($0)" == page }) { model.index = i }
        if model.step == .done { model.arrive() }
        model.onClose = { [weak self] in self?.setupWindow?.close() }
        let window = NSWindow(contentViewController: NSHostingController(rootView: OnboardingView(model: model)))
        window.styleMask = [.titled, .closable]
        window.title = String(localized: L10n.setupMacWindowTitle)
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.center()
        onboarding = model
        setupWindow = window
        UserDefaults.standard.set(true, forKey: Self.shownKey)
        model.watch()
        bringToFront(window)
    }

    /// An accessory app's window can't come to the front; while one shows, the app is regular.
    private func bringToFront(_ window: NSWindow) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate()
        window.makeKeyAndOrderFront(nil)
    }

    public func windowWillClose(_ notification: Notification) {
        guard (notification.object as? NSWindow) === setupWindow else { return }
        onboarding?.stopWatching()
        onboarding = nil
        setupWindow = nil
        NSApp.setActivationPolicy(.accessory)
    }

    /// Only Quit: the app shows a menu bar of its own only while the setup window is open.
    private static func mainMenu() -> NSMenu {
        let menu = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: String(localized: L10n.trayMacQuit),
                        action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        menu.addItem(appItem)
        return menu
    }
}
