// The host's game library: a poster grid fetched over the management API, from which titles are
// launched. Reached by TAPPING a paired host's card — this is the primary destination for a host
// that has a library, with streaming the desktop as the card's menu action.
//
// Three behaviours here exist to make the round trip (browse → play → quit → browse) hold together,
// all from a 2026-08-16 field report: the catalog is cached so a sleeping host still shows its
// titles, opening the screen wakes that host so it is warm by the time one is picked, and the
// position in the grid survives the stream. Titles already running are marked and sorted first.
//
// Gated on the host being PAIRED — see `HomeView.hostCard` for why the pin is load-bearing
// rather than cosmetic.

import PunktfunkKit
import SwiftUI

/// Which library shelf is open: a host, and — when it was opened from a PINNED host+preset card
/// (design/client-settings-profiles.md §5.2a) — that card's preset, which every title launched off
/// the shelf then runs with, exactly as the card's own tap would.
///
/// One value rather than a host plus a preset carried beside it: a host and its pinned cards are
/// different cards on the grid, so "which library" is not answered by the host alone. That is also
/// why `id` folds the preset in — a presentation keyed on the host would not re-present when you
/// move between a host's own shelf and one of its pins.
struct LibraryTarget: Identifiable, Hashable {
    let host: StoredHost
    /// `.inherit` from the host's own card (its binding decides, as it always has); `.preset` from
    /// a pinned card. `.defaults` never reaches here — nothing opens a library "with the globals".
    var preset: PresetSelection = .inherit

    var id: String {
        switch preset {
        case .inherit: host.id.uuidString
        case .defaults: "\(host.id.uuidString)#defaults"
        case .preset(let id): "\(host.id.uuidString)#\(id)"
        }
    }

    /// The pinned preset's id, if this shelf belongs to a pinned card.
    var pinnedPresetID: String? {
        if case .preset(let id) = preset { return id }
        return nil
    }

    /// What the screen calls itself: the host, and the preset when a pinned card opened it — the
    /// same `host · preset` shape that card wears, so which shelf you are on is on screen rather
    /// than remembered from the card you pressed. A pin whose preset has since been deleted
    /// resolves as no preset everywhere else, and reads as the plain host here.
    @MainActor func title(in catalog: PresetStore) -> String {
        guard let id = pinnedPresetID, let preset = catalog.preset(id: id) else {
            return host.displayName
        }
        return "\(host.displayName) \u{b7} \(preset.name)"
    }
}

extension LibraryTarget {
    /// Every shelf there is: each paired host, then each preset pinned to it. The Library's title
    /// menu lists these, on the touch UI and the Mac alike.
    @MainActor static func shelves(of hosts: [StoredHost], presets: PresetStore) -> [LibraryTarget] {
        hosts.filter { $0.pinnedSHA256 != nil }.flatMap { host in
            [LibraryTarget(host: host)]
                + presets.catalog.pinned(for: host).map {
                    LibraryTarget(host: host, preset: .preset($0.id))
                }
        }
    }
}

struct LibraryView: View {
    @ObservedObject var store: HostStore
    /// The shelf being browsed — the host, plus the pinned preset when a pinned card opened it.
    let target: LibraryTarget
    /// Tapping a title starts a session that asks the host to launch it (the library id is passed
    /// through). `nil` ⇒ browse-only (cards aren't tappable). The PRESET a launch runs with is the
    /// caller's to apply: it holds `target` and connects with `target.preset`.
    var onLaunch: ((String) -> Void)? = nil
    /// Stream this shelf's host without launching anything — "Resume <title>" while it has a
    /// game up. nil ⇒ browse-only, the same gate `onLaunch` uses.
    var onConnect: (() -> Void)? = nil
    /// The Library tab's presentation (design §2.5): sections, search and Customize, no Close.
    var inTab = false
    /// Stream a saved host's desktop, launching nothing: the tab's Desktops section.
    var onConnectHost: ((StoredHost) -> Void)?
    /// Drawn above the tab's sections, inside the scroll, so it moves with the title: the host
    /// filter.
    var tabHeader: AnyView?
    #if DEBUG
    /// Shot harness: a canned phase in place of the fetch (`ShotGallery.swift`).
    var shotPhase: ShotLibraryPhase?
    /// Shot harness: a section layout and favorites that never touch the device's own.
    var shotLayout: String?
    var shotFavorites: [String]?
    /// Shot harness: opens Customize.
    var shotCustomize = false
    #endif
    /// The touch grid's sort (the shared `library_sort` key, the same one the console's bar
    /// writes) and its grouping (touch-only — sections are the touch analogue of the console's
    /// Collections place).
    @AppStorage(DefaultsKey.librarySort) private var sortRaw = ""
    @AppStorage(DefaultsKey.libraryGroupBy) private var groupByRaw = ""
    @Environment(\.dismiss) private var dismiss
    /// Resolves a pinned shelf's preset NAME for the title (the target carries only its id).
    @ObservedObject private var presets = PresetStore.shared
    /// The shared "what is up on this host" answer, which this screen both READS (the menu's
    /// Resume row) and FEEDS: its own `/status` fetch below is the freshest one anybody has.
    @ObservedObject private var nowPlayingStore = NowPlayingStore.shared
    @ObservedObject private var favorites = LibraryFavorites.shared
    @AppStorage(DefaultsKey.librarySections) private var sectionsRaw = ""
    @State private var search = ""
    @State private var showCustomize = false
    /// The title whose details sheet is up (the title menu's Details…).
    @State private var detailGame: GameEntry?
    /// A Play pressed on that sheet, run once the sheet is down so the session never presents
    /// over a sheet that is still leaving.
    @State private var launchAfterDetails: String?
    /// The title End Game is asking about, and what the host said when it refused.
    @State private var endingGame: GameEntry?
    @State private var endGameNotice: String?
    /// The title Remove Download is asking about.
    @State private var removingGame: GameEntry?
    /// The shelf went behind a full-screen details page with its catalog loaded (tvOS).
    @State private var keptForDetail = false

    /// The host this shelf belongs to — every fetch, every poster URL and the launch itself address
    /// it, and a pinned shelf is the same host seen through one of its cards.
    private var host: StoredHost { target.host }

