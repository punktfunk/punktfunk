// The host grid's cards: a saved host (tap to connect, ⓘ for its page, a short context menu) and
// an mDNS-discovered host (tap to save + connect). Both share the "monogram module" look — a
// squared brand-purple tile beside a bold Geist name and one line under it, in a hairline panel.
// A saved card's second line is its status and preset; its address lives on the host page.

import PunktfunkKit
import SwiftUI

/// Shared host-card sizing — touch-first on iOS, compact on macOS, roomy on tvOS.
struct CardMetrics {
    let tile: CGFloat       // monogram tile side
    let monogram: CGFloat   // monogram letter point size
    let name: CGFloat       // host-name point size
    let meta: CGFloat       // second line: a saved card's status, a discovered card's address
    let status: CGFloat     // status-label (mono) point size
    let padding: CGFloat
    let spacing: CGFloat    // tile ↔ text gap
    let radius: CGFloat

    static var current: CardMetrics {
        #if os(iOS) || os(visionOS)
        CardMetrics(tile: 54, monogram: 26, name: 19, meta: 13, status: 11,
                    padding: 16, spacing: 14, radius: 12)
        #elseif os(tvOS)
        // 10-foot sizes — the 24pt-name tier read like a phone card from the couch.
        CardMetrics(tile: 84, monogram: 42, name: 30, meta: 20, status: 17,
                    padding: 24, spacing: 22, radius: 18)
        #else
        CardMetrics(tile: 44, monogram: 21, name: 15, meta: 12, status: 10.5,
                    padding: 13, spacing: 12, radius: 10)
        #endif
    }
}

/// First letter of a host name, uppercased — the monogram glyph. Falls back to a bullet.
func monogram(_ name: String) -> String {
    guard let first = name.trimmingCharacters(in: .whitespacesAndNewlines).first else { return "•" }
    return String(first).uppercased()
}

/// The squared host tile. `filled` = a solid brand-purple chip (saved hosts); otherwise a tinted
/// outline (discovered hosts). Shows a spinner in place of the glyph while connecting.
///
/// `mark` is the host's OS mark, and it REPLACES the monogram when we have one: it identifies the
/// machine better than its initial ever did, and on a row of similarly-named boxes the initial says
/// nothing the name beneath it doesn't already say. A host that advertises no OS chain — or one we
/// ship no art for — keeps its letter, so a mixed row still reads as one set.
func monogramTile(
    _ letter: String, osChain: String?, m: CardMetrics, connecting: Bool, filled: Bool
) -> some View {
    let shape = RoundedRectangle(cornerRadius: m.radius - 3, style: .continuous)
    return ZStack {
        shape.fill(filled
            ? AnyShapeStyle(LinearGradient(
                colors: [Color.brand, Color.brand.opacity(0.72)],
                startPoint: .top, endPoint: .bottom))
            : AnyShapeStyle(Color.brand.opacity(0.14)))
        if connecting {
            ProgressView().tint(filled ? .white : Color.brand)
        } else if let mark = osIconImage(for: osChain) {
            // Template asset — tints from foregroundStyle exactly like the letter it stands in for.
            // Labelled, because this is where the OS is now announced: it used to ride the status
            // row below, which no longer carries it.
            mark
                .resizable()
                .scaledToFit()
                .frame(width: m.monogram, height: m.monogram)
                .foregroundStyle(filled ? Color.white : Color.brand)
                .accessibilityLabel(osChain ?? "")
        } else {
            // Fixed size (not Dynamic Type): the glyph is pinned inside a fixed tile, so it must
            // not scale up and spill out at large accessibility text sizes. minimumScaleFactor +
            // the clip below are belt-and-suspenders for an unusually wide glyph.
            Text(letter)
                .font(.geistFixed(m.monogram, .bold))
                .minimumScaleFactor(0.5)
                .lineLimit(1)
                .foregroundStyle(filled ? Color.white : Color.brand)
        }
    }
    .frame(width: m.tile, height: m.tile)
    .clipShape(shape)
    .overlay {
        if !filled {
            shape.strokeBorder(Color.brand.opacity(0.45), lineWidth: 1)
        }
    }
}

