// App Store screenshot scenes — the actual screens we render, each wired with mock data so it
// looks populated without a live host. Every scene is built from the REAL app views (HomeView,
// SettingsView, PairSheet, TrustCardView) so the screenshots track the shipping UI; only the
// live stream is faked (StreamView needs a real punktfunk/1 connection — see ShotStreamHero).

#if DEBUG
import PunktfunkKit
import SwiftUI

/// One screen to capture: a name (→ file suffix), the canvas orientation, a color scheme, and a
/// factory that builds the populated view on the main actor.
struct ShotScene {
    let name: String
    let orientation: ShotOrientation
    let colorScheme: ColorScheme
    /// macOS: the canvas without window chrome, as the app runs a session full screen. Every
    /// other scene is the app's own titled window.
    var macFullScreen = false
    let make: @MainActor () -> AnyView
}

@MainActor
enum ShotScenes {
    static var all: [ShotScene] {
        var scenes: [ShotScene] = [
            ShotScene(name: "01-stream", orientation: .landscape, colorScheme: .dark,
                      macFullScreen: true) {
                AnyView(ShotStreamHero())
            },
            ShotScene(name: "02-hosts", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotHome())
            },
            ShotScene(name: "03-pair", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotPair())
            },
            ShotScene(name: "04-trust", orientation: .landscape, colorScheme: .dark) {
                AnyView(ShotTrust())
            },
            ShotScene(name: "05-settings", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotSettings())
            },
            // The launch hold, settled: what the player looks at from the tap until the game is
            // actually running.
            ShotScene(name: "13-launch-hold", orientation: .landscape, colorScheme: .dark) {
                AnyView(ShotLaunchHold())
            },
            // The same screen ARRIVING, on a loop: the cover leaves its shelf tile every few
            // seconds so the flight can be watched rather than guessed at from a still.
            ShotScene(name: "13b-launch-hold-flight", orientation: .landscape, colorScheme: .dark) {
                AnyView(ShotLaunchHold(flight: true))
            },
            // Variant galleries: every state of one component on one sheet (ShotGallery.swift).
            ShotScene(name: "14-gallery-host-cards", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotGalleryView(title: "Host cards", variants: ShotMock.hostCardVariants))
            },
            ShotScene(name: "14c-gallery-host-page", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotGalleryView(title: "Host page", variants: ShotMock.hostPageVariants))
            },
            ShotScene(name: "14b-gallery-library-tiles", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotGalleryView(
                    title: "Library tiles", variants: ShotMock.libraryTileVariants, minWidth: 150))
            },
            ShotScene(name: "14d-gallery-library-states", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotGalleryView(
                    title: "Library states", variants: ShotMock.libraryStateVariants))
            },
            ShotScene(name: "14e-gallery-speed-test", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotGalleryView(title: "Speed test", variants: ShotMock.speedTestVariants))
            },
            ShotScene(name: "15-library-touch", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotLibraryTouch())
            },
            ShotScene(name: "16-host-page", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotHostPage())
            },
        ]
        #if os(iOS) || os(macOS)
        scenes += [
            // The Library tab with its host filter (iOS) and the Mac's Library row. Landscape like
            // the store set's other shelves.
            ShotScene(name: "15f-library-filter", orientation: .landscape, colorScheme: .dark) {
                AnyView(ShotLibraryFilter())
            },
            // The host page as sections beside a sidebar: the iPad's sheet, the Mac's window.
            ShotScene(name: "16f-host-sections", orientation: .natural, colorScheme: .dark) {
                AnyView(HostSectionsView(
                    hostID: ShotMock.battlestationID, store: ShotMock.pageStore, handOff: { _ in }))
            },
            // The connect overlay's Liquid Glass modal over the touch grid, in each phase.
            ShotScene(name: "09d-connecting-modal", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotConnect(kind: .connecting))
            },
            ShotScene(name: "09e-waking-modal", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotConnect(kind: .waking))
            },
            ShotScene(name: "09f-wake-timed-out-modal", orientation: .natural, colorScheme: .dark) {
                AnyView(ShotConnect(kind: .timedOut))
            },
            // FEEL THE GAME — the controller test panel with injected pads. Gated with the
            // console block because ControllerTestView doesn't build on tvOS, not because it
            // is a console screen. Landscape like the rest of the store set: the app is built
            // for horizontal use, so the two pads sit as side-by-side columns (see the scene).
            ShotScene(name: "12-controllers", orientation: .landscape, colorScheme: .dark) {
                AnyView(ShotControllers())
            },
            // The gamepad UI: the console the app swaps in when a controller is connected.
            ShotScene(name: "06-console", orientation: .landscape, colorScheme: .dark) {
                AnyView(ShotConsole())
            },
        ]
        #endif
        #if os(macOS)
        // The Mac's host window, as a card's ⓘ opens it, some of its sections, and the Library's
        // Customize popover.
        scenes += [
            ShotScene(name: "16b-host-window", orientation: .natural, colorScheme: .dark) {
                AnyView(MacHostWindow(hostID: ShotMock.battlestationID, store: ShotMock.pageStore))
            },
            ShotScene(name: "16e-host-window-presets", orientation: .natural, colorScheme: .dark) {
                AnyView(MacHostWindow(
                    hostID: ShotMock.battlestationID, store: ShotMock.pageStore, section: .presets))
            },
            ShotScene(name: "15e-customize-mac", orientation: .natural, colorScheme: .dark) {
                // On material, as the popover draws it: a list fill shows up as a dark slab.
                AnyView(LibrarySectionsPanel(shotLayout: "").frame(width: 320, height: 250)
                    .background(.regularMaterial, in: .rect(cornerRadius: 12)))
            },
            ShotScene(name: "16c-host-window-connection", orientation: .natural, colorScheme: .dark) {
                AnyView(MacHostWindow(
                    hostID: ShotMock.battlestationID, store: ShotMock.pageStore,
                    section: .connection))
            },
            ShotScene(name: "16d-host-window-speed-test", orientation: .natural, colorScheme: .dark) {
                AnyView(MacHostWindow(
                    hostID: ShotMock.battlestationID, store: ShotMock.pageStore,
                    section: .speedTest))
            },
        ]
        #endif
        #if os(iOS)
        // The Library tab with every section filled: Desktops, Recently Played, Favorites,
        // Launchers and Games.
        scenes.append(ShotScene(name: "15b-library-sections", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotLibrarySections())
        })
        scenes.append(ShotScene(name: "15c-title-details", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTitleDetails())
        })
        // One section switched off and one moved, as Customize shows them.
        scenes.append(ShotScene(name: "15d-library-customize", orientation: .natural, colorScheme: .dark) {
            AnyView(LibrarySectionsPanel(shotLayout: "desktops,favorites,recent,-launchers,games"))
        })
        #endif
        #if os(tvOS)
        // The TV's tab bar: the hosts, and the Library tab on the mock catalog.
        scenes.append(ShotScene(name: "20-tv-hosts-tab", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .hosts))
        })
        scenes.append(ShotScene(name: "20b-tv-library-tab", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .library))
        })
        // The settings sidebar on General and on Display.
        scenes.append(ShotScene(name: "21-tv-settings-general", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .settings))
        })
        scenes.append(ShotScene(name: "21b-tv-settings-display", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .settings, category: .display))
        })
        // A focused row with its caption, About with the app's icon, and Audio's footer.
        scenes.append(ShotScene(name: "21c-tv-settings-row-focus", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVSettingsRowFocus())
        })
        scenes.append(ShotScene(name: "21d-tv-settings-about", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .settings, category: .about))
        })
        scenes.append(ShotScene(name: "21e-tv-settings-audio", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .settings, category: .audio))
        })
        // Presets on the TV: the Editing pane, and Display edited in the "4K HDR" preset.
        scenes.append(ShotScene(name: "22-tv-presets", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .settings, scope: .preset(ShotMock.hdrPresetID), editing: true))
        })
        scenes.append(ShotScene(name: "22b-tv-preset-scope", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(
                tab: .settings, category: .display, scope: .preset(ShotMock.hdrPresetID)))
        })
        // The dial's editor: the TV default ring beside its shortcuts.
        scenes.append(ShotScene(name: "23-tv-quick-actions", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .settings, category: .quickActions))
        })
        // Back from a slot's list: the tab bar comes back with the pane.
        scenes.append(ShotScene(name: "23b-tv-quick-actions-back", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVTabs(tab: .settings, category: .quickActions, pushPop: true))
        })
        // The host page as the TV pushes it, its speed test waiting for Start, its Connection fields.
        scenes.append(ShotScene(name: "24-tv-host-page", orientation: .natural, colorScheme: .dark) {
            AnyView(NavigationStack {
                HostSectionsView(
                    hostID: ShotMock.battlestationID, store: ShotMock.pageStore, handOff: { _ in })
            })
        })
        scenes.append(ShotScene(name: "24b-tv-host-speed-test", orientation: .natural, colorScheme: .dark) {
            AnyView(NavigationStack {
                HostSectionsView(
                    hostID: ShotMock.battlestationID, store: ShotMock.pageStore, section: .speedTest,
                    handOff: { _ in })
            })
        })
        scenes.append(ShotScene(name: "24c-tv-host-connection", orientation: .natural, colorScheme: .dark) {
            AnyView(NavigationStack {
                HostSectionsView(
                    hostID: ShotMock.battlestationID, store: ShotMock.pageStore, section: .connection,
                    handOff: { _ in })
            })
        })
        // The Library's details: a title's sheet, and Customize with one section off.
        scenes.append(ShotScene(name: "25-tv-title-details", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTitleDetails())
        })
        scenes.append(ShotScene(name: "25b-tv-library-customize", orientation: .natural, colorScheme: .dark) {
            AnyView(LibrarySectionsPanel(shotLayout: "desktops,favorites,recent,-launchers,games"))
        })
        // Cover cards, the first focused, and the tab's games grid first, for its gaps.
        scenes.append(ShotScene(name: "25c-tv-cards-focus", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVCards())
        })
        scenes.append(ShotScene(name: "25d-tv-library-grid", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVLibraryGrid())
        })
        // Customize as the Library presents it, and under a pinned scheme, for the focused row.
        scenes.append(ShotScene(name: "25e-tv-customize-sheet", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVCustomizeSheet())
        })
        scenes.append(ShotScene(name: "25f-tv-customize-pinned", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVCustomizePinned())
        })
        scenes.append(ShotScene(name: "25g-tv-customize-moved", orientation: .natural, colorScheme: .dark) {
            AnyView(ShotTVCustomizeMoved())
        })
        #endif
        return scenes
    }
}

