// PIN pairing sheet. The host shows the pairing PIN in its web console (port 47992 →
// Pairing; also printed in the host's log when armed via --allow-pairing); the user
// types it here. The ceremony is SPAKE2, so a wrong PIN buys an
// attacker exactly one online guess — for the user a typo just means "try again" (the
// host rate-limits ceremonies to one per 2 s). Success returns the host's now-VERIFIED
// fingerprint: the caller pins it, no manual comparison needed, and the host stores this
// client's identity in return.
//
// This is the TOUCH/desktop presentation (and tvOS's, where the focus engine drives the same
// fields). A controller can't reach a `Form`'s text fields on iOS/macOS, so the console pairs on
// its own screen instead — same ceremony, via the shared `PairCeremony`.

import Foundation
import PunktfunkKit
import SwiftUI

struct PairSheet: View {
    @Environment(\.dismiss) private var dismiss
    let host: StoredHost
    /// Called with the verified host fingerprint after a successful ceremony.
    let onPaired: (Data) -> Void

    @State private var pin = ""
    // Same source the connect path knocks with (`DeviceName.current`), so a device the operator
    // approves from the console's pending list and one that pairs by PIN land under one name.
    @State private var clientName = DeviceName.current
    @StateObject private var ceremony = PairCeremony()

    private var busy: Bool { ceremony.busy }
    private var errorText: String? { ceremony.errorText }
    #if os(tvOS)
    private enum EditField: String, Identifiable {
        case pin, clientName
        var id: String { rawValue }
    }
    @State private var editing: EditField?
    #endif

    var body: some View {
        #if os(tvOS)
        VStack(spacing: 24) {
            Text("The PIN is shown in the host's web console "
                + "(https://<host>:47992 → Pairing). "
                + "Pairing verifies both sides at once — no fingerprint comparison "
                + "needed.")
                .font(.geist(22, relativeTo: .callout)) // TV-legible (system callout is ~25 there)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            TVFieldRow(
                label: "PIN", value: pin, placeholder: "Shown in the host's web console"
            ) { editing = .pin }
            TVFieldRow(
                label: "Device name", value: clientName, placeholder: "Apple TV"
            ) { editing = .clientName }
            if let errorText {
                Text(errorText)
                    .font(.geist(22, relativeTo: .callout))
                    .foregroundStyle(.red)
            }
            HStack(spacing: 32) {
                Button("Cancel", role: .cancel) {
                    ceremony.abandon()
                    dismiss()
                }
                if busy {
                    ProgressView()
                }
                Button("Pair") { runCeremony() }
                    .disabled(busy || pin.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
        .navigationTitle("Pair with \(host.displayName)")
        .onDisappear { ceremony.abandon() }
        .fullScreenCover(item: $editing) { field in
            switch field {
            case .pin:
                TVTextEntry(
                    title: "PIN (shown in the host's web console)", text: pin,
                    keyboardType: .numberPad
                ) {
                    pin = $0.trimmingCharacters(in: .whitespaces)
                    editing = nil
                }
            case .clientName:
                TVTextEntry(title: "Device name", text: clientName) {
                    clientName = $0
                    editing = nil
                }
            }
        }
        #else
        VStack(spacing: 0) {
            Form {
                Section {
                    TextField(
                        "PIN", text: $pin,
                        prompt: Text("Shown in the host's web console"))
                        .font(.geistFixed(16)) // prominent, but on-brand mono (not oversized title3)
                        #if os(iOS) || os(visionOS)
                        .keyboardType(.numberPad)
                        #endif
                    TextField(
                        "Client name", text: $clientName,
                        prompt: Text(Self.clientNamePrompt))
                        #if os(tvOS)
                        .labelsHidden() // prefilled → tvOS floats the label off-center
                        #endif
                } header: {
                    Label("Pair with \(host.displayName)", systemImage: "lock.shield")
                        .foregroundStyle(.tint)
                } footer: {
                    Text("The PIN is shown in the host's web console "
                        + "(https://<host>:47992 → Pairing). "
                        + "Pairing verifies both sides at once — no fingerprint "
                        + "comparison needed.")
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                }
                if let errorText {
                    Section {
                        Text(errorText)
                            .font(.geist(16, relativeTo: .callout))
                            .foregroundStyle(.red)
                    }
                }
            }
            #if !os(tvOS)
        .formStyle(.grouped)
        // Bring the grouped form's default system text down to the app's Geist scale so the sheet
        // doesn't read oversized / out of place (matches AddHostSheet). The PIN field keeps its own
        // explicit Geist Mono font.
        .font(.geist(12, relativeTo: .callout))
        .controlSize(.small)
        #endif
            HStack {
                Button("Cancel", role: .cancel) {
                    ceremony.abandon()
                    dismiss()
                }
                #if !os(tvOS)
                .keyboardShortcut(.cancelAction)
                #endif
                Spacer()
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .padding(.trailing, 8)
                }
                Button("Pair") { runCeremony() }
                    .glassProminentButtonStyle()
                    #if !os(tvOS)
                    .keyboardShortcut(.defaultAction)
                    #endif
                    .disabled(busy || pin.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            #if os(iOS) || os(visionOS)
            .controlSize(.large)
            #endif
            .padding(16)
        }
        #if os(macOS)
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        #endif
        #if os(iOS) || os(visionOS)
        // Bottom sheet instead of a full-screen modal (Liquid Glass background on iOS 26).
        // .medium rests; .large is included so the sheet grows to keep the Pair/Cancel row
        // above the keyboard when the PIN field is focused. Hide the grabber while the ceremony
        // is in flight — dismissal is disabled then (interactiveDismissDisabled), so a drag
        // would only rubber-band; the always-enabled Cancel button is the exit.
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(busy ? .hidden : .visible)
        #endif
        .interactiveDismissDisabled(busy)
        .onDisappear { ceremony.abandon() } // any other dismissal path
        #endif
    }

    /// The field prompt names the device you are actually on — it said "this Mac" on every
    /// platform, which on an iPhone is simply wrong.
    private static var clientNamePrompt: String {
        #if os(macOS)
        "How the host lists this Mac"
        #else
        "How the host lists this device"
        #endif
    }

    private func runCeremony() {
        ceremony.run(
            host: host.address, port: host.port, pin: pin, clientName: clientName
        ) { fingerprint in
            onPaired(fingerprint)
            dismiss()
        }
    }
}

#if DEBUG
extension PairSheet {
    /// Screenshot-harness seed (`ShotScenes`). A capture of the untouched sheet shows an empty PIN
    /// field, a DISABLED "Pair", and — because the client name defaults to the device's
    /// own — whatever the capture simulator happens to be called (`pf-shot-iphone-6.9` reached App
    /// Store Connect that way). Seeding both fields captures the ceremony as a user meets it,
    /// mid-entry, with a live primary button.
    ///
    /// An extension so `PairSheet` keeps its memberwise initialiser, and THIS file so it can reach
    /// the private state.
    init(
        host: StoredHost, shotPIN: String, shotClientName: String,
        onPaired: @escaping (Data) -> Void
    ) {
        self.init(host: host, onPaired: onPaired)
        _pin = State(initialValue: shotPIN)
        _clientName = State(initialValue: shotClientName)
    }
}
#endif
