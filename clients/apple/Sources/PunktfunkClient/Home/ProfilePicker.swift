// The profile picker: who is playing on this box. A connect asks the paired host for its profiles
// (`ProfileFetch`) and shows this when the console's rule says so (`ConnectPlan.afterProfiles`).
// "Switch profile…" opens it directly: a pick is remembered, Ask every time forgets it. A sheet on
// every platform, with larger tiles on tvOS. `SeatWait` is the sheet a connect shows while the
// picked profile's seat starts.

import PunktfunkKit
import SwiftUI

/// What the host answered `GET /api/v1/profiles/enumerate` with.
enum ProfileAnswer {
    /// nil: the box has no profiles.
    case listed([ListedProfile]?)
    case failed
}

enum ProfileFetch {
    /// Seconds a connect waits for the list before it dials with the remembered pick.
    static let wait: TimeInterval = 3

    /// Ask `host` for its profiles, giving up after `seconds` (nil waits for the transport).
    /// `mgmt` overrides the saved management port, as the console's row carries it. An unpaired
    /// host fails without a request. Every list lands in `HostProfileStore` too.
    static func list(
        _ host: StoredHost, mgmt: UInt16? = nil, within seconds: TimeInterval? = nil
    ) async -> ProfileAnswer {
        guard case .success(let target) = MgmtTarget.make(host: host, port: mgmt) else {
            return .failed
        }
        let ask: @Sendable () async -> ProfileAnswer = {
            do {
                let rows = try await LibraryClient.profiles(target)
                // An empty list is a box with no profiles to choose from.
                return .listed(rows?.isEmpty == false ? rows : nil)
            } catch {
                return .failed
            }
        }
        // The transport does not stop on a cancel, so the cap must not wait for it.
        let answer = if let seconds {
            await Deadline.first(within: seconds, fallback: .failed, ask)
        } else {
            await ask()
        }
        await HostProfileStore.shared.learn(host, answer)
        return answer
    }
}

/// How many profiles each paired host listed, from every list this app asked for. A card offers
/// Switch profile from it, as the console does from its own.
@MainActor
final class HostProfileStore: ObservableObject {
    static let shared = HostProfileStore()

    /// 300 s, as host power: profiles change when an operator edits them.
    private var cache = TTLCache<Int>(ttl: 300)

    /// What `host` last listed; 0 until a list lands.
    func listed(_ host: StoredHost) -> Int { cache[host.id.uuidString] ?? 0 }

    /// Ask unless the last ask is younger than the TTL. A miss waits the TTL out too, so a host
    /// that refuses the list is not asked on every beat.
    func refresh(_ host: StoredHost) {
        guard !DemoMode.isDemo(host), cache.ask(host.id.uuidString, now: Date()) != nil
        else { return }
        Task { @MainActor in _ = await ProfileFetch.list(host) }
    }

    func learn(_ host: StoredHost, _ answer: ProfileAnswer) {
        guard case .listed(let rows) = answer else { return }
        objectWillChange.send()
        cache.store(rows?.count ?? 0, for: host.id.uuidString, now: Date())
    }
}

extension ProfileFetch {
    /// Start profile `id`'s stopped seat on `host`. Throws what stopped it, for
    /// `LibraryClient.wakeFailedLine`; an unpaired host fails without a request.
    static func wake(_ host: StoredHost, mgmt: UInt16? = nil, id: String) async throws {
        let target = try MgmtTarget.make(host: host, port: mgmt).get()
        try await LibraryClient.wakeProfile(id: id, on: target)
    }
}

/// A connect waiting for a profile's seat to come up. Closing the sheet cancels the poll.
@MainActor
final class SeatWait: ObservableObject, Identifiable {
    let title: String
    @Published var detail: String?
    var task: Task<Void, Never>?

    init(title: String, detail: String?) {
        self.title = title
        self.detail = detail
    }
}

struct SeatWaitView: View {
    @ObservedObject var wait: SeatWait
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(spacing: 24) {
            ProgressView()
            Text(wait.title)
                .font(.title2.bold())
                .multilineTextAlignment(.center)
            if let detail = wait.detail {
                Text(detail).foregroundStyle(.secondary).multilineTextAlignment(.center)
            }
            Button(L10n.commonCancel) { dismiss() }
        }
        .padding(32)
        #if os(macOS)
        .frame(minWidth: 380)
        #endif
        .onDisappear { wait.task?.cancel() }
    }
}

/// A picker waiting on the player.
struct ProfileAsk: Identifiable {
    enum Content {
        /// `saved` is the id of the listed profile that leads: the saved pick before a connect,
        /// the remembered pick on Switch profile. `gone` names a remembered pick the box no
        /// longer lists.
        case choose([ListedProfile], saved: String?, gone: String?)
        case failed
    }

    let id = UUID()
    let host: StoredHost
    var hostName: String { host.displayName }
    let content: Content
    /// A connect waits on the pick: the sheet offers Remember on this device. Without one it
    /// is Switch profile, which offers Ask every time.
    var connects = false
    /// Runs once the player has picked, before the sheet closes: the pick, with `remember` set
    /// when this device keeps it, or nil for Ask every time.
    let pick: (ProfilePick?) -> Void
}

/// Opens the picker for "Switch profile…": a pick is remembered, Ask every time forgets it, and
/// nothing connects.
@MainActor
final class ProfileSwitch: ObservableObject {
    @Published var ask: ProfileAsk?
    private var fetching: Task<Void, Never>?