/// The default host's mark: a star on the tile's corner.
private struct DefaultHostBadge: View {
    let size: CGFloat

    var body: some View {
        // Resizable, so the star centres on its own bounds rather than on a text baseline.
        Image(systemName: "star.fill")
            .resizable()
            .scaledToFit()
            .fontWeight(.bold)
            .frame(width: size * 0.2, height: size * 0.2)
            .foregroundStyle(.white)
            .frame(width: size * 0.38, height: size * 0.38)
            .background(Circle().fill(Color.brand))
            .overlay(Circle().strokeBorder(.background, lineWidth: max(1, size * 0.035)))
            .offset(x: size * 0.06, y: size * 0.06)
            .accessibilityLabel("Default host")
            #if os(macOS)
            .help("Default host: Start in opens here")
            #endif
    }
}

/// Everything a card's preset affordances need: the catalog to offer, what this host is bound
/// and pinned to, and the acts a menu can perform (design/client-settings-profiles.md §5.2/§5.2a).
///
/// Passed as one value rather than eight closures because every surface that renders a host card
/// has to offer the SAME set — a menu that quietly lacks "Pin as card" on one screen is how a
/// feature becomes folklore.
struct HostPresetMenu {
    var presets: [StreamPreset]
    /// The host's default preset — the chip, and the checkmark in "Connect with ▸".
    var boundID: String?
    var pinnedIDs: [String]
    /// A ONE-OFF connect. Never rebinds: rebinding is `setDefault`, an explicit act (§5.2).
    var connectWith: (PresetSelection) -> Void
    var setDefault: (String?) -> Void
    var togglePin: (String) -> Void
}

/// Everything a saved host's card and its page can do. The grid builds one per card and the
/// host page asks the same builder, so the menu and the page never offer different sets. An
/// optional act is nil where the host can't take it yet: the library, the speed test and the
/// logs need a pairing, a wake needs an offline host with a known MAC.
struct HostActions {
    var connect: () -> Void
    var pair: () -> Void
    var forget: () -> Void
    var remove: () -> Void
    var browseLibrary: (() -> Void)?
    var speedTest: (() -> Void)?
    var sendLogs: (() -> Void)?
    var wake: (() -> Void)?
    /// Opens the profile picker to change who plays. nil until this device has picked one.
    var switchProfile: (() -> Void)?
    var copyLink: (() -> Void)?
    /// Opens the host page. nil on a pinned card, which is a shortcut rather than a host.
    var showDetails: (() -> Void)?
    /// What the host says this device may do to it (`design/host-actions.md` §7).
    var power: [HostAction] = []
    var runPower: (HostAction) -> Void = { _ in }
    var presets: HostPresetMenu?
}

/// Where one surface runs the acts that need more than the store: a sheet, a dial, a window.
struct HostActionSurface {
    var connect: (PresetSelection) -> Void
    var pair: () -> Void
    var browse: (PresetSelection) -> Void
    var speedTest: () -> Void
    var sendLogs: () -> Void
    var wake: () -> Void
    var showDetails: () -> Void
    var runPower: (HostAction) -> Void
    /// Only the grid opens the profile picker; a surface that can't leaves this empty.
    var switchProfile: () -> Void = {}
}

