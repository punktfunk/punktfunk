// The host page (design/apple-touch-ui-overhaul.md §2.4): everything about one saved host that
// is not "connect to it", with the acts its card's menu offers. An iPhone pushes it as one form;
// the Mac, the iPad and the TV show one section at a time beside a sidebar (HostSectionsView). It
// reads the live record by id, so an edit shows at once and a removal closes it.

import PunktfunkKit
import SwiftUI

/// The host page's parts: one form on touch, where the speed test is a page of its own, and one
/// sidebar row each in the Mac's host window.
enum HostSection: String, CaseIterable, Identifiable {
    case overview, presets, screen, connection, speedTest, pairing, power

    var id: Self { self }

    var title: String {
        switch self {
        case .overview: String(localized: L10n.appleHostcardSectionOverview)
        case .presets: String(localized: L10n.hostcardSectionPresets)
        case .screen: String(localized: L10n.hostcardSectionScreen)
        case .connection: String(localized: L10n.hostcardSectionConnection)
        case .speedTest: String(localized: L10n.speedtestTitle)
        case .pairing: String(localized: L10n.hostcardSectionPairing)
        case .power: String(localized: L10n.hostcardSectionPower)
        }
    }

    var symbol: String {
        switch self {
        case .overview: "desktopcomputer"
        case .presets: "slider.horizontal.3"
        case .screen: "display"
        case .connection: "network"
        case .speedTest: "gauge.with.needle"
        case .pairing: "lock"
        case .power: "power"
        }
    }
}

struct HostDetailView: View {
    @ObservedObject var store: HostStore
    let hostID: StoredHost.ID
    /// The surface's per-host builder (`HostActions(host:…)`).
    let actions: (StoredHost) -> HostActions
    /// One section alone (the Mac's host window), or nil for every section in one form.
    var only: HostSection?
    @ObservedObject private var nowPlaying = NowPlayingStore.shared
    @AppStorage(DefaultsKey.defaultHost) private var defaultHostID = ""
    @AutoWake private var autoWake
    @Environment(\.dismiss) private var dismiss
    @State private var confirmForget = false
    @State private var confirmRemove = false

    var body: some View {
        if let host = store.hosts.first(where: { $0.id == hostID }) {
            page(host, actions(host))
        } else {
            // Removed while open, from here or from anywhere else.
            Color.clear.onAppear { dismiss() }
        }
    }

    private func page(_ host: StoredHost, _ a: HostActions) -> some View {
        let online = store.probedOnline.contains(host.id)
        let playing = online ? nowPlaying.title(for: host) : nil
        let status = HostStatus(
            host: host, isOnline: online, isConnecting: false, nowPlaying: playing,
            autoWake: autoWake)
        return Form {
            if shows(.overview) {
                Section {
                    header(host, status)
                    Button(action: a.connect) {
                        Label(
                            playing.map { L10n.appleHostcardResumeGame(game: $0) }
                                ?? String(localized: L10n.commonConnect),
                            systemImage: "play.fill")
                    }
                    if let browse = a.browseLibrary {
                        Button(action: browse) {
                            Label(L10n.hostcardBrowseLibrary, systemImage: "square.grid.2x2")
                        }
                    }
                }
            }
            if shows(.presets) { presetsSection(host, a) }
            if shows(.screen) { screenSection(a) }
            if shows(.connection) { connectionSection(host, a) }
            if shows(.pairing) { pairingSection(host, a) }
            if shows(.power) { powerSection(a) }
            if shows(.overview) {
                if let sendLogs = a.sendLogs {
                    Section {
                        Button(L10n.hostcardSendLogs, systemImage: "doc.text", action: sendLogs)
                    } footer: {
                        Text(L10n.hostcardSendLogsHint)
                    }
                }
                Section {
                    // A Mac form draws even a destructive button in the window's tint.
                Button(L10n.hostcardRemove, role: .destructive) { confirmRemove = true }
                    .tint(.red)
                    // On the button, so iOS 26 opens the dialog from the row that asked.
                    .confirmationDialog(
                        L10n.hostcardRemoveTitle(host: host.displayName), isPresented: $confirmRemove,
                        titleVisibility: .visible
                    ) {
                        Button(L10n.commonRemove, role: .destructive, action: a.remove)
                    } message: {
                        Text(L10n.hostcardRemoveBody)
                    }
                } footer: {
                    Text(L10n.appleHostcardRemoveHint)
                }
            }
        }
        #if os(macOS)
        .formStyle(.grouped)
        #endif
        // A TV shows this only inside its page of sections, which names the host over its sidebar.
        #if !os(tvOS)
        .navigationTitle(pageTitle(host))
        #endif
        #if os(iOS) || os(visionOS)
        .navigationBarTitleDisplayMode(.inline)
        #endif
    }

