// Variant galleries for the shot harness. One scene lays out every state of a component, so a
// card or tile change is reviewed as a sheet of states instead of one mock at a time. The
// catalogs below are the single source: the `14-gallery-*` scenes and the `#Preview`s at the
// bottom mount the same lists. iPad captures fit a whole sheet; a phone shows its top.

#if DEBUG
import PunktfunkKit
import SwiftUI

/// One state of a component, with the caption the gallery prints above it.
struct GalleryVariant: Identifiable {
    let name: String
    let make: @MainActor () -> AnyView
    var id: String { name }
}

/// Caption over content for each variant, in as many columns as fit the width.
struct ShotGalleryView: View {
    let title: String
    let variants: [GalleryVariant]
    /// The narrowest column: a host card wants a phone's width, a poster a fraction of it.
    var minWidth: CGFloat = 340

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                Text(title)
                    .font(.geist(24, .bold, relativeTo: .title2))
                LazyVGrid(
                    columns: [GridItem(.adaptive(minimum: minWidth), spacing: 16, alignment: .top)],
                    alignment: .leading, spacing: 20
                ) {
                    ForEach(variants) { variant in
                        VStack(alignment: .leading, spacing: 6) {
                            Text(variant.name)
                                .font(.geist(11, .semibold, relativeTo: .caption2))
                                .tracking(0.8)
                                .textCase(.uppercase)
                                .foregroundStyle(.secondary)
                            variant.make()
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
            .padding()
        }
    }
}

@MainActor
extension ShotMock {
    /// Every state a host card shows, saved and discovered.
    static var hostCardVariants: [GalleryVariant] {
        let battlestation = StoredHost(
            id: battlestationID, name: "Battlestation", address: "192.168.1.20", port: 9777,
            pinnedSHA256: fingerprint, macAddresses: ["a4:b1:c2:d3:e4:f5"], osChain: "windows/11")
        let livingRoom = StoredHost(
            id: livingRoomID, name: "Living Room PC", address: "192.168.1.41", port: 9777,
            pinnedSHA256: hostFingerprint(1), osChain: "linux/fedora/bazzite")
        let office = StoredHost(
            id: officeID, name: "Office NUC", address: "192.168.1.33", port: 9777,
            pinnedSHA256: hostFingerprint(4), presetID: couchPresetID, osChain: "linux/ubuntu")
        let workshop = StoredHost(
            id: workshopID, name: "Workshop", address: "10.0.0.7", port: 9777,
            pinnedSHA256: hostFingerprint(2), macAddresses: ["de:ad:be:ef:00:07"],
            osChain: "linux/arch")
        let bedroom = StoredHost(
            id: bedroomID, name: "Bedroom Mini", address: "192.168.1.77", port: 9777,
            pinnedSHA256: hostFingerprint(6), osChain: "windows/11")
        let unpaired = StoredHost(
            name: "Studio PC", address: "192.168.1.58", port: 9777, osChain: "windows/11")
        let crowded = StoredHost(
            id: editingID, name: "Living Room Gaming PC (behind the TV)",
            address: "fd7a:115c:a1e0::1a2b", port: 9777, pinnedSHA256: hostFingerprint(5),
            presetID: hdrPresetID, osChain: "linux/nobara")
        return [
            GalleryVariant(name: "Online") { card(battlestation) },
            GalleryVariant(name: "Playing") { card(livingRoom, playing: "Hollow Knight") },
            GalleryVariant(name: "Bound preset") { card(office, bound: couchPresetID) },
            GalleryVariant(name: "Pinned preset card") { card(battlestation, pinned: hdrPreset) },
            GalleryVariant(name: "Default host") { card(battlestation, isDefault: true) },
            GalleryVariant(name: "Connecting") { card(battlestation, connecting: true) },
            GalleryVariant(name: "Offline, MAC known") { card(workshop, online: false) },
            GalleryVariant(name: "Offline") { card(bedroom, online: false) },
            GalleryVariant(name: "Not paired") { card(unpaired) },
            GalleryVariant(name: "Everything at once") {
                card(crowded, isDefault: true, bound: hdrPresetID,
                     playing: "Starfall Vale: Definitive Edition")
            },
            GalleryVariant(name: "Discovered") { discovered(pairing: false) },
            GalleryVariant(name: "Discovered, pairing required") { discovered(pairing: true) },
        ]
    }