    @State private var games: [GameEntry] = []
    @State private var loading = false
    /// Bumped by Reload and Retry: the load runs in the view's task, which leaving cancels.
    @State private var reloadToken = 0
    /// Held back a moment, so a cache that answers at once never flashes a spinner.
    @State private var spinnerDue = false
    /// The catalogs this run has shown, by host: a shelf the filter switches back to opens on its
    /// titles rather than on an empty frame and a spinner.
    @MainActor private static var shown: [String: [GameEntry]] = [:]
    @State private var errorText: String?
    /// What the host has launched right now, keyed by library id — the `Resume` affordance. Empty
    /// on an older host, an unreachable one, or while the catalog is being served from cache.
    @State private var running: [String: RunningGame] = [:]
    /// `/status` downloads by library id, and this device's grants: host state, as `running`.
    @State private var downloads: [String: HostDownload] = [:]
    @State private var grants: UInt32?
    /// When the catalog on screen was fetched, if it came from disk rather than from the host.
    /// Non-nil ⇒ these titles are a memory, not an observation, and the view says so.
    @State private var servedFromCacheAt: Date?
    /// Guards the one-shot scroll restore. `onAppear` fires again on every re-layout (and once per
    /// section), and re-scrolling after the player has started browsing would yank the grid out
    /// from under them — which is a worse bug than the one being fixed.
    @State private var restoredScroll = false
    /// Cover-art loader (the same paired identity + host pinning as the list fetch, reused across
    /// every poster in the grid). Built alongside `games` in `load()`; dropped on disappear.
    @State private var artLoader: (any LibraryArtSource)?
    #if os(iOS) || os(visionOS) || os(macOS)
    /// The plain grid's hardware-keyboard cursor (a game id), and the grid width the column count
    /// is derived from. nil until the first arrow press, so a touch user never sees a selection
    /// they didn't ask for.
    @State private var keyCursor: String?
    @State private var gridWidth: CGFloat = 0
    #endif

    /// The TV's Library tab: its tab bar names the place and holds the shelf's actions.
    private var tvTab: Bool {
        #if os(tvOS)
        inTab
        #else
        false
        #endif
    }

