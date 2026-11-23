// The setup window's pages. Every page sits on one scaffold: an icon, a title and a sentence at
// a fixed height, then the page's cards. The footer moves between pages.
import AppKit
import SwiftUI

struct OnboardingView: View {
    @Bindable var model: Onboarding
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// Every page has this size, so the window can be placed before it first lays out.
    static let size = CGSize(width: 600, height: 600)

    var body: some View {
        VStack(spacing: 0) {
            page
                .id(model.step)
                .transition(transition)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            Divider().opacity(0.5)
            footer
        }
        // The window adds its title bar's height to this size, and the page fills that too.
        .frame(width: Self.size.width)
        .frame(minHeight: Self.size.height, maxHeight: .infinity)
        .background { Backdrop() }
        .ignoresSafeArea()
        .tint(.brand)
        .animation(.snappy(duration: 0.3), value: model.index)
    }

    /// A page slides in from the side it lies on; with reduced motion it only fades.
    private var transition: AnyTransition {
        if reduceMotion { return .opacity }
        let shift: CGFloat = model.forward ? 28 : -28
        return .asymmetric(insertion: .opacity.combined(with: .offset(x: shift)),
                           removal: .opacity.combined(with: .offset(x: -shift)))
    }

    @ViewBuilder private var page: some View {
        switch model.step {
        case .welcome:
            Page(title: L10n.setupMacWelcomeTitle, text: L10n.setupMacWelcomeBody, top: 76, heroHeight: 124) {
                BrandMark(size: 116).shadow(color: Color.brand.opacity(0.35), radius: 22, y: 10)
            } content: {
                Card { NoteRow(symbol: "network", text: Text(L10n.setupMacWelcomeNetwork)) }
            }
        case .move:
            Page(title: L10n.setupMacMoveTitle, text: L10n.setupMacMoveBody) {
                IconTile(symbol: "arrow.down.app.fill")
            } content: {
                if let folder = Relocation.folderName(of: model.store.context.bundle) {
                    Card { NoteRow(symbol: "folder.fill", text: Text(L10n.setupMacMoveState(folder: folder))) }
                }
                Button(L10n.setupMacMove) { model.move() }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .padding(.top, 4)
                if model.moveFailed {
                    Text(L10n.setupMacMoveFailed).font(.callout).foregroundStyle(.red)
                        .multilineTextAlignment(.center).fixedSize(horizontal: false, vertical: true)
                }
            }
        case .permissions:
            Page(title: L10n.setupMacPermissionsTitle, text: L10n.setupMacPermissionsBody) {
                IconTile(symbol: "lock.shield.fill")
            } content: {
                Card {
                    GrantRow(symbol: "rectangle.dashed.badge.record", title: L10n.setupMacScreenTitle, text: L10n.setupMacScreenBody) {
                        grantControl(.of(granted: model.grants.screen, asked: model.screenAsked),
                                     ask: L10n.setupMacScreenAllow) { model.screenAction() }
                    }
                    RowDivider()
                    GrantRow(symbol: "keyboard.fill", title: L10n.setupMacInputTitle, text: L10n.setupMacInputBody) {
                        grantControl(.of(granted: model.grants.input, asked: model.inputAsked),
                                     ask: L10n.setupMacInputAllow) { model.inputAction() }
                    }
                    RowDivider()
                    GrantRow(symbol: "speaker.wave.2.fill", title: L10n.setupMacAudioTitle, text: L10n.setupMacAudioBody) {
                        grantControl(.of(granted: model.grants.audio == .allowed, asked: model.audioAsked),
                                     ask: L10n.setupMacAudioAllow) { model.audioAction() }
                    }
                }
                if model.needsReopen { ReopenBanner { model.reopen() } }
            }
        case .login:
            Page(title: L10n.setupMacLoginTitle, text: L10n.setupMacLoginBody) {
                IconTile(symbol: "power")
            } content: {
                Card {
                    HStack(spacing: 12) {
                        IconTile(symbol: "clock.arrow.circlepath", size: 34)
                        Text(L10n.trayMacStartAtLogin).font(.headline)
                        Spacer()
                        Toggle(isOn: $model.startAtLogin) { Text(L10n.trayMacStartAtLogin) }
                            .labelsHidden()
                            .toggleStyle(.switch)
                            .disabled(!model.store.inApplications)
                    }
                    .padding(.horizontal, 14)
                    .padding(.vertical, 12)
                }
                if !model.store.inApplications {
                    Text(L10n.trayMacLoginElsewhere).font(.callout).foregroundStyle(.secondary)
                        .multilineTextAlignment(.center).fixedSize(horizontal: false, vertical: true)
                } else if model.store.login == .needsApproval {
                    Button(L10n.trayMacLoginApproval) { LoginItem.openSettings() }.buttonStyle(.link)
                }
            }
        case .done:
            Page(title: L10n.setupMacDoneTitle, text: L10n.setupMacDoneBody) {
                IconTile(symbol: "checkmark", color: .green)
            } content: {
                if let password = model.consolePassword { PasswordCard(password: password) }
                Card {
                    SummaryRow(symbol: "rectangle.dashed.badge.record", title: Text(L10n.setupMacScreenTitle)) {
                        GrantState(grant: frozen(model.grants.screen, asked: model.screenAsked), asked: model.screenAsked)
                    }
                    RowDivider(inset: 44)
                    SummaryRow(symbol: "keyboard", title: Text(L10n.setupMacInputTitle)) {
                        GrantState(grant: frozen(model.grants.input, asked: model.inputAsked), asked: model.inputAsked)
                    }
                    RowDivider(inset: 44)
                    SummaryRow(symbol: "speaker.wave.2", title: Text(L10n.setupMacAudioTitle)) {
                        GrantState(grant: model.grants.audio, asked: model.audioAsked)
                    }
                    if let verdict = model.controllers {
                        RowDivider(inset: 44)
                        SummaryRow(symbol: verdict.available ? "gamecontroller.fill" : "gamecontroller",
                                   title: Text(verdict.text), detail: verdict.impact) { EmptyView() }
                    }
                }
            }
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Dots(count: model.steps.count, index: model.index)
            Spacer()
            if model.index > 0 && model.step != .done {
                Button(L10n.commonBack) { model.back() }
            }
            if model.step == .done {
                Button(L10n.trayMacOpenConsole) { model.store.openConsole() }
                Button(L10n.commonDone) { model.finish() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
            } else if model.step == .permissions && model.grants.missingRequired {
                // The grants are this page's main buttons; Return must not skip them.
                Button(L10n.commonNext) { model.next() }
            } else {
                Button(L10n.commonNext) { model.next() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
            }
        }
        .controlSize(.large)
        .padding(.horizontal, 24)
        .padding(.vertical, 16)
    }

    /// A grant macOS answers only after a restart: asked in this run, its answer is not known.
    private func frozen(_ granted: Bool, asked: Bool) -> Grant {
        granted ? .allowed : asked ? .unknown : .denied
    }

    /// One grant's control: its button until macOS allows it, then the state.
    @ViewBuilder
    private func grantControl(_ action: GrantAction, ask: LocalizedStringResource, run: @escaping () -> Void) -> some View {
        switch action {
        case .none: GrantState(grant: .allowed)
        case .ask:
            Button(L10n.setupMacAllow, action: run).buttonStyle(.borderedProminent).accessibilityLabel(Text(ask))
        case .openSettings: Button(L10n.setupMacOpenSettings, action: run)
        }
    }
}

/// The window's ground: the system's own colour with the brand's glow from the top edge.
private struct Backdrop: View {
    var body: some View {
        ZStack(alignment: .top) {
            Color(nsColor: .windowBackgroundColor)
            EllipticalGradient(colors: [Color.brand.opacity(0.24), Color.brand.opacity(0)],
                               center: .top, startRadiusFraction: 0, endRadiusFraction: 0.7)
                .frame(height: 400)
        }
    }
}

private struct Page<Hero: View, Content: View>: View {
    let title: LocalizedStringResource
    let text: LocalizedStringResource
    var top: CGFloat = 40
    var heroHeight: CGFloat = 84
    @ViewBuilder let hero: () -> Hero
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(spacing: 0) {
            hero().frame(height: heroHeight)
            Text(title).font(.largeTitle.weight(.bold)).multilineTextAlignment(.center)
                .padding(.top, 14)
            Text(text).font(.title3).foregroundStyle(.secondary).multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 430)
                .padding(.top, 6)
            VStack(spacing: 12) { content() }.padding(.top, 20)
        }
        .padding(.horizontal, 44)
        .padding(.top, top)
    }
}