// MARK: - Mock data

@MainActor
enum ShotMock {
    // Stable ids so the store, the adverts and the preset bindings all point at the same things
    // across every scene and every run.
    static let battlestationID = UUID(uuidString: "5B0D1E00-0000-4000-8000-000000000001")!
    static let livingRoomID = UUID(uuidString: "5B0D1E00-0000-4000-8000-000000000002")!
    static let workshopID = UUID(uuidString: "5B0D1E00-0000-4000-8000-000000000003")!
    static let officeID = UUID(uuidString: "5B0D1E00-0000-4000-8000-000000000004")!
    static let editingID = UUID(uuidString: "5B0D1E00-0000-4000-8000-000000000005")!
    static let bedroomID = UUID(uuidString: "5B0D1E00-0000-4000-8000-000000000006")!

    static let hdrPresetID = "a71c4e0d9f22"
    static let couchPresetID = "3e88b107c4da"
    /// Overrides a few Display rows, so a scene editing it shows the marks.
    static let hdrPreset: StreamPreset = {
        var preset = StreamPreset(name: "4K HDR", id: hdrPresetID, accent: "#8B7BF7")
        preset.overrides.width = 3840
        preset.overrides.height = 2160
        preset.overrides.hdrEnabled = true
        return preset
    }()
    static let couchPreset = StreamPreset(
        name: "Couch 1080p", id: couchPresetID, accent: "#4FD1A5")

