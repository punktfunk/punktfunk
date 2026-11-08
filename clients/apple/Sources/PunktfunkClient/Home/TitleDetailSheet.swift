// A title's details (design/apple-touch-ui-overhaul.md §2.5): cover, what it is, what the host
// recorded about playing it, and its acts: Play / Resume, a favorite heart, Copy Link. Opened
// from the title menu's Details…; a tap on the poster still just plays. A TV shows it full
// screen, its facts in a labelled grid.

import PunktfunkKit
import SwiftUI

struct TitleDetailSheet: View {
    let game: GameEntry
    let artLoader: (any LibraryArtSource)?
    /// Play, Resume (the title is up on the host) or Connect (the desktop entry).
    var playLabel = String(localized: L10n.commonPlay)
    /// nil hides the heart: favorites belong to the Library tab.
    var isFavorite: Bool?
    var onToggleFavorite: () -> Void = {}
    /// nil ⇒ browse-only, the same gate the poster tap uses.
    var onPlay: (() -> Void)?
    var onCopyLink: (() -> Void)?
    /// Set while the host runs a launch of this device's: End Game (the caller confirms).
    var onEndGame: (() -> Void)?
    /// The host the title is on, among a TV page's facts.
    var host: StoredHost?
    @Environment(\.dismiss) private var dismiss

    #if os(tvOS)
    // 10-foot sizes. Back closes the page, so it carries no Done.
    private let posterWidth: CGFloat = 380
    private let posterRadius: CGFloat = 16
    private let titleSize: CGFloat = 52
    private let factSize: CGFloat = 28
    private let actionSpacing: CGFloat = 24
    #else
    private let posterWidth: CGFloat = 112
    private let posterRadius: CGFloat = 10
    private let titleSize: CGFloat = 20
    private let factSize: CGFloat = 13
    private let actionSpacing: CGFloat = 10
    #endif

