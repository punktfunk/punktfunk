// The home screen: a grid of saved hosts + an "On this network" section of mDNS-discovered
// hosts, with the add/settings toolbar and the pairing / speed-test / add / settings
// navigation. The connect logic lives in ContentView (it reads the @AppStorage stream mode) and
// is passed in as closures.

import PunktfunkKit
import SwiftUI
// The tvOS slide transition ships with the Xcode PROJECT only — its manifest breaks SwiftPM's
// whole-graph validation, so `swift build` never sees it. `canImport` rather than `os(tvOS)` so
// the tvOS sources still TYPECHECK from the command line (the hand recipe in the Apple client's
// README), where the module is genuinely absent; in the app it is present and the transition
// applies exactly as before.
#if os(tvOS) && canImport(SwiftUINavigationTransitions)
import SwiftUINavigationTransitions
#endif

struct HomeView: View {
    @ObservedObject var store: HostStore
    @ObservedObject var model: SessionModel
    @ObservedObject var discovery: HostDiscovery
    /// The preset catalog — the source of the card chips, the "Connect with ▸" menu, and the
    /// pinned host+preset cards the grid renders alongside their host (design §5.2a).
    @ObservedObject private var presets = PresetStore.shared
    @Binding var showAddHost: Bool
    @Binding var pairingTarget: StoredHost?
    @Binding var speedTestTarget: StoredHost?
    @Binding var libraryTarget: LibraryTarget?
    #if !os(macOS)
    @Binding var showSettings: Bool
    #endif
    /// Start a session with this host, using the given preset selection — `.inherit` for a plain
    /// card tap (the host's binding), an explicit pick from "Connect with ▸" or a pinned card.
    let connect: (StoredHost, PresetSelection) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void
    /// Pairing succeeded (tvOS PairSheet route) — pin + connect (ContentView guards staleness).
    let onPaired: (StoredHost, Data) -> Void
    /// Picked a title in the (experimental) library — start a session that launches it, with the
    /// shelf's preset (a pinned card's own; the host's binding on its primary card).
    let onLaunchTitle: (LibraryTarget, String) -> Void
    /// Stream a shelf's host without launching anything (its menu's Connect / Resume row).
    let onConnectShelf: (LibraryTarget) -> Void
    /// Explicit Wake-on-LAN of an offline host — fires the packet and waits for it to come online
    /// (the "Waking…" overlay), without connecting. Routed through ContentView's HostWaker.
    let wake: (StoredHost) -> Void
    #if os(macOS)
    @Environment(\.openWindow) private var openWindow
    #else
    /// The host whose page is pushed.
    @State private var detailTarget: StoredHost.ID?
    #endif
    #if os(iOS)
    @Environment(\.horizontalSizeClass) private var sizeClass
    /// The host whose page is up as the iPad's sheet of sections.
    @State private var sectionsHost: StoredHost?
    #endif
    #if os(iOS) || os(tvOS)
    /// An act the sectioned page handed back, run once the page is gone.
    @State private var pendingHandOff: HostPageRequest?
    #endif
    /// The start-screen pointer; the default host's card carries the accent bar.
    @AppStorage(DefaultsKey.defaultHost) private var defaultHostID = ""
    /// The outcome of the last "Send Logs to Host" — drives its alert.
    @State private var sendLogsResult: (ok: Bool, message: String)?
    /// What each paired host says this device may do to it (`design/host-actions.md` §7).
    @StateObject private var hostPower = HostPowerStore.shared
    /// What each paired host is playing right now — refreshed on the same beat below.
    @StateObject private var nowPlaying = NowPlayingStore.shared
    /// A destructive host action awaiting its confirmation.
    @State private var confirmHostAction: PendingHostAction?
    @State private var hostActionResult: (ok: Bool, message: String)?
    // How this device shows its own list. `.added` is the default because it is what the grid
    // did before it could sort at all — an update should not rearrange anyone's hosts.
    @AppStorage(DefaultsKey.hostSort) private var sortRaw = HostSort.added.rawValue
    @AppStorage(DefaultsKey.hostGrouping) private var groupingRaw = HostGrouping.none.rawValue