    /// The catalog the host cards read their chips and pinned cards from. Seeded once, on the
    /// first store build — `PresetStore` is a singleton, and in shot mode its write-back is
    /// suppressed, so this never reaches a real user's catalog.
    static func installPresets() {
        guard !presetsInstalled else { return }
        presetsInstalled = true
        PresetStore.shared.debugSet([hdrPreset, couchPreset])
    }

    private static var presetsInstalled = false

    /// A populated saved-host grid: the most-recent host bound to a preset (its chip), a second
    /// paired machine, and one asleep box we hold a MAC for (so its card offers Wake-on-LAN). OS
    /// chains give every tile its real vendor mark instead of a letter monogram.
    ///
    /// No PINNED host+preset card: it renders a second tile for the SAME host, which is the
    /// feature working as designed but reads as a duplicate to anyone meeting the app in a store
    /// listing. The binding chip carries the preset story on its own.
    static func hostStore() -> HostStore {
        installPresets()
        let store = HostStore()
        store.hosts = [
            StoredHost(
                id: battlestationID, name: "Battlestation", address: "192.168.1.20", port: 9777,
                pinnedSHA256: fingerprint, lastConnected: Date().addingTimeInterval(-420),
                macAddresses: ["a4:b1:c2:d3:e4:f5"], presetID: hdrPresetID,
                osChain: "windows/11"),
            StoredHost(
                id: livingRoomID, name: "Living Room PC", address: "192.168.1.41", port: 9777,
                pinnedSHA256: hostFingerprint(1), lastConnected: Date().addingTimeInterval(-86_400),
                macAddresses: ["b8:27:eb:11:22:33"], osChain: "linux/fedora/bazzite"),
            StoredHost(
                id: officeID, name: "Office NUC", address: "192.168.1.33", port: 9777,
                pinnedSHA256: hostFingerprint(4), lastConnected: Date().addingTimeInterval(-259_200),
                presetID: couchPresetID, osChain: "linux/ubuntu"),
            StoredHost(
                id: workshopID, name: "Workshop", address: "10.0.0.7", port: 9777,
                pinnedSHA256: hostFingerprint(2), macAddresses: ["de:ad:be:ef:00:07"],
                osChain: "linux/arch"),
            StoredHost(
                id: editingID, name: "Editing Rig", address: "192.168.1.62", port: 9777,
                pinnedSHA256: hostFingerprint(5), lastConnected: Date().addingTimeInterval(-604_800),
                osChain: "linux/nobara"),
            StoredHost(
                id: bedroomID, name: "Bedroom Mini", address: "192.168.1.77", port: 9777,
                pinnedSHA256: hostFingerprint(6), macAddresses: ["00:1a:2b:3c:4d:5e"],
                osChain: "windows/11"),
        ]
        // Which cards read ONLINE. Seeded, because presence is a live probe and a capture has no
        // network — the same three hosts `discovery()` advertises, so both halves agree.
        store.debugSetProbedOnline([battlestationID, livingRoomID, officeID])
        return store
    }

    /// Discovery, seeded rather than live. Three saved hosts advertise (matching the seeded
    /// reachable set above), "Workshop" stays quiet so the grid shows an asleep machine, and one
    /// genuinely new host populates the "On this network" section.
    ///
    /// A live browse made the shot non-deterministic AND leaked whatever was on the capturing
    /// machine's LAN into the App Store listing.
    static func discovery() -> HostDiscovery {
        let discovery = HostDiscovery()
        discovery.debugSet([
            HostDiscovery.debugAdvert(
                id: "battlestation", name: "Battlestation", host: "192.168.1.20",
                fingerprintHex: fingerprint.hexLower, macAddresses: ["a4:b1:c2:d3:e4:f5"],
                osChain: "windows/11"),
            HostDiscovery.debugAdvert(
                id: "living-room", name: "Living Room PC", host: "192.168.1.41",
                fingerprintHex: hostFingerprint(1).hexLower, macAddresses: ["b8:27:eb:11:22:33"],
                osChain: "linux/fedora/bazzite"),
            HostDiscovery.debugAdvert(
                id: "office-nuc", name: "Office NUC", host: "192.168.1.33",
                fingerprintHex: hostFingerprint(4).hexLower, osChain: "linux/ubuntu"),
            HostDiscovery.debugAdvert(
                id: "studio", name: "Studio PC", host: "192.168.1.58",
                fingerprintHex: hostFingerprint(3).hexLower, requiresPairing: true, allowsTofu: false,
                osChain: "windows/11"),
        ])
        return discovery
    }

