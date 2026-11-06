// Add a host: name (optional) + address + port + Wake-on-LAN MAC → a card in the grid. A blank
// MAC is learned from the host's advert; the host page edits a saved host in place. The first
// actual connection still runs the trust-on-first-use prompt.

import PunktfunkKit
import SwiftUI

struct AddHostSheet: View {
    @Environment(\.dismiss) private var dismiss

    let onSave: (StoredHost) -> Void

    @State private var name = ""
    @State private var address = ""
    @State private var port = 9777
    @State private var mac = ""
    #if !os(tvOS)
    /// Share the clipboard with this host (design clipboard-and-file-transfer.md §5.3). Off by
    /// default; honored only when the host advertises the capability at connect. Absent on tvOS,
    /// which has no pasteboard to share.
    @State private var clipboardSync = false
    #endif
    #if os(tvOS)
    private enum EditField: String, Identifiable {
        case name, address, port, mac
        var id: String { rawValue }
    }
    @State private var editingField: EditField?
    #endif
    #if os(visionOS)
    /// The form's rows, measured: a visionOS sheet ignores detents and a Form has no height of its
    /// own, so the form is pinned to this and the sheet fits the result.
    @State private var formHeight: CGFloat?
    /// One gap round the close button, the form and the action, past the sheet's round corners.
    private static let margin: CGFloat = 24
    #else
    private static let margin: CGFloat = 16
    #endif

    /// One rule for every host form (see `HostFormDraft`): a blank port means the default, an
    /// out-of-range one is refused rather than silently clamped, and a pasted `address:port` is
    /// split rather than stored whole.
    private var draft: HostFormDraft {
        HostFormDraft(name: name, address: address, port: String(port))
    }
    private var canSave: Bool { draft.canSave }