    private func shows(_ section: HostSection) -> Bool { only == nil || only == section }

    #if !os(tvOS)
    /// The host's name, except in the iPad's sheet of sections: its sidebar names the host, so the
    /// pane names its section.
    private func pageTitle(_ host: StoredHost) -> String {
        #if os(iOS) || os(visionOS)
        if let only { return only.title }
        #endif
        return host.displayName
    }
    #endif

    private func header(_ host: StoredHost, _ status: HostStatus) -> some View {
        let m = CardMetrics.current
        return HStack(spacing: m.spacing) {
            monogramTile(
                monogram(host.displayName), osChain: host.osChain, m: m, connecting: false,
                filled: host.pinnedSHA256 != nil)
            VStack(alignment: .leading, spacing: 4) {
                // A page of sections names the host in its sidebar, window title or TV heading.
                if only == nil {
                    Text(host.displayName)
                        .font(.geist(m.name + 2, .bold, relativeTo: .title3))
                        .lineLimit(2)
                }
                HostStatusLine(status: status, size: m.meta)
            }
        }
        .padding(.vertical, 4)
    }

    @ViewBuilder private func presetsSection(_ host: StoredHost, _ a: HostActions) -> some View {
        if let menu = a.presets, !menu.presets.isEmpty {
            Section {
                // A binding to a deleted preset reads as Default settings, as a connect does.
                let boundID = Binding(
                    get: { menu.presets.contains { $0.id == menu.boundID } ? menu.boundID ?? "" : "" },
                    set: { menu.setDefault($0.isEmpty ? nil : $0) })
                #if os(tvOS)
                TVSelectionRow(
                    title: String(localized: L10n.hostcardConnectWith),
                    options: [(label: String(localized: L10n.hostcardDefaultSettings), tag: "")]
                        + menu.presets.map { (label: $0.name, tag: $0.id) },
                    selection: boundID)
                #else
                Picker(L10n.hostcardConnectWith, selection: boundID) {
                    Text(L10n.hostcardDefaultSettings).tag("")
                    ForEach(menu.presets) { preset in
                        Text(preset.name).tag(preset.id)
                    }
                }
                #endif
                ForEach(menu.presets) { preset in
                    Toggle(L10n.hostcardPinPreset(preset: preset.name), isOn: Binding(
                        get: { menu.pinnedIDs.contains(preset.id) },
                        set: { _ in menu.togglePin(preset.id) }))
                }
            } header: {
                Text(L10n.hostcardSectionPresets)
            } footer: {
                Text(L10n.appleHostcardPresetsHint)
            }
        } else if only == .presets {
            Section {
                Text(L10n.appleHostcardNoPresetsBody)
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// The host's screens for this device, the pick checked. A row the host can't open now
    /// says why under it; a locked host shows its reason instead of rows.
    @ViewBuilder private func screenSection(_ a: HostActions) -> some View {
        if let screens = a.screens {
            Section {
                if screens.locked {
                    Text(L10n.screenLockedHint)
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(screens.targets) { row in
                        screenRow(row, current: row.target == screens.current) { a.pickScreen(row) }
                    }
                }
            } header: {
                Text(L10n.hostcardSectionScreen)
            } footer: {
                Text(L10n.screenHint)
            }
        }
    }

    private func screenRow(
        _ row: ScreenTarget, current: Bool, pick: @escaping () -> Void
    ) -> some View {
        Button(action: pick) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text(row.name)
                        .foregroundStyle(.primary)
                    if let line = row.reason ?? (row.detail.isEmpty ? nil : row.detail) {
                        Text(line)
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 8)
                if current {
                    Image(systemName: "checkmark")
                        .foregroundStyle(.tint)
                }
            }
            .contentShape(Rectangle())
        }
        #if os(macOS)
        // A grouped Mac form draws a default button as a push button; this is a row.
        .buttonStyle(.plain)
        #endif
        .disabled(row.reason != nil)
        .accessibilityAddTraits(current ? .isSelected : [])
    }

    /// How to reach the host, edited in place. The speed test's button is the iPhone's: a page of
    /// sections has a Speed Test section instead.
    private func connectionSection(_ host: StoredHost, _ a: HostActions) -> some View {
        Section {
            HostConnectionFields(host: host) { store.update($0) }
            LabeledContent(
                String(localized: L10n.hostcardManagementPort),
                value: String(host.effectiveMgmtPort))
            #if !os(tvOS)
            Toggle(L10n.hostcardShareClipboard, isOn: Binding(
                get: { host.clipboardSync == true },
                set: { on in
                    var shared = host
                    shared.clipboardSync = on ? true : nil // nil keeps the key out of the JSON
                    store.update(shared)
                }))
            #endif
            if only == nil, let speedTest = a.speedTest {
                Button(L10n.hostcardSpeedTest, systemImage: "speedometer", action: speedTest)
            }
            if let copyLink = a.copyLink {
                Button(L10n.hostcardCopyLink, systemImage: "link", action: copyLink)
            }
        } header: {
            Text(L10n.hostcardSectionConnection)
        } footer: {
            if only == nil, host.pinnedSHA256 == nil {
                Text(L10n.appleHostcardSpeedTestPairFirstHint)
            }
        }
    }

    private func pairingSection(_ host: StoredHost, _ a: HostActions) -> some View {
        let paired = host.pinnedSHA256 != nil
        return Section {
            LabeledContent(
                String(localized: L10n.appleHostcardPairStatus),
                value: String(
                    localized: paired ? L10n.hostcardPaired : L10n.hostcardStatusNotPaired))
            if let pin = host.pinnedSHA256 {
                let hex = pin.hexLower
                LabeledContent(
                    String(localized: L10n.appleHostcardFingerprint),
                    value: "\(hex.prefix(8))…\(hex.suffix(4))")
            }
            Button(
                paired ? L10n.appleHostcardPairAgainWithPin : L10n.hostcardPairWithPin,
                systemImage: "number", action: a.pair)
            if paired {
                Toggle(L10n.hostcardDefaultHost, isOn: Binding(
                    get: { defaultHostID.lowercased() == host.id.uuidString.lowercased() },
                    set: { defaultHostID = $0 ? host.id.uuidString : "" }))
                Button(L10n.hostcardForgetIdentity, role: .destructive) { confirmForget = true }
                    .tint(.red)
                    .confirmationDialog(
                        L10n.hostcardForgetIdentityTitle(host: host.displayName),
                        isPresented: $confirmForget,
                        titleVisibility: .visible
                    ) {
                        Button(L10n.hostcardForgetIdentityConfirm, role: .destructive, action: a.forget)
                    } message: {
                        Text(L10n.hostcardForgetIdentityBody)
                    }
            }
        } header: {
            Text(L10n.hostcardSectionPairing)
        } footer: {
            Text(paired ? L10n.appleHostcardDefaultHint : L10n.appleHostcardPairingHint)
        }
    }

    @ViewBuilder private func powerSection(_ a: HostActions) -> some View {
        if a.wake != nil || !a.power.isEmpty {
            Section {
                if let wake = a.wake {
                    Button(L10n.hostcardWake, systemImage: "power", action: wake)
                }
                ForEach(a.power) { action in
                    // iOS reddens a destructive title and leaves the symbol on the accent, so
                    // the symbol is coloured itself. The tint is for the Mac, whose form
                    // draws a destructive title in the window's tint.
                    Button(role: action.danger ? .destructive : nil) {
                        a.runPower(action)
                    } label: {
                        Label {
                            Text(
                                action.available
                                    ? action.label
                                    : L10n.hostcardActionUnavailable(action: action.label))
                        } icon: {
                            if action.danger {
                                Image(systemName: "power").foregroundStyle(.red)
                            } else {
                                Image(systemName: "power")
                            }
                        }
                    }
                    .tint(action.danger ? Color.red : nil)
                }
            } header: {
                Text(L10n.hostcardSectionPower)
            } footer: {
                Text(L10n.appleHostcardPowerHint)
            }
        } else if only == .power {
            Section {
                Text(L10n.appleHostcardNoPowerBody)
                    .foregroundStyle(.secondary)
            }
        }
    }
}

/// The Connection section's fields under the host form's rules: a field saves when focus leaves it
/// or on Return, a TV row when its keyboard closes. What the rules refuse snaps back to the record.
private struct HostConnectionFields: View {
    let host: StoredHost
    let onSave: (StoredHost) -> Void
    @State private var form: HostFormDraft
    /// The form as it last loaded the record: a field still equal to it is one the player left.
    @State private var loaded: HostFormDraft
    #if os(tvOS)
    @State private var editing: Field?
    #else
    @FocusState private var focused: Field?
    #endif

