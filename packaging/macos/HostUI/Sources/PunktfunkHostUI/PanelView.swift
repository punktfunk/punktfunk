// The panel the menu bar item opens: status first, then the live session, pairing, what needs
// the person, and the actions. The system draws the panel's background; nothing here paints one.
import SwiftUI

/// The app's name as the bundle calls it.
let appName = (Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String) ?? "Punktfunk"

struct PanelView: View {
    @Bindable var store: HostStore

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Header(store: store)
            if let error = store.error {
                Label { Text(error) } icon: { Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.red) }
                    .font(.callout)
                    .padding(.horizontal, 14)
                    .padding(.bottom, 10)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if !sessions.isEmpty {
                SectionDivider()
                SessionSection(store: store, rows: sessions)
            }
            if store.state.summary != nil {
                SectionDivider()
                PairingSection(store: store)
            }
            if hasAttention {
                SectionDivider()
                AttentionSection(store: store)
            }
            SectionDivider()
            ActionSection(store: store)
        }
        .frame(width: 320)
        .tint(.brand)
        .disabled(store.busy)
    }

    private var sessions: [RuntimeStatus.Row] {
        let live = store.runtime?.sessions.filter { $0.phase != "done" && $0.phase != "ending" } ?? []
        if !live.isEmpty || !store.state.isStreaming { return live }
        // Until the first status read, the summary names the session.
        let s = store.state.summary
        let mode = s?.session.map { "\($0.width)x\($0.height)@\($0.fps)" } ?? ""
        return [RuntimeStatus.Row(id: nil, client: s?.clientName ?? "", clientName: s?.clientName, mode: mode, hdr: false, phase: "streaming")]
    }

    private var hasAttention: Bool {
        store.grants.missingRequired || store.state.summary?.pinPending == true || store.state.keptDisplays > 0
            || store.update?.available == true || !store.consoleUp
    }
}

private struct SectionDivider: View {
    var body: some View { Divider().padding(.horizontal, 14).padding(.vertical, 4) }
}

private struct Header: View {
    let store: HostStore

    var body: some View {
        HStack(spacing: 10) {
            BrandMark(size: 30)
            VStack(alignment: .leading, spacing: 2) {
                HStack(alignment: .firstTextBaseline) {
                    Text(appName).font(.headline)
                    Spacer()
                    Text(store.context.version).font(.caption).foregroundStyle(.secondary).monospacedDigit()
                }
                HStack(spacing: 5) {
                    stateIcon
                    Text(store.state.line).foregroundStyle(.secondary).lineLimit(1)
                }
                .font(.subheadline)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder private var stateIcon: some View {
        switch store.state {
        case .starting: ProgressView().controlSize(.mini)
        case .unavailable: Image(systemName: "exclamationmark.triangle").foregroundStyle(.secondary)
        case .running where store.state.isStreaming:
            Image(systemName: "dot.radiowaves.left.and.right").foregroundStyle(Color.brand)
        case .running: Image(systemName: "checkmark.circle").foregroundStyle(.secondary)
        }
    }
}

private struct SessionSection: View {
    let store: HostStore
    let rows: [RuntimeStatus.Row]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                let name = row.clientName ?? row.client
                HStack(alignment: .top, spacing: 10) {
                    Image(systemName: "display").font(.title3).foregroundStyle(Color.brand).frame(width: 24)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(name).fontWeight(.medium).lineLimit(1)
                        Group {
                            Readout(parts: picture(row))
                            if let stream = stream(row) { Readout(parts: stream) }
                        }
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        if let id = row.id, store.confirm != .endSession(id: id, name: name) {
                            Button(L10n.trayMacSessionEnd) { store.confirm = .endSession(id: id, name: name) }
                                .controlSize(.small)
                                .padding(.top, 4)
                        }
                    }
                    Spacer(minLength: 0)
                }
                if case .endSession(let id, _) = store.confirm, id == row.id {
                    ConfirmBox(question: L10n.trayMacSessionEndConfirm(name: name),
                               action: String(localized: L10n.trayMacSessionEnd),
                               onCancel: { store.confirm = nil }, onConfirm: { store.endSession(id) })
                }
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 6)
    }

    private func picture(_ row: RuntimeStatus.Row) -> [String] {
        guard row.phase == "streaming" else { return [String(localized: L10n.trayMacSessionConnecting)] }
        var parts: [String] = []
        if let m = Format.mode(row.mode) {
            parts.append(L10n.trayMacSessionMode(width: "\(m.width)", height: "\(m.height)", fps: "\(m.fps)"))
        }
        if row.hdr { parts.append("HDR") }
        return parts
    }

    /// Codec and bitrate: `/status` reports them for one session only.
    private func stream(_ row: RuntimeStatus.Row) -> [String]? {
        guard row.phase == "streaming", row.id == store.runtime?.sessionId, let s = store.runtime?.stream else { return nil }
        return [Format.codec(s.codec), Format.bitrate(s.bitrateKbps)]
    }
}