    var body: some View {
        #if os(tvOS)
        tvPage
        #else
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    HStack(alignment: .top, spacing: 16) {
                        poster
                        info
                    }
                    if let about = game.description, !about.isEmpty {
                        Text(about)
                            .font(.geist(14, relativeTo: .body))
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if let stats = PlayStatsText.summary(game.stats) {
                        Label(stats, systemImage: "clock")
                            .font(.geist(factSize, relativeTo: .subheadline))
                            .foregroundStyle(.secondary)
                    }
                    actions
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(20)
            }
            #if os(iOS) || os(visionOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button(L10n.commonDone) { dismiss() }
                }
            }
        }
        #endif
    }

    private var poster: some View {
        PosterImage(
            candidates: game.art.posterCandidates, title: game.title, loader: artLoader,
            icon: game.iconToken)
            .aspectRatio(2.0 / 3.0, contentMode: .fit)
            .frame(width: posterWidth)
            .clipShape(RoundedRectangle(cornerRadius: posterRadius, style: .continuous))
    }

    private var actions: some View {
        HStack(spacing: actionSpacing) {
            if let onPlay {
                Button(action: onPlay) {
                    Label(playLabel, systemImage: "play.fill")
                        // A TV button keeps its own size; stretched, it read as a banner.
                        #if !os(tvOS)
                        .frame(maxWidth: .infinity)
                        #endif
                }
                .buttonStyle(.borderedProminent)
            }
            if let isFavorite {
                Button(action: onToggleFavorite) {
                    Image(systemName: isFavorite ? "heart.fill" : "heart")
                }
                .buttonStyle(.bordered)
                .accessibilityLabel(isFavorite ? L10n.libFavoriteRemove : L10n.libFavoriteAdd)
            }
            if let onCopyLink {
                Button(action: onCopyLink) { Image(systemName: "link") }
                    .buttonStyle(.bordered)
                    .accessibilityLabel(L10n.hostcardCopyLink)
            }
            if let onEndGame {
                Button(role: .destructive, action: onEndGame) { Image(systemName: "xmark.circle") }
                    .buttonStyle(.bordered)
                    .accessibilityLabel(L10n.libEndGame)
            }
        }
        #if os(tvOS)
        .fixedSize()
        #else
        .controlSize(.large)
        #endif
    }

    /// Store · platform, whichever the host sent.
    private var origin: String? {
        let line = [Optional(game.storeLabel), game.platform].compactMap { $0 }
            .filter { !$0.isEmpty }.joined(separator: " \u{b7} ")
        return line.isEmpty ? nil : line
    }

    #if os(tvOS)
    /// Full screen over the title's art, blurred: the cover, then what the title is, its acts and
    /// its facts in a labelled grid.
    private var tvPage: some View {
        HStack(alignment: .top, spacing: 64) {
            poster
            VStack(alignment: .leading, spacing: 32) {
                VStack(alignment: .leading, spacing: 8) {
                    Text(game.title)
                        .font(.geist(titleSize, .bold, relativeTo: .title))
                        .fixedSize(horizontal: false, vertical: true)
                    if let origin {
                        Text(origin)
                            .font(.geist(factSize, relativeTo: .subheadline))
                            .foregroundStyle(.secondary)
                    }
                }
                actions
                Grid(alignment: .leading, horizontalSpacing: 40, verticalSpacing: 14) {
                    ForEach(tvFacts) { fact in
                        GridRow {
                            Text(fact.label)
                                .foregroundStyle(.secondary)
                            Text(fact.value)
                        }
                    }
                }
                .font(.geist(factSize, relativeTo: .body))
            }
            .frame(maxWidth: 960, alignment: .leading)
        }
        .padding(80)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background { backdrop }
    }

    /// The title's wide art, else its cover, blurred and dimmed behind the page.
    private var backdrop: some View {
        PosterImage(
            candidates: [game.art.hero, game.art.header].compactMap { $0.flatMap { URL(string: $0) } }
                + game.art.posterCandidates,
            title: game.title, loader: artLoader, icon: game.iconToken)
            .aspectRatio(contentMode: .fill)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .clipped()
            .blur(radius: 60)
            .overlay(Color.black.opacity(0.55))
            .ignoresSafeArea()
    }

    private struct Fact: Identifiable {
        let id: String
        let label: String
        let value: String
    }

    /// One row per fact the host sent, in reading order.
    private var tvFacts: [Fact] {
        let rows: [(id: String, label: LocalizedStringResource, value: String?)] = [
            ("host", L10n.libFactHost, host?.displayName),
            ("hostSets", L10n.libFactHostSets, game.overrides?.summary),
            ("developer", L10n.libFactDeveloper, game.developer),
            ("publisher", L10n.libFactPublisher, game.publisher == game.developer ? nil : game.publisher),
            ("released", L10n.libFactReleased, game.releaseYear.map { String($0) }),
            ("genres", L10n.libFactGenres, game.genres?.joined(separator: ", ")),
            ("lastPlayed", L10n.libFactLastPlayed, PlayStatsText.lastPlayed(game.stats)),
            ("lastSession", L10n.libFactLastSession, PlayStatsText.lastSession(game.stats)),
            ("playTime", L10n.libFactPlayTime, PlayStatsText.playTime(game.stats)),
            ("launches", L10n.libFactLaunches,
             game.stats.flatMap { $0.launchCount > 0 ? String($0.launchCount) : nil }),
        ]
        return rows.compactMap { row in
            guard let value = row.value, !value.isEmpty else { return nil }
            return Fact(id: row.id, label: String(localized: row.label), value: value)
        }
    }
    #else
    private var info: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(game.title)
                .font(.geist(titleSize, .semibold, relativeTo: .title3))
                .fixedSize(horizontal: false, vertical: true)
            ForEach(facts, id: \.self) { line in
                Text(line)
                    .font(.geist(factSize, relativeTo: .subheadline))
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// Store · platform, the credits, the genres (tags when there are none), the player count,
    /// then what the host sets for it: a line only when it has something.
    private var facts: [String] {
        let credits = [
            game.developer, game.publisher == game.developer ? nil : game.publisher,
            game.releaseYear.map { String($0) },
        ]
        let kinds = (game.genres ?? []).isEmpty ? (game.tags ?? []) : (game.genres ?? [])
        let players = game.players.flatMap { $0 > 1 ? L10n.libPlayersUpTo(count: $0) : nil }
        return [
            origin ?? "",
            credits.compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " \u{b7} "),
            kinds.joined(separator: ", "),
            players ?? "",
            game.overrides?.hostLine ?? "",
        ].filter { !$0.isEmpty }
    }
    #endif
}