    var body: some View {
        NavigationStack {
            Group {
                if store.hosts.isEmpty && discoveredUnsaved.isEmpty {
                    #if os(tvOS)
                    emptyState // no pull-to-refresh on a remote; the action row carries Refresh
                    #else
                    // Inside a ScrollView purely so the pull gesture works on the ONE screen
                    // where a rescan matters most: the one that found nothing.
                    ScrollView {
                        emptyState
                            .frame(maxWidth: .infinity)
                            .containerRelativeFrame(.vertical)
                    }
                    .refreshable { await discovery.rescan() }
                    #endif
                } else {
                    ScrollView {
                        if !store.hosts.isEmpty {
                            LazyVStack(alignment: .leading, spacing: gridSpacing) {
                                ForEach(hostGroups) { group in
                                    if let title = group.title {
                                        groupHeader(title, accent: group.accent)
                                    }
                                    LazyVGrid(columns: gridColumns, spacing: gridSpacing) {
                                        ForEach(group.cards) { card in
                                            hostCard(card.host, pinned: card.pinned)
                                        }
                                    }
                                }
                            }
                            .padding()
                            // Mirror of the action row's focusSection below: an UPWARD move from
                            // the centered buttons must land back in the grid even when no card
                            // sits in the buttons' columns (a lone top-left card, say). The grid
                            // spans the row, so the section catches every upward ray.
                            #if os(tvOS)
                            .focusSection()
                            #endif
                        }
                        if !discoveredUnsaved.isEmpty {
                            discoveredSection
                        }
                        #if os(tvOS)
                        // Actions live below the hosts, not between them.
                        HStack(spacing: 32) {
                            Button {
                                showAddHost = true
                            } label: {
                                Label("Add Host", systemImage: "plus")
                            }
                            refreshButton
                        }
                        .padding(.top, 24)
                        // One FULL-WIDTH focus target for any downward move out of the grid.
                        // focusSection alone is not enough: the engine tests the section's
                        // FRAME, and a content-hugging centered HStack only overlaps the middle
                        // columns — a swipe down from an outer card dead-ends and the actions
                        // are unreachable by remote. Stretching the section across the row means
                        // every column's downward ray hits it.
                        .frame(maxWidth: .infinity)
                        .focusSection()
                        #endif
                    }
                    #if !os(tvOS)
                    .refreshable { await discovery.rescan() }
                    #endif
                }
            }
            // A TV's tab bar names the place, so its root carries no title.
            #if !os(tvOS)
            .navigationTitle("Punktfunk")
            #endif
            // Browse the LAN for advertised hosts only while the grid is up — not during a
            // session. The home appears/disappears as the stream swaps in and out.
            .onAppear { discovery.start() }
            .onDisappear { discovery.stop() }
            // Presence while the grid is up (`HostStore.keepPresence`); the `.task` is cancelled
            // on disappear, matching `discovery.stop()`.
            .task {
                await store.keepPresence(
                    discovery: discovery, power: hostPower, nowPlaying: nowPlaying)
            }
            // The host page, from a card's ⓘ or its menu (design §2.4), and the speed test pushed
            // from it. The Mac opens both in the host's own window (`MacHostWindow`).
            #if !os(macOS)
            .navigationDestination(item: $detailTarget) { id in
                #if os(tvOS)
                // The iPad's sectioned page; what it hands back runs once the pop has finished.
                HostSectionsView(hostID: id, store: store) { request in
                    pendingHandOff = request
                    detailTarget = nil
                }
                .onDisappear(perform: runHandOff)
                #else
                HostDetailView(
                    store: store, hostID: id, actions: { hostActions(for: $0, pinned: nil) })
                #endif
            }
            .navigationDestination(item: $speedTestTarget) { host in
                SpeedTestView(host: host)
                    .navigationTitle("Speed Test")
                    #if os(iOS)
                    .navigationBarTitleDisplayMode(.inline)
                    #endif
            }
            #endif
            #if os(tvOS)
            // Pushed routes — the Settings-app navigation feel (push animation, Menu
            // pops) instead of modal overlays.
            .navigationDestination(isPresented: $showAddHost) {
                AddHostSheet { store.add($0) }
            }
            .navigationDestination(item: $pairingTarget) { host in
                PairSheet(host: host) { fingerprint in onPaired(host, fingerprint) }
            }
            #endif
            #if !os(tvOS)
            .toolbar {
                #if os(iOS)
                // Adjacent trailing items share one glass pill (the system default).
                ToolbarItem(placement: .topBarTrailing) { settingsButton }
                if showsArrangeMenu {
                    ToolbarItem(placement: .topBarTrailing) { arrangeMenu }
                }
                ToolbarItem(placement: .topBarTrailing) { refreshButton }
                ToolbarItem(placement: .topBarTrailing) { addHostButton }
                #else
                if showsArrangeMenu {
                    ToolbarItem(placement: .primaryAction) {
                        arrangeMenu
                            .help("Sort and group the host list")
                    }
                }
                ToolbarItem(placement: .primaryAction) {
                    refreshButton
                        .help("Scan the network for hosts again")
                }
                ToolbarItem(placement: .primaryAction) {
                    addHostButton
                        .help("Add a host")
                }
                ToolbarItem {
                    SettingsLink {
                        Label("Settings", systemImage: "gearshape")
                    }
                    .help("Stream mode and settings")
                }
                #endif
            }
            #endif
        }
        .alert(
            sendLogsResult?.ok == true ? "Logs sent" : "Couldn't send logs",
            isPresented: Binding(
                get: { sendLogsResult != nil },
                set: { if !$0 { sendLogsResult = nil } })
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(sendLogsResult?.message ?? "")
        }
        // A destructive host action asks first: restart and shut down lose whatever is running
        // on that machine, and a mis-tap on a phone must not be able to do that. Sleep is
        // reversible from the same menu ("Wake Host"), so it never reaches here.
        .alert(
            confirmHostAction.map { "\($0.action.label)?" } ?? "",
            isPresented: Binding(
                get: { confirmHostAction != nil },
                set: { if !$0 { confirmHostAction = nil } })
        ) {
            Button("Cancel", role: .cancel) { confirmHostAction = nil }
            if let pending = confirmHostAction {
                Button(pending.action.label, role: .destructive) {
                    confirmHostAction = nil
                    runHostAction(pending.action, on: pending.host)
                }
            }
        } message: {
            Text(
                confirmHostAction.map {
                    "This ends every stream from \($0.host.displayName) and anything running "
                        + "on it. You'll need to wake or start it again."
                } ?? "")
        }
        .alert(
            hostActionResult?.ok == true ? "On its way" : "Couldn't do that",
            isPresented: Binding(
                get: { hostActionResult != nil },
                set: { if !$0 { hostActionResult = nil } })
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(hostActionResult?.message ?? "")
        }
        #if os(macOS)
        .frame(minWidth: 480, minHeight: 360)
        #endif
        .tvPushSlide()
        #if !os(tvOS)
        .sheet(isPresented: $showAddHost) {
            AddHostSheet { store.add($0) }
        }
        #if os(iOS)
        // SettingsView owns its own NavigationSplitView (sidebar + detail) and Done button, so it
        // is presented directly — wrapping it in a NavigationStack here would nest a split view in
        // a stack (double title bars). `settingsSheetSizing()` widens the sheet on iPad for the
        // two-column layout.
        .sheet(isPresented: $showSettings) {
            SettingsView()
                .settingsSheetSizing()
        }
        // The iPad's host page, laid out like the Mac's host window.
        .sheet(item: $sectionsHost, onDismiss: runHandOff) { host in
            HostSectionsView(hostID: host.id, store: store) { request in
                pendingHandOff = request
                sectionsHost = nil
            }
            .settingsSheetSizing()
        }
        #endif
        #endif
    }