/// Technical readout tokens, set apart by a dot.
private struct Readout: View {
    let parts: [String]

    var body: some View {
        HStack(spacing: 4) {
            ForEach(Array(parts.enumerated()), id: \.offset) { i, part in
                if i > 0 { Text(verbatim: "·") }
                Text(part)
            }
        }
        .fixedSize()
        .accessibilityElement(children: .combine)
    }
}

private struct PairingSection: View {
    let store: HostStore

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let pin = store.pairing?.pin, store.pairing?.armed == true {
                PinBox(store: store, pin: pin)
            } else if store.pairing?.enabled != false {
                MenuRow(icon: "plus.circle", title: String(localized: L10n.trayMacPair)) { store.pair() }
            }
            ForEach(store.pending) { device in
                PendingRow(store: store, device: device)
            }
        }
        .padding(.horizontal, 6)
    }
}

private struct PinBox: View {
    let store: HostStore
    let pin: String

    var body: some View {
        GroupBox {
            VStack(spacing: 8) {
                Group {
                    if let device = store.pairBoundTo {
                        Text(L10n.trayMacPairBoundFor(name: device.name))
                    } else {
                        Text(L10n.trayMacPairEnter)
                    }
                }
                .font(.callout)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                // Padding, not letter spacing, holds the digits apart.
                HStack(spacing: 0) {
                    ForEach(Array(pin.enumerated()), id: \.offset) { _, digit in
                        Text(String(digit)).padding(.horizontal, 5)
                    }
                }
                .font(.system(.largeTitle, design: .rounded).weight(.semibold).monospacedDigit())
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(pin.map(String.init).joined(separator: " "))
                HStack {
                    if let deadline = store.pairDeadline {
                        TimelineView(.periodic(from: .now, by: 1)) { context in
                            Label(L10n.trayMacPairExpires(time: Format.clock(Int(deadline.timeIntervalSince(context.date).rounded()))),
                                  systemImage: "timer")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .monospacedDigit()
                        }
                    }
                    Spacer()
                    Button(L10n.commonCancel) { store.cancelPairing() }.controlSize(.small)
                }
            }
            .frame(maxWidth: .infinity)
            .padding(4)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
    }
}

private struct PendingRow: View {
    let store: HostStore
    let device: PendingDevice

    /// A knock from outside the network can't be approved: its name proves nothing there.
    private var remote: Bool { device.source == "wan" }

    private var badge: LocalizedStringResource {
        device.relayed ? L10n.trayMacPendingThroughWeb
            : device.unfamiliarOrigin ? L10n.trayMacPendingFromNewPage : L10n.trayMacPendingFromInternet
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 10) {
                Image(systemName: "person.crop.circle.badge.questionmark").font(.title3)
                    .foregroundStyle(.secondary).frame(width: 24)
                Text(device.name).fontWeight(.medium).lineLimit(1)
                Spacer(minLength: 4)
                if remote {
                    Button(L10n.trayMacPendingArm) { store.pair(for: device) }
                        .help(Text(L10n.trayMacPendingRemoteHint))
                } else {
                    Button(L10n.trayMacPendingApprove) { store.confirm = .approve(id: device.id) }
                }
                Button { store.deny(device) } label: { Image(systemName: "xmark") }
                    .buttonStyle(.borderless)
                    .help(Text(L10n.trayMacPendingDeny))
                    .accessibilityLabel(Text(L10n.trayMacPendingDeny))
            }
            .controlSize(.small)
            // The code tells two devices of one name apart; a remote knock can't be approved, so
            // its line says where it came from instead.
            HStack(spacing: 6) {
                Text(Format.age(device.ageSecs))
                if remote {
                    Text(badge).font(.caption2)
                        .padding(.horizontal, 5).padding(.vertical, 1)
                        .overlay(Capsule().strokeBorder(.secondary.opacity(0.6)))
                } else {
                    Text(Format.deviceCode(device.fingerprint)).monospaced()
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .padding(.leading, 34)
            if store.isApproving(device) {
                approveBox.padding(.top, 4)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
    }

    private var approveBox: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 6) {
                Text(L10n.trayMacPendingCode).font(.caption).foregroundStyle(.secondary)
                Text(Format.deviceCode(device.fingerprint)).font(.title3.monospaced())
                Text(L10n.trayMacPendingCodeHint).font(.caption).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                HStack {
                    Button(L10n.trayMacPendingGuest) { store.approve(device, guest: true) }
                        .help(Text(L10n.trayMacPendingGuestHint))
                    Spacer()
                    Button(L10n.commonCancel) { store.confirm = nil }.keyboardShortcut(.cancelAction)
                    Button(L10n.trayMacPendingApprove) { store.approve(device, guest: false) }
                        .keyboardShortcut(.defaultAction)
                        .buttonStyle(.borderedProminent)
                }
                .controlSize(.small)
            }
            .padding(4)
        }
    }
}