    static let host = StoredHost(
        id: battlestationID, name: "Battlestation", address: "192.168.1.20", port: 9777,
        pinnedSHA256: fingerprint, osChain: "windows/11")

    /// What the pairing sheet calls THIS device. Taken from the platform, not from
    /// `UIDevice.current.name` — on a capture simulator that is the harness's own throwaway name
    /// (`pf-shot-iphone-6.9` went out on the store listing that way).
    static var clientDeviceName: String {
        #if os(tvOS)
        "Apple TV"
        #elseif os(macOS)
        "MacBook Pro"
        #else
        UIDevice.current.userInterfaceIdiom == .pad ? "iPad Pro" : "iPhone"
        #endif
    }

    /// A believable shelf for the library: the demo host's titles (`DemoMode.games`)
    /// plus the Steam launcher, which stays artless by design and renders its brand mark.
    static let games: [GameEntry] = DemoMode.games + {
        let json = """
        [{"id": "steam:launcher", "store": "steam", "title": "Steam", "art": {},
          "role": "launcher", "icon": "steam"}]
        """
        return (try? JSONDecoder().decode([GameEntry].self, from: Data(json.utf8))) ?? []
    }()

    /// A plausible-looking 32-byte SHA-256 for the trust card / pin lock glyphs.
    static let fingerprint = hostFingerprint(0)

    /// Distinct per host — `StoredHost.matches` prefers a fingerprint comparison, so sharing one
    /// across the mock grid made a single advert light up every card.
    static func hostFingerprint(_ seed: Int) -> Data {
        Data((0..<32).map { UInt8((($0 &* 37) &+ 0x1d &+ (seed &* 91)) & 0xff) })
    }
}

// MARK: - Home

private struct ShotHome: View {
    @StateObject private var store = ShotMock.hostStore()
    @StateObject private var model = SessionModel()
    @StateObject private var discovery = ShotMock.discovery()

    var body: some View {
        #if os(macOS)
        // The window as ContentView builds it: the sidebar shell around the host grid.
        MacShellView(
            store: store, selection: .constant(.hosts),
            hosts: HomeView(
                store: store, model: model, discovery: discovery,
                showAddHost: .constant(false), pairingTarget: .constant(nil),
                speedTestTarget: .constant(nil), libraryTarget: .constant(nil),
                connect: { _, _ in }, connectDiscovered: { _ in },
                onPaired: { _, _ in }, onLaunchTitle: { _, _ in }, onConnectShelf: { _ in },
                wake: { _ in }),
            onLaunch: { _, _ in }, onConnectShelf: { _ in }, onConnectHost: { _ in })
        #elseif os(iOS)
        ShotTouchTabs(selection: .hosts) { home } library: { Color.clear }
        #else
        home
        #endif
    }

    #if !os(macOS)
    private var home: some View {
        HomeView(
            store: store, model: model, discovery: discovery,
            showAddHost: .constant(false), pairingTarget: .constant(nil),
            speedTestTarget: .constant(nil), libraryTarget: .constant(nil),
            showSettings: .constant(false),
            connect: { _, _ in }, connectDiscovered: { _ in },
            onPaired: { _, _ in }, onLaunchTitle: { _, _ in }, onConnectShelf: { _ in },
            wake: { _ in })
    }
    #endif
}

#if os(iOS)
/// Hosts and Library as ContentView's tabs: a tab bar on iPhone, a sidebar-able bar on iPad.
struct ShotTouchTabs<Hosts: View, Library: View>: View {
    let selection: TouchTab
    @ViewBuilder let hosts: Hosts
    @ViewBuilder let library: Library

    var body: some View {
        if #available(iOS 18, *) {
            TabView(selection: .constant(selection)) {
                Tab("Hosts", systemImage: "desktopcomputer", value: TouchTab.hosts) { hosts }
                Tab("Library", systemImage: "square.grid.2x2", value: TouchTab.library) { library }
            }
            .tabViewStyle(.sidebarAdaptable)
        } else {
            TabView(selection: .constant(selection)) {
                hosts
                    .tabItem { Label("Hosts", systemImage: "desktopcomputer") }
                    .tag(TouchTab.hosts)
                library
                    .tabItem { Label("Library", systemImage: "square.grid.2x2") }
                    .tag(TouchTab.library)
            }
        }
    }
}
#endif

#if os(tvOS)
/// The TV's tab bar as the app draws it, over the mock hosts and catalog.
private struct ShotTVTabs: View {
    let tab: TouchTab
    var category: SettingsCategory = .general
    var scope: SettingsScope = .defaults
    var editing = false
    var pushPop = false

