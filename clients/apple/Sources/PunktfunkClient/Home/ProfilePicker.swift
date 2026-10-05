// The profile picker: who is playing on this box. A connect asks the paired host for its profiles
// first (`ProfileFetch`), applies the shared rule (`HostProfiles.pickerDecision`) and shows this
// only when the rule says so. "Switch profile…" opens it directly and saves the pick without
// connecting. A sheet on iOS and macOS, a focus grid on tvOS.

import PunktfunkKit
import SwiftUI

/// What the host answered `GET /api/v1/profiles/enumerate` with.
enum ProfileAnswer {
    /// nil: the box has no profiles.
    case listed([ListedProfile]?)
    case failed
}

enum ProfileFetch {
    /// Seconds a connect waits for the list before it dials with the saved pick.
    static let wait: TimeInterval = 3

    /// Ask `host` for its profiles, giving up after `seconds` (nil waits for the transport).
    /// `mgmt` overrides the saved management port, as the console's row carries it.
    static func list(
        _ host: StoredHost, mgmt: UInt16? = nil, within seconds: TimeInterval? = nil
    ) async -> ProfileAnswer {
        guard let identity = (try? ClientIdentityStore.shared.load())?.identity else {
            return .failed
        }
        let ask: @Sendable () async -> ProfileAnswer = {
            do {
                let rows = try await LibraryClient.profiles(
                    address: host.address, port: mgmt ?? host.effectiveMgmtPort,
                    certPEM: identity.certPEM, keyPEM: identity.keyPEM,
                    hostFingerprint: host.pinnedSHA256)
                // An empty list is a box with no profiles to choose from.
                return .listed(rows?.isEmpty == false ? rows : nil)
            } catch {
                return .failed
            }
        }
        guard let seconds else { return await ask() }
        return await withTaskGroup(of: ProfileAnswer.self) { group in
            group.addTask(operation: ask)
            group.addTask {
                try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
                return .failed
            }
            let first = await group.next() ?? .failed
            group.cancelAll()
            return first
        }
    }
}

/// A picker waiting on the player.
struct ProfileAsk: Identifiable {
    enum Content {
        /// `saved` is the id of the remembered pick the box still lists; `gone` the name of one
        /// it no longer does.
        case choose([ListedProfile], saved: String?, gone: String?)
        case failed
    }

    let id = UUID()
    let hostName: String
    let content: Content
    /// Runs once the player has picked, before the sheet closes.
    let pick: (ProfilePick) -> Void
}

/// Opens the picker for "Switch profile…": a pick is saved and nothing connects.
@MainActor
final class ProfileSwitch: ObservableObject {
    @Published var ask: ProfileAsk?
    private var fetching: Task<Void, Never>?

    func start(_ host: StoredHost, store: HostStore) {
        fetching?.cancel()
        fetching = Task { [weak self] in
            let answer = await ProfileFetch.list(host)
            guard !Task.isCancelled, let self else { return }
            let saved = store.hosts.first { $0.id == host.id }?.pickedProfile ?? host.pickedProfile
            switch answer {
            case .listed(let rows):
                ask = ProfileAsk(
                    hostName: host.displayName,
                    content: .choose(
                        rows ?? [], saved: rows?.first { $0.id == saved?.id }?.id, gone: nil),
                    pick: { store.setProfile(host.id, $0) })
            case .failed:
                ask = ProfileAsk(hostName: host.displayName, content: .failed, pick: { _ in })
            }
        }
    }
}

struct ProfilePickerView: View {
    let ask: ProfileAsk
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(spacing: 24) {
            Text("Who's playing on \(ask.hostName)?")
                .font(.title2.bold())
                .multilineTextAlignment(.center)
            switch ask.content {
            case .failed:
                Text("Couldn't load the profiles.").foregroundStyle(.secondary)
            case .choose(let rows, let saved, let gone):
                if let gone {
                    Text("\(gone) is gone from this host.").foregroundStyle(.secondary)
                }
                if rows.isEmpty {
                    Text("No profiles on this host.").foregroundStyle(.secondary)
                } else {
                    ScrollView {
                        LazyVGrid(
                            columns: [GridItem(.adaptive(minimum: tile + 24), spacing: 24)],
                            spacing: 24
                        ) {
                            // The saved pick leads, so it is where focus starts.
                            ForEach(rows.sorted { ($0.id == saved) && ($1.id != saved) }) { row in
                                card(row, saved: row.id == saved)
                            }
                        }
                        .padding(8)
                    }
                }
            }
            Button(cancelTitle) { dismiss() }
        }
        .padding(32)
        #if os(macOS)
        .frame(minWidth: 460, minHeight: 320)
        #endif
    }

    private var cancelTitle: String {
        if case .choose(let rows, _, _) = ask.content, !rows.isEmpty { return "Cancel" }
        return "Close"
    }

    private var tile: CGFloat {
        #if os(tvOS)
        180
        #else
        88
        #endif
    }

    private func card(_ row: ListedProfile, saved: Bool) -> some View {
        Button {
            ask.pick(row.pick)
            dismiss()
        } label: {
            VStack(spacing: 8) {
                ProfileAvatar(name: row.displayName, accent: row.accent, size: tile)
                    .overlay {
                        if saved {
                            Circle().strokeBorder(Color.primary, lineWidth: 3).padding(-6)
                        }
                    }
                    .padding(6)
                Text(row.displayName).font(.headline).lineLimit(1)
                if let note = row.note {
                    Text(note).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                        .multilineTextAlignment(.center)
                }
            }
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
    }
}

/// A profile's initials on its accent: the host card's mark and the picker's circle.
struct ProfileAvatar: View {
    let name: String
    /// `#RRGGBB`; the app's brand colour when absent or unreadable.
    var accent: String?
    let size: CGFloat

    var body: some View {
        Text(HostProfiles.initials(name))
            .font(.system(size: size * 0.4, weight: .bold, design: .rounded))
            .foregroundStyle(.white)
            .minimumScaleFactor(0.5)
            .lineLimit(1)
            .frame(width: size, height: size)
            .background(Circle().fill(Color(hex: accent ?? "") ?? .brand))
    }
}
