// A saved host's page as sections beside a sidebar: the Mac's host window, the iPad's host
// sheet and the TV's pushed page. Acts that belong to the grid (connect, browse, wake, pair) go
// back through `handOff`, which closes the page; edits, power, logs and the speed test stay on it.

import PunktfunkKit
import SwiftUI

/// An act the host page hands back to the window that shows the grid.
enum HostPageRequest: Equatable {
    case connect(StoredHost.ID, PresetSelection)
    case browse(StoredHost.ID)
    case wake(StoredHost.ID)
    case pair(StoredHost.ID)

    var hostID: StoredHost.ID {
        switch self {
        case .connect(let id, _), .browse(let id), .wake(let id), .pair(let id): id
        }
    }
}

struct HostSectionsView: View {
    let hostID: StoredHost.ID
    @ObservedObject var store: HostStore
    /// Runs an act that leaves the page, and closes the page.
    let handOff: (HostPageRequest) -> Void
    @ObservedObject private var presets = PresetStore.shared
    @ObservedObject private var hostPower = HostPowerStore.shared
    @ObservedObject private var hostScreens = HostScreenStore.shared
    @Environment(\.dismiss) private var dismiss
    @State private var section: HostSection
    @State private var confirmPower: PendingHostAction?
    /// The last send-logs or power outcome, for its alert.
    @State private var outcome: (title: String, message: String)?
    #if os(tvOS)
    @FocusState private var focusedSection: HostSection?
    #endif

    init(
        hostID: StoredHost.ID, store: HostStore, section: HostSection = .overview,
        handOff: @escaping (HostPageRequest) -> Void
    ) {
        self.hostID = hostID
        self.store = store
        self.handOff = handOff
        _section = State(initialValue: section)
    }