    var body: some View {
        TabView(selection: .constant(tab)) {
            ShotHome()
                .tabItem { TouchTab.hostsLabel }
                .tag(TouchTab.hosts)
            ShotLibraryFilter()
                .tabItem { Label("Library", systemImage: "square.grid.2x2") }
                .tag(TouchTab.library)
            NavigationStack { settings }
                .tabItem { Label("Settings", systemImage: "gearshape") }
                .tag(TouchTab.settings)
        }
        .onAppear { ShotMock.installPresets() }
    }

    private var settings: SettingsView {
        var view = SettingsView(initialCategory: category, initialScope: scope, startsOnEditing: editing)
        view.shotPushPop = pushPop
        return view
    }
}

/// Settings with focus on the pane's first row, for its lift and caption. No tab bar: a TV's
/// focus starts on the tab bar.
private struct ShotTVSettingsRowFocus: View {
    var body: some View {
        NavigationStack { settings }
            .onAppear { ShotMock.installPresets() }
    }

    private var settings: SettingsView {
        var view = SettingsView()
        view.shotFocusesPane = true
        return view
    }
}

/// Cover cards with the first focused, as the Library draws them. No tab bar, which takes focus
/// first.
private struct ShotTVCards: View {
    var body: some View {
        HStack(alignment: .top, spacing: 40) {
            ForEach(Array(ShotMock.games.prefix(4))) { game in
                Button {} label: {
                    GameCard(
                        game: game, artLoader: ShotPosterArt.source, caption: "2 hr ago",
                        host: ShotMock.pageStore.hosts[0])
                }
                .buttonStyle(TVCardButtonStyle())
                .frame(width: 220)
            }
        }
    }
}

/// The Library tab with its games grid first, for the grid's gaps.
private struct ShotTVLibraryGrid: View {
    var body: some View {
        NavigationStack {
            LibraryView(
                store: ShotMock.pageStore, target: LibraryTarget(host: ShotMock.pageStore.hosts[0]),
                onLaunch: { _ in }, inTab: true,
                shotPhase: .catalog(ShotMock.games, running: []),
                shotLayout: "games,desktops,favorites,recent,launchers")
        }
    }
}

/// The Library opening its Customize sheet, focus on the sheet's first row.
private struct ShotTVCustomizeSheet: View {
    var body: some View {
        NavigationStack {
            LibraryView(
                store: ShotMock.pageStore, target: LibraryTarget(host: ShotMock.pageStore.hosts[0]),
                onLaunch: { _ in }, inTab: true,
                shotPhase: .catalog(ShotMock.games, running: []), shotCustomize: true)
        }
    }
}

/// The Customize panel in a sheet under a pinned dark scheme, as the palette ink pins it, focus
/// moved to its second row.
private struct ShotTVCustomizePinned: View {
    var body: some View {
        Color.clear
            .sheet(isPresented: .constant(true)) { LibrarySectionsPanel(shotMovesFocus: true) }
            .environment(\.colorScheme, .dark)
    }
}

/// The same with no scheme pinned.
private struct ShotTVCustomizeMoved: View {
    var body: some View {
        Color.clear
            .sheet(isPresented: .constant(true)) { LibrarySectionsPanel(shotMovesFocus: true) }
    }
}
#endif

// MARK: - Library

// MARK: - Launch hold

/// The launch hold over the real shelf.
///
/// The shelf underneath is the touch grid, not a backdrop image, which is what makes the flight
/// real: its posters publish their rects to `TileFrames` exactly as they do in the app, so the
/// cover here leaves the tile it is drawn in rather than a rect this scene made up. `flight`
/// replays the arrival every few seconds — the still frame cannot show it.
private struct ShotLaunchHold: View {
    var flight = false
    /// Which mock title launches. Its tile has to be on screen for a rect to exist.
    private let launched = ShotMock.games.first { $0.id == "steam:starfall" } ?? ShotMock.games[0]

    @State private var showing = false
    /// Bumped per replay, for the same reason the app counts launches: without a fresh
    /// identity the second hold inherits the first one's state and never flies.
    @State private var cycle = 0

    var body: some View {
        ZStack {
            ShotLibraryTouch()
            if showing {
                LaunchHoldView(
                    entry: launched, host: nil, connecting: true,
                    sourceRect: TileFrames.rect(launched.id),
                    artOverride: ShotPosterArt.source,
                    onShow: {})
                    .id(cycle)
                    .transition(.asymmetric(insertion: .identity, removal: .opacity))
            }
        }
        .animation(.easeInOut(duration: 0.3), value: showing)
        .task {
            guard flight else {
                // The still: the hold is the whole frame, so there is nothing to wait for.
                showing = true
                return
            }
            // Let the shelf's own entrance finish first, so the tile the cover flies out of is
            // where it will finally sit and the only thing moving after that is the cover.
            try? await Task.sleep(nanoseconds: 2_600_000_000)
            showing = true
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 2_600_000_000)
                showing = false
                try? await Task.sleep(nanoseconds: 900_000_000)
                cycle += 1
                showing = true
            }
        }
    }
}

#if os(iOS) || os(macOS)
/// The connect overlay (the real `ConnectOverlay`) in each phase — instant "Connecting…"
/// feedback, the "Waking…" wait, and the wake-timed-out prompt — as the touch UI's Liquid Glass
/// modal over the host grid.
private struct ShotConnect: View {
    enum Kind { case connecting, waking, timedOut }
    let kind: Kind

    @StateObject private var store = ShotMock.hostStore()
    @StateObject private var waker = HostWaker()