extension HostActions {
    /// What `host` offers, gated in one place for the grid and the Mac's host window. A pinned
    /// card connects and browses with ITS preset and carries no host acts: it is a shortcut, not
    /// a second host.
    @MainActor init(
        host: StoredHost, pinned: StreamPreset?, online: Bool, store: HostStore,
        presets: [StreamPreset], power: [HostAction], surface: HostActionSurface
    ) {
        let selection: PresetSelection = pinned.map { .preset($0.id) } ?? .inherit
        // Library, speed test and logs dial with the pinned identity, and an unpinned host would
        // accept any certificate. So they wait for a pairing. The demo host has neither a speed
        // test nor a log inbox.
        let paired = host.pinnedSHA256 != nil
        let measurable = paired && !DemoMode.isDemo(host)
        let wakeable = pinned == nil && !online && !host.wakeMacs.isEmpty
            && PunktfunkConnection.wakeOnLANAvailable
        self.init(
            connect: { surface.connect(selection) },
            pair: surface.pair,
            forget: { store.forgetIdentity(host) },
            remove: { store.remove(host) },
            browseLibrary: paired ? { surface.browse(selection) } : nil,
            speedTest: measurable ? surface.speedTest : nil,
            sendLogs: measurable ? surface.sendLogs : nil,
            wake: wakeable ? surface.wake : nil,
            switchProfile: pinned == nil && paired && host.pickedProfile != nil
                && !DemoMode.isDemo(host) ? surface.switchProfile : nil,
            copyLink: LinkClipboard.isAvailable
                ? { LinkClipboard.copy(DeepLink.forHost(host, preset: pinned?.id).urlString) }
                : nil,
            showDetails: pinned == nil ? surface.showDetails : nil,
            power: pinned == nil ? power : [],
            runPower: surface.runPower,
            presets: HostPresetMenu(
                presets: presets,
                boundID: host.presetID,
                pinnedIDs: host.pinnedPresetIDs ?? [],
                connectWith: surface.connect,
                setDefault: { store.setPreset(host.id, presetID: $0) },
                togglePin: { id in
                    let pinned = (host.pinnedPresetIDs ?? []).contains(id)
                    store.setPinned(host.id, presetID: id, pinned: !pinned)
                }))
    }
}

/// A host's state as one plain sentence: a saved card's second line and the host page's subtitle.
enum HostStatus: Equatable {
    case connecting
    case playing(String)
    case online
    case notPaired
    /// Offline, and a tap sends a Wake-on-LAN packet before it dials.
    case offlineWakes
    case offline

    init(
        host: StoredHost, isOnline: Bool, isConnecting: Bool, nowPlaying: String?, autoWake: Bool
    ) {
        if isConnecting {
            self = .connecting
        } else if isOnline {
            if host.pinnedSHA256 == nil {
                self = .notPaired
            } else if let nowPlaying {
                self = .playing(nowPlaying)
            } else {
                self = .online
            }
        } else if autoWake, !host.wakeMacs.isEmpty, PunktfunkConnection.wakeOnLANAvailable {
            self = .offlineWakes
        } else {
            self = .offline
        }
    }

    var text: String {
        switch self {
        case .connecting: return "Connecting…"
        case .playing(let title): return "Playing \(title)"
        case .online: return "Online"
        case .notPaired: return "Not paired"
        case .offlineWakes: return "Offline · wakes on tap"
        case .offline: return "Offline"
        }
    }

    var isPlaying: Bool {
        if case .playing = self { return true }
        return false
    }
}

/// The status sentence behind its presence dot: green while the host answers, grey when it
/// doesn't, none while dialing (the tile spins instead). A game up reads in green.
struct HostStatusLine: View {
    let status: HostStatus
    let size: CGFloat
    /// The dot alone: a card that shows its preset drops a plain "Online" beside it.
    var dotOnly = false

    var body: some View {
        HStack(spacing: 6) {
            switch status {
            case .connecting:
                EmptyView()
            case .offline, .offlineWakes:
                dot(Color.secondary.opacity(0.4))
            case .online, .notPaired, .playing:
                dot(Color.green)
            }
            if !dotOnly {
                Text(status.text)
                    .lineLimit(1)
            }
        }
        .font(.geist(size, .medium, relativeTo: .footnote))
        .foregroundStyle(status.isPlaying ? AnyShapeStyle(Color.green) : AnyShapeStyle(.secondary))
        // With the words gone the dot still has to say it.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(status.text)
    }

    private func dot(_ color: Color) -> some View {
        Circle()
            .fill(color)
            .frame(width: size * 0.5, height: size * 0.5)
            .accessibilityHidden(true) // the sentence says it
    }
}