/// A symbol on a rounded tile, as System Settings draws its icons.
private struct IconTile: View {
    let symbol: String
    var size: CGFloat = 64
    var color: Color = .brand

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: size * 0.27, style: .continuous)
        shape.fill(color.gradient)
            .overlay {
                shape.strokeBorder(LinearGradient(colors: [.white.opacity(0.4), .white.opacity(0)],
                                                  startPoint: .top, endPoint: .bottom))
            }
            .overlay {
                Image(systemName: symbol).font(.system(size: size * 0.44, weight: .semibold)).foregroundStyle(.white)
            }
            .frame(width: size, height: size)
            .shadow(color: color.opacity(0.32), radius: size * 0.16, y: size * 0.08)
            .accessibilityHidden(true)
    }
}

private struct Card<Content: View>: View {
    @Environment(\.colorScheme) private var scheme
    @ViewBuilder let content: () -> Content

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 12, style: .continuous)
        VStack(spacing: 0) { content() }
            .frame(maxWidth: .infinity)
            .background(Color.white.opacity(scheme == .dark ? 0.06 : 0.8), in: shape)
            .overlay { shape.strokeBorder(Color.primary.opacity(scheme == .dark ? 0.1 : 0.07)) }
    }
}

private struct RowDivider: View {
    var inset: CGFloat = 60
    var body: some View { Divider().padding(.leading, inset) }
}