    /// The shelf with its confirmations and the download poll, apart from `body` so the type
    /// checker answers in time.
    private var prompted: some View {
        content
            .confirmationDialog(
                endingGame.map { "End \($0.title)?" } ?? "",
                isPresented: Binding(get: { endingGame != nil }, set: { if !$0 { endingGame = nil } }),
                titleVisibility: .visible,
                presenting: endingGame
            ) { game in
                Button("End Game", role: .destructive) { endGame(game) }
            } message: { _ in
                Text("Unsaved progress in the game is lost.")
            }
            .confirmationDialog(
                removingGame.map { "Remove \($0.title)?" } ?? "",
                isPresented: Binding(get: { removingGame != nil }, set: { if !$0 { removingGame = nil } }),
                titleVisibility: .visible,
                presenting: removingGame
            ) { game in
                Button("Remove Download", role: .destructive) { changeInstall(game, .remove) }
            } message: { _ in
                Text("Saves stay on the host.")
            }
            .alert(
                endGameNotice ?? "",
                isPresented: Binding(get: { endGameNotice != nil }, set: { if !$0 { endGameNotice = nil } })
            ) {
                Button("OK", role: .cancel) {}
            }
            // Percentages move while a download runs and the shelf is up.
            .task(id: downloads.values.contains(where: \.live)) {
                while downloads.values.contains(where: \.live), !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(2))
                    await refreshStatus()
                }
            }
    }

    var body: some View {
        prompted
            // In the tab the host filter names the shelf, so the title names the place; a TV's
            // tab bar already does.
            .modifier(LibraryTitle(title: tvTab ? nil : inTab ? "Library" : "\(shelfTitle) — Library"))
            #if os(iOS) || os(visionOS)
            .modifier(LibraryTitleMode(inTab: inTab))
            #endif
            .toolbar {
                #if os(macOS)
                ToolbarItemGroup {
                    sortMenu
                    if inTab { customizeButton }
                    reloadButton
                }
                #else
                if !tvTab {
                    ToolbarItem(placement: .primaryAction) { reloadButton }
                    ToolbarItem(placement: .primaryAction) { sortMenu }
                }
                #if os(iOS) || os(visionOS)
                if inTab {
                    ToolbarItem(placement: .primaryAction) { customizeButton }
                }
                #endif
                #endif
                // A gamepad-only user can't swipe-to-dismiss the sheet this view is presented in
                // (ContentView's `.sheet(item: $libraryTarget)`) — give it a focusable, dpad-reachable
                // Close action. tvOS already has its own pushed-navigation back (Menu button).
                #if !os(tvOS)
                if !inTab {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Close") { dismiss() }
                    }
                }
                #endif
            }
            // A TV's `.searchable` is a keyboard band over the shelf; search there wants a tab.
            #if os(iOS) || os(visionOS) || os(macOS)
            .modifier(TitleSearch(active: inTab, text: $search))
            #endif
            #if os(tvOS)
            // A TV's sheet is a narrow card; the details want the screen.
            .fullScreenCover(item: $detailGame, onDismiss: launchPendingTitle) { detailSheet($0) }
            #else
            .sheet(item: $detailGame, onDismiss: launchPendingTitle) { detailSheet($0) }
            #endif
            // Before the first frame: a shelf seen this run opens on its titles, any other one on
            // the (held back) spinner rather than a flash of the empty state.
            .onAppear {
                guard games.isEmpty else { return }
                if let seen = Self.shown[host.id.uuidString] { games = seen } else { loading = true }
            }
            .task(id: reloadToken) {
                if keptForDetail { keptForDetail = false } else { await load() }
            }
            .task(id: loading) {
                spinnerDue = false
                guard loading else { return }
                try? await Task.sleep(for: .milliseconds(300))
                spinnerDue = loading
            }
            .onDisappear {
                // tvOS's full-screen details hide the shelf without leaving it: keep the loader
                // they draw with, and skip the reload on return unless a load was cut short.
                if detailGame != nil {
                    keptForDetail = !loading
                    return
                }
                // Hand the loader off before clearing it, so its pooled connections are closed
                // rather than left open on a screen the user has left.
                let leaving = artLoader
                artLoader = nil
                Task { await leaving?.close() }
            }
            #if os(tvOS)
            .sheet(isPresented: $showCustomize) { LibrarySectionsPanel() }
            #endif
            #if DEBUG && os(tvOS)
            .onAppear { if shotCustomize { showCustomize = true } }
            #endif
    }

    /// Says the titles below are remembered rather than observed — shown only while that is true,
    /// and never as an error: a cached library is a working library, and a host that is still
    /// waking is the case this whole path exists to serve.
    @ViewBuilder private var staleNote: some View {
        if let text = staleness.text {
            HStack(spacing: 6) {
                Image(systemName: staleness.symbol)
                Text(text)
            }
            .font(.geist(12, relativeTo: .caption))
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal)
            .padding(.top, 8)
        }
    }

    @ViewBuilder private var content: some View {
        if inTab {
            tabBody
        } else if loading && games.isEmpty {
            ProgressView("Loading library…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let errorText, games.isEmpty {
            errorState(errorText)
        } else if games.isEmpty {
            emptyState
        } else {
            VStack(spacing: 0) {
                staleNote
                grid
            }
        }
    }

    /// A catalog collated by the shared rules: launchers lead (design D4), then one section per
    /// group under the chosen grouping (none = one section of games), each in the chosen sort.
    private func sections(of items: [GameEntry]) -> [(label: String, games: [GameEntry])] {
        let groupBy: LibraryGroupBy?
        switch groupByRaw {
        case "platform": groupBy = .platform
        case "store": groupBy = .store
        default: groupBy = nil
        }
        return LibraryCollation.collate(items, sort: LibrarySortKey(stored: sortRaw), groupBy: groupBy)
            .map { group in
                // The ungrouped bucket names itself "All"; on this grid it has always been "Games".
                let label = (groupBy == nil && group.key != .launchers) ? "Games" : group.label
                return (label, group.indices.map { items[$0] })
            }
    }

    /// The plain grid's sections. Headers only when there is more than one.
    private var sections: [(label: String, games: [GameEntry])] {
        sections(of: ordered)
    }

    private var grid: some View {
        let sections = self.sections
        return ScrollViewReader { proxy in
            keyNavigation(sections: sections, proxy: proxy) {
                ScrollView {
                    gridBlock(sections, showsHeaders: sections.count > 1, proxy: proxy)
                        .padding(.vertical)
                }
            }
        }
    }

    /// Headed poster sections: put back where the player was, and measured for the keyboard
    /// cursor. Leaving a stream re-presents this view, so the last title opened from this shelf
    /// is scrolled back to once, without animation.
    private func gridBlock(
        _ sections: [(label: String, games: [GameEntry])], showsHeaders: Bool,
        proxy: ScrollViewProxy
    ) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            ForEach(Array(sections.enumerated()), id: \.offset) { _, section in
                if showsHeaders { sectionHeader(section.label) }
                tiles(section.games)
            }
        }
        .padding(.horizontal)
        .onAppear {
            guard !restoredScroll, let last = LibraryScrollMemory.last(forHost: host.id.uuidString),
                  sections.contains(where: { $0.games.contains { $0.id == last } })
            else { return }
            restoredScroll = true
            proxy.scrollTo(last, anchor: .center)
        }
        #if os(iOS) || os(visionOS) || os(macOS)
        // Measured without taking part in layout: it tells the keyboard cursor how many columns
        // `.adaptive` produced.
        .background {
            GeometryReader { geo in
                Color.clear
                    .onAppear { gridWidth = geo.size.width }
                    .onChange(of: geo.size.width) { _, w in gridWidth = w }
            }
        }
        #endif
    }

    /// Arrow keys pick a title in the grid and Return launches it: an iPad on a Magic Keyboard
    /// with no pad sees this grid.
    @ViewBuilder private func keyNavigation(
        sections: [(label: String, games: [GameEntry])], proxy: ScrollViewProxy,
        @ViewBuilder content: () -> some View
    ) -> some View {
        #if os(iOS) || os(visionOS) || os(macOS)
        content()
            .gamepadKeyNavigation(
                active: onLaunch != nil,
                onMove: { direction in
                    guard let next = gridNav(sections: sections.map(\.games))
                        .move(from: keyCursor, direction) else { return }
                    keyCursor = next
                    withAnimation(.easeOut(duration: 0.18)) { proxy.scrollTo(next, anchor: .center) }
                },
                onConfirm: {
                    guard let launch = launchAndRemember, let id = keyCursor else { return }
                    launch(id)
                })
        #else
        content()
        #endif
    }

    // MARK: - The Library tab

    /// The Library tab: the stored sections in order, each hidden while it has nothing to show.
    private var tabBody: some View {
        let groups = gameSections
        return ScrollViewReader { proxy in
            keyNavigation(sections: groups, proxy: proxy) {
                ScrollView {
                    VStack(alignment: .leading, spacing: 26) {
                        #if os(tvOS)
                        tvActions
                        #endif
                        tabHeader
                        staleNote
                        ForEach(sectionLayout.visible) { section in
                            tabSection(section, groups: groups, proxy: proxy)
                        }
                        shelfState
                    }
                    .padding(.vertical)
                }
            }
        }
    }

    @ViewBuilder private func tabSection(
        _ section: LibrarySection, groups: [(label: String, games: [GameEntry])],
        proxy: ScrollViewProxy
    ) -> some View {
        switch section {
        case .desktops:
            let paired = store.hosts.filter { $0.pinnedSHA256 != nil }
            if let onConnectHost, !paired.isEmpty {
                row(section) {
                    ForEach(paired) { host in
                        LibraryDesktopTile(
                            host: host, isOnline: store.probedOnline.contains(host.id),
                            nowPlaying: nowPlayingStore.title(for: host),
                            action: { onConnectHost(host) })
                    }
                }
            }
        case .recent:
            let played = recentTitles
            if !played.isEmpty {
                row(section) {
                    ForEach(played) { game in
                        tile(game, caption: PlayStatsText.lastPlayed(game.stats), scope: "recent")
                            .frame(width: rowTileWidth)
                    }
                }
            }
        case .favorites:
            let marked = favoriteTitles
            if !marked.isEmpty {
                row(section) {
                    ForEach(marked) { game in
                        tile(game, scope: "favorites").frame(width: rowTileWidth)
                    }
                }
            }
        case .launchers:
            let launchers = shelfGames.filter(\.isLauncher)
            if !launchers.isEmpty {
                row(section) {
                    ForEach(launchers) { game in
                        tile(game, scope: "launchers").frame(width: rowTileWidth)
                    }
                }
            }
        case .games:
            if !groups.isEmpty {
                gridBlock(groups, showsHeaders: true, proxy: proxy)
            }
        }
    }

    /// A headed horizontal row.
    private func row<Content: View>(
        _ section: LibrarySection, @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            sectionHeader(section.label)
                .padding(.horizontal)
            ScrollView(.horizontal, showsIndicators: false) {
                LazyHStack(alignment: .top, spacing: Self.rowSpacing) {
                    content()
                }
                .padding(.horizontal)
                .padding(.vertical, Self.rowLift)
            }
            #if os(tvOS)
            // A focused card's shadow reaches past the row; clipped, it ended in a hard edge.
            .scrollClipDisabled()
            #endif
        }
        #if os(tvOS)
        // A full-width target: a move down from anywhere, the actions at the right included,
        // lands in the row even where its tiles don't reach.
        .focusSection()
        #endif
    }

    /// Loading, error or empty, in place of the shelf's sections. Desktops still show above it.
    @ViewBuilder private var shelfState: some View {
        if games.isEmpty {
            Group {
                if loading {
                    if spinnerDue { ProgressView("Loading library…") }
                } else if let errorText {
                    errorState(errorText)
                } else {
                    emptyState
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.top, 24)
        }
    }

    private var sectionLayout: LibrarySectionLayout {
        #if DEBUG
        if let shotLayout { return LibrarySectionLayout(stored: shotLayout) }
        #endif
        return LibrarySectionLayout(stored: sectionsRaw)
    }

    /// A row's poster width: the grid's column minimum, so a row's posters match the grid's. On a
    /// TV a row's cards are larger than the grid's, with room around them for the focused one.
    #if os(tvOS)
    private var rowTileWidth: CGFloat { 280 }
    private static let rowSpacing: CGFloat = 40
    private static let rowLift: CGFloat = 20
    /// The grid's gap both ways: a row's, which leaves a focused card room to grow.
    private static let gridSpacing: CGFloat = rowSpacing
    #else
    private var rowTileWidth: CGFloat { 132 }
    private static let rowSpacing: CGFloat = 14
    private static let rowLift: CGFloat = 0
    private static let gridSpacing: CGFloat = 18
    #endif

    /// The shelf the search leaves, launchers included, in the host's order.
    private var shelfGames: [GameEntry] {
        guard !search.isEmpty else { return games }
        return games.filter { $0.title.localizedStandardContains(search) }
    }

    /// The Games section: the searched shelf without its launchers, which have their own row;
    /// anything running leads, as on the plain grid.
    private var gameSections: [(label: String, games: [GameEntry])] {
        let titles = shelfGames.filter { !$0.isLauncher }
        return sections(of: running.isEmpty ? titles : LibraryOrder.display(titles, running: Set(running.keys)))
    }

    /// Played titles, newest first, at most twelve: the shared Recent order, cut at the first
    /// title never played.
    private var recentTitles: [GameEntry] {
        // Filtered first: the sort then runs over the played titles, not the shelf.
        let titles = shelfGames.filter {
            !$0.isLauncher && ($0.stats?.lastPlayedUnixMs ?? 0) > 0
        }
        let order = LibraryCollation.collate(titles, sort: .recent, groupBy: nil).first?.indices ?? []
        return Array(order.map { titles[$0] }.prefix(12))
    }

    private var favoriteIDs: [String] {
        #if DEBUG
        if let shotFavorites { return shotFavorites }
        #endif
        return favorites.ids(for: host.id.uuidString)
    }

    /// Favorited titles, in the shelf's current sort.
    private var favoriteTitles: [GameEntry] {
        let marked = Set(favoriteIDs)
        guard !marked.isEmpty else { return [] }
        // Filtered first: the sort then runs over the favorites, not the shelf.
        let titles = shelfGames.filter { marked.contains($0.id) }
        return LibraryCollation.collate(titles, sort: LibrarySortKey(stored: sortRaw), groupBy: nil)
            .flatMap(\.indices).map { titles[$0] }
    }

    #if os(visionOS)
    private static let customizeWidth: CGFloat = 440
    #else
    private static let customizeWidth: CGFloat = 320
    #endif

    private var customizeButton: some View {
        Button { showCustomize = true } label: {
            Label("Customize", systemImage: "slider.horizontal.3")
        }
        #if os(iOS) || os(visionOS)
        // A popover on the iPad, as on the Mac; an iPhone shows it as a sheet. visionOS rows carry
        // larger type and insets, so its popover is wider.
        .popover(isPresented: $showCustomize) {
            LibrarySectionsPanel().frame(minWidth: Self.customizeWidth, minHeight: 440)
        }
        #endif
        #if os(macOS)
        // A popover on the Mac (design §4): the panel is a short list, not a task.
        .popover(isPresented: $showCustomize) {
            LibrarySectionsPanel().frame(width: 320, height: 250)
        }
        #endif
    }

    #if os(tvOS)
    /// Sort, Customize and Reload over the shelf, named: the TV's tab has no navigation bar, and
    /// focus doesn't cross from the tab bar to its row's end. A full-width target, so a move down
    /// from the tab bar lands here.
    private var tvActions: some View {
        HStack(spacing: 24) {
            sortMenu
            customizeButton
            reloadButton
        }
        .padding(.horizontal)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .focusSection()
    }
    #endif

    #if os(iOS) || os(visionOS) || os(macOS)
    /// The keyboard cursor's model over the two grid sections. Rebuilt per press from the live
    /// sections so it can never point into a stale list.
    private func gridNav(sections: [[GameEntry]]) -> LibraryGridNav {
        LibraryGridNav(
            sections: sections.filter { !$0.isEmpty }.map { $0.map(\.id) },
            columns: columnCount)
    }

    /// How many columns `.adaptive(minimum:spacing:)` fits into the measured width — the same
    /// arithmetic the layout does, so up/down move exactly one visual row rather than a guess.
    /// Falls back to one column before the first measurement lands.
    private var columnCount: Int {
        let minimum: CGFloat = 130 // matches `columns` below on iOS/macOS
        let spacing: CGFloat = 18
        // The VStack's `.padding()` is inside the measured width, so take it back off.
        let usable = gridWidth - 32
        guard usable > 0 else { return 1 }
        return max(1, Int((usable + spacing) / (minimum + spacing)))
    }
    #endif

    private func tiles(_ entries: [GameEntry]) -> some View {
        LazyVGrid(columns: columns, spacing: Self.gridSpacing) {
            ForEach(entries) { game in
                tile(game, caption: sortCaption(game))
            }
        }
    }

    /// One poster: a tap launches it, a long-press or right-click offers the title's own acts.
    /// `scope` keeps a row's copy of a title from sharing the grid's scroll id or its tile rect,
    /// so the launch flies out of the copy that was pressed.
    private func tile(_ game: GameEntry, caption: String? = nil, scope: String = "") -> some View {
        let key = scope.isEmpty ? game.id : "\(scope):\(game.id)"
        return Group {
            if launchAndRemember != nil {
                Button { launch(game.id, frameID: key) } label: {
                    card(game, caption: caption, frameID: key)
                        #if os(visionOS)
                        // The plain style's gaze highlight takes the system's large radius round
                        // the tile; this one runs concentric with the poster's corners, a little out.
                        .contentShape(
                            .hoverEffect, RoundedRectangle(cornerRadius: 16, style: .continuous).inset(by: -6))
                        #endif
                }
                    // A TV's plain style draws a platter round the label, a second card round the
                    // card; this one lifts the card itself.
                    #if os(tvOS)
                    .buttonStyle(TVCardButtonStyle())
                    #else
                    .buttonStyle(.plain)
                    #endif
            } else {
                card(game, caption: caption, frameID: key)
            }
        }
        .id(key)
        .contextMenu { titleMenu(game) }
    }

    private func card(_ game: GameEntry, caption: String?, frameID: String) -> GameCard {
        GameCard(
            game: game, artLoader: artLoader, selected: isKeyCursor(game),
            isRunning: running[game.id] != nil,
            badge: TileBadge.forTitle(game.install, download: downloads[game.id]),
            caption: caption, host: host, frameID: frameID)
    }

    /// A title's own acts, one level below a host card's (design §2.5): Play / Resume leads,
    /// then what a tap cannot do.
    @ViewBuilder private func titleMenu(_ game: GameEntry) -> some View {
        if let launch = launchAndRemember {
            Button(playLabel(game), systemImage: "play.fill") { launch(game.id) }
        }
        if inTab, game.id != LibraryCollation.desktopID {
            let marked = favoriteIDs.contains(game.id)
            Button(
                marked ? "Remove from Favorites" : "Add to Favorites",
                systemImage: marked ? "heart.slash" : "heart"
            ) {
                favorites.toggle(game.id, host: host.id.uuidString)
            }
        }
        if game.id != LibraryCollation.desktopID {
            Button("Details…", systemImage: "info.circle") { detailGame = game }
        }
        if LinkClipboard.isAvailable {
            Button("Copy Link", systemImage: "link") { copyLink(game) }
        }
        if let files = InstallAction.forTitle(
            game.install, download: downloads[game.id], grants: grants)
        {
            Button(files.label(game.install), systemImage: Self.filesSymbol(files)) {
                if files == .remove { removingGame = game } else { changeInstall(game, files) }
            }
        }
        if canEnd(game) {
            Button("End Game", systemImage: "xmark.circle", role: .destructive) { endingGame = game }
        }
    }

    private static func filesSymbol(_ action: InstallAction) -> String {
        switch action {
        case .install, .resume: return "arrow.down.circle"
        case .pause: return "pause.circle"
        case .remove: return "trash"
        }
    }

    /// Ask the host to change `game`'s files. A refusal, or no way to ask, says why; a removal
    /// turns the tile to "not installed" here, and `/status` is read again either way.
    private func changeInstall(_ game: GameEntry, _ action: InstallAction) {
        guard let target = MgmtTarget.make(host: host).target(orSay: { endGameNotice = $0 })
        else { return }
        Task {
            let outcome = await LibraryClient.changeInstall(appID: game.id, action: action, on: target)
            if outcome == .done, action == .remove, let i = games.firstIndex(where: { $0.id == game.id }) {
                games[i].install?.state = "missing"
            }
            if outcome != .done { endGameNotice = outcome.notice(action, title: game.title) }
            await refreshStatus()
        }
    }

    /// What the host runs and downloads, and what this device may do there.
    private func refreshStatus() async {
        let current = host
        guard case .success(let target) = MgmtTarget.make(host: current) else { return }
        applyStatus(await LibraryClient.status(target), for: current)
    }

    private func applyStatus(
        _ status: (games: [RunningGame], downloads: [HostDownload], grants: UInt32?),
        for current: StoredHost
    ) {
        running = Dictionary(
            status.games.filter(\.isUp).compactMap { g in g.appID.map { ($0, g) } },
            // Two sessions can have the same title up (the host admits concurrent sessions); for a
            // Resume badge either one is the same answer, and the endable one carries End Game.
            uniquingKeysWith: { first, other in other.endable == true ? other : first })
        downloads = Dictionary(
            status.downloads.map { ($0.appID, $0) }, uniquingKeysWith: { _, last in last })
        grants = status.grants
        // The host cards read the same fact from the store; hand it this answer rather than
        // letting their TTL ask the host a second time for what we just fetched. It also carries
        // the entries no badge can: a launch the host cannot track has no id to key on.
        nowPlayingStore.adopt(status.games, for: current)
    }

    /// The host runs a launch of this device's of `game`, so it lets this device end it.
    private func canEnd(_ game: GameEntry) -> Bool {
        game.id != LibraryCollation.desktopID && running[game.id]?.endable == true
    }

    /// Ask the host to end `game`. Gone either way drops the badge; a refusal, or no way to ask,
    /// says why.
    private func endGame(_ game: GameEntry) {
        let current = host
        guard let target = MgmtTarget.make(host: current).target(orSay: { endGameNotice = $0 })
        else { return }
        Task {
            let outcome = await LibraryClient.endGame(appID: game.id, on: target)
            if outcome.gameGone {
                running[game.id] = nil
                nowPlayingStore.invalidate(current)
            } else {
                endGameNotice = outcome.notice(title: game.title)
            }
        }
    }

    private func playLabel(_ game: GameEntry) -> String {
        if game.id == LibraryCollation.desktopID { return "Connect" }
        return running[game.id] != nil ? "Resume" : "Play"
    }

    private func detailSheet(_ game: GameEntry) -> some View {
        TitleDetailSheet(
            game: game, artLoader: artLoader, playLabel: playLabel(game),
            isFavorite: inTab ? favoriteIDs.contains(game.id) : nil,
            onToggleFavorite: { favorites.toggle(game.id, host: host.id.uuidString) },
            onPlay: launchAndRemember == nil ? nil : {
                launchAfterDetails = game.id
                detailGame = nil
            },
            onCopyLink: LinkClipboard.isAvailable ? { copyLink(game) } : nil,
            onEndGame: canEnd(game) ? {
                detailGame = nil
                endingGame = game
            } : nil,
            host: host)
            #if os(iOS) || os(visionOS)
            .presentationDetents([.medium, .large])
            #elseif os(macOS)
            .frame(minWidth: 440, minHeight: 360)
            #endif
    }

    private func launchPendingTitle() {
        guard let id = launchAfterDetails else { return }
        launchAfterDetails = nil
        launchAndRemember?(id)
    }

    /// What a tile says under its title for the active sort (design P6): when it was last
    /// played under Recent, how long under Most played, nothing otherwise.
    private func sortCaption(_ game: GameEntry) -> String? {
        switch LibrarySortKey(stored: sortRaw) {
        // Blank, not nil, for a title with nothing recorded: its tile keeps the caption line.
        case .recent: return PlayStatsText.lastPlayed(game.stats) ?? ""
        case .playTime: return PlayStatsText.playTime(game.stats) ?? ""
        default: return nil
        }
    }

    /// Put this title's self-emitted `punktfunk://` link on the clipboard: the shelf's host,
    /// the pinned card's preset when a pin opened it, and the game's own `launch=` id — so
    /// the URL boots straight into the title, the way a host card's link opens the desktop
    /// (design/client-deep-links.md §5).
    ///
    /// Addressed to the STORE's current record rather than the one the shelf was opened with,
    /// so a host re-addressed while browsing hands out the address it actually has now.
    private func copyLink(_ game: GameEntry) {
        let current = store.hosts.first { $0.id == host.id } ?? host
        LinkClipboard.copy(
            DeepLink.forHost(current, launch: game.id, preset: target.pinnedPresetID).urlString)
    }

    /// Whether the keyboard cursor is on this tile (always false where there is no keyboard
    /// navigation to have moved it).
    private func isKeyCursor(_ game: GameEntry) -> Bool {
        #if os(iOS) || os(visionOS) || os(macOS)
        keyCursor == game.id
        #else
        false
        #endif
    }

    private func sectionHeader(_ text: String) -> some View {
        #if os(tvOS)
        let size: CGFloat = 24
        #else
        let size: CGFloat = 12
        #endif
        return Text(text)
            .font(.geist(size, .semibold, relativeTo: .caption))
            .tracking(1.1)
            .foregroundStyle(.secondary)
    }

    private var columns: [GridItem] {
        #if os(tvOS)
        let minW: CGFloat = 220
        #else
        let minW: CGFloat = 130
        #endif
        // Top-aligned like the shelves: a two-line title must not lift its poster above the row.
        return [GridItem(.adaptive(minimum: minW), spacing: Self.gridSpacing, alignment: .top)]
    }

    private func errorState(_ text: String) -> some View {
        VStack(spacing: 16) {
            Image(systemName: "exclamationmark.triangle")
                .font(.largeTitle)
                .foregroundStyle(.secondary)
            Text(text)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 420)
            Button("Retry") { reloadToken += 1 }
                .glassProminentButtonStyle()
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var emptyState: some View {
        VStack(spacing: 12) {
            Image(systemName: "square.grid.2x2")
                .font(.largeTitle)
                .foregroundStyle(.secondary)
            Text("No games found on this host")
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var reloadButton: some View {
        Button { reloadToken += 1 } label: {
            Label("Reload", systemImage: "arrow.clockwise")
        }
        .disabled(loading)
    }

    /// Sort and group for the plain grid — the console's bar, as a menu. The sort is the shared
    /// key (Default · A–Z · Platform · Store); the grouping is this grid's own (sections stand in
    /// for the console's Collections place).
    private var sortMenu: some View {
        Menu {
            Picker("Sort", selection: $sortRaw) {
                ForEach(LibrarySortKey.all, id: \.stored) { key in
                    Text(key.label).tag(key.stored)
                }
            }
            Picker("Group by", selection: $groupByRaw) {
                Text("None").tag("")
                Text("Platform").tag("platform")
                Text("Store").tag("store")
            }
        } label: {
            Label("Sort & group", systemImage: "line.3.horizontal.decrease.circle")
        }
    }

    private func load() async {
        #if DEBUG
        if let shotPhase {
            applyShot(shotPhase)
            return
        }
        #endif
        // The demo host serves no management API; its shelf is built in.
        if DemoMode.isDemo(host) {
            artLoader = DemoMode.art
            games = DemoMode.games
            running = [:]
            servedFromCacheAt = nil
            errorText = nil
            loading = false
            return
        }
        loading = true
        errorText = nil
        // Dev hook, the twin of the desktop console's `PUNKTFUNK_FAKE_LIBRARY`: a file holding
        // the host's `/api/v1/library` JSON (or the shared collate vectors file, whose `library`
        // array is the same shape) stands in for the host, so the grid, the sort bar and the
        // collections can be exercised on a Mac with no host at all. No wake, no cache, no
        // `/status`, and no art — the posters are placeholders.
        if let fake = ProcessInfo.processInfo.environment["PUNKTFUNK_FAKE_LIBRARY"], !fake.isEmpty {
            loadFake(path: fake)
            return
        }
        let current = store.hosts.first { $0.id == host.id } ?? host
        guard let target = MgmtTarget.make(host: current).target(orSay: { errorText = $0 }) else {
            games = []
            loading = false
            return
        }
        // Built ahead of the first suspension: a remounted shelf draws its restored tiles
        // before `load()` resumes, and every poster needs this waiting. The fetch's outcome
        // doesn't gate it — cached posters render with the host still down.
        artLoader = LibraryArtLoader(target)

        // Show the catalog we already have BEFORE talking to the host. A library is the screen a
        // player uses to decide what to play, and an empty one while a sleeping box boots is the
        // opposite of useful — so the last-known titles go up immediately, marked as remembered,
        // and are replaced the moment the host answers.
        if let cached = await LibraryCache.shared?.load(hostID: current.id.uuidString) {
            games = cached.games.launchersFirst
            servedFromCacheAt = cached.fetchedAt
            Self.shown[current.id.uuidString] = games
        }
        // ...and wake the box while the player is still choosing. Waking has always been bound to
        // CONNECTING, which is too late to help: by then they have picked a title and are waiting
        // out a cold boot. Opening the library is the earliest honest signal that someone intends
        // to play.
        //
        // Sent up front and unconditionally rather than only when the host looks offline — the
        // same shape as the client core's own `orchestrate` path, and for the same reason: a magic
        // packet is a single fire-and-forget datagram that an already-awake machine ignores, so
        // waiting to find out whether it is needed costs more than sending it.
        let waking = !current.wakeMacs.isEmpty && PunktfunkConnection.wakeOnLANAvailable
        if waking {
            let (macs, address) = (current.wakeMacs, current.address)
            DispatchQueue.global(qos: .userInitiated).async { // blocking sends — off main
                PunktfunkConnection.wakeOnLAN(macs: macs, lastKnownIP: address)
            }
        }

        // A woken box takes 20–60 s to answer, so one attempt would almost always land on a host
        // that is still POSTing. Retry across that window when we sent a packet; without one, ask
        // exactly once and report what happened, as before.
        let attempts = waking ? 12 : 1
        for attempt in 0..<attempts {
            if Task.isCancelled { break }
            do {
                // `launchersFirst` groups launcher entries ahead of titles once, here, so the grid
                // inherits the D4 ordering.
                let fetched = try await LibraryClient.fetch(target).launchersFirst
                games = fetched
                servedFromCacheAt = nil
                errorText = nil
                await LibraryCache.shared?.store(fetched, hostID: current.id.uuidString)
                Self.shown[current.id.uuidString] = games
                break
            } catch {
                // Anything other than "can't reach it" is settled — a rejected certificate does not
                // become acceptable by waiting, and retrying an unpaired host twelve times just
                // delays telling the user what is actually wrong.
                let unreachable: Bool
                if case .unreachable = error as? LibraryError { unreachable = true } else {
                    unreachable = false
                }
                let more = unreachable && attempt + 1 < attempts
                if !more {
                    // A cached catalog outranks the error: the titles on screen are still the right
                    // ones to choose from, and replacing them with a red message because the host
                    // is asleep is precisely what this cache exists to prevent. The staleness note
                    // carries the situation instead.
                    if games.isEmpty {
                        // `LibraryError` reports a phrase; this state has no title of its
                        // own, so it supplies the frame the console shells get for free.
                        let why = (error as? LibraryError)?.errorDescription
                            ?? error.localizedDescription
                        errorText = "Couldn't load the library — \(why)"
                    }
                    break
                }
                try? await Task.sleep(nanoseconds: 5 * NSEC_PER_SEC)
            }
        }
        // Left mid-load: the next appearance loads again, so ask the host nothing more.
        if Task.isCancelled { return }

        // What's up on the host right now — never fatal, and deliberately after the catalog so a
        // slow `/status` can't hold the titles back.
        applyStatus(await LibraryClient.status(target), for: current)
        loading = false
    }

    #if DEBUG
    private func applyShot(_ phase: ShotLibraryPhase) {
        artLoader = ShotPosterArt.source
        switch phase {
        case .loading:
            loading = true
        case .error(let text):
            errorText = text
        case .empty:
            games = []
        case .catalog(let list, let staleness, let up):
            games = list.launchersFirst
            servedFromCacheAt = staleness == .none ? nil : Date()
            loading = staleness == .waking
            running = Dictionary(
                up.compactMap { id in ShotMock.running(id).map { (id, $0) } },
                uniquingKeysWith: { first, _ in first })
        }
    }
    #endif

    /// The `PUNKTFUNK_FAKE_LIBRARY` path: a plain `[GameEntry]` array, or a `{ "library": [...] }`
    /// wrapper (the shared vectors file). A bad file reads as an error state, not a crash.
    private func loadFake(path: String) {
        defer { loading = false }
        struct Wrapped: Decodable { let library: [GameEntry] }
        servedFromCacheAt = nil
        running = [:]
        // A source that fails every URL, so the posters settle on their placeholders rather
        // than wait on a loader this path never builds.
        artLoader = ShotArtSource(fixtures: [:])
        guard let data = FileManager.default.contents(atPath: path) else {
            games = []
            errorText = "PUNKTFUNK_FAKE_LIBRARY: can't read \(path)"
            return
        }
        let decoder = JSONDecoder()
        if let list = try? decoder.decode([GameEntry].self, from: data) {
            games = list.launchersFirst
        } else if let wrapped = try? decoder.decode(Wrapped.self, from: data) {
            games = wrapped.library.launchersFirst
        } else {
            games = []
            errorText = "PUNKTFUNK_FAKE_LIBRARY: \(path) is not a library JSON"
        }
    }

    /// Every launch from this shelf goes through here, so the player's position is recorded on
    /// exactly one path however they picked the title — a tap or the keyboard.
    /// `nil` in browse-only mode, which is what keeps the tiles untappable there.
    private var launchAndRemember: ((String) -> Void)? {
        guard onLaunch != nil else { return nil }
        return { launch($0) }
    }

    /// `frameID` names the pressed tile's rect when a title shows more than once; otherwise the
    /// entry's own id.
    private func launch(_ id: String, frameID: String? = nil) {
        guard let onLaunch else { return }
        // The desktop tile is the host, not one of its titles: it connects with no launch id, and
        // there is no position to remember for something that is not in the catalog.
        if id == LibraryCollation.desktopID {
            onConnect?()
            return
        }
        LibraryScrollMemory.remember(id, forHost: host.id.uuidString)
        LaunchedEntry.remember(games.first { $0.id == id }, from: TileFrames.rect(frameID ?? id))
        onLaunch(id)
    }

    /// `host`, or `host · preset` for a pinned card's shelf — the desktop's title shape.
    private var shelfTitle: String {
        target.title(in: presets)
    }

    /// The catalog in display order — `LibraryOrder.display`, the desktop's `order()`: launcher
    /// entries lead, and anything already running leads WITHIN its band, so getting back into it
    /// is the first thing on the screen rather than something to scroll for.
    private var ordered: [GameEntry] {
        // …and the desktop tile leads all of it, so streaming the host itself is one press
        // rather than a menu — and a host with no plugins still has something to press.
        // Only where a launch is possible: browse-only mode has nothing to connect with.
        let all = onConnect == nil ? games : [desktopTile] + games
        guard !running.isEmpty else { return all }
        return LibraryOrder.display(all, running: Set(running.keys))
    }

    /// The tile names what the press does, so it does not read "Desktop" while pressing it
    /// resumes a game the host already has up.
    private var desktopTile: GameEntry {
        let playing = nowPlayingStore.title(for: host)
        return LibraryCollation.desktopEntry(
            title: playing.map { "Resume \($0)" } ?? "Desktop")
    }

    /// Whether the titles on screen are remembered rather than observed, and what the host is
    /// doing about it — the three-state staleness both presentations show. Never an error: a
    /// cached library is a working library.
    private var staleness: LibraryStaleness {
        guard servedFromCacheAt != nil else { return .none }
        return loading ? .waking : .offline
    }
}

#if DEBUG
/// A shelf phase the shot harness shows without a host.
enum ShotLibraryPhase {
    case loading
    case error(String)
    case empty
    case catalog([GameEntry], staleness: LibraryStaleness = .none, running: [String] = [])
}
#endif

/// The catalog's provenance, as the shelf states it. Three states rather than a flag so "waking
/// the host…" can never be shown while nothing is happening — the same enum the desktop console
/// keeps (`Stale::{No, Waking, Offline}`), with its exact wording.
enum LibraryStaleness: Equatable {
    case none
    /// Served from disk; a fetch (and a wake) is in flight.
    case waking
    /// Served from disk; the host did not answer.
    case offline

    /// The note the shelf shows, or nil when the titles are live.
    var text: String? {
        switch self {
        case .none: return nil
        case .waking: return "Last known library — waking the host…"
        case .offline: return "Last known library — the host didn't answer"
        }
    }

    var symbol: String {
        self == .waking ? "arrow.clockwise" : "wifi.slash"
    }
}

/// One poster tile. Steam vs custom is marked with a badge; the art walks the candidate URLs
/// (portrait → header → hero) and finally a text placeholder.
struct GameCard: View {
    let game: GameEntry
    let artLoader: (any LibraryArtSource)?
    /// The hardware-keyboard cursor is on this tile — drawn as an accent ring, since the plain
    /// grid has no other way to say "Return launches THIS one".
    var selected = false
    /// This title is already up on the host, so picking it resumes rather than starts. Worth
    /// saying on the tile: the host has quietly adopted a running launch instead of starting a
    /// second copy for a while now, but nothing ever told the player that — so choosing a game
    /// they were already playing looked identical to starting one, and read as a relaunch.
    var isRunning = false
    /// The title's files when they aren't simply installed: downloading, paused, failed, missing.
    var badge: TileBadge? = nil
    /// A line under the title for what the current sort or section is about.
    var caption: String? = nil
    /// The host the title is on, named under the title with its OS mark.
    var host: StoredHost? = nil
    /// The key the poster publishes its rect under (`TileFrames`); the entry's id when nil.
    var frameID: String? = nil

    #if os(tvOS)
    @Environment(\.isFocused) private var focused
    private static let titleSize: CGFloat = 22
    private static let captionSize: CGFloat = 19
    /// The card's inset round the cover, and the cover's corners: the card's less that inset, so
    /// the two curves run parallel.
    private static let cardPadding: CGFloat = 12
    private static let cardRadius: CGFloat = 22
    private static let coverRadius: CGFloat = cardRadius - cardPadding
    #else
    private static let titleSize: CGFloat = 12
    private static let captionSize: CGFloat = 11
    private static let coverRadius: CGFloat = 10
    #endif

    var body: some View {
        #if os(tvOS)
        // One card, the cover inset on it and the title under it. Focused, it turns white as a
        // system row does, and lifts (`TVCardButtonStyle`). The ink covers the poster too: a
        // launcher's placeholder mark drew white on the white card.
        VStack(alignment: .leading, spacing: 12) {
            poster
            VStack(alignment: .leading, spacing: 4) {
                Text(game.title)
                    .font(.geist(Self.titleSize, .semibold, relativeTo: .caption))
                    .lineLimit(2, reservesSpace: true)
                hostLine
                if let caption {
                    Text(caption)
                        .font(.geist(Self.captionSize, relativeTo: .caption2))
                        .foregroundStyle(.secondary)
                        .lineLimit(1, reservesSpace: true)
                }
            }
            .padding(.horizontal, 8)
            .padding(.bottom, 4)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .foregroundStyle(focused ? Color.black : Color.primary)
        .padding(Self.cardPadding)
        .background(
            focused ? Color.white : Color.primary.opacity(0.1),
            in: RoundedRectangle(cornerRadius: Self.cardRadius, style: .continuous))
        .contentShape(RoundedRectangle(cornerRadius: Self.cardRadius, style: .continuous))
        #else
        VStack(alignment: .leading, spacing: 6) {
            poster
            Text(game.title)
                .font(.geist(Self.titleSize, relativeTo: .caption))
                // Two lines held for every title, so every tile in a row stands the same height.
                .lineLimit(2, reservesSpace: true)
                .foregroundStyle(.secondary)
            hostLine
            if let caption {
                Text(caption)
                    .font(.geist(Self.captionSize, relativeTo: .caption2))
                    .foregroundStyle(.tertiary)
                    .lineLimit(1, reservesSpace: true)
            }
        }
        #endif
    }

    /// The host the title is on: its OS mark and name.
    @ViewBuilder private var hostLine: some View {
        if let host {
            HStack(spacing: 6) {
                if let mark = osIconImage(for: host.osChain) {
                    mark.resizable().scaledToFit()
                        .frame(width: Self.captionSize, height: Self.captionSize)
                }
                Text(host.displayName)
                    .lineLimit(1)
            }
            .font(.geist(Self.captionSize, relativeTo: .caption2))
            .foregroundStyle(.secondary)
        }
    }

    private var poster: some View {
        PosterImage(
            candidates: game.art.posterCandidates, title: game.title, loader: artLoader,
            icon: game.iconToken, frameID: frameID ?? game.id)
            .aspectRatio(2.0 / 3.0, contentMode: .fit)
            .frame(maxWidth: .infinity)
            .clipShape(RoundedRectangle(cornerRadius: Self.coverRadius, style: .continuous))
            .overlay {
                if selected {
                    RoundedRectangle(cornerRadius: Self.coverRadius, style: .continuous)
                        .strokeBorder(.tint, lineWidth: 3)
                }
            }
            .overlay(alignment: .topLeading) {
                StoreBadge(label: game.storeLabel, isLauncher: game.isLauncher)
            }
            // Opposite corner from the store badge so the two never collide on a narrow tile.
            .overlay(alignment: .topTrailing) {
                if isRunning { RunningBadge() } else if let badge { FilesBadge(badge: badge) }
            }
    }
}

/// The navigation title, or none.
private struct LibraryTitle: ViewModifier {
    let title: String?

    func body(content: Content) -> some View {
        if let title {
            content.navigationTitle(title)
        } else {
            content
        }
    }
}

#if os(tvOS)
/// A TV cover card's button: the card lifts under focus, with no platter round it. The card
/// brightens its own fill (`GameCard`).
struct TVCardButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        Lift(label: configuration.label, pressed: configuration.isPressed)
    }

    private struct Lift: View {
        let label: ButtonStyleConfiguration.Label
        let pressed: Bool
        @Environment(\.isFocused) private var focused

        var body: some View {
            label
                .scaleEffect(focused ? 1.08 : 1)
                .shadow(color: .black.opacity(focused ? 0.45 : 0), radius: 14, y: 8)
                .opacity(pressed ? 0.85 : 1)
                .animation(.easeOut(duration: 0.18), value: focused)
        }
    }
}
#endif

#if os(iOS) || os(visionOS) || os(macOS)
/// The Library tab's and the Mac shelf's title search. The other presentations have none.
private struct TitleSearch: ViewModifier {
    let active: Bool
    @Binding var text: String

    func body(content: Content) -> some View {
        if active {
            content.searchable(text: $text, prompt: "Search titles")
        } else {
            content
        }
    }
}
#endif

#if os(iOS) || os(visionOS)
/// The title's mode. On a phone the tab holds its title at the leading edge, scrolled or not,
/// where the Hosts tab's collapsed title sits: iOS centers a collapsed title, which pressed
/// "Library" against the toolbar. The iPad and iOS before 26 keep the system title.
private struct LibraryTitleMode: ViewModifier {
    let inTab: Bool
    @Environment(\.horizontalSizeClass) private var sizeClass

    func body(content: Content) -> some View {
        let system = content.navigationBarTitleDisplayMode(inTab ? .automatic : .inline)
        #if os(visionOS)
        system
        #else
        if #available(iOS 26, *) {
            if inTab && sizeClass == .compact {
                content
                    .toolbarTitleDisplayMode(.inline)
                    .toolbar(removing: .title)
                    .toolbar {
                        ToolbarItem(placement: .topBarLeading) {
                            // The large title's face, at its own width: the bar clips it otherwise.
                            Text("Library")
                                .font(.geist(34, .bold, relativeTo: .largeTitle))
                                .fixedSize()
                                .accessibilityAddTraits(.isHeader)
                        }
                        .sharedBackgroundVisibility(.hidden)
                    }
            } else {
                system
            }
        } else {
            system
        }
        #endif
    }
}
#endif