    var body: some View {
        ShotHome()
            .overlay {
                ConnectOverlay(
                    connectingHostName: kind == .connecting ? "Battlestation" : nil,
                    waker: waker,
                    onCancelConnect: {})
            }
            .onAppear {
                switch kind {
                case .connecting:
                    break
                case .waking:
                    waker.debugSet(.init(
                        hostID: store.hosts.first?.id ?? UUID(),
                        hostName: "Battlestation", connectsAfter: true, seconds: 14))
                case .timedOut:
                    waker.debugSet(.init(
                        hostID: store.hosts.first?.id ?? UUID(),
                        hostName: "Battlestation", connectsAfter: true, seconds: 90, timedOut: true))
                }
            }
    }

}

// MARK: - Console

/// The console over the mock hosts, as ContentView mounts it for a connected controller.
private struct ShotConsole: View {
    @StateObject private var store = ShotMock.hostStore()
    @StateObject private var model = SessionModel()
    @StateObject private var discovery = ShotMock.discovery()
    @StateObject private var waker = HostWaker()
    /// Mounted once the scene has turned: a console built before the rotation kept its
    /// portrait width.
    @State private var mounted = false

    var body: some View {
        ZStack {
            if mounted {
                ConsoleHomeView(
                    store: store, model: model, discovery: discovery, waker: waker,
                    entry: .constant(nil), notice: .constant(nil), pairing: .constant(nil),
                    linkConfirm: .constant(nil), runLink: { _ in }, onFailed: {},
                    onPaired: { _, _ in }, connect: { _, _ in }, connectDiscovered: { _ in },
                    requestAccess: { _ in }, requestAccessDiscovered: { _ in },
                    launchTitle: { _, _ in }, connectShelf: { _ in }, wakeOnly: { _ in })
            }
        }
        .task {
            try? await Task.sleep(nanoseconds: 1_200_000_000)
            mounted = true
        }
    }
}

// MARK: - Controllers (the pads the store listing names)

/// The FEEL THE GAME frame: the controller test panel rendering the two pads the listing talks
/// about. A GCController cannot be constructed, so the panel draws injected `ShotPad`s — the
/// DualSense leads with the feedback surface (adaptive-trigger effects, rumble backend, lightbar
/// + player LEDs), the Xbox pad carries the input readout, frozen mid-game.
private struct ShotControllers: View {
    var body: some View {
        #if os(macOS)
        // The app presents the panel as a sheet on Settings → Controllers, at its minimum size.
        ShotMacSettingsWindow(
            tab: "Controllers",
            sheet: AnyView(ControllerTestView(shotPads: Self.pads).frame(width: 420, height: 540)))
        #else
        // Landscape canvas: one column per pad, so neither story is cut by the short height —
        // the DualSense feedback surface left, the Xbox live-input readout right.
        HStack(spacing: 0) {
            ControllerTestView(shotPads: [Self.pads[0]])
            ControllerTestView(shotPads: [Self.pads[1]])
        }
        #endif
    }

    /// Transport/battery/player ride in `detail` — the panel has no dedicated battery row.
    /// Each pad shows a different half of the panel: the DualSense skips the input card (the
    /// effect grid is the marketing point), the Xbox pad skips rumble and shows the readout.
    static let pads: [ControllerTestView.ShotPad] = [
        .init(
            name: "DualSense Wireless Controller",
            detail: "Bluetooth · 85% · Player 1",
            isDualSense: true, hasAdaptiveTriggers: true, hasLight: true,
            rumbleBackend: "DualSense HID · Bluetooth"),
        .init(
            name: "Xbox Wireless Controller",
            detail: "Bluetooth · 60% · Player 2",
            isDualSense: false, hasAdaptiveTriggers: false, hasLight: false,
            input: .init(
                leftStick: .init(x: -0.31, y: 0.54),
                rightStick: .init(x: 0.72, y: -0.16),
                leftTrigger: 0.08, rightTrigger: 0.62,
                buttons: [
                    ("A", true), ("B", false), ("X", false), ("Y", false),
                    ("LB", false), ("RB", true), ("L3", false), ("R3", false),
                    ("Menu", false), ("Opts", false),
                    ("↑", false), ("↓", false), ("←", false), ("→", false),
                ])),
    ]
}
#endif

// MARK: - Settings

private struct ShotSettings: View {
    var body: some View {
        #if os(macOS)
        ShotMacSettingsWindow()
        #elseif os(iOS)
        // SettingsView owns its NavigationSplitView (sidebar + detail) and Done button, so it is
        // rendered directly — a wrapping NavigationStack would nest a split view in a stack. Open
        // on Display rather than the bare category list: resolution, frame rate, bitrate, HDR and
        // codec are what someone reads a streaming app's settings shot to find out.
        SettingsView(initialCategory: .display)
        #else
        NavigationStack { SettingsView() }
        #endif
    }
}