    private enum Field: String, Identifiable {
        case name, address, port, macs

        var id: Self { self }

        var text: WritableKeyPath<HostFormDraft, String> {
            switch self {
            case .name: \.name
            case .address: \.address
            case .port: \.port
            case .macs: \.macs
            }
        }

        #if os(tvOS)
        /// The TV keyboard's prompt.
        var prompt: String {
            switch self {
            case .name: String(localized: L10n.hostsFieldNamePlaceholder)
            case .address: String(localized: L10n.hostsFieldAddressPlaceholder)
            case .port: String(localized: L10n.hostsFieldPort)
            case .macs: String(localized: L10n.appleHostsFieldMacEntry)
            }
        }
        #endif
    }

    init(host: StoredHost, onSave: @escaping (StoredHost) -> Void) {
        self.host = host
        self.onSave = onSave
        _form = State(initialValue: HostFormDraft(record: host))
        _loaded = State(initialValue: HostFormDraft(record: host))
    }

    var body: some View {
        #if os(tvOS)
        TVFieldRow(
            label: String(localized: L10n.hostsFieldName), value: form.name,
            placeholder: String(localized: L10n.hostsFieldNamePlaceholder)
        ) { editing = .name }
            .fullScreenCover(item: $editing) { field in
                TVTextEntry(
                    title: field.prompt, text: form[keyPath: field.text],
                    keyboardType: field == .port ? .numberPad : .default
                ) { value in
                    form[keyPath: field.text] = value.trimmingCharacters(in: .whitespaces)
                    editing = nil
                    save()
                }
            }
            .onChange(of: host) { _, saved in if editing == nil { load(saved) } }
        TVFieldRow(
            label: String(localized: L10n.hostsFieldAddress), value: form.address, placeholder: ""
        ) { editing = .address }
        TVFieldRow(
            label: String(localized: L10n.hostsFieldPort), value: form.port, placeholder: ""
        ) { editing = .port }
        TVFieldRow(
            label: String(localized: L10n.hostsFieldMac), value: form.macs,
            placeholder: String(localized: L10n.hostsFieldMacPlaceholder)
        ) { editing = .macs }
        #else
        row(
            String(localized: L10n.hostsFieldName), .name,
            prompt: String(localized: L10n.hostsFieldNamePlaceholder))
            .onChange(of: host) { _, saved in if focused == nil { load(saved) } }
            .onChange(of: focused) { save() }
        row(
            String(localized: L10n.hostsFieldAddress), .address,
            prompt: String(localized: L10n.hostsFieldAddressPlaceholder))
        row(String(localized: L10n.hostsFieldPort), .port, prompt: String(HostFormDraft.defaultPort))
        row(
            String(localized: L10n.hostsFieldMac), .macs,
            prompt: String(localized: L10n.hostsFieldMacPlaceholder))
        #endif
    }

    #if !os(tvOS)
    /// A labelled field with its value trailing, as in a settings row.
    private func row(_ label: String, _ field: Field, prompt: String) -> some View {
        LabeledContent(label) {
            TextField(label, text: $form[dynamicMember: field.text], prompt: Text(prompt))
                .labelsHidden()
                .multilineTextAlignment(.trailing)
                .autocorrectionDisabled()
                #if os(iOS) || os(visionOS)
                .textInputAutocapitalization(field == .name ? .words : .never)
                .keyboardType(field == .port ? .numberPad : .default)
                #endif
                .focused($focused, equals: field)
                .onSubmit(save)
        }
    }
    #endif

    /// Writes the fields the player changed (`HostFormDraft.commit`), then shows the record as
    /// saved: a pasted `address:port` split, the MACs normalised.
    private func save() {
        var updated = host
        form.commit(to: &updated, changedFrom: loaded)
        if updated != host { onSave(updated) }
        load(updated)
    }

    private func load(_ saved: StoredHost) {
        form = HostFormDraft(record: saved)
        loaded = form
    }
}