    var body: some View {
        Group {
            #if os(tvOS)
            tvContent
            #else
            NavigationSplitView {
                List(sections, selection: sectionSelection) { section in
                    Label(section.title, systemImage: section.symbol)
                }
                #if os(macOS)
                .navigationSplitViewColumnWidth(min: 150, ideal: 170, max: 220)
                #else
                .navigationTitle(store.hosts.first { $0.id == hostID }?.displayName ?? "")
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button(L10n.commonDone) { dismiss() }
                    }
                }
                #endif
            } detail: {
                sectionPane
                    #if os(macOS)
                    .navigationSubtitle(section.title)
                    #endif
            }
            #endif
        }
        .alert(
            confirmPower.map { L10n.hostPowerConfirmTitle(action: $0.action.label) } ?? "",
            isPresented: Binding(
                get: { confirmPower != nil }, set: { if !$0 { confirmPower = nil } })
        ) {
            Button(L10n.commonCancel, role: .cancel) { confirmPower = nil }
            if let pending = confirmPower {
                Button(pending.action.label, role: .destructive) {
                    confirmPower = nil
                    run(pending.action, on: pending.host)
                }
            }
        } message: {
            Text(
                confirmPower.map {
                    L10n.hostcardActionConfirmBody(host: $0.host.displayName)
                } ?? "")
        }
        .alert(
            outcome?.title ?? "",
            isPresented: Binding(get: { outcome != nil }, set: { if !$0 { outcome = nil } })
        ) {
            Button(L10n.commonOk, role: .cancel) {}
        } message: {
            Text(outcome?.message ?? "")
        }
        .onAppear {
            if let host = store.hosts.first(where: { $0.id == hostID }) {
                hostPower.refresh(host)
                hostScreens.refresh(host)
            }
        }
        #if os(macOS)
        // System type sizes, like the Settings window: the touch UI's 17 pt Geist makes a Mac
        // form's rows and footers oversized.
        .font(nil)
        #endif
    }

    /// The chosen section: the speed test's own page, or the host page cut to that section.
    @ViewBuilder private var sectionPane: some View {
        if section == .speedTest {
            speedTestPane
        } else {
            HostDetailView(store: store, hostID: hostID, actions: actions, only: section)
        }
    }

    /// The speed test waits for Start here: picking the row is not asking for a burst.
    @ViewBuilder private var speedTestPane: some View {
        if let host = store.hosts.first(where: { $0.id == hostID }) {
            if host.pinnedSHA256 != nil {
                SpeedTestView(host: host, startsOnAppear: false)
                    #if os(macOS)
                    .navigationTitle(host.displayName)
                    #elseif os(iOS) || os(visionOS)
                    .navigationTitle(HostSection.speedTest.title)
                    #endif
            } else {
                ContentUnavailableView(
                    String(localized: L10n.hostcardPairFirst), systemImage: "lock",
                    description: Text(
                        L10n.appleHostcardSpeedTestPairFirstBody(host: host.displayName)))
            }
        }
    }

    /// The sidebar's rows. The demo host has no speed test to run; a host that offers this
    /// device no screens has no Screen row.
    private var sections: [HostSection] {
        let offersScreens = store.hosts.first { $0.id == hostID }
            .flatMap(hostScreens.targets(for:)) != nil
        return HostSection.allCases.filter {
            ($0 != .speedTest || hostID != DemoMode.hostID) && ($0 != .screen || offersScreens)
        }
    }

    /// A click on empty space deselects a List; the page always shows a section.
    private var sectionSelection: Binding<HostSection?> {
        Binding(get: { section }, set: { if let next = $0 { section = next } })
    }

    /// The grid's acts, run from here: sheets and prompts on this page, the rest handed back.
    private func actions(for host: StoredHost) -> HostActions {
        HostActions(
            host: host, pinned: nil, online: store.probedOnline.contains(host.id), store: store,
            presets: presets.presets, power: hostPower.actions(for: host),
            screens: hostScreens.targets(for: host),
            surface: HostActionSurface(
                connect: { handOff(.connect(host.id, $0)) },
                pair: { handOff(.pair(host.id)) },
                browse: { _ in handOff(.browse(host.id)) },
                speedTest: { section = .speedTest },
                sendLogs: {
                    Task {
                        let sent = await SendLogs.toHost(host)
                        outcome = (
                            String(
                                localized: sent.ok
                                    ? L10n.appleHostcardLogsSentTitle
                                    : L10n.appleHostcardLogsFailedTitle),
                            sent.message)
                    }
                },
                wake: { handOff(.wake(host.id)) },
                showDetails: {},
                runPower: { power($0, on: host) },
                screenRefused: {
                    outcome = (String(localized: L10n.appleHostcardActionFailedTitle), $0)
                }))
    }

    /// A power action on its `HostOffers.route`, as the grid runs it.
    private func power(_ action: HostAction, on host: StoredHost) {
        switch HostOffers.route(action: action) {
        case .explain(let why):
            outcome = (String(localized: L10n.appleHostcardActionFailedTitle), why)
        case .confirm: confirmPower = PendingHostAction(host: host, action: action)
        case .run: run(action, on: host)
        }
    }

    private func run(_ action: HostAction, on host: StoredHost) {
        Task {
            let done = await hostPower.invoke(action, on: host)
            outcome = (
                String(
                    localized: done.ok
                        ? L10n.appleHostcardActionDoneTitle : L10n.appleHostcardActionFailedTitle),
                done.message)
        }
    }
}

#if os(tvOS)
extension HostSectionsView {
    /// On a TV the sections are a sidebar that focus picks, beside the chosen one, as in Settings;
    /// the host's name heads the page over both, left-aligned.
    var tvContent: some View {
        VStack(alignment: .leading, spacing: 28) {
            Text(store.hosts.first { $0.id == hostID }?.displayName ?? "")
                .font(.geist(48, .bold, relativeTo: .title))
                .lineLimit(1)
            HStack(alignment: .top, spacing: 48) {
                VStack(alignment: .leading, spacing: 0) {
                    VStack(alignment: .leading, spacing: 8) {
                        ForEach(sections) { item in
                            Button {
                                section = item
                            } label: {
                                HStack {
                                    Label(item.title, systemImage: item.symbol)
                                    Spacer(minLength: 16)
                                    if item == section {
                                        Image(systemName: "chevron.forward")
                                            .foregroundStyle(.secondary)
                                    }
                                }
                            }
                            .buttonStyle(TVSidebarRowStyle(chosen: item == section))
                            .focused($focusedSection, equals: item)
                        }
                    }
                    .tvSidebarCard()
                    Spacer(minLength: 0)
                }
                .frame(width: 460)
                .focusSection()
                sectionPane
                    .tvPaneRoom()
                    .frame(maxWidth: .infinity)
                    .focusSection()
            }
        }
        .padding(.horizontal, 60)
        // Focus enters on the chosen section, so a page opened on one stays on it.
        .defaultFocus($focusedSection, section)
        .onChange(of: focusedSection) { _, item in
            if let item { section = item }
        }
    }
}
#endif