    // MARK: - Cards

    /// The grid's bands, ordered and divided per this device's preference — cards and all, so a
    /// pinned card can be filed under the preset it connects with rather than under its host's
    /// binding (`HostArrangement`).
    private var hostGroups: [HostGroup] {
        HostArrangement.groups(
            hosts: store.hosts, catalog: presets.catalog,
            online: Set(store.hosts.filter(isOnline).map(\.id)),
            sort: HostSort(rawValue: sortRaw) ?? .added,
            grouping: HostGrouping(rawValue: groupingRaw) ?? .none)
    }

    /// Online = answered the last reachability probe. A live advert is deliberately NOT enough:
    /// it is a cache entry a sleeping host keeps alive for up to 75 minutes (`HostStore.isReachable`).
    /// One definition, used by the cards and by the Status grouping alike.
    private func isOnline(_ host: StoredHost) -> Bool {
        store.probedOnline.contains(host.id)
    }

    private func groupHeader(_ title: String, accent: String?) -> some View {
        HStack(spacing: 7) {
            Circle()
                .fill(Color(hex: accent ?? "") ?? Color.secondary.opacity(0.5))
                .frame(width: 7, height: 7)
                .accessibilityHidden(true) // the title says it
            Text(title)
                .font(.geist(13, .semibold, relativeTo: .subheadline))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
                .tracking(0.6)
        }
        .padding(.top, 4)
    }