    func start(_ host: StoredHost, store: HostStore) {
        fetching?.cancel()
        fetching = Task { [weak self] in
            let answer = await ProfileFetch.list(host)
            guard !Task.isCancelled, let self else { return }
            let saved = (store.hosts.first { $0.id == host.id } ?? host).pickedProfile
            let remembered = saved?.remember == true ? saved?.id : nil
            switch answer {
            case .listed(let rows):
                ask = ProfileAsk(
                    host: host,
                    content: .choose(
                        rows ?? [], saved: rows?.first { $0.id == remembered }?.id, gone: nil),
                    pick: { store.setProfile(host.id, $0) })
            case .failed:
                ask = ProfileAsk(host: host, content: .failed, pick: { _ in })
            }
        }
    }
}

struct ProfilePickerView: View {
    let ask: ProfileAsk
    @Environment(\.dismiss) private var dismiss
    /// Before a connect: keep the pick on this device. Off until the player turns it on.
    @State private var remember = false

    var body: some View {
        VStack(spacing: 24) {
            Text(L10n.profileTitle(host: ask.hostName))
                .font(.title2.bold())
                .multilineTextAlignment(.center)
            switch ask.content {
            case .failed:
                Text(L10n.profileLoadFailed).foregroundStyle(.secondary)
            case .choose(let rows, let saved, let gone):
                if let gone {
                    Text(L10n.profileGone(name: gone)).foregroundStyle(.secondary)
                }
                if rows.isEmpty {
                    Text(L10n.profileNone).foregroundStyle(.secondary)
                } else {
                    ScrollView {
                        LazyVGrid(
                            columns: [GridItem(.adaptive(minimum: tile + 24), spacing: 24)],
                            spacing: 24
                        ) {
                            // The saved pick leads, so it is where focus starts.
                            ForEach(rows.sorted { ($0.id == saved) && ($1.id != saved) }) { row in
                                card(row, current: !ask.connects && row.id == saved)
                            }
                            if !ask.connects && rows.count > 1 { askCard(current: saved == nil) }
                        }
                        .padding(8)
                    }
                    if ask.connects {
                        Toggle(L10n.profileRememberDevice, isOn: $remember).fixedSize()
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

    private var cancelTitle: LocalizedStringResource {
        if case .choose(let rows, _, _) = ask.content, !rows.isEmpty { return L10n.commonCancel }
        return L10n.commonClose
    }

    private var tile: CGFloat {
        #if os(tvOS)
        180
        #else
        88
        #endif
    }

    /// A profile's card. Switch profile remembers the pick; a connect keeps it only with Remember
    /// on. `current`: what this host does now, ringed.
    private func card(_ row: ListedProfile, current: Bool) -> some View {
        Button {
            var pick = row.pick
            pick.remember = !ask.connects || remember
            ask.pick(pick)
            dismiss()
        } label: {
            VStack(spacing: 8) {
                ProfileAvatar(
                    name: row.displayName, accent: row.accent, size: tile, picture: row.avatar,
                    host: ask.host
                )
                    .overlay { ring(current) }
                    .padding(6)
                HStack(spacing: 4) {
                    // Opens with its account password or PIN, or this device's face or finger.
                    if row.unlock != .none {
                        Image(systemName: "lock.fill")
                            .imageScale(.small)
                            .foregroundStyle(.secondary)
                            .accessibilityLabel(L10n.profileLocked)
                    }
                    Text(row.displayName).font(.headline).lineLimit(1)
                }
                if let note = row.note {
                    Text(note).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                        .multilineTextAlignment(.center)
                }
            }
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(current ? .isSelected : [])
    }

    /// Ask every time: Switch profile forgets the pick, so every connect asks.
    private func askCard(current: Bool) -> some View {
        Button {
            ask.pick(nil)
            dismiss()
        } label: {
            VStack(spacing: 8) {
                Image(systemName: "questionmark")
                    .font(.system(size: tile * 0.4, weight: .bold, design: .rounded))
                    .frame(width: tile, height: tile)
                    .background(Circle().fill(Color.secondary.opacity(0.2)))
                    .overlay { ring(current) }
                    .padding(6)
                Text(L10n.profileAskEveryTime).font(.headline).lineLimit(1)
            }
            .frame(maxWidth: .infinity)
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(current ? .isSelected : [])
    }

    @ViewBuilder private func ring(_ shown: Bool) -> some View {
        if shown { Circle().strokeBorder(Color.primary, lineWidth: 3).padding(-6) }
    }
}

/// A profile's face, the host card's mark and the picker's circle: its picture over its initials
/// on its accent.
struct ProfileAvatar: View {
    let name: String
    /// `#RRGGBB`; the app's brand colour when absent or unreadable.
    var accent: String?
    let size: CGFloat
    /// Host-relative URL of the picture.
    var picture: String?
    var host: StoredHost?

    var body: some View {
        let letters = HostProfiles.initials(name)
        Group {
            if let ink = InitialsInk(letters, fontSize: size * 0.4) {
                ink.fill(.white)
            } else {
                // An emoji has no outline; it keeps the text's own centring.
                Text(letters)
                    .font(.system(size: size * 0.4, weight: .bold, design: .rounded))
                    .foregroundStyle(.white)
                    .minimumScaleFactor(0.5)
                    .lineLimit(1)
            }
        }
        .frame(width: size, height: size)
        .background(Circle().fill(Color(hex: accent ?? "") ?? .brand))
        .overlay {
            if let picture, let host {
                ProfilePicture(path: picture, host: host, size: size)
            }
        }
    }
}
