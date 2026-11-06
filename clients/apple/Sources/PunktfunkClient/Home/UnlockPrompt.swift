// The password prompt of a profile on a person's own account (CONTRACT §6.2): a masked field the
// system's password autofill can fill, the profile's name, one switch, and the host's sentence
// when it refused. A sheet on iOS, iPadOS, visionOS and macOS; the system keyboard on tvOS. The
// typed password lives in the view's state only while it is up, and leaves it as an UnlockSecret.

import PunktfunkKit
import SwiftUI

/// A prompt a connect waits on. Answers once: what was entered, or nil for Cancel.
@MainActor
final class UnlockAsk: Identifiable {
    let hostName: String
    let profileName: String
    let error: String?
    let switchTitle: String
    private var answer: ((ProfileUnlocker.Entry?) -> Void)?

    init(
        hostName: String, profileName: String, error: String?, switchTitle: String,
        answer: @escaping (ProfileUnlocker.Entry?) -> Void
    ) {
        self.hostName = hostName
        self.profileName = profileName
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
    #else
    @FocusState private var focused: Bool
    #endif

    private var title: String { L10n.unlockTitle(name: ask.profileName) }
    private var note: String { L10n.unlockIntro(host: ask.hostName) }

    var body: some View {
        #if os(tvOS)
        VStack(spacing: 24) {
            Text(title).font(.geist(48, .bold, relativeTo: .title))
            Text(note)
                .font(.geist(22, relativeTo: .callout))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            TVFieldRow(
                label: String(localized: L10n.unlockPassword),
                value: password.isEmpty ? "" : "••••••••",
                placeholder: String(localized: L10n.unlockPasswordPlaceholder)
            ) { entering = true }
            Toggle(ask.switchTitle, isOn: $switchOn)
            if let error = ask.error {
                Text(error).font(.geist(22, relativeTo: .callout)).foregroundStyle(.red)
            }
            HStack(spacing: 32) {
                Button(L10n.commonCancel, role: .cancel) { dismiss() }
                Button(L10n.unlockSignIn, action: submit).disabled(password.isEmpty)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
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
                    SecureField(
                        String(localized: L10n.unlockPassword), text: $password,
                        prompt: Text(L10n.unlockPasswordPlaceholder))
                        .textContentType(.password)
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
        .onAppear { focused = true }
        .onDisappear(perform: close)
        #endif
    }

    /// Hand the password over as an UnlockSecret and drop the view's copy.
    private func submit() {
        guard !password.isEmpty else { return }
        let entry = ProfileUnlocker.Entry(password: UnlockSecret(password), switchOn: switchOn)
        password = ""
        ask.finish(entry)
        dismiss()
    }

    /// Any other way out is Cancel.
    private func close() {
        password = ""
        ask.finish(nil)
    }
}