    /// A poster in each state the touch grid draws.
    static var libraryTileVariants: [GalleryVariant] {
        let byID = Dictionary(
            (games + tileExtras).map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        func tile(
            _ id: String, running: Bool = false, selected: Bool = false, caption: String? = nil
        ) -> AnyView {
            guard let game = byID[id] else { return AnyView(EmptyView()) }
            return AnyView(GameCard(
                game: game, artLoader: ShotPosterArt.source, selected: selected,
                isRunning: running, caption: caption))
        }
        func desktop(_ title: String) -> AnyView {
            AnyView(GameCard(game: LibraryCollation.desktopEntry(title: title), artLoader: nil))
        }
        return [
            GalleryVariant(name: "Steam") { tile("steam:starfall") },
            GalleryVariant(name: "Custom, platform") { tile("custom:aurora") },
            GalleryVariant(name: "Running") { tile("steam:starfall", running: true) },
            GalleryVariant(name: "Keyboard cursor") { tile("heroic:neon", selected: true) },
            GalleryVariant(name: "Launcher") { tile("steam:launcher") },
            GalleryVariant(name: "No art") { tile("steam:prototype") },
            GalleryVariant(name: "Long title") { tile("custom:collection") },
            GalleryVariant(name: "Desktop tile") { desktop("Desktop") },
            GalleryVariant(name: "Resume tile") { desktop("Resume Hollow Knight") },
            GalleryVariant(name: "Caption, recent") { tile("steam:starfall", caption: "2 hr. ago") },
            GalleryVariant(name: "Caption, most played") { tile("custom:aurora", caption: "14 hr") },
            GalleryVariant(name: "Desktops row") {
                AnyView(LibraryDesktopTile(host: host, isOnline: true, nowPlaying: nil, action: {}))
            },
            GalleryVariant(name: "Desktops row, playing") {
                AnyView(LibraryDesktopTile(
                    host: host, isOnline: true, nowPlaying: "Hollow Knight", action: {}))
            },
        ]
    }

    /// The shelf in every phase it can be in before, during and after a fetch.
    static var libraryStateVariants: [GalleryVariant] {
        func shelf(_ phase: ShotLibraryPhase) -> AnyView {
            let frame = RoundedRectangle(cornerRadius: 12, style: .continuous)
            return AnyView(NavigationStack {
                LibraryView(
                    store: galleryStore, target: LibraryTarget(host: host), onLaunch: { _ in },
                    onConnect: {}, shotPhase: phase)
            }
            .frame(height: 440)
            .clipShape(frame)
            .overlay { frame.strokeBorder(.quaternary, lineWidth: 1) })
        }
        return [
            GalleryVariant(name: "Loading") { shelf(.loading) },
            GalleryVariant(name: "Error") {
                shelf(.error("Couldn't load the library — the host didn't answer"))
            },
            GalleryVariant(name: "Empty") { shelf(.empty) },
            GalleryVariant(name: "Remembered, waking") { shelf(.catalog(games, staleness: .waking)) },
            GalleryVariant(name: "Remembered, offline") {
                shelf(.catalog(games, staleness: .offline))
            },
            GalleryVariant(name: "Live, one title up") {
                shelf(.catalog(games, running: ["steam:starfall"]))
            },
            GalleryVariant(name: "No paired host") {
                AnyView(LibraryNoHostView(showHosts: {}).frame(height: 440))
            },
        ]
    }

    /// Two titles the store mock lacks: one with no art at all, one whose name wraps.
    static let tileExtras: [GameEntry] = decodeGames("""
        [
          {"id": "steam:prototype", "store": "steam", "title": "Untitled Prototype", "art": {}},
          {"id": "custom:collection", "store": "custom", "platform": "PS2",
           "title": "The Extraordinarily Long Title of a Remastered Anniversary Collection",
           "art": {"portrait": "shot://art/ember"}}
        ]
        """)

    /// A `/status` entry for `id`. Decoded: `RunningGame`'s memberwise init is PunktfunkKit's.
    static func running(_ id: String, title: String = "Starfall Vale") -> RunningGame? {
        let json = #"{"app_id": "\#(id)", "title": "\#(title)", "state": "running"}"#
        return try? JSONDecoder().decode(RunningGame.self, from: Data(json.utf8))
    }

    static func decodeGames(_ json: String) -> [GameEntry] {
        (try? JSONDecoder().decode([GameEntry].self, from: Data(json.utf8))) ?? []
    }

    /// One store for every gallery shelf; the shelves read it only to address the host.
    private static let galleryStore = hostStore()

    private static func card(
        _ host: StoredHost, online: Bool = true, connecting: Bool = false, isDefault: Bool = false,
        bound: String? = nil, pinned: StreamPreset? = nil, playing: String? = nil
    ) -> AnyView {
        AnyView(HostCardView(
            host: host, isOnline: online, isConnecting: connecting, isDefaultHost: isDefault,
            isBusy: false, actions: stubActions(host, online: online, bound: bound, pinned: pinned),
            pinnedPreset: pinned, nowPlaying: playing))
    }

    /// A card's or a page's acts with nothing behind them, gated the way the grid gates them.
    static func stubActions(
        _ host: StoredHost, online: Bool, bound: String? = nil, pinned: StreamPreset? = nil
    ) -> HostActions {
        let paired = host.pinnedSHA256 != nil
        return HostActions(
            connect: {}, pair: {}, forget: {}, remove: {},
            browseLibrary: paired ? {} : nil, speedTest: paired ? {} : nil,
            sendLogs: paired ? {} : nil,
            wake: pinned == nil && !online && !host.wakeMacs.isEmpty ? {} : nil,
            copyLink: {}, showDetails: pinned == nil ? {} : nil,
            power: pinned == nil && paired && online ? powerGrant : [],
            presets: HostPresetMenu(
                presets: [hdrPreset, couchPreset], boundID: bound ?? host.presetID,
                pinnedIDs: pinned.map { [$0.id] } ?? [], connectWith: { _ in },
                setDefault: { _ in }, togglePin: { _ in }))
    }

    /// What a host with the Host-power grant offers: sleep, then the two that ask first.
    static let powerGrant = [
        HostAction(id: "power.sleep", title: "Sleep"),
        HostAction(id: "power.reboot", title: "Restart", danger: true),
        HostAction(id: "power.shutdown", title: "Shut Down", danger: true),
    ]

    /// The host page in the states it has to hold: a paired host with a game up and power
    /// granted, an asleep one with a known MAC, and one never paired.
    static var hostPageVariants: [GalleryVariant] {
        func page(_ id: UUID) -> AnyView {
            let frame = RoundedRectangle(cornerRadius: 12, style: .continuous)
            return AnyView(NavigationStack { hostPage(id) }
                .frame(height: 760)
                .clipShape(frame)
                .overlay { frame.strokeBorder(.quaternary, lineWidth: 1) })
        }
        return [
            GalleryVariant(name: "Paired, playing, power granted") { page(battlestationID) },
            GalleryVariant(name: "Offline, MAC known") { page(workshopID) },
            GalleryVariant(name: "Not paired") { page(studioID) },
        ]
    }

    /// One host page on the page store, with the grid's gating and no network behind it.
    static func hostPage(_ id: UUID) -> HostDetailView {
        HostDetailView(store: pageStore, hostID: id) { host in
            stubActions(host, online: pageStore.probedOnline.contains(host.id))
        }
    }

    /// The speed test page mid-burst, finished and failed: canned runs, no network.
    static var speedTestVariants: [GalleryVariant] {
        func page(_ phase: SpeedTestPhase, until seconds: Double) -> AnyView {
            AnyView(NavigationStack {
                SpeedTestView(host: host, shotRun: (phase, speedTrace(until: seconds)))
            }
            .frame(height: 620))
        }
        let done = PunktfunkConnection.ProbeResult(
            done: true, recvBytes: 547_500_000, recvPackets: 380_200, hostBytes: 549_150_000,
            hostPackets: 381_300, elapsedMs: 5_000, throughputKbps: 876_000, lossPct: 0.3)
        return [
            GalleryVariant(name: "Measuring") { page(.probing, until: 2.4) },
            GalleryVariant(name: "Done") { page(.done(done), until: 5) },
            GalleryVariant(name: "Couldn't reach the host") {
                page(
                    .failed("Couldn't reach 192.168.1.20. It may be asleep, or streaming to "
                        + "something else."),
                    until: 0)
            },
        ]
    }

    /// A plausible burst: a quick ramp, then around 900 Mbps with one dip, polled every 200 ms.
    static func speedTrace(until seconds: Double) -> PunktfunkConnection.ProbeTrace {
        let rates: [Double] = [
            310, 620, 840, 905, 890, 912, 930, 870, 760, 880, 915, 925, 900, 910, 895, 920,
            905, 890, 912, 908, 900, 915, 898, 910, 905,
        ]
        var trace = PunktfunkConnection.ProbeTrace()
        var bytes: UInt64 = 0
        for (i, mbps) in rates.enumerated() where Double(i + 1) * 0.2 <= seconds + 0.001 {
            bytes += UInt64(mbps * 25_000) // 200 ms at `mbps`
            let ms = UInt32((i + 1) * 200)
            trace.add(.init(
                done: false, recvBytes: bytes, recvPackets: 0, hostBytes: bytes, hostPackets: 0,
                elapsedMs: ms, throughputKbps: UInt32(Double(bytes) * 8 / Double(ms)), lossPct: 0))
        }
        return trace
    }

    static let studioID = UUID(uuidString: "5B0D1E00-0000-4000-8000-000000000007")!

    /// The grid's hosts plus one never paired. Built once: it also tells `NowPlayingStore` that
    /// Battlestation has a game up, and doing that from a view's body would re-render forever.
    static let pageStore: HostStore = {
        let store = hostStore()
        store.hosts.append(StoredHost(
            id: studioID, name: "Studio PC", address: "192.168.1.58", port: 9777,
            osChain: "windows/11"))
        store.debugSetProbedOnline([battlestationID, livingRoomID, officeID, studioID])
        if let battlestation = store.hosts.first(where: { $0.id == battlestationID }) {
            NowPlayingStore.shared.adopt(
                [running("steam:starfall")].compactMap { $0 }, for: battlestation)
        }
        return store
    }()

    private static func discovered(pairing: Bool) -> AnyView {
        let advert = HostDiscovery.debugAdvert(
            id: pairing ? "studio" : "den", name: pairing ? "Studio PC" : "Den Steam Deck",
            host: pairing ? "192.168.1.58" : "192.168.1.90",
            fingerprintHex: hostFingerprint(pairing ? 3 : 7).hexLower,
            requiresPairing: pairing, allowsTofu: !pairing,
            osChain: pairing ? "windows/11" : "linux/steamos")
        return AnyView(DiscoveredCardView(discovered: advert, isBusy: false, onConnect: {}))
    }
}

/// The host page for a paired host with a game up and the power grant.
struct ShotHostPage: View {
    var body: some View {
        NavigationStack { ShotMock.hostPage(ShotMock.battlestationID) }
    }
}

#if os(iOS)
/// The Library tab on the mock catalog, every section filled, inside the real tab bar. The
/// layout and favorites are the scene's own, so a capture never writes the device's.
struct ShotLibrarySections: View {
    @StateObject private var store = ShotMock.hostStore()

