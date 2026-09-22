// The console as this app mounts it: one Metal view over the shared shell, in the place
// `GamepadHomeView` held. Everything inside — home, library, settings, pairing, the host menu —
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
    let onPaired: (StoredHost, Data) -> Void
    let connect: (StoredHost, PresetSelection) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void
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
            .onChange(of: model.phase) { was, now in report(from: was, to: now) }
            .onChange(of: entry) { _, shelf in
                guard let shelf, let console else { return }
                console.navigate(to: shelf, pin: preset(of: shelf))
                entry = nil
            }
            .onChange(of: waker.waking) { _, _ in console?.pushWake() }
            // A screen this app owns, asked for by a console row (`PlatformScreen`). The console
            // keeps drawing underneath and takes no input while one is up.
            .sheet(isPresented: platformScreen) {
                platformScreenBody.onDisappear { console?.platformScreen = nil }
            }
    }

    @ViewBuilder private var content: some View {
        if let console {
            ConsoleView(
                bridge: console.bridge, device: console.device, queue: console.queue,
                delegate: console
            )
            #if os(tvOS)
            // Bound only while the console has somewhere to go back to: at its root the Menu
            // press belongs to tvOS, which is what takes the player Home (the HIG's rule).
            .onExitCommand(perform: console.bridge.atRoot ? nil : { _ = console.back() })
            #endif
        } else {
            // No Metal device, or the shell refused to build: the app's own UI is the fallback,
            // and `ConsoleModel` has already said why in the log.
            Color.black
        }
    }

    private func start() {
        guard console == nil else { return }
        let opening = entry
        entry = nil
        let model = ConsoleModel(
            entry: opening?.host, pin: opening.flatMap(preset(of:)), store: store,
            discovery: discovery, presets: presets, power: .shared, nowPlaying: .shared,
            waker: waker,
            actions: ConsoleModel.Actions(
                connect: connect, connectDiscovered: connectDiscovered, launchTitle: launchTitle,
                connectShelf: connectShelf, wakeOnly: wakeOnly,
                cancelConnect: { self.model.disconnect() },
                showStream: { self.model.revealStream() },
                paired: onPaired, quit: onQuit))
        model?.attach()
        console = model
    }

    private var platformScreen: Binding<Bool> {
        Binding(get: { console?.platformScreen != nil }, set: { if !$0 { console?.platformScreen = nil } })
    }

    @ViewBuilder private var platformScreenBody: some View {
        switch console?.platformScreen {
        case "licenses": AcknowledgementsView()
        default: EmptyView()
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
