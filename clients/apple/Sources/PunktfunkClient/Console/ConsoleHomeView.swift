// The console as this app mounts it: one Metal view over the shared shell, where the controller
// home goes. Everything inside — home, library, settings, pairing, the host menu —
// is drawn by the console; this view only owns its lifetime and the wiring to the app.

import PunktfunkKit
import PunktfunkShared
import SwiftUI

struct ConsoleHomeView: View {
    @ObservedObject var store: HostStore
    @ObservedObject var model: SessionModel
    @ObservedObject var discovery: HostDiscovery
    @ObservedObject var waker: HostWaker
    @ObservedObject var presets = PresetStore.shared
    /// A shelf to open: at first mount it is where the console starts, later it is a
    /// navigation inside it. Cleared once taken, so it never lingers as an open layer.
    @Binding var entry: LibraryTarget?
    /// Something to say that has no screen of its own — a deep link that went nowhere. The
    /// console shows it as a toast; the app's alert stays out of the way while the console is up.
    @Binding var notice: String?
    /// A pairing asked for outside the console (the trust card's "Pair instead", a Mac host
    /// window): the console's Pair screen takes it. Cleared once taken.
    @Binding var pairing: StoredHost?
    /// A link waiting on the player's word: the console asks, in place of a system alert.
    @Binding var linkConfirm: ContentView.DeepLinkConfirm?
    let runLink: (ContentView.DeepLinkConfirm) -> Void
    /// The console could not be built (no Metal device, or the shell refused).
    let onFailed: () -> Void
    let onPaired: (StoredHost, Data) -> Void
    let connect: (StoredHost, PresetSelection) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void
    let requestAccess: (StoredHost) -> Void
    let requestAccessDiscovered: (DiscoveredHost) -> Void
    let launchTitle: (LibraryTarget, String) -> Void
    let connectShelf: (LibraryTarget) -> Void
    let wakeOnly: (StoredHost) -> Void
    /// A Back the console did not take. On a TV the press is the system's, so nothing is bound
    /// while the console sits at its root and this never fires there.
    var onQuit: () -> Void = {}

    @State private var console: ConsoleModel?

    var body: some View {
        content
            .ignoresSafeArea()
            .onAppear(perform: start)
            .onDisappear {
                console?.detach()
                console = nil
            }
            // The console's host rows read the same sweep as the touch home, and nothing else
            // runs it while the console is up.
            .task {
                await store.keepPresence(
                    discovery: discovery, power: .shared, nowPlaying: .shared)
            }
            .onChange(of: model.phase) { was, now in report(from: was, to: now) }
            .onChange(of: model.errorMessage) { _, message in
                // The app's "Connection failed" alert stays down while the console is up, so the
                // console says it — on the card that asked for the connect.
                guard let message, !message.isEmpty else { return }
                console?.session(.failed, message: message)
                model.errorMessage = nil
            }
            .onChange(of: notice) { _, text in
                guard let text, !text.isEmpty else { return }
                console?.notice(text)
                notice = nil
            }
            .onChange(of: entry) { _, shelf in
                guard let shelf, let console else { return }
                console.navigate(to: shelf, pin: preset(of: shelf))
                entry = nil
            }
            .onChange(of: pairing?.id) { _, _ in takePairing() }
            .onChange(of: linkConfirm?.id) { _, _ in takeLinkConfirm() }
            .onChange(of: waker.waking) { _, _ in console?.pushWake() }
    }

    @ViewBuilder private var content: some View {
        if let console {
            ConsoleView(
                bridge: console.bridge, device: console.device, queue: console.queue,
                delegate: console
            )
            #if os(tvOS)
            .modifier(SystemEntryCover(model: console))
            #endif
        } else {
            // Black for the frame before `onFailed` swaps in the app's own UI.
            Color.black
        }
    }

    private func start() {
        guard console == nil else { return }
        let opening = entry
        entry = nil
        guard
            let built = ConsoleModel(
                entry: opening?.host, pin: opening.flatMap(preset(of:)), store: store,
                discovery: discovery, presets: presets, power: .shared, nowPlaying: .shared,
                waker: waker,
                actions: ConsoleModel.Actions(
                    connect: connect, connectDiscovered: connectDiscovered,
                    requestAccess: requestAccess, requestAccessDiscovered: requestAccessDiscovered,
                    launchTitle: launchTitle, connectShelf: connectShelf, wakeOnly: wakeOnly,
                    cancelConnect: { self.model.disconnect() },
                    showStream: { self.model.revealStream() },
                    holding: { self.model.consoleHold = $0 },
                    paired: onPaired, quit: onQuit))
        else {
            onFailed()
            return
        }
        built.attach()
        console = built
        // Set while the console was off screen: a session that just ended says why here, since
        // `onChange` never sees a value that was already there at mount.
        if let message = model.errorMessage, !message.isEmpty {
            built.session(.ended, message: message)
            model.errorMessage = nil
        }
        takePairing()
        takeLinkConfirm()
    }

    private func takePairing() {
        guard let host = pairing, let console else { return }
        pairing = nil
        console.pair(host)
    }

    private func takeLinkConfirm() {
        guard let confirm = linkConfirm, let console else { return }
        console.prompt(
            id: "link", title: "Open this link?", message: confirm.message,
            choices: [confirm.actionTitle, "Cancel"]
        ) { choice in
            // The link may have been replaced while the question was up.
            guard linkConfirm?.id == confirm.id else { return }
            linkConfirm = nil
            if choice == 0 { runLink(confirm) }
        }
    }

    private func preset(of shelf: LibraryTarget) -> StreamPreset? {
        shelf.pinnedPresetID.flatMap { id in presets.presets.first { $0.id == id } }
    }

    /// Where the session the console asked for stands, in the shell's own vocabulary. Idle
    /// is only news after a session: at launch it is where the app already is.
    private func report(from was: SessionModel.Phase, to now: SessionModel.Phase) {
        switch now {
        case .connecting, .awaitingTrust: console?.session(.connecting)
        case .streaming: console?.session(.streaming)
        case .idle where was != .idle: console?.session(.ended)
        case .idle: break
        }
    }
}

#if os(tvOS)
/// A console field typed on the tvOS keyboard, which is where iPhone typing and dictation
/// live. The model raises it when the console opens a field.
private struct SystemEntryCover: ViewModifier {
    @ObservedObject var model: ConsoleModel

    func body(content: Content) -> some View {
        content.fullScreenCover(item: $model.systemEntry) { entry in
            TVTextEntry(
                title: entry.label, text: entry.text,
                keyboardType: entry.digits ? .numberPad : .default
            ) { text in model.finishEntry(text) }
        }
    }
}
#endif