    var body: some View {
        TabView(selection: .constant(TouchTab.library)) {
            Color.clear
                .tabItem { Label("Hosts", systemImage: "desktopcomputer") }
                .tag(TouchTab.hosts)
            NavigationStack {
                LibraryView(
                    store: store, target: LibraryTarget(host: ShotMock.host), onLaunch: { _ in },
                    onConnect: {}, inTab: true, onConnectHost: { _ in },
                    shotPhase: .catalog(ShotMock.games, running: ["steam:starfall"]),
                    shotLayout: "", shotFavorites: ["custom:aurora", "gog:ember"])
            }
            .tabItem { Label("Library", systemImage: "square.grid.2x2") }
            .tag(TouchTab.library)
        }
    }
}
#endif

/// The details sheet for a played title that is up on the host, marked a favorite.
struct ShotTitleDetails: View {
    var body: some View {
        TitleDetailSheet(
            game: ShotMock.games.first { $0.id == "steam:starfall" } ?? ShotMock.games[0],
            artLoader: ShotPosterArt.source, playLabel: "Resume", isFavorite: true,
            onPlay: {}, onCopyLink: LinkClipboard.isAvailable ? {} : nil,
            host: ShotMock.pageStore.hosts[0])
    }
}

/// The touch grid on the mock catalog with one title up — what the Library tab grows from.
struct ShotLibraryTouch: View {
    @StateObject private var store = ShotMock.hostStore()

