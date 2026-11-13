// "Hidden for <name>": what a locked profile hides on a host, pushed over its shelf from the
// library's sort menu. The titles are dimmed; pressing one shows it for the profile again. Fetched
// each time it opens and never cached (`HideForMe`).

import PunktfunkKit
import SwiftUI

struct HiddenTitlesView: View {
    let host: StoredHost
    /// The profile this device plays as and may hide titles for.
    let profile: ListedProfile
    let artLoader: (any LibraryArtSource)?
    /// The host shows `GameEntry` again: it joins the shelf.
    let onShown: (GameEntry) -> Void
    #if DEBUG
    /// Shot harness: the list in place of the fetch.
    var shotHidden: [GameEntry]?
    #endif

    /// nil while the list loads, or when it failed.
    @State private var games: [GameEntry]?
    /// Why the list didn't load.
    @State private var failure: String?
    @State private var reload = 0
    /// Titles asked back whose answer is still out.
    @State private var asked = Set<String>()
    @State private var refusal: String?
    @State private var doneLine: String?
    #if os(tvOS)
    @FocusState private var focused: String?
    #endif

    private var showLabel: String { L10n.libShowFor(name: profile.displayName) }

    var body: some View {
        content
            .navigationTitle(L10n.libHiddenFor(name: profile.displayName))
            .task(id: reload) { await load() }
            .modifier(DoneLine(line: $doneLine))
            .alert(
                refusal ?? "",
                isPresented: Binding(get: { refusal != nil }, set: { if !$0 { refusal = nil } })
            ) {
                Button(L10n.commonOk, role: .cancel) {}
            }
    }

    @ViewBuilder private var content: some View {
        if let failure {
            ContentUnavailableView {
                Label(String(localized: L10n.libLoadFailed), systemImage: "exclamationmark.triangle")
            } description: {
                Text(failure)
            } actions: {
                Button(L10n.commonRetry) { reload += 1 }
            }
        } else if let games, games.isEmpty {
            ContentUnavailableView(
                L10n.libHiddenFor(name: profile.displayName), systemImage: "eye.slash",
                description: Text(L10n.libHiddenNone(name: profile.displayName)))
        } else if let games {
            ScrollView {
                LazyVGrid(columns: columns, spacing: Self.spacing) {
                    ForEach(games) { card($0) }
                }
                .padding()
            }
        } else {
            ProgressView(String(localized: L10n.libLoading))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    /// A hidden title, dimmed, over what pressing it does at full strength.
    private func card(_ game: GameEntry) -> some View {
        #if os(tvOS)
        let dim = focused == game.id ? 0.8 : 0.45
        #else
        let dim = 0.45
        #endif
        return Button { show(game) } label: {
            VStack(alignment: .leading, spacing: 6) {
                GameCard(game: game, artLoader: artLoader).opacity(dim)
                Label(showLabel, systemImage: "eye")
                    .font(.geist(Self.actionSize, .medium, relativeTo: .caption))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        #if os(tvOS)
        .buttonStyle(TVCardButtonStyle())
        .focused($focused, equals: game.id)
        #else
        .buttonStyle(.plain)
        #endif
        .contextMenu {
            Button(showLabel, systemImage: "eye") { show(game) }
        }
    }

    private func load() async {
        #if DEBUG
        if let shotHidden {
            games = shotHidden
            return
        }
        #endif
        (games, failure) = (nil, nil)
        guard let target = MgmtTarget.make(host: host).target(orSay: { failure = $0 }) else { return }
        do {
            games = try await HideForMe.hidden(profile: profile.id, on: target)
        } catch {
            if Task.isCancelled { return }
            failure = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        }
    }

    /// Ask the host to show `game` again, once until it answers. What it took leaves this list,
    /// focus moving to the title in its place; a refusal says why.
    private func show(_ game: GameEntry) {
        guard !asked.contains(game.id),
              let target = MgmtTarget.make(host: host).target(orSay: { refusal = $0 })
        else { return }
        asked.insert(game.id)
        Task {
            let outcome = await HideForMe.setHidden(
                game.id, hidden: false, profile: profile.id, on: target)
            asked.remove(game.id)
            let line = outcome.notice(hidden: false, title: game.title, name: profile.displayName)
            guard outcome == .done else {
                refusal = line
                return
            }
            #if os(tvOS)
            if focused == game.id, let games {
                focused = LibraryShelf.neighbour(of: game.id, in: [games.map(\.id)])
            }
            #endif
            games?.removeAll { $0.id == game.id }
            onShown(game)
            doneLine = line
        }
    }

    #if os(tvOS)
    private static let spacing: CGFloat = 40
    private static let actionSize: CGFloat = 22
    private var columns: [GridItem] {
        [GridItem(.adaptive(minimum: 220), spacing: Self.spacing, alignment: .top)]
    }
    #else
    private static let spacing: CGFloat = 18
    private static let actionSize: CGFloat = 12
    private var columns: [GridItem] {
        [GridItem(.adaptive(minimum: 130), spacing: Self.spacing, alignment: .top)]
    }
    #endif
}

/// A hide's or a show's line, a moment over the screen's foot. VoiceOver reads it as it shows.
struct DoneLine: ViewModifier {
    @Binding var line: String?

    #if os(tvOS)
    private static let size: CGFloat = 26
    #else
    private static let size: CGFloat = 14
    #endif

    func body(content: Content) -> some View {
        content.overlay(alignment: .bottom) {
            if let line {
                Text(line)
                    .font(.geist(Self.size, .medium, relativeTo: .footnote))
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 18)
                    .padding(.vertical, 10)
                    .glassBackground(Capsule())
                    .padding(.horizontal)
                    .padding(.bottom, 24)
                    .allowsHitTesting(false)
                    .accessibilityHidden(true)
                    .task(id: line) {
                        AccessibilityNotification.Announcement(line).post()
                        do {
                            try await Task.sleep(for: .seconds(3))
                            self.line = nil
                        } catch {}
                    }
            }
        }
    }
}