    var body: some View {
        #if os(tvOS)
        // No inline text editing on tvOS — Settings-style value rows; pressing one
        // raises the SYSTEM fullscreen keyboard (TVTextEntry).
        VStack(spacing: 24) {
            TVFieldRow(
                label: String(localized: L10n.hostsFieldName), value: name,
                placeholder: String(localized: L10n.hostsFieldNamePlaceholder)
            ) { editingField = .name }
            TVFieldRow(
                label: String(localized: L10n.hostsFieldAddress), value: address,
                placeholder: String(localized: L10n.hostsFieldAddressPlaceholder)
            ) { editingField = .address }
            TVFieldRow(
                label: String(localized: L10n.hostsFieldPort), value: String(port), placeholder: ""
            ) { editingField = .port }
            TVFieldRow(
                label: String(localized: L10n.hostsFieldMac), value: mac,
                placeholder: String(localized: L10n.hostsFieldMacPlaceholder)
            ) { editingField = .mac }
            HStack(spacing: 32) {
                Button(L10n.commonCancel, role: .cancel) { dismiss() }
                Button(L10n.hostsAddConfirm) { save() }.disabled(!canSave)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
        .navigationTitle(L10n.hostsAddTitle)
        .fullScreenCover(item: $editingField) { field in
            switch field {
            case .name:
                TVTextEntry(title: String(localized: L10n.hostsFieldNamePlaceholder), text: name) {
                    name = $0
                    editingField = nil
                }
            case .address:
                TVTextEntry(
                    title: String(localized: L10n.hostsFieldAddressPlaceholder), text: address,
                    keyboardType: .URL
                ) {
                    address = $0.trimmingCharacters(in: .whitespaces)
                    editingField = nil
                }
            case .port:
                TVTextEntry(
                    title: String(localized: L10n.hostsFieldPort), text: String(port),
                    keyboardType: .numberPad
                ) {
                    if let value = Int($0), (1...65535).contains(value) { port = value }
                    editingField = nil
                }
            case .mac:
                TVTextEntry(title: String(localized: L10n.appleHostsFieldMacEntry), text: mac) {
                    mac = $0.trimmingCharacters(in: .whitespaces)
                    editingField = nil
                }
            }
        }
        #else
        VStack(spacing: 0) {
            #if os(visionOS)
            // A visionOS sheet doesn't swipe away: the close button is the way out without adding
            // a host. Untinted, so the brand tint stays on the one primary action.
            HStack {
                Text(L10n.hostsAddTitle)
                    .font(.geist(22, .semibold, relativeTo: .title3))
                    .accessibilityAddTraits(.isHeader)
                Spacer()
                Button(L10n.commonClose, systemImage: "xmark") { dismiss() }
                    .labelStyle(.iconOnly)
                    .buttonBorderShape(.circle)
                    .tint(nil)
            }
            .padding([.top, .horizontal], Self.margin)
            #endif
            Form {
                TextField(
                    String(localized: L10n.hostsFieldName), text: $name,
                    prompt: Text(L10n.hostsFieldNamePlaceholder))
                TextField(
                    String(localized: L10n.hostsFieldAddress), text: $address,
                    prompt: Text(L10n.hostsFieldAddressPlaceholder))
                    .autocorrectionDisabled()
                    #if os(iOS) || os(visionOS)
                    .textInputAutocapitalization(.never)
                    .keyboardType(.URL)
                    #endif
                TextField(
                    String(localized: L10n.hostsFieldPort), value: $port,
                    format: .number.grouping(.never))
                TextField(
                    String(localized: L10n.hostsFieldMac), text: $mac,
                    prompt: Text(L10n.hostsFieldMacPlaceholder))
                    .autocorrectionDisabled()
                    #if os(iOS) || os(visionOS)
                    .textInputAutocapitalization(.never)
                    #endif
                #if !os(tvOS)
                Toggle(L10n.hostsClipboard, isOn: $clipboardSync)
                #endif
            }
            #if !os(tvOS)
            .formStyle(.grouped)
            #endif
            #if os(iOS) || os(visionOS)
            // The sheet is sized to its content, so there is nothing to scroll.
            .scrollDisabled(true)
            #endif
            #if os(visionOS)
            // The form's own margins differ from the sheet's; `margin` below sets every gap.
            .contentMargins(0, for: .scrollContent)
            .onScrollGeometryChange(for: CGFloat.self) {
                $0.contentSize.height + $0.contentInsets.top + $0.contentInsets.bottom
            } action: { _, height in
                // The first reading lands before any row is laid out.
                if height > 0 { formHeight = height }
            }
            // A first guess, not nil: an unsized form fits to zero and never lays out a row.
            .frame(height: formHeight ?? 400)
            .padding([.top, .horizontal], Self.margin)
            #endif
            #if os(macOS)
            // macOS ONLY: the grouped form's default system text is oversized next to the app's
            // Geist typography, so the panel reads out of place at full size. iOS keeps the app's
            // body size — a touch target and a field label there are not a Mac panel's, and 12pt
            // made this the one sheet in the app you had to squint at.
            .font(.geist(12, relativeTo: .callout))
            .controlSize(.small)
            #endif
            #if os(macOS)
            HStack {
                Button(L10n.commonCancel, role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button(L10n.hostsAddConfirm) { save() }
                    .glassProminentButtonStyle()
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canSave)
            }
            .padding(16)
            #else
            Button { save() } label: {
                Text(L10n.hostsAddConfirm).frame(maxWidth: .infinity)
            }
                .glassProminentButtonStyle()
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)
                .disabled(!canSave)
                .padding(Self.margin)
            #endif
        }
        #if os(iOS)
        // Sized to its content: four fields, the clipboard toggle and the action row.
        .presentationDetents([.height(392 + 44)])
        .presentationDragIndicator(.visible)
        #elseif os(visionOS)
        .presentationSizing(.form.fitted(horizontal: false, vertical: true))
        #endif
        #if os(macOS)
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        #endif
        #endif
    }

    private func save() {
        var host = StoredHost(name: "", address: "")
        draft.apply(to: &host)
        host.macAddresses = Self.parseMacs(mac)
        #if !os(tvOS)
        // nil when off: the key stays absent from the saved JSON (forward-compat, and "never
        // opted in" and "opted out" read the same — off).
        host.clipboardSync = clipboardSync ? true : nil
        #endif
        onSave(host)
        dismiss()
    }

    /// Split comma/space/newline-separated MACs, keep only well-formed `aa:bb:cc:dd:ee:ff` (six hex
    /// octets, normalized lower-case); nil when none are valid, so clearing the field clears the
    /// stored MAC.
    static func parseMacs(_ s: String) -> [String]? {
        let macs = s
            .split(whereSeparator: { ",; \n\t".contains($0) })
            .map { $0.trimmingCharacters(in: .whitespaces).lowercased() }
            .filter { m in
                let parts = m.split(separator: ":")
                return parts.count == 6 && parts.allSatisfy { $0.count == 2 && UInt8($0, radix: 16) != nil }
            }
        return macs.isEmpty ? nil : macs
    }
}