private struct GrantRow<Trailing: View>: View {
    let symbol: String
    let title: LocalizedStringResource
    let text: LocalizedStringResource
    @ViewBuilder let trailing: () -> Trailing

    var body: some View {
        HStack(spacing: 12) {
            IconTile(symbol: symbol, size: 34)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                Text(text).font(.subheadline).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            trailing()
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
    }
}

/// A sentence beside a symbol, for what a page only says.
private struct NoteRow: View {
    let symbol: String
    let text: Text

    var body: some View {
        HStack(spacing: 12) {
            IconTile(symbol: symbol, size: 34)
            text.font(.callout).fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
    }
}

private struct SummaryRow<Trailing: View>: View {
    let symbol: String
    let title: Text
    var detail: String?
    @ViewBuilder let trailing: () -> Trailing

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol).foregroundStyle(.secondary).frame(width: 20)
            VStack(alignment: .leading, spacing: 1) {
                title
                if let detail { Text(detail).font(.caption).foregroundStyle(.secondary) }
            }
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            trailing()
        }
        .font(.callout)
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
    }
}

/// The restart a screen recording or control grant needs before macOS applies it.
private struct ReopenBanner: View {
    let run: () -> Void

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 12, style: .continuous)
        HStack(spacing: 10) {
            Image(systemName: "arrow.clockwise.circle.fill").font(.title2).foregroundStyle(Color.brand)
            Text(L10n.setupMacScreenReopenHint).font(.callout).fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button(L10n.setupMacScreenReopen, action: run).buttonStyle(.borderedProminent)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(Color.brand.opacity(0.1), in: shape)
        .overlay { shape.strokeBorder(Color.brand.opacity(0.25)) }
    }
}

/// The console's first password: hidden until asked for, with a button that copies it.
private struct PasswordCard: View {
    let password: String
    @State private var shown = false
    @State private var copied = false

    var body: some View {
        Card {
            VStack(alignment: .leading, spacing: 8) {
                Text(L10n.wizardPasswordTitle).font(.headline)
                HStack(spacing: 8) {
                    Text(verbatim: shown ? password : String(repeating: "•", count: password.count))
                        .font(.system(.title3, design: .monospaced))
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 10)
                        .padding(.vertical, 6)
                        .background(Color.primary.opacity(0.06), in: RoundedRectangle(cornerRadius: 7, style: .continuous))
                        .accessibilityHidden(!shown)
                    Button { shown.toggle() } label: { Image(systemName: shown ? "eye.slash" : "eye") }
                        .help(Text(shown ? L10n.wizardPasswordHide : L10n.wizardPasswordShow))
                        .accessibilityLabel(Text(shown ? L10n.wizardPasswordHide : L10n.wizardPasswordShow))
                    Button(copied ? L10n.wizardPasswordCopied : L10n.wizardPasswordCopy) { copy() }
                }
                Text(L10n.setupMacPasswordHint).font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func copy() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(password, forType: .string)
        copied = true
        Task {
            try? await Task.sleep(for: .seconds(2))
            copied = false
        }
    }
}

private struct GrantState: View {
    let grant: Grant
    var asked = false

    var body: some View {
        switch grant {
        case .allowed: line(L10n.setupMacStateAllowed, "checkmark.circle.fill", .green)
        case .denied: line(L10n.setupMacStateDenied, "xmark.circle", .secondary)
        case .unknown:
            line(asked ? L10n.setupMacStateAsked : L10n.setupMacStateNotAsked, "circle.dashed", .secondary)
        }
    }

    /// The icon carries the colour; the word stays in the text colour, which reads on any ground.
    private func line(_ text: LocalizedStringResource, _ icon: String, _ tint: Color) -> some View {
        Label { Text(text) } icon: { Image(systemName: icon).foregroundStyle(tint) }
            .font(.callout.weight(.medium))
            .fixedSize()
    }
}

private struct Dots: View {
    let count: Int
    let index: Int

    var body: some View {
        HStack(spacing: 6) {
            ForEach(0..<count, id: \.self) { i in
                Capsule().fill(i == index ? Color.brand : Color.primary.opacity(0.18))
                    .frame(width: i == index ? 20 : 7, height: 7)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(L10n.setupMacStepA11y(step: "\(index + 1)", total: "\(count)"))
    }
}