    private func hostCard(_ host: StoredHost, pinned: StreamPreset?) -> some View {
        HostCardView(
            host: host,
            isOnline: isOnline(host),
            isConnecting: model.phase == .connecting && model.activeHost?.id == host.id,
            // The bar marks the default HOST; pinned cards stay quiet.
            isDefaultHost: pinned == nil && host.id == defaultHost,
            isBusy: model.isBusy,
            actions: hostActions(for: host, pinned: pinned),
            pinnedPreset: pinned,
            nowPlaying: nowPlaying.title(for: host))
    }

    /// Everything a card and the host page can do for `host`, run on this grid's sheets.
    private func hostActions(for host: StoredHost, pinned: StreamPreset?) -> HostActions {
        HostActions(
            host: host, pinned: pinned, online: isOnline(host), store: store,
            presets: presets.presets, power: hostPower.actions(for: host),
            surface: HostActionSurface(
                connect: { connect(host, $0) },
                pair: { if !model.isBusy { pairingTarget = host } },
                browse: { libraryTarget = LibraryTarget(host: host, preset: $0) },
                speedTest: { if !model.isBusy { speedTestTarget = host } },
                sendLogs: { Task { sendLogsResult = await SendLogs.toHost(host) } },
                wake: { wake(host) },
                showDetails: { showDetails(host) },
                runPower: { hostAction($0, on: host) }))
    }

    /// The host page: its own window on the Mac, a sheet of sections on the iPad, pushed on the
    /// iPhone and Apple TV.
    private func showDetails(_ host: StoredHost) {
        #if os(macOS)
        openWindow(id: MacHostWindow.sceneID, value: host.id)
        #elseif os(iOS)
        if sizeClass == .regular { sectionsHost = host } else { detailTarget = host.id }
        #else
        detailTarget = host.id
        #endif
    }

    #if os(iOS) || os(tvOS)
    /// The iPad's host sheet or the TV's host page closed on an act that belongs to the grid: run
    /// it now it is gone.
    private func runHandOff() {
        guard let request = pendingHandOff else { return }
        pendingHandOff = nil
        guard let host = store.hosts.first(where: { $0.id == request.hostID }) else { return }
        switch request {
        case .connect(_, let selection): connect(host, selection)
        case .browse: libraryTarget = LibraryTarget(host: host)
        case .wake: wake(host)
        case .pair: if !model.isBusy { pairingTarget = host }
        }
    }
    #endif

    /// A host action picked from a card's menu: explain an unavailable one, confirm a
    /// destructive one, run the rest.
    private func hostAction(_ action: HostAction, on host: StoredHost) {
        guard action.available else {
            hostActionResult = (
                false,
                action.unavailableReason ?? "\(action.label) isn't available right now")
            return
        }
        if action.danger {
            confirmHostAction = PendingHostAction(host: host, action: action)
        } else {
            runHostAction(action, on: host)
        }
    }

    private func runHostAction(_ action: HostAction, on host: StoredHost) {
        Task { hostActionResult = await hostPower.invoke(action, on: host) }
    }