#if os(macOS)
/// The host grid with the app's real Settings window in front, as ⌘, opens it: the toolbar tabs
/// only exist in the Settings scene. `tab` picks a toolbar tab by its label; `sheet` rides on the
/// Settings window the way Test Controller does.
private struct ShotMacSettingsWindow: View {
    var tab: String?
    var sheet: AnyView?
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        ShotHome().task {
            // Once the capture window has settled on the canvas.
            try? await Task.sleep(nanoseconds: 2_500_000_000)
            openSettings()
            try? await Task.sleep(nanoseconds: 500_000_000)
            arrange()
        }
    }

    private func arrange() {
        let windows = NSApp.windows
        let canvas = ShotDevice.mac.points(.natural)
        guard let main = windows.first(where: { $0.frame.size == canvas }),
              let settings = windows.first(where: {
                  $0.identifier?.rawValue.contains("Settings") == true
              })
        else { return }
        settings.setFrameOrigin(NSPoint(
            x: main.frame.midX - settings.frame.width / 2,
            y: main.frame.midY - settings.frame.height / 2))
        if let tab, let item = settings.toolbar?.items.first(where: { $0.label == tab }),
           let action = item.action {
            NSApp.sendAction(action, to: item.target, from: item)
        }
        // An inactive app draws grey controls.
        NSApp.activate(ignoringOtherApps: true)
        settings.makeKeyAndOrderFront(nil)
        if let sheet {
            settings.beginSheet(NSWindow(contentViewController: NSHostingController(
                rootView: sheet.tint(.brand))))
        }
    }
}
#endif

// MARK: - Pair (PIN ceremony)

private struct ShotPair: View {
    /// The PIN as the host's web console shows it, and a device name that doesn't depend on what
    /// the capture simulator happens to be called.
    private var sheet: some View {
        PairSheet(
            host: ShotMock.host, shotPIN: "418 306",
            shotClientName: ShotMock.clientDeviceName, onPaired: { _ in })
    }

    var body: some View {
        #if os(iOS)
        // PRESENT it, don't rebuild it. `PairSheet` is a bottom sheet on iOS — it carries its own
        // `.presentationDetents([.medium, .large])` and the system's Liquid Glass background, both
        // of which only exist inside a real `.sheet`. Composed into a ZStack instead (what this
        // scene used to do), the detents were inert, the grouped Form stretched to the full height
        // of the screen, and the capture was a thin strip of content over a huge black void.
        ShotHome()
            .sheet(isPresented: .constant(true)) {
                // Pinned to one detent. The sheet ships `[.medium, .large]` so it can grow over
                // the keyboard, and the resting height leaves a wide empty band between the form
                // and the button row; a capture wants the snug version.
                sheet.presentationDetents([.fraction(0.52)])
            }
        #elseif os(tvOS)
        // tvOS pushes the ceremony as a full screen (HomeView's `navigationDestination`).
        NavigationStack { sheet }
        #else
        // macOS: the window-modal sheet the app presents. It is a window of its own, which the
        // driver's capture of the window's rect takes in.
        ShotHome().sheet(isPresented: .constant(true)) { sheet }
        #endif
    }
}

// MARK: - Trust (TOFU card over the blurred live stream)

private struct ShotTrust: View {
    var body: some View {
        ZStack {
            ShotDesktopFrame()
                .blur(radius: 32)
                .overlay(Color.black.opacity(0.45))
            TrustCardView(
                fingerprint: ShotMock.fingerprint, hostName: "Battlestation",
                onCancel: {}, onTrust: {}, onPairInstead: {})
        }
    }
}

// MARK: - Stream hero

/// The marketing hero: a stand-in streamed frame with the real glass HUD chip on top.
/// StreamView can't render here (it needs a live punktfunk/1 connection), so the frame is
/// synthetic — set `PUNKTFUNK_SHOT_HERO=/path/to/frame.png` to drop in a real captured frame.
/// The frame fills the display; the HUD stays inside the safe area. The status bar and home
/// indicator hide, as they do for a live session.
private struct ShotStreamHero: View {
    #if os(macOS)
    /// The App Store canvas's scale: a 1× monitor still shoots the 2× Mac.
    private let scale = ShotDevice.mac.scale
    #else
    @Environment(\.displayScale) private var scale
    #endif

    var body: some View {
        GeometryReader { geo in
            // The whole display in pixels: the safe-area frame plus its insets, at backing scale.
            let insets = geo.safeAreaInsets
            ShotHUD(
                width: Int(((geo.size.width + insets.leading + insets.trailing) * scale).rounded()),
                height: Int(((geo.size.height + insets.top + insets.bottom) * scale).rounded()))
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topTrailing)
        }
        .background { ShotDesktopFrame() }
        .background(Color.black.ignoresSafeArea())
        #if os(iOS)
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        #endif
    }
}

/// A faithful copy of StreamHUDView's overlay (which needs a live PunktfunkConnection for the
/// mode line), reusing the app's real `.glassBackground`. The mode line is the capturing
/// display's own pixel size and max refresh rate.
private struct ShotHUD: View {
    let width: Int
    let height: Int

    /// 812.4 Mb/s at 5120×1440@240: the bitrate scales with pixel rate at this density.
    private static let bitsPerPixelFrame = 0.459

