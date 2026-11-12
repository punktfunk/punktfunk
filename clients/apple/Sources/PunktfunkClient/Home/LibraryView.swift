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
    /// Wake the host and wait for it (the Waking… overlay), then run the closure. Offered when
    /// auto-wake is off and the host didn't answer; nil offers nothing.
    var wake: ((StoredHost, @escaping () -> Void) -> Void)?
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
    @AppStorage(DefaultsKey.librarySort) private var sortRaw = SettingDefault.librarySort
    @AppStorage(DefaultsKey.libraryGroupBy) private var groupByRaw = ""
    @AutoWake private var autoWake
    @Environment(\.dismiss) private var dismiss
    /// Resolves a pinned shelf's preset NAME for the title (the target carries only its id).
    @ObservedObject private var presets = PresetStore.shared
    /// The shared "what is up on this host" answer, which this screen both READS (the menu's
    /// Resume row) and FEEDS: its own `/status` fetch below is the freshest one anybody has.
    @ObservedObject private var nowPlayingStore = NowPlayingStore.shared
    @ObservedObject private var favorites = LibraryFavorites.shared
    @AppStorage(DefaultsKey.librarySections) private var sectionsRaw =
        SettingDefault.librarySections
    @State private var search = ""
    @State private var showCustomize = false
    /// The title whose details sheet is up (the title menu's Details…).
    @State private var detailGame: GameEntry?
    /// An act pressed on that sheet that runs once the sheet is down
    /// (`LibraryShelf.Act.waitsForDetailsToClose`).
    @State private var afterDetails: (act: LibraryShelf.Act, game: GameEntry)?
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

    /// The titles, where they came from, and what the host runs: `LibraryShelf.reduce` fills it.
    @State private var shelf = LibraryShelf.State()
    /// Bumped by Reload and Retry: the load runs in the view's task, which leaving cancels.
    @State private var reloadToken = 0
    /// Held back a moment, so a cache that answers at once never flashes a spinner.
    @State private var spinnerDue = false
    /// The catalogs this run has shown, by host: a shelf the filter switches back to opens on its
    /// titles rather than on an empty frame and a spinner.
    @MainActor private static var shown: [String: [GameEntry]] = [:]
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
                endingGame.map { L10n.libEndGameConfirmTitle(title: $0.title) } ?? "",
                isPresented: Binding(get: { endingGame != nil }, set: { if !$0 { endingGame = nil } }),
                titleVisibility: .visible,
                presenting: endingGame
            ) { game in
                Button(L10n.libEndGame, role: .destructive) { endGame(game) }
            } message: { _ in
                Text(L10n.libEndGameConfirmBody)
            }
            .confirmationDialog(
                removingGame.map { L10n.libRemoveConfirmTitle(title: $0.title) } ?? "",
                isPresented: Binding(get: { removingGame != nil }, set: { if !$0 { removingGame = nil } }),
                titleVisibility: .visible,
                presenting: removingGame
            ) { game in
                Button(L10n.libDownloadRemove, role: .destructive) { changeInstall(game, .remove) }
            } message: { _ in
                Text(L10n.libRemoveConfirmBody)
            }
            .alert(
                endGameNotice ?? "",
                isPresented: Binding(get: { endGameNotice != nil }, set: { if !$0 { endGameNotice = nil } })
            ) {
                Button(L10n.commonOk, role: .cancel) {}
            }
            // Percentages move while a download runs and the shelf is up.
            .task(id: shelf.polls) {
                while shelf.polls, !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(2))
                    await refreshStatus()
                }
            }
    }

    var body: some View {
        prompted
            // In the tab the host filter names the shelf, so the title names the place; a TV's
            // tab bar already does.
            .modifier(LibraryTitle(
                title: tvTab ? nil
                    : inTab ? String(localized: L10n.libTitle) : L10n.libTitleHost(host: shelfTitle)))
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
                        Button(L10n.commonClose) { dismiss() }
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
            .fullScreenCover(item: $detailGame, onDismiss: runAfterDetails) { detailSheet($0) }
            #else
            .sheet(item: $detailGame, onDismiss: runAfterDetails) { detailSheet($0) }
            #endif
            // Before the first frame: a shelf seen this run opens on its titles, any other one on
            // the (held back) spinner rather than a flash of the empty state.
            .onAppear {
                guard shelf.games.isEmpty else { return }
                if let seen = Self.shown[host.id.uuidString] { shelf.games = seen } else { shelf.loading = true }
            }
            .task(id: reloadToken) {
                if keptForDetail { keptForDetail = false } else { await load() }
            }
            .task(id: shelf.loading) {
                spinnerDue = false
                guard shelf.loading else { return }
                try? await Task.sleep(for: .milliseconds(300))
                spinnerDue = shelf.loading
            }
            .onDisappear {
                // tvOS's full-screen details hide the shelf without leaving it: keep the loader
                // they draw with, and skip the reload on return unless a load was cut short.
                if detailGame != nil {
                    keptForDetail = !shelf.loading
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
        if let text = shelf.staleness.text {
            HStack(spacing: 6) {
                Image(systemName: shelf.staleness.symbol)
                Text(text)
                wakeButton
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
        } else if shelf.loading && shelf.games.isEmpty {
            ProgressView(String(localized: L10n.libLoading))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let errorText = shelf.errorText, shelf.games.isEmpty {
            errorState(errorText)
        } else if shelf.games.isEmpty {
            emptyState
        } else {
            VStack(spacing: 0) {
                staleNote
                grid
            }
        }
    }

    private var sort: LibrarySortKey { LibrarySortKey(stored: sortRaw) }
    private var groupBy: LibraryGroupBy? { LibraryGroupBy(stored: groupByRaw) }

    /// The plain grid, launchers included. Headers only when there is more than one section.
    private var grid: some View {
        let sections = LibraryShelf.grid(
            shelf.games, sort: sort, groupBy: groupBy, running: Set(shelf.running.keys))
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
        _ sections: [LibraryShelf.Section], showsHeaders: Bool, proxy: ScrollViewProxy
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
        sections: [LibraryShelf.Section], proxy: ScrollViewProxy,
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
    /// The rows are computed once here, over the titles the search leaves.
    private var tabBody: some View {
        let rows = LibraryShelf.rows(
            shelf.games, sort: sort, groupBy: groupBy, running: Set(shelf.running.keys),
            favorites: Set(favoriteIDs), search: search)
        return ScrollViewReader { proxy in
            keyNavigation(sections: rows.games, proxy: proxy) {
                ScrollView {
                    VStack(alignment: .leading, spacing: 26) {
                        #if os(tvOS)
                        tvActions
                        #endif
                        tabHeader
                        staleNote
                        ForEach(sectionLayout.visible) { section in
                            tabSection(section, rows: rows, proxy: proxy)
                        }
                        shelfState
                    }
                    .padding(.vertical)
                }
            }
        }
    }

    @ViewBuilder private func tabSection(
        _ section: LibrarySection, rows: LibraryShelf.Rows, proxy: ScrollViewProxy
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
            if !rows.recent.isEmpty {
                row(section) {
                    ForEach(rows.recent) { game in
                        tile(game, caption: PlayStatsText.lastPlayed(game.stats), scope: "recent")
                            .frame(width: rowTileWidth)
                    }
                }
            }
        case .favorites:
            if !rows.favorites.isEmpty {
                row(section) {
                    ForEach(rows.favorites) { game in
                        tile(game, scope: "favorites").frame(width: rowTileWidth)
                    }
                }
            }
        case .launchers:
            if !rows.launchers.isEmpty {
                row(section) {
                    ForEach(rows.launchers) { game in
                        tile(game, scope: "launchers").frame(width: rowTileWidth)
                    }
                }
            }
        case .games:
            if !rows.games.isEmpty {
                gridBlock(rows.games, showsHeaders: true, proxy: proxy)
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
        if shelf.games.isEmpty {
            Group {
                if shelf.loading {
                    if spinnerDue { ProgressView(String(localized: L10n.libLoading)) }
                } else if let errorText = shelf.errorText {
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

    private var favoriteIDs: [String] {
        #if DEBUG
        if let shotFavorites { return shotFavorites }
        #endif
        return favorites.ids(for: host.id.uuidString)
    }

    #if os(visionOS)
    private static let customizeWidth: CGFloat = 440
    #else
    private static let customizeWidth: CGFloat = 320
    #endif

    private var customizeButton: some View {
        Button { showCustomize = true } label: {
            Label(L10n.libCustomize, systemImage: "slider.horizontal.3")
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
                }
                    // A TV's plain style draws a platter round the label, a second card round the
                    // card; this one lifts the card itself.
                    #if os(tvOS)
                    .buttonStyle(TVCardButtonStyle())
                    #else
                    .buttonStyle(.plain)
                    #endif
                    #if os(visionOS)
                    // Concentric with the poster's corners, 6 pt out.
                    .cardHover(
                        RoundedRectangle(cornerRadius: GameCard.coverRadius, style: .continuous)
                            .inset(by: -6))
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
            isRunning: shelf.running[game.id] != nil,
            badge: TileBadge.forTitle(game.install, download: shelf.downloads[game.id]),
            caption: caption, host: host, frameID: frameID, artRound: shelf.artRound)
    }

    /// A title's own acts, one level below a host card's (design §2.5).
    private func titleMenu(_ game: GameEntry) -> some View {
        ForEach(titleActs(game), id: \.self) { act in
            Button(
                act.label(game.install), systemImage: act.symbol,
                role: act == .endGame ? .destructive : nil
            ) { perform(act, on: game) }
        }
    }

    private func titleActs(_ game: GameEntry) -> [LibraryShelf.Act] {
        LibraryShelf.acts(
            game, running: shelf.running[game.id], download: shelf.downloads[game.id],
            grants: shelf.grants, canLaunch: onLaunch != nil,
            favorite: inTab ? favoriteIDs.contains(game.id) : nil,
            canCopyLink: LinkClipboard.isAvailable)
    }

    private func perform(_ act: LibraryShelf.Act, on game: GameEntry) {
        switch act {
        case .play, .resume, .desktop: launch(game.id)
        case .favorite, .unfavorite: favorites.toggle(game.id, host: host.id.uuidString)
        case .details: detailGame = game
        case .copyLink: copyLink(game)
        case .files(.remove): removingGame = game
        case .files(let action): changeInstall(game, action)
        case .endGame: endingGame = game
        }
    }

    /// Ask the host to change `game`'s files. A refusal, or no way to ask, says why; a removal
    /// turns the tile to "not installed" here, and `/status` is read again either way.
    private func changeInstall(_ game: GameEntry, _ action: InstallAction) {
        guard let target = MgmtTarget.make(host: host).target(orSay: { endGameNotice = $0 })
        else { return }
        Task {
            let outcome = await LibraryClient.changeInstall(appID: game.id, action: action, on: target)
            if outcome == .done, action == .remove,
               let i = shelf.games.firstIndex(where: { $0.id == game.id }) {
                shelf.games[i].install?.state = "missing"
            }
            if outcome != .done { endGameNotice = outcome.notice(action, title: game.title) }
            await refreshStatus()
        }
    }

    /// What the host runs and downloads, and what this device may do there. A read the host
    /// didn't answer changes nothing.
    private func refreshStatus() async {
        let current = host
        guard case .success(let target) = MgmtTarget.make(host: current) else { return }
        let answer = await LibraryClient.statusIfAnswered(target)
        shelf = shelf.applying(answer)
        // The host cards read what runs from the store: hand it this answer rather than let
        // their TTL ask again. It also carries what no badge can: an untracked launch has no id.
        if let answer { nowPlayingStore.adopt(answer.games, for: current) }
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
                shelf.running[game.id] = nil
                nowPlayingStore.invalidate(current)
            } else {
                endGameNotice = outcome.notice(title: game.title)
            }
        }
    }

    /// The title menu's acts on the details sheet, less Details and the files act.
    private func detailSheet(_ game: GameEntry) -> some View {
        let acts = titleActs(game)
        let play = acts.first { [.play, .resume, .desktop].contains($0) }
        let heart = acts.first { $0 == .favorite || $0 == .unfavorite }
        func pressed(_ act: LibraryShelf.Act?) -> (() -> Void)? {
            guard let act, acts.contains(act) else { return nil }
            return { fromDetails(act, game) }
        }
        return TitleDetailSheet(
            game: game, artLoader: artLoader, playLabel: play?.label(nil) ?? "",
            isFavorite: heart.map { $0 == .unfavorite }, onToggleFavorite: pressed(heart) ?? {},
            onPlay: pressed(play), onCopyLink: pressed(.copyLink), onEndGame: pressed(.endGame),
            host: host)
            #if os(iOS) || os(visionOS)
            .presentationDetents([.medium, .large])
            #elseif os(macOS)
            .frame(minWidth: 440, minHeight: 360)
            #endif
    }

    /// An act pressed on the details sheet: one that waits for the sheet runs from its dismissal.
    private func fromDetails(_ act: LibraryShelf.Act, _ game: GameEntry) {
        guard act.waitsForDetailsToClose else { return perform(act, on: game) }
        afterDetails = (act, game)
        detailGame = nil
    }

    private func runAfterDetails() {
        guard let pending = afterDetails else { return }
        afterDetails = nil
        perform(pending.act, on: pending.game)
    }

    /// What a tile says under its title for the active sort (design P6): when it was last
    /// played under Recent, how long under Most played, nothing otherwise.
    private func sortCaption(_ game: GameEntry) -> String? {
        switch sort {
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
            HStack(spacing: 12) {
                wakeButton
                Button(L10n.commonRetry) { reloadToken += 1 }
                    .glassProminentButtonStyle()
            }
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// The wake offer: the explicit wake, then a reload once the host answers.
    @ViewBuilder private var wakeButton: some View {
        if shelf.wakeOffered, let wake {
            Button(L10n.hostcardWake) {
                wake(store.hosts.first { $0.id == host.id } ?? host) { reloadToken += 1 }
            }
            .buttonStyle(.bordered)
        }
    }

    private var emptyState: some View {
        VStack(spacing: 12) {
            Image(systemName: "square.grid.2x2")
                .font(.largeTitle)
                .foregroundStyle(.secondary)
            Text(L10n.libEmpty)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var reloadButton: some View {
        Button { reloadToken += 1 } label: {
            Label(L10n.libReload, systemImage: "arrow.clockwise")
        }
        .disabled(shelf.loading)
    }

    /// Sort and group for the plain grid — the console's bar, as a menu. The sort is the shared
    /// key (Default · A–Z · Platform · Store); the grouping is this grid's own (sections stand in
    /// for the console's Collections place). Each list sits under its heading (`MenuChoices`): a
    /// menu lists a picker's options without its title.
    private var sortMenu: some View {
        Menu {
            MenuChoices(
                title: String(localized: L10n.libSort),
                choices: LibrarySortKey.all.map { MenuChoice(label: $0.label, tag: $0.stored) },
                selection: $sortRaw)
            MenuChoices(
                title: String(localized: L10n.libGroup),
                choices: [
                    MenuChoice(label: String(localized: L10n.libGroupNone), tag: ""),
                    MenuChoice(label: String(localized: L10n.libGroupPlatform), tag: "platform"),
                    MenuChoice(label: String(localized: L10n.libGroupStore), tag: "store"),
                ],
                selection: $groupByRaw)
        } label: {
            Label(L10n.libSortGroup, systemImage: "line.3.horizontal.decrease.circle")
        }
        #if os(visionOS)
        // A circle like the toolbar's other buttons; the indicator widened it to a capsule.
        .menuIndicator(.hidden)
        .buttonBorderShape(.circle)
        #endif
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
            shelf.games = DemoMode.games
            shelf.running = [:]
            shelf.servedFromCacheAt = nil
            shelf.errorText = nil
            shelf.loading = false
            return
        }
        shelf.loading = true
        shelf.errorText = nil
        shelf.wakeOffered = false
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
        guard let target = MgmtTarget.make(host: current).target(orSay: { shelf.errorText = $0 })
        else {
            shelf.games = []
            shelf.loading = false
            return
        }
        // Built ahead of the first suspension: a remounted shelf draws its restored tiles
        // before `load()` resumes, and every poster needs this waiting. The fetch's outcome
        // doesn't gate it — cached posters render with the host still down.
        artLoader = LibraryArtLoader(target)

        // The last-known titles go up before the host is asked, marked as remembered, and the
        // box is woken while the player is still choosing: opening the library is the earliest
        // honest signal that someone intends to play. Auto-wake off asks once and offers a wake.
        let hostID = current.id.uuidString
        let events = LibraryLoad.run(
            target: target, hostID: hostID, wakeMacs: current.wakeMacs, autoWake: autoWake,
            profile: current.pickedProfile?.id)
        let canWake = WakePolicy.canWake(current.wakeMacs)
        for await event in events {
            shelf = LibraryShelf.reduce(shelf, event, autoWake: autoWake, canWake: canWake)
            switch event {
            case .cached, .fetched: Self.shown[hostID] = shelf.games
            case .status(let answer?): nowPlayingStore.adopt(answer.games, for: current)
            case .waking, .failed, .status(nil): break
            }
        }
    }

    #if DEBUG
    private func applyShot(_ phase: ShotLibraryPhase) {
        artLoader = ShotPosterArt.source
        switch phase {
        case .loading:
            shelf.loading = true
        case .error(let text):
            shelf.errorText = text
        case .empty:
            shelf.games = []
        case .catalog(let list, let staleness, let up):
            shelf.games = list.launchersFirst
            shelf.servedFromCacheAt = staleness == .none ? nil : Date()
            shelf.loading = staleness == .waking
            shelf.running = Dictionary(
                up.compactMap { id in ShotMock.running(id).map { (id, $0) } },
                uniquingKeysWith: { first, _ in first })
        }
    }
    #endif

    /// The `PUNKTFUNK_FAKE_LIBRARY` path: a plain `[GameEntry]` array, or a `{ "library": [...] }`
    /// wrapper (the shared vectors file). A bad file reads as an error state, not a crash.
    private func loadFake(path: String) {
        defer { shelf.loading = false }
        struct Wrapped: Decodable { let library: [GameEntry] }
        shelf.servedFromCacheAt = nil
        shelf.running = [:]
        // A source that fails every URL, so the posters settle on their placeholders rather
        // than wait on a loader this path never builds.
        artLoader = ShotArtSource(fixtures: [:])
        guard let data = FileManager.default.contents(atPath: path) else {
            shelf.games = []
            shelf.errorText = "PUNKTFUNK_FAKE_LIBRARY: can't read \(path)"
            return
        }
        let decoder = JSONDecoder()
        if let list = try? decoder.decode([GameEntry].self, from: data) {
            shelf.games = list.launchersFirst
        } else if let wrapped = try? decoder.decode(Wrapped.self, from: data) {
            shelf.games = wrapped.library.launchersFirst
        } else {
            shelf.games = []
            shelf.errorText = "PUNKTFUNK_FAKE_LIBRARY: \(path) is not a library JSON"
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
        LaunchedEntry.remember(shelf.games.first { $0.id == id }, from: TileFrames.rect(frameID ?? id))
        onLaunch(id)
    }

    /// `host`, or `host · preset` for a pinned card's shelf — the desktop's title shape.
    private var shelfTitle: String {
        target.title(in: presets)
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
    /// The shelf's art round (`PosterImage.round`).
    var artRound = 0

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
    /// The poster's corners; the Vision Pro's hover highlight runs concentric with them.
    static let coverRadius: CGFloat = 10
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
            icon: game.iconToken, frameID: frameID ?? game.id, round: artRound)
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
            .overlay(alignment: .bottomLeading) {
                if let line = game.overrides?.hostLine { HostOverrideBadge(line: line) }
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
            content.searchable(text: $text, prompt: String(localized: L10n.libSearchTitles))
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
                            Text(L10n.libTitle)
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
