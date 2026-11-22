// The setup window's pages. Each grant page says why in one sentence, shows the live state
// and offers one button; the footer moves between pages.
import SwiftUI

struct OnboardingView: View {
    @Bindable var model: Onboarding
    /// Every page has this size, so the window can be placed before it first lays out.
    static let size = CGSize(width: 540, height: 440)

    var body: some View {
        VStack(spacing: 0) {
            page
                .padding(.horizontal, 48)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            HStack {
                Dots(count: model.steps.count, index: model.index)
                Spacer()
                if model.index > 0 && model.step != .done {
                    Button(L10n.commonBack) { model.back() }
                }
                if model.step == .done {
                    Button(L10n.commonDone) { model.finish() }
                        .keyboardShortcut(.defaultAction)
                        .buttonStyle(.borderedProminent)
                } else {
                    Button(L10n.commonNext) { model.next() }
                        .keyboardShortcut(.defaultAction)
                }
            }
            .controlSize(.large)
            .padding(20)
        }
        .frame(width: Self.size.width, height: Self.size.height)
        .tint(.brand)
    }

    @ViewBuilder private var page: some View {
        switch model.step {
        case .welcome:
            Page(icon: { BrandMark(size: 64) }, title: L10n.setupMacWelcomeTitle, text: L10n.setupMacWelcomeBody) {
                Label { Text(L10n.setupMacWelcomeNetwork) } icon: { Image(systemName: "network") }
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: 400)
            }
        case .move:
            Page(symbol: "folder", title: L10n.setupMacMoveTitle, text: L10n.setupMacMoveBody) {
                StateLine(text: L10n.setupMacMoveState(folder: folderName), icon: "info.circle", tint: .secondary)
                Button(L10n.setupMacMove) { model.move() }.buttonStyle(.borderedProminent)
                if model.moveFailed {
                    Text(L10n.setupMacMoveFailed).font(.callout).foregroundStyle(.red)
                        .multilineTextAlignment(.center).fixedSize(horizontal: false, vertical: true)
                }
            }
        case .screen:
            Page(symbol: "rectangle.dashed.badge.record", title: L10n.setupMacScreenTitle, text: L10n.setupMacScreenBody) {
                if model.grants.screen {
                    GrantState(grant: .allowed)
                } else if model.screenAsked {
                    StateLine(text: String(localized: L10n.setupMacScreenReopenHint), icon: "arrow.clockwise.circle", tint: .secondary)
                    HStack {
                        Button(L10n.setupMacOpenSettings) { model.screenAction() }
                        Button(L10n.setupMacScreenReopen) { model.reopen() }.buttonStyle(.borderedProminent)
                    }
                } else {
                    GrantState(grant: .denied)
                    Button(L10n.setupMacScreenAllow) { model.screenAction() }.buttonStyle(.borderedProminent)
                }
            }
        case .input:
            Page(symbol: "keyboard", title: L10n.setupMacInputTitle, text: L10n.setupMacInputBody) {
                GrantState(grant: model.grants.input ? .allowed : .denied)
                grantButton(GrantAction.of(granted: model.grants.input, asked: model.inputAsked),
                            ask: L10n.setupMacInputAllow) { model.inputAction() }
            }
        case .audio:
            Page(symbol: "speaker.wave.2", title: L10n.setupMacAudioTitle, text: L10n.setupMacAudioBody) {
                GrantState(grant: model.grants.audio, asked: model.audioAsked)
                grantButton(GrantAction.of(granted: model.grants.audio == .allowed, asked: model.audioAsked),
                            ask: L10n.setupMacAudioAllow) { model.audioAction() }
            }
        case .login:
            Page(symbol: "person.badge.clock", title: L10n.setupMacLoginTitle, text: L10n.setupMacLoginBody) {
                Toggle(isOn: $model.startAtLogin) { Text(L10n.trayMacStartAtLogin) }
                    .toggleStyle(.switch)
                    .disabled(!model.store.inApplications)
                if !model.store.inApplications {
                    Text(L10n.trayMacLoginElsewhere).font(.callout).foregroundStyle(.secondary)
                        .multilineTextAlignment(.center).fixedSize(horizontal: false, vertical: true)
                } else if model.store.login == .needsApproval {
                    Button(L10n.trayMacLoginApproval) { LoginItem.openSettings() }.buttonStyle(.link)
                }
            }
        case .done:
            Page(symbol: "checkmark.circle", title: L10n.setupMacDoneTitle, text: L10n.setupMacDoneBody) {
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 6) {
                    GridRow { Text(L10n.setupMacScreenTitle); GrantState(grant: model.grants.screen ? .allowed : .denied) }
                    GridRow { Text(L10n.setupMacInputTitle); GrantState(grant: model.grants.input ? .allowed : .denied) }
                    GridRow { Text(L10n.setupMacAudioTitle); GrantState(grant: model.grants.audio, asked: model.audioAsked) }
                }
                .font(.callout)
                if let verdict = model.controllers {
                    VStack(spacing: 2) {
                        Label {
                            Text(verdict.text)
                        } icon: {
                            Image(systemName: verdict.available ? "gamecontroller.fill" : "gamecontroller")
                        }
                        if let impact = verdict.impact { Text(impact).foregroundStyle(.secondary) }
                    }
                    .font(.callout)
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: 400)
                }
                Button(L10n.trayMacOpenConsole) { model.store.openConsole() }
            }
        }
    }

    private var folderName: String {
        FileManager.default.displayName(atPath: model.store.context.bundle.deletingLastPathComponent().path)
    }

    @ViewBuilder
    private func grantButton(_ action: GrantAction, ask: LocalizedStringResource, run: @escaping () -> Void) -> some View {
        switch action {
        case .ask: Button(ask, action: run).buttonStyle(.borderedProminent)
        case .openSettings: Button(L10n.setupMacOpenSettings, action: run).buttonStyle(.borderedProminent)
        case .none: EmptyView()
        }
    }
}