/// A saved host in two lines: the name, then its status with the preset chip. A tap connects; ⓘ
/// and the menu's Host Details… open the host page; a star on the tile marks the default host.
/// The same view renders a pinned host+preset card, a shortcut whose menu carries only its own
/// acts (§5.2a).
struct HostCardView: View {
    let host: StoredHost
    /// Answered the last reachability probe.
    let isOnline: Bool
    let isConnecting: Bool
    /// The host Start in opens on, explicit or derived. Its tile carries a star.
    let isDefaultHost: Bool
    let isBusy: Bool
    let actions: HostActions
    /// Set on a pinned card: the preset it connects with. nil = the host's own card.
    var pinnedPreset: StreamPreset? = nil
    /// What the host has up right now (`NowPlayingStore`), if anything.
    var nowPlaying: String? = nil
    @AppStorage(DefaultsKey.autoWake) private var autoWake = SettingDefault.autoWake

    /// The preset this card announces: a pinned card's own, else the host's binding.
    private var shownPreset: StreamPreset? {
        pinnedPreset ?? actions.presets.flatMap { menu in
            menu.boundID.flatMap { id in menu.presets.first { $0.id == id } }
        }
    }

    var body: some View {
        let m = CardMetrics.current
        let status = HostStatus(
            host: host, isOnline: isOnline, isConnecting: isConnecting, nowPlaying: nowPlaying,
            autoWake: autoWake)
        return ZStack(alignment: .trailing) {
            Button(action: actions.connect) {
                HStack(spacing: m.spacing) {
                    monogramTile(monogram(host.displayName), osChain: host.osChain,
                                 m: m, connecting: isConnecting, filled: host.pinnedSHA256 != nil)
                        .opacity(isOnline || isConnecting ? 1 : 0.55)
                        .overlay(alignment: .bottomTrailing) {
                            if isDefaultHost { DefaultHostBadge(size: m.tile) }
                        }
                        .overlay(alignment: .topTrailing) {
                            // Who plays here; the pick stores no colour.
                            if let pick = host.pickedProfile {
                                ProfileAvatar(name: pick.displayName, size: m.tile * 0.38)
                                    .overlay(Circle().strokeBorder(.background, lineWidth: 2))
                                    .offset(x: m.tile * 0.06, y: -m.tile * 0.06)
                                    .accessibilityLabel("Playing as \(pick.displayName)")
                            }
                        }
                    VStack(alignment: .leading, spacing: 4) {
                        Text(host.displayName)
                            .font(.geist(m.name, .bold, relativeTo: .title3))
                            .foregroundStyle(.primary)
                            .lineLimit(1)
                        HStack(spacing: 8) {
                            HostStatusLine(
                                status: status, size: m.meta,
                                dotOnly: shownPreset != nil && status == .online)
                            if let preset = shownPreset {
                                // Whole even when the status has to shorten: the preset says what a
                                // tap does, and the host page has the full status.
                                PresetChip(
                                    preset: preset, size: m.status,
                                    prominent: pinnedPreset != nil)
                                    .fixedSize()
                            }
                        }
                        // One height with or without a chip, so preset cards line up with the rest.
                        .frame(height: m.meta * 1.4, alignment: .leading)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .padding(m.padding)
                .padding(.trailing, infoInset(m))
                .frame(maxWidth: .infinity, alignment: .leading)
                #if !os(tvOS)
                // tvOS: the .card button style owns platter + focus motion; extra chrome mutes it.
                .background(.regularMaterial)
                .clipShape(RoundedRectangle(cornerRadius: m.radius, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: m.radius, style: .continuous)
                        .strokeBorder(.quaternary, lineWidth: 1)
                }
                #endif
            }
            #if os(tvOS)
            .buttonStyle(.card)
            #elseif os(iOS) || os(visionOS)
            .buttonStyle(HostCardButtonStyle(cornerRadius: m.radius))
            #else
            .buttonStyle(.plain)
            #endif
            .disabled(isBusy)
            .contextMenu { menuItems }
            #if os(tvOS)
            // A TV card has no ⓘ: Play/Pause opens the page, as the menu's Host Details… does.
            .onPlayPauseCommand { actions.showDetails?() }
            #endif
            #if !os(tvOS)
            // A sibling of the card, not part of its label: a button inside a button never
            // receives the tap. tvOS reaches the page from the context menu instead.
            if let showDetails = actions.showDetails {
                Button(action: showDetails) {
                    Image(systemName: "info.circle")
                        .font(.system(size: m.name * 0.95))
                        .frame(width: 44, height: 44)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                #if os(iOS) || os(visionOS)
                .hoverEffect(.highlight)
                #endif
                .padding(.trailing, 4)
                .accessibilityLabel("Details for \(host.displayName)")
                #if os(macOS)
                .help("Host details")
                #endif
            }
            #endif
        }
        #if os(macOS)
        .help("\(host.address):\(String(host.port))")
        #endif
    }

    /// Room kept at the trailing edge for ⓘ, so the preset chip never slides under it.
    private func infoInset(_ m: CardMetrics) -> CGFloat {
        #if os(tvOS)
        0
        #else
        actions.showDetails == nil ? 0 : max(0, 48 - m.padding)
        #endif
    }

    /// The daily acts, five rows at most (design §2.3). Setup, diagnostics, power and removal
    /// live on the host page. A pinned card offers only its shortcut's acts.
    @ViewBuilder private var menuItems: some View {
        if let pinned = pinnedPreset {
            if let browse = actions.browseLibrary {
                Button("Browse Library", systemImage: "square.grid.2x2", action: browse)
            }
            if let copyLink = actions.copyLink {
                Button("Copy Link", systemImage: "link", action: copyLink)
            }
            if let presets = actions.presets {
                Button("Unpin Card", systemImage: "pin.slash", role: .destructive) {
                    presets.togglePin(pinned.id)
                }
            }
        } else {
            if let presets = actions.presets {
                connectWithMenu(presets)
            }
            if let browse = actions.browseLibrary {
                Button("Browse Library", systemImage: "square.grid.2x2", action: browse)
            }
            if let wake = actions.wake {
                Button("Wake Host", systemImage: "power", action: wake)
            }
            if let switchProfile = actions.switchProfile {
                Button("Switch profile…", systemImage: "person.crop.circle", action: switchProfile)
            }
            if let copyLink = actions.copyLink {
                Button("Copy Link", systemImage: "link", action: copyLink)
            }
            if let showDetails = actions.showDetails {
                Button("Host Details…", systemImage: "info.circle", action: showDetails)
            }
        }
    }

    /// "Connect with ▸": a one-off pick that never rebinds the host, with a checkmark on what a
    /// plain tap uses. Rebinding lives on the host page.
    @ViewBuilder private func connectWithMenu(_ menu: HostPresetMenu) -> some View {
        if !menu.presets.isEmpty {
            Menu {
                Button {
                    menu.connectWith(.defaults)
                } label: {
                    checkable("Default settings", on: menu.boundID == nil)
                }
                ForEach(menu.presets) { preset in
                    Button {
                        menu.connectWith(.preset(preset.id))
                    } label: {
                        checkable(preset.name, on: menu.boundID == preset.id)
                    }
                }
            } label: {
                Label("Connect with", systemImage: "slider.horizontal.3")
            }
        }
    }

    /// A menu row that carries a checkmark when it is the current choice. Built as two shapes
    /// rather than one `Label` with an empty symbol name — `Image(systemName: "")` is not a
    /// blank image, it is an invalid one.
    @ViewBuilder private func checkable(_ title: String, on: Bool) -> some View {
        if on {
            Label(title, systemImage: "checkmark")
        } else {
            Text(title)
        }
    }
}

/// The preset a card connects with, as a tinted pill after the status. Quiet on a bound primary
/// card (it only answers "what will a click do?"); prominent on a pinned card, where the preset
/// IS the reason the card exists — which is where the catalog's `accent` earns its keep.
///
/// Prominence is fill and weight only, never TYPE SIZE: the card fixes its second line's height,
/// and a chip taller than that would make pinned cards taller than their host's.
struct PresetChip: View {
    let preset: StreamPreset
    let size: CGFloat
    var prominent = false

    var body: some View {
        let tint = preset.accentColor
        return HStack(spacing: 5) {
            Circle()
                .fill(tint)
                .frame(width: size * 0.55, height: size * 0.55)
                .accessibilityHidden(true) // the name is right there
            Text(preset.name)
                .font(.geist(size, prominent ? .bold : .semibold, relativeTo: .caption2))
                .lineLimit(1)
        }
        .foregroundStyle(tint)
        .padding(.horizontal, 7)
        .padding(.vertical, 2)
        .background(Capsule().fill(tint.opacity(prominent ? 0.24 : 0.12)))
        .accessibilityLabel("Preset \(preset.name)")
    }
}

/// A host found on the LAN but not yet saved. A tinted-outline monogram + dashed panel border
/// distinguish it from saved cards; tapping saves it and connects (or pairs, if required).
struct DiscoveredCardView: View {
    let discovered: DiscoveredHost
    let isBusy: Bool
    let onConnect: () -> Void

    var body: some View {
        let m = CardMetrics.current
        return Button(action: onConnect) {
            HStack(spacing: m.spacing) {
                monogramTile(monogram(discovered.name), osChain: discovered.osChain,
                              m: m, connecting: false, filled: false)
                VStack(alignment: .leading, spacing: 4) {
                    Text(discovered.name)
                        .font(.geist(m.name, .bold, relativeTo: .title3))
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    Text("\(discovered.host):\(String(discovered.port))")
                        .font(.geist(m.meta, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    HStack(spacing: 6) {
                        // The advert's OS mark is the tile's glyph now (see monogramTile).
                        Image(systemName: discovered.requiresPairing
                            ? "lock.fill" : "antenna.radiowaves.left.and.right")
                            .font(.system(size: m.status))
                            .accessibilityHidden(true) // decorative; the adjacent text says the state
                        Text(discovered.requiresPairing ? "PAIRING REQUIRED" : "DISCOVERED")
                    }
                    .font(.geist(m.status, .medium, relativeTo: .caption2))
                    .tracking(0.8)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                }
                Spacer(minLength: 0)
            }
            .padding(m.padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            #if !os(tvOS)
            .background(.regularMaterial)
            .clipShape(RoundedRectangle(cornerRadius: m.radius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: m.radius, style: .continuous)
                    .strokeBorder(
                        Color.secondary.opacity(0.3),
                        style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
            }
            #endif
        }
        #if os(tvOS)
        .buttonStyle(.card)
        #elseif os(iOS) || os(visionOS)
        .buttonStyle(HostCardButtonStyle(cornerRadius: m.radius))
        #else
        .buttonStyle(.plain)
        #endif
        .disabled(isBusy)
    }
}

#if os(iOS) || os(visionOS)
/// The iOS host-card press/hover treatment, one style for both idioms:
/// - iPhone: a subtle scale-down on press + a light impact haptic on press-down. (`hoverEffect` is
///   inert without a pointer.)
/// - iPad: the system pointer "magnet" — the cursor morphs into a highlight that conforms to the
///   card's rounded rect on hover. (`sensoryFeedback` is inert without a Taptic Engine, and the
///   press scale doubles as click feedback.)
struct HostCardButtonStyle: ButtonStyle {
    var cornerRadius: CGFloat

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.96 : 1)
            .animation(.spring(response: 0.3, dampingFraction: 0.65), value: configuration.isPressed)
            // Conform the pointer highlight to the card's rounded rect, not its square bounds.
            .contentShape(.hoverEffect, RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
            .hoverEffect(.highlight)
            // Light tap on press-down (nil on release so it fires once, on touch). No haptic
            // hardware on iPad → silently ignored there.
            .sensoryFeedback(trigger: configuration.isPressed) { _, pressed in
                pressed ? .impact(weight: .light) : nil
            }
    }
}
#endif