    var body: some View {
        NavigationStack {
            LibraryView(
                store: store, target: LibraryTarget(host: ShotMock.host), onLaunch: { _ in },
                onConnect: {}, shotPhase: .catalog(ShotMock.games, running: ["steam:starfall"]))
        }
    }
}

/// The Library with its host filter over the mock hosts, on the mock catalog, in the app's own
/// frame: the Mac shell's Library row, the iOS Library tab.
struct ShotLibraryFilter: View {
    var body: some View {
        #if os(macOS)
        MacShellView(
            store: ShotMock.pageStore, selection: .constant(.library), hosts: EmptyView(),
            onLaunch: { _, _ in }, onConnectShelf: { _ in }, onConnectHost: { _ in },
            libraryShotPhase: .catalog(ShotMock.games, running: ["steam:starfall"]))
        #elseif os(iOS)
        ShotTouchTabs(selection: .library) { Color.clear } library: { library }
        #else
        library
        #endif
    }

    private var library: some View {
        LibraryTabView(
            store: ShotMock.pageStore, onLaunch: { _, _ in }, onConnectShelf: { _ in },
            onConnectHost: { _ in }, showHosts: {},
            shotPhase: .catalog(ShotMock.games, running: ["steam:starfall"]))
    }
}

#Preview("Host cards") {
    ShotGalleryView(title: "Host cards", variants: ShotMock.hostCardVariants)
        .preferredColorScheme(.dark)
}

#Preview("Host page") {
    ShotGalleryView(title: "Host page", variants: ShotMock.hostPageVariants)
        .preferredColorScheme(.dark)
}

#Preview("Library tiles") {
    ShotGalleryView(title: "Library tiles", variants: ShotMock.libraryTileVariants, minWidth: 150)
        .preferredColorScheme(.dark)
}

#Preview("Library states") {
    ShotGalleryView(title: "Library states", variants: ShotMock.libraryStateVariants)
        .preferredColorScheme(.dark)
}

#if os(iOS)
#Preview("Library tab") {
    ShotLibrarySections()
        .preferredColorScheme(.dark)
}

#Preview("Customize") {
    LibrarySectionsPanel()
        .preferredColorScheme(.dark)
}
#endif

#Preview("Library") {
    ShotLibraryTouch()
        .preferredColorScheme(.dark)
}
#endif
