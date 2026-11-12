// The prompt of a profile on a person's own account (CONTRACT §6.2) or of a locked one: a masked
// field for its password, which autofill can fill, or its PIN on a number pad; one switch; the
// host's sentence when it refused. A sheet; on tvOS the system keyboard, or the PIN's own pad.
// What was typed lives in the view's state only while it is up, and leaves as an UnlockSecret.

import Accessibility
import PunktfunkKit
import SwiftUI

/// A prompt a connect waits on. Answers once: what was entered, or nil for Cancel.
@MainActor
final class UnlockAsk: Identifiable {
    let hostName: String
    let profileName: String
    /// The PIN form, not the password's.
    let pin: Bool
    let error: String?
    let switchTitle: String
    private var answer: ((ProfileUnlocker.Entry?) -> Void)?

    init(
        hostName: String, profileName: String, pin: Bool, error: String?, switchTitle: String,
        answer: @escaping (ProfileUnlocker.Entry?) -> Void
    ) {
        self.hostName = hostName
        self.profileName = profileName
        self.pin = pin
        self.error = error
        self.switchTitle = switchTitle
        self.answer = answer
    }

    func finish(_ entry: ProfileUnlocker.Entry?) {
        guard let answer else {
            entry?.password.wipe()
            return
        }
        self.answer = nil
        answer(entry)
    }
}

struct UnlockPromptView: View {
    let ask: UnlockAsk
    @Environment(\.dismiss) private var dismiss
    @State private var password = ""
    @State private var switchOn = false
    #if os(tvOS)
    @State private var entering = false
    /// The PIN pad's digits, in the buffer the dial wipes; `typed` redraws the dots.
    @State private var digits = UnlockSecret(capacity: 8)
    @State private var typed = 0
    #if DEBUG
    /// The shot harness: digits already on the pad.
    var shotTyped = 0
    #endif
    #else
    @FocusState private var focused: Bool
    #endif

    private var title: String { L10n.unlockTitle(name: ask.profileName) }
    private var note: String {
        ask.pin ? L10n.unlockPinIntro(host: ask.hostName) : L10n.unlockIntro(host: ask.hostName)
    }
    private var label: String { String(localized: ask.pin ? L10n.unlockPin : L10n.unlockPassword) }
    private var placeholder: LocalizedStringResource {
        ask.pin ? L10n.unlockPinPlaceholder : L10n.unlockPasswordPlaceholder
    }