    private var discoveredSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("On this network", systemImage: "antenna.radiowaves.left.and.right")
                .font(.geist(15, .semibold, relativeTo: .headline))
                .foregroundStyle(.secondary)
                .padding(.horizontal)
            LazyVGrid(columns: gridColumns, spacing: gridSpacing) {
                ForEach(discoveredUnsaved) { discovered in
                    DiscoveredCardView(
                        discovered: discovered, isBusy: model.isBusy,
                        onConnect: { connectDiscovered(discovered) })
                }
            }
        }
        .padding([.horizontal, .bottom])
        .padding(.top, store.hosts.isEmpty ? 0 : 8)
        // Same reachability contract as the saved grid above — see its focusSection comment.
        #if os(tvOS)
        .focusSection()
        #endif
    }

    /// Discovered hosts not already saved (see `HostDiscovery.unsaved` — shared with the gamepad
    /// launcher so both screens classify hosts identically).
    private var discoveredUnsaved: [DiscoveredHost] {
        discovery.unsaved(among: store.hosts)
    }

    /// The host Start in opens on: explicit, or the only paired one.
    private var defaultHost: StoredHost.ID? {
        StartScreen.defaultHost(id: defaultHostID, hosts: store.hosts).host?.id
    }

    // MARK: - Chrome

    private var emptyState: some View {
        ContentUnavailableView {
            Label("No Hosts", systemImage: "rectangle.connected.to.line.below")
        } description: {
            Text("Add your Punktfunk host with the + button, or scan the network again.")
        } actions: {
            Button("Add Host") { showAddHost = true }
                .glassProminentButtonStyle()
                #if os(iOS)
                .controlSize(.large)
                #endif
            // The screen a host SHOULD have appeared on is where a rescan is worth offering
            // outright rather than hiding behind a pull gesture.
            Button("Scan Again") { discovery.refresh() }
                .disabled(discovery.isScanning)
                #if os(iOS)
                .controlSize(.large)
                #endif
        }
    }

    private var addHostButton: some View {
        Button {
            showAddHost = true
        } label: {
            Label("Add Host", systemImage: "plus")
        }
    }

    /// Re-run mDNS discovery from scratch. Discovery heals itself now (`HostDiscovery`'s sweep),
    /// so this is the fallback the field asked for — and the fastest way past the iOS
    /// local-network permission gate, which only a NEW browser can clear.
    private var refreshButton: some View {
        Button {
            discovery.refresh()
        } label: {
            Label("Refresh", systemImage: "arrow.clockwise")
        }
        .disabled(discovery.isScanning)
    }

    #if !os(tvOS)
    /// One host has no order and nothing to divide, so the control stays out of the way until
    /// there is a list to arrange.
    private var showsArrangeMenu: Bool { store.hosts.count > 1 }

    /// Sort and group, as two inline pickers in one menu — both are about the same list, and
    /// splitting them across two toolbar items would say otherwise.
    private var arrangeMenu: some View {
        Menu {
            Picker("Sort By", selection: $sortRaw) {
                ForEach(HostSort.allCases) { option in
                    Label(option.label, systemImage: option.symbol).tag(option.rawValue)
                }
            }
            .pickerStyle(.inline)
            Divider()
            Picker("Group By", selection: $groupingRaw) {
                ForEach(HostGrouping.allCases) { option in
                    Label(option.label, systemImage: option.symbol).tag(option.rawValue)
                }
            }
            .pickerStyle(.inline)
        } label: {
            Label("Sort and Group", systemImage: "line.3.horizontal.decrease.circle")
        }
        #if os(macOS)
        // A Menu draws its label in the ACCENT colour where a Button draws it in the label
        // colour, so next to Add Host and Settings this one came out brand-purple. macOS only:
        // on iOS every toolbar item is accent-tinted, and pinning this one to primary would make
        // it the odd one out there instead.
        .tint(.primary)
        #endif
    }
    #endif

    #if !os(macOS)
    private var settingsButton: some View {
        Button {
            showSettings = true
        } label: {
            Label("Settings", systemImage: "gearshape")
        }
    }
    #endif

    /// The columns fill the width everywhere, so no window width leaves a gutter beside the cards:
    /// adaptive packs as many as fit and widens them to close the gap, which keeps a card under
    /// twice the minimum. Touch-first on iOS: one column on iPhone portrait, 3–4 on iPad.
    private var gridColumns: [GridItem] {
        #if os(macOS)
        [GridItem(.adaptive(minimum: 250), spacing: 16)]
        #elseif os(tvOS)
        // Tracks CardMetrics' 10-foot sizes — at the 30pt name a 320pt column truncates
        // every hostname longer than ~10 characters.
        [GridItem(.adaptive(minimum: 460), spacing: 48)]
        #else
        [GridItem(.adaptive(minimum: 280), spacing: 16)]
        #endif
    }

    private var gridSpacing: CGFloat {
        #if os(tvOS)
        48 // the focused card scales up — give it room instead of overlapping siblings
        #else
        16
        #endif
    }
}

extension View {
    /// The Settings-app slide for every push in a tvOS stack; SwiftUI's own is a bare crossfade.
    /// Spring-driven at ~0.87 damping: it settles fast, with no visible overshoot.
    @ViewBuilder func tvPushSlide() -> some View {
        #if os(tvOS) && canImport(SwiftUINavigationTransitions)
        customNavigationTransition(
            .slide.animation(.interpolatingSpring(stiffness: 300, damping: 30)))
        #else
        self
        #endif
    }
}