    /// The display's max refresh rate. `PUNKTFUNK_SHOT_FPS` overrides it: a Simulator reports
    /// 60 even for a 120 Hz device.
    private var fps: Int {
        if let fps = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_FPS"].flatMap(Int.init) {
            return fps
        }
        #if os(macOS)
        return NSScreen.main?.maximumFramesPerSecond ?? 60
        #else
        return UIApplication.shared.connectedScenes
            .compactMap { ($0 as? UIWindowScene)?.screen.maximumFramesPerSecond }.first ?? 60
        #endif
    }

    private var modeLine: String {
        let mbps = Double(width * height * fps) * Self.bitsPerPixelFrame / 1_000_000
        return "\(width)×\(height)@\(fps)  \(fps) fps  " + String(format: "%.1f Mb/s", mbps)
    }

    var body: some View {
        VStack(alignment: .trailing, spacing: 4) {
            HStack(spacing: 6) {
                Circle().fill(Color.accentColor).frame(width: 7, height: 7)
                Text(modeLine)
                    .font(.system(.caption, design: .monospaced))
            }
            Text("end-to-end 2.9 ms p50 · 3.8 p95 · capture→on-glass")
                .font(.system(.caption2, design: .monospaced))
                .foregroundStyle(.secondary)
            Text("= host+network 1.3 + decode 0.7 + display 0.9")
                .font(.system(.caption2, design: .monospaced))
                .foregroundStyle(.secondary)
            #if os(macOS)
            Text("⌘⎋ releases the mouse")
                .font(.geist(11, relativeTo: .caption2)).foregroundStyle(.secondary)
            #elseif os(tvOS)
            Text("Press Menu to disconnect")
                .font(.geist(12, relativeTo: .caption)).foregroundStyle(.secondary)
            #endif
        }
        .padding(10)
        .glassBackground(RoundedRectangle(cornerRadius: 10))
        .padding(10)
    }
}

/// A synthetic "streamed frame" — a synthwave scene that reads as game content without shipping
/// any real art. Replaced wholesale when `PUNKTFUNK_SHOT_HERO` points at a real PNG. Fills the
/// whole display, safe area included, like the real stream.
private struct ShotDesktopFrame: View {
    var body: some View {
        Group {
            if let image = Self.overrideImage {
                // Fill without growing the layout: the clear view sets the size, the image crops.
                Color.clear.overlay { image.resizable().scaledToFill() }.clipped()
            } else {
                synthetic
            }
        }
        .ignoresSafeArea()
    }

    private var synthetic: some View {
        ZStack {
            LinearGradient(
                colors: [
                    Color(red: 0.05, green: 0.02, blue: 0.16),
                    Color(red: 0.35, green: 0.05, blue: 0.42),
                    Color(red: 0.95, green: 0.30, blue: 0.35),
                    Color(red: 0.99, green: 0.62, blue: 0.32),
                ],
                startPoint: .top, endPoint: .bottom)
            Canvas { ctx, size in
                let horizon = size.height * 0.52
                // Sun.
                let sunR = size.height * 0.20
                let sun = CGRect(x: size.width / 2 - sunR, y: horizon - sunR * 1.6,
                                 width: sunR * 2, height: sunR * 2)
                ctx.fill(Path(ellipseIn: sun),
                         with: .linearGradient(
                            Gradient(colors: [Color(red: 1, green: 0.95, blue: 0.5),
                                              Color(red: 1, green: 0.35, blue: 0.45)]),
                            startPoint: CGPoint(x: sun.midX, y: sun.minY),
                            endPoint: CGPoint(x: sun.midX, y: sun.maxY)))
                // Sun scanlines — clip a copy so the base context stays unclipped (GraphicsContext
                // is a value type; there is no resetClip).
                var sunCtx = ctx
                sunCtx.clip(to: Path(ellipseIn: sun))
                for i in 0..<7 {
                    let y = sun.minY + sun.height * (0.55 + Double(i) * 0.07)
                    let bar = CGRect(x: sun.minX, y: y, width: sun.width,
                                     height: sun.height * (0.012 + Double(i) * 0.006))
                    sunCtx.fill(Path(bar), with: .color(.black.opacity(0.85)))
                }
                // Perspective grid below the horizon.
                ctx.opacity = 0.55
                let cx = size.width / 2
                for col in -10...10 {
                    var p = Path()
                    p.move(to: CGPoint(x: cx, y: horizon))
                    p.addLine(to: CGPoint(x: cx + Double(col) * size.width * 0.11,
                                          y: size.height))
                    ctx.stroke(p, with: .color(Color(red: 0.6, green: 0.95, blue: 1)),
                               lineWidth: 1.5)
                }
                var row = horizon
                var step = size.height * 0.012
                while row < size.height {
                    var p = Path()
                    p.move(to: CGPoint(x: 0, y: row))
                    p.addLine(to: CGPoint(x: size.width, y: row))
                    ctx.stroke(p, with: .color(Color(red: 0.6, green: 0.95, blue: 1)),
                               lineWidth: 1.5)
                    step *= 1.32
                    row += step
                }
            }
        }
        .overlay(alignment: .bottomLeading) {
            // A small "now playing" chip so the frame reads as live content, not a wallpaper.
            HStack(spacing: 8) {
                Image(systemName: "gamecontroller.fill")
                Text("Streaming from Battlestation")
                    .font(.geist(16, .semibold, relativeTo: .callout))
            }
            .padding(.horizontal, 14).padding(.vertical, 9)
            .glassBackground(Capsule())
            .padding(18)
        }
    }

    /// `PUNKTFUNK_SHOT_HERO=/abs/path.png` → use a real captured frame as the hero background.
    static var overrideImage: Image? {
        guard let path = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_HERO"],
              !path.isEmpty, FileManager.default.fileExists(atPath: path) else { return nil }
        #if os(macOS)
        guard let ns = NSImage(contentsOfFile: path) else { return nil }
        return Image(nsImage: ns)
        #else
        guard let ui = UIImage(contentsOfFile: path) else { return nil }
        return Image(uiImage: ui)
        #endif
    }
}
#endif