    var body: some View {
        #if os(tvOS)
        VStack(spacing: 24) {
            Text(title).font(.geist(48, .bold, relativeTo: .title))
            Text(note)
                .font(.geist(22, relativeTo: .callout))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            if ask.pin {
                TVPinPad(digits: digits, typed: $typed, submit: submit)
            } else {
                TVFieldRow(
                    label: label,
                    value: password.isEmpty ? "" : "••••••••",
                    placeholder: String(localized: placeholder)
                ) { entering = true }
            }
            Toggle(ask.switchTitle, isOn: $switchOn)
            if let error = ask.error {
                Text(error).font(.geist(22, relativeTo: .callout)).foregroundStyle(.red)
            }
            HStack(spacing: 32) {
                Button(L10n.commonCancel, role: .cancel) { dismiss() }
                Button(L10n.unlockSignIn, action: submit).disabled(!ready)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
        .onPlayPauseCommand { if ask.pin { submit() } }
        .onAppear {
            announceError()
            #if DEBUG
            for _ in 0..<shotTyped { digits.append(UInt8(ascii: "7")) }
            typed = digits.count
            #endif
        }
        .onDisappear(perform: close)
        .fullScreenCover(isPresented: $entering) {
            TVTextEntry(
                title: L10n.appleUnlockPasswordFor(name: ask.profileName), text: "", secure: true
            ) {
                if !$0.isEmpty { password = $0 }
                entering = false
            }
        }
        #else
        VStack(spacing: 0) {
            Form {
                Section {
                    SecureField(label, text: $password, prompt: Text(placeholder))
                        .textContentType(ask.pin ? nil : .password)
                        #if os(iOS) || os(visionOS)
                        .keyboardType(ask.pin ? .numberPad : .default)
                        #endif
                        .focused($focused)
                        .onSubmit(submit)
                    Toggle(ask.switchTitle, isOn: $switchOn)
                } header: {
                    Label(title, systemImage: "lock").foregroundStyle(.tint)
                } footer: {
                    Text(note)
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                }
                if let error = ask.error {
                    Section {
                        Text(error)
                            .font(.geist(16, relativeTo: .callout))
                            .foregroundStyle(.red)
                    }
                }
            }
            .formStyle(.grouped)
            .font(.geist(12, relativeTo: .callout))
            .controlSize(.small)
            HStack {
                Button(L10n.commonCancel, role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button(L10n.unlockSignIn, action: submit)
                    .glassProminentButtonStyle()
                    .keyboardShortcut(.defaultAction)
                    .disabled(password.isEmpty)
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
        .presentationDetents([.medium, .large])
        #endif
        .onAppear {
            focused = true
            announceError()
        }
        .onDisappear(perform: close)
        #endif
    }

    /// VoiceOver reads the host's sentence or the prompt's own line as the prompt opens. High
    /// priority: the sheet's own arrival would cut a default one off.
    private func announceError() {
        guard let error = ask.error else { return }
        var line = AttributedString(error)
        line.accessibilitySpeechAnnouncementPriority = .high
        AccessibilityNotification.Announcement(line).post()
    }

    /// Something the form can send: on the TV's pad, 4 digits at least.
    private var ready: Bool {
        #if os(tvOS)
        if ask.pin { return typed >= 4 }
        #endif
        return !password.isEmpty
    }

    /// Hand what was typed over as an UnlockSecret and drop the view's copy.
    private func submit() {
        guard ready else { return }
        let entry = ProfileUnlocker.Entry(password: takeTyped(), switchOn: switchOn)
        password = ""
        ask.finish(entry)
        dismiss()
    }

    /// What was typed, in a secret the view no longer holds. The pad's buffer itself goes:
    /// the dial wipes it.
    private func takeTyped() -> UnlockSecret {
        #if os(tvOS)
        if ask.pin {
            let pin = digits
            digits = UnlockSecret(capacity: 8)
            typed = 0
            return pin
        }
        #endif
        return UnlockSecret(password)
    }

    /// Any other way out is Cancel.
    private func close() {
        password = ""
        #if os(tvOS)
        digits.wipe()
        typed = 0
        #endif
        ask.finish(nil)
    }
}

#if os(tvOS)
/// A locked profile's PIN on the TV: a dot per digit, never the digit, over a pad of its own.
/// tvOS gives a masked system field its password keyboard and autofill, and a plain one shows
/// the digits on the room's largest screen. The order matches the controller UI's pad.
private struct TVPinPad: View {
    let digits: UnlockSecret
    @Binding var typed: Int
    let submit: () -> Void
    @FocusState private var focused: Key?

    enum Key: Hashable {
        case digit(UInt8), delete, done
    }

    private static let rows: [[Key]] = [
        [.digit(1), .digit(2), .digit(3)],
        [.digit(4), .digit(5), .digit(6)],
        [.digit(7), .digit(8), .digit(9)],
        [.delete, .digit(0), .done],
    ]

    var body: some View {
        VStack(spacing: 20) {
            dots
            Grid(horizontalSpacing: 16, verticalSpacing: 12) {
                ForEach(Self.rows, id: \.self) { row in
                    GridRow {
                        ForEach(row, id: \.self, content: key)
                    }
                }
            }
            .focusSection()
        }
        .defaultFocus($focused, .digit(1))
        .onAppear { focused = .digit(1) }
    }

    /// The field's row: its label, then a dot per digit typed or the placeholder.
    private var dots: some View {
        HStack {
            Text(L10n.unlockPin)
            Spacer()
            if typed == 0 {
                Text(L10n.unlockPinPlaceholder).foregroundStyle(.secondary)
            } else {
                HStack(spacing: 14) {
                    ForEach(0..<typed, id: \.self) { _ in
                        Circle().frame(width: 16, height: 16)
                    }
                }
            }
        }
        .padding(.horizontal, 36)
        .frame(height: 66)
        .background(.white.opacity(0.08), in: Capsule())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(L10n.unlockPin))
        .accessibilityValue(L10n.unlockPinDigitsA11y(count: typed))
    }

    private func key(_ key: Key) -> some View {
        Button { press(key) } label: {
            Group {
                switch key {
                case .digit(let digit):
                    Text(String(digit))
                case .delete:
                    Image(systemName: "delete.left").accessibilityLabel(Text(L10n.commonDelete))
                case .done:
                    Label(L10n.commonDone, systemImage: "checkmark")
                }
            }
            .font(.geist(30, .semibold, relativeTo: .title3))
            .frame(width: 170, height: 48)
        }
        .disabled(key == .done && typed < 4)
        .focused($focused, equals: key)
    }

    private func press(_ key: Key) {
        switch key {
        case .digit(let digit): digits.append(UInt8(ascii: "0") + digit)
        case .delete: digits.removeLast()
        case .done: return submit()
        }
        typed = digits.count
    }
}
#endif