private struct Page<Icon: View, Content: View>: View {
    @ViewBuilder let icon: () -> Icon
    let title: LocalizedStringResource
    let text: LocalizedStringResource
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(spacing: 14) {
            icon().padding(.bottom, 4)
            Text(title).font(.title2.weight(.semibold)).multilineTextAlignment(.center)
            Text(text).foregroundStyle(.secondary).multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 400)
            VStack(spacing: 12) { content() }.padding(.top, 6)
        }
    }
}

extension Page where Icon == PageSymbol {
    init(symbol: String, title: LocalizedStringResource, text: LocalizedStringResource,
         @ViewBuilder content: @escaping () -> Content)
    {
        self.init(icon: { PageSymbol(name: symbol) }, title: title, text: text, content: content)
    }
}

private struct PageSymbol: View {
    let name: String
    var body: some View {
        Image(systemName: name).font(.system(size: 46, weight: .regular)).foregroundStyle(Color.brand)
            .frame(height: 64).accessibilityHidden(true)
    }
}

private struct StateLine: View {
    let text: String
    let icon: String
    let tint: Color

    var body: some View {
        Label { Text(text).foregroundStyle(.secondary) } icon: { Image(systemName: icon).foregroundStyle(tint) }
            .font(.callout)
            .fixedSize(horizontal: false, vertical: true)
    }
}

private struct GrantState: View {
    let grant: Grant
    var asked = false

    var body: some View {
        switch grant {
        case .allowed:
            StateLine(text: String(localized: L10n.setupMacStateAllowed), icon: "checkmark.circle.fill", tint: .green)
        case .denied:
            StateLine(text: String(localized: L10n.setupMacStateDenied), icon: "xmark.circle", tint: .secondary)
        case .unknown:
            StateLine(text: String(localized: asked ? L10n.setupMacStateAsked : L10n.setupMacStateNotAsked),
                      icon: "circle.dashed", tint: .secondary)
        }
    }
}

private struct Dots: View {
    let count: Int
    let index: Int

    var body: some View {
        HStack(spacing: 6) {
            ForEach(0..<count, id: \.self) { i in
                Circle().fill(i == index ? Color.brand : Color.secondary.opacity(0.35)).frame(width: 7, height: 7)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(L10n.setupMacStepA11y(step: "\(index + 1)", total: "\(count)"))
    }
}