private struct AttentionSection: View {
    let store: HostStore

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            if store.grants.missingRequired {
                MenuRow(icon: "exclamationmark.shield", title: String(localized: L10n.trayMacAttentionPermissions)) {
                    store.showOnboarding()
                }
            }
            if store.state.summary?.pinPending == true {
                MenuRow(icon: "key", title: String(localized: L10n.trayApprovePairing)) { store.openConsole() }
            }
            let kept = store.state.keptDisplays
            if kept > 0 {
                MenuRow(icon: "display.2", title: L10n.trayReleaseDisplays(count: kept)) { store.confirm = .release }
                if store.confirm == .release {
                    ConfirmBox(question: L10n.trayMacReleaseConfirm(count: kept),
                               action: String(localized: L10n.trayMacRelease),
                               onCancel: { store.confirm = nil }, onConfirm: { store.releaseDisplays() })
                        .padding(.horizontal, 8)
                }
            }
            if store.update?.available == true, let version = store.update?.manifest?.version {
                MenuRow(icon: "arrow.down.circle", title: L10n.trayMacUpdateAvailable(version: version)) {
                    store.openUpdate()
                }
            }
            if !store.consoleUp {
                MenuRow(icon: "exclamationmark.triangle", title: String(localized: L10n.trayMacConsoleDown), action: nil)
            }
        }
        .padding(.horizontal, 6)
    }
}

private struct ActionSection: View {
    @Bindable var store: HostStore

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            MenuRow(icon: "globe", title: String(localized: L10n.trayMacOpenConsole)) { store.openConsole() }
            loginRow
            MenuRow(icon: "folder", title: String(localized: L10n.trayMacOpenLogs)) { store.openLogs() }
            SectionDivider().padding(.horizontal, -6)
            MenuRow(icon: "arrow.clockwise", title: String(localized: L10n.trayMacRestart)) { store.restart() }
            MenuRow(icon: "power", title: String(localized: L10n.trayMacQuit), shortcut: "⌘Q") { store.quit() }
                .keyboardShortcut("q")
        }
        .padding(.horizontal, 6)
        .padding(.bottom, 6)
    }

    private var loginRow: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 8) {
                Image(systemName: "clock.arrow.circlepath").frame(width: 20).foregroundStyle(.secondary)
                Toggle(isOn: Binding(get: { store.login == .on || store.login == .needsApproval },
                                     set: { store.setLogin($0) }))
                {
                    Text(L10n.trayMacStartAtLogin).frame(maxWidth: .infinity, alignment: .leading)
                }
                .toggleStyle(.switch)
                .controlSize(.mini)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .disabled(!store.inApplications || store.login == .unavailable)
            Group {
                if !store.inApplications {
                    Text(L10n.trayMacLoginElsewhere).foregroundStyle(.secondary)
                } else if store.login == .needsApproval {
                    Button(L10n.trayMacLoginApproval) { LoginItem.openSettings() }.buttonStyle(.link)
                }
            }
            .font(.caption)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.leading, 36)
            .padding(.trailing, 8)
        }
    }
}

/// A row that acts like a menu item: icon, title, highlight under the pointer. Without an
/// action it only informs, in secondary text.
struct MenuRow: View {
    let icon: String
    let title: String
    var shortcut: String?
    let action: (() -> Void)?
    @State private var hover = false

    var body: some View {
        if let action {
            Button(action: action) { row.background(RoundedRectangle(cornerRadius: 6).fill(hover ? Color.primary.opacity(0.08) : .clear)) }
                .buttonStyle(.plain)
                .onHover { hover = $0 }
        } else {
            row.foregroundStyle(.secondary)
        }
    }

    private var row: some View {
        HStack(spacing: 8) {
            Image(systemName: icon).frame(width: 20).foregroundStyle(.secondary)
            Text(title).lineLimit(2).fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 4)
            if let shortcut { Text(verbatim: shortcut).foregroundStyle(.secondary) }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .contentShape(Rectangle())
    }
}

private struct ConfirmBox: View {
    let question: String
    let action: String
    let onCancel: () -> Void
    let onConfirm: () -> Void

    var body: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 8) {
                Text(question).font(.callout).fixedSize(horizontal: false, vertical: true)
                HStack {
                    Spacer()
                    Button(L10n.commonCancel, action: onCancel).keyboardShortcut(.cancelAction)
                    Button(action, role: .destructive, action: onConfirm)
                        .keyboardShortcut(.defaultAction)
                        .buttonStyle(.borderedProminent)
                }
                .controlSize(.small)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(4)
        }
    }
}
