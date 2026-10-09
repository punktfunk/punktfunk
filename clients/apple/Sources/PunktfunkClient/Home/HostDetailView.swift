// The host page (design/apple-touch-ui-overhaul.md §2.4): everything about one saved host that
// is not "connect to it", with the acts its card's menu offers. An iPhone pushes it as one form;
// the Mac, the iPad and the TV show one section at a time beside a sidebar (HostSectionsView). It
// reads the live record by id, so an edit shows at once and a removal closes it.

import PunktfunkKit
import SwiftUI

/// The host page's parts: one form on touch, where the speed test is a page of its own, and one
/// sidebar row each in the Mac's host window.
enum HostSection: String, CaseIterable, Identifiable {
    case overview, presets, connection, speedTest, pairing, power

    var id: Self { self }

    var title: String {
        switch self {
        case .overview: "Overview"
        case .presets: "Presets"
        case .connection: "Connection"
        case .speedTest: "Speed Test"
        case .pairing: "Pairing"
        case .power: "Power"
        }
    }

    var symbol: String {
        switch self {
        case .overview: "desktopcomputer"
        case .presets: "slider.horizontal.3"
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
    @AppStorage(DefaultsKey.autoWake) private var autoWake = SettingDefault.autoWake
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
                        Label(playing.map { "Resume \($0)" } ?? "Connect", systemImage: "play.fill")
                    }
                    if let browse = a.browseLibrary {
                        Button(action: browse) {
                            Label("Browse Library", systemImage: "square.grid.2x2")
                        }
                    }
                }
            }
            if shows(.presets) { presetsSection(host, a) }
            if shows(.connection) { connectionSection(host, a) }
            if shows(.pairing) { pairingSection(host, a) }
            if shows(.power) { powerSection(a) }
            if shows(.overview) {
                if let sendLogs = a.sendLogs {
                    Section {
                        Button("Send Logs to Host", systemImage: "doc.text", action: sendLogs)
                    } footer: {
                        Text("Uploads this device's recent log to the host, for a bug report.")
                    }
                }
                Section {
                    // A Mac form draws even a destructive button in the window's tint.
                Button("Remove Host", role: .destructive) { confirmRemove = true }
                    .tint(.red)
                    // On the button, so iOS 26 opens the dialog from the row that asked.
                    .confirmationDialog(
                        "Remove \(host.displayName)?", isPresented: $confirmRemove,
                        titleVisibility: .visible
                    ) {
                        Button("Remove Host", role: .destructive, action: a.remove)
                    } message: {
                        Text("You can add it again later.")
                    }
                } footer: {
                    Text("Deletes the host from this device. The host itself is untouched.")
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
                    title: "Connect with",
                    options: [(label: "Default settings", tag: "")]
                        + menu.presets.map { (label: $0.name, tag: $0.id) },
                    selection: boundID)
                #else
                Picker("Connect with", selection: boundID) {
                    Text("Default settings").tag("")
                    ForEach(menu.presets) { preset in
                        Text(preset.name).tag(preset.id)
                    }
                }
                #endif
                ForEach(menu.presets) { preset in
                    Toggle("Pin \u{201C}\(preset.name)\u{201D} as a card", isOn: Binding(
                        get: { menu.pinnedIDs.contains(preset.id) },
                        set: { _ in menu.togglePin(preset.id) }))
                }
            } header: {
                Text("Presets")
            } footer: {
                Text("A tap on the card connects with the chosen preset. A pinned preset gets its "
                    + "own card next to this host.")
            }
        } else if only == .presets {
            Section {
                Text("No presets yet. Make one in Settings, then choose it here.")
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// How to reach the host, edited in place. The speed test's button is the iPhone's: a page of
    /// sections has a Speed Test section instead.
    private func connectionSection(_ host: StoredHost, _ a: HostActions) -> some View {
        Section {
            HostConnectionFields(host: host) { store.update($0) }
            LabeledContent("Management port", value: String(host.effectiveMgmtPort))
            #if !os(tvOS)
            Toggle("Share clipboard with this host", isOn: Binding(
                get: { host.clipboardSync == true },
                set: { on in
                    var shared = host
                    shared.clipboardSync = on ? true : nil // nil keeps the key out of the JSON
                    store.update(shared)
                }))
            #endif
            if only == nil, let speedTest = a.speedTest {
                Button("Test Network Speed…", systemImage: "speedometer", action: speedTest)
            }
            if let copyLink = a.copyLink {
                Button("Copy Link", systemImage: "link", action: copyLink)
            }
        } header: {
            Text("Connection")
        } footer: {
            if only == nil, host.pinnedSHA256 == nil {
                Text("Pair first to test the network speed.")
            }
        }
    }

    private func pairingSection(_ host: StoredHost, _ a: HostActions) -> some View {
        let paired = host.pinnedSHA256 != nil
        return Section {
            LabeledContent("Status", value: paired ? "Paired" : "Not paired")
            if let pin = host.pinnedSHA256 {
                let hex = pin.hexLower
                LabeledContent("Fingerprint", value: "\(hex.prefix(8))…\(hex.suffix(4))")
            }
            Button(paired ? "Pair Again with PIN…" : "Pair with PIN…", systemImage: "number",
                   action: a.pair)
            if paired {
                Toggle("Default host", isOn: Binding(
                    get: { defaultHostID.lowercased() == host.id.uuidString.lowercased() },
                    set: { defaultHostID = $0 ? host.id.uuidString : "" }))
                Button("Forget Identity…", role: .destructive) { confirmForget = true }
                    .tint(.red)
                    .confirmationDialog(
                        "Forget the identity of \(host.displayName)?", isPresented: $confirmForget,
                        titleVisibility: .visible
                    ) {
                        Button("Forget Identity", role: .destructive, action: a.forget)
                    } message: {
                        Text("The next connect asks for a PIN again.")
                    }
            }
        } header: {
            Text("Pairing")
        } footer: {
            Text(paired
                ? "Start in opens the default host's library. With one paired host, that host is "
                    + "the default without this switch."
                : "Pairing lets this device browse the library, test the network and send logs.")
        }
    }

    @ViewBuilder private func powerSection(_ a: HostActions) -> some View {
        if a.wake != nil || !a.power.isEmpty {
            Section {
                if let wake = a.wake {
                    Button("Wake Host", systemImage: "power", action: wake)
                }
                ForEach(a.power) { action in
                    // iOS reddens a destructive title and leaves the symbol on the accent, so
                    // the symbol is coloured itself. The tint is for the Mac, whose form
                    // draws a destructive title in the window's tint.
                    Button(role: action.danger ? .destructive : nil) {
                        a.runPower(action)
                    } label: {
                        Label {
                            Text(action.available ? action.label : "\(action.label) (Unavailable)")
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
                Text("Power")
            } footer: {
                Text("Restart and shut down end every stream from this host, and ask first.")
            }
        } else if only == .power {
            Section {
                Text("This host offers this device no power actions.")
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
    @State private var name: String
    @State private var address: String
    @State private var port: String
    @State private var macs: String
    #if os(tvOS)
    @State private var editing: Field?
    #else
    @FocusState private var focused: Field?
    #endif

    private enum Field: String, Identifiable {
        case name, address, port, macs

        var id: Self { self }

        #if os(tvOS)
        /// The TV keyboard's prompt.
        var prompt: String {
            switch self {
            case .name: "Name (optional, e.g. Living Room)"
            case .address: "IP or hostname"
            case .port: "Port"
            case .macs: "MAC address(es), comma-separated — aa:bb:cc:dd:ee:ff"
            }
        }
        #endif
    }

    init(host: StoredHost, onSave: @escaping (StoredHost) -> Void) {
        self.host = host
        self.onSave = onSave
        _name = State(initialValue: host.name)
        _address = State(initialValue: host.address)
        _port = State(initialValue: String(host.port))
        _macs = State(initialValue: host.wakeMacs.joined(separator: ", "))
    }

    var body: some View {
        #if os(tvOS)
        TVFieldRow(label: "Name", value: name, placeholder: "Optional") { editing = .name }
            .fullScreenCover(item: $editing) { field in
                TVTextEntry(
                    title: field.prompt, text: text(field),
                    keyboardType: field == .port ? .numberPad : .default
                ) { value in
                    set(field, value)
                    editing = nil
                    save()
                }
            }
            .onChange(of: host) { _, saved in if editing == nil { load(saved) } }
        TVFieldRow(label: "Address", value: address, placeholder: "") { editing = .address }
        TVFieldRow(label: "Port", value: port, placeholder: "") { editing = .port }
        TVFieldRow(label: "Wake-on-LAN", value: macs, placeholder: "Not learned yet") {
            editing = .macs
        }
        #else
        row("Name", .name, $name, prompt: "Optional")
            .onChange(of: host) { _, saved in if focused == nil { load(saved) } }
            .onChange(of: focused) { save() }
        row("Address", .address, $address, prompt: "IP or hostname")
        row("Port", .port, $port, prompt: String(HostFormDraft.defaultPort))
        row("Wake-on-LAN", .macs, $macs, prompt: "Not learned yet")
        #endif
    }

    #if os(tvOS)
    private func text(_ field: Field) -> String {
        switch field {
        case .name: name
        case .address: address
        case .port: port
        case .macs: macs
        }
    }

    private func set(_ field: Field, _ value: String) {
        let value = value.trimmingCharacters(in: .whitespaces)
        switch field {
        case .name: name = value
        case .address: address = value
        case .port: port = value
        case .macs: macs = value
        }
    }
    #else
    /// A labelled field with its value trailing, as in a settings row.
    private func row(
        _ label: String, _ field: Field, _ text: Binding<String>, prompt: String
    ) -> some View {
        LabeledContent(label) {
            TextField(label, text: text, prompt: Text(prompt))
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

    /// Writes what the rules take (name, address and port together, the MACs on their own), then
    /// shows the record as saved: a pasted `address:port` split, the MACs normalised.
    private func save() {
        var updated = host
        let draft = HostFormDraft(name: name, address: address, port: port)
        if draft.canSave { draft.apply(to: &updated) }
        let parsed = AddHostSheet.parseMacs(macs)
        if parsed != nil || macs.allSatisfy(\.isWhitespace) { updated.macAddresses = parsed }
        if updated != host { onSave(updated) }
        load(updated)
    }

    private func load(_ saved: StoredHost) {
        name = saved.name
        address = saved.address
        port = String(saved.port)
        macs = saved.wakeMacs.joined(separator: ", ")
    }
}
