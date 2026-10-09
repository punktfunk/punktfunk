// The Library tab (design/apple-touch-ui-overhaul.md §2.5) on iPhone, iPad and Apple TV, and the
// Mac's Library row: one shelf, picked from the host filter over it and remembered. `LibraryView`
// owns the fetch, cache, wake and art; this view picks the shelf, and what to say with no paired
// host. A written `libraryTarget` lands here through ContentView's `showShelfInTab` or
// `showShelfInSidebar`.

import PunktfunkKit
import SwiftUI

#if os(iOS) || os(visionOS) || os(tvOS)
/// The touch UI's destinations: the remote-desktop face and the gaming face. A TV adds Settings,
/// which a remote reaches best from the tab bar.
enum TouchTab: Hashable {
    case hosts
    case library
    #if os(tvOS)
    case settings
    #endif
}

#if os(tvOS)
extension TouchTab {
    /// The Hosts tab's label. Its symbol draws in one layer: in its preferred two, the focused
    /// tab's white pill left it white.
    static var hostsLabel: some View {
        Label {
            Text("Hosts")
        } icon: {
            Image(systemName: "desktopcomputer").symbolRenderingMode(.monochrome)
        }
    }
}
#endif
#endif

struct LibraryTabView: View {
    @ObservedObject var store: HostStore
    let onLaunch: (LibraryTarget, String) -> Void
    let onConnectShelf: (LibraryTarget) -> Void
    /// Stream a saved host's desktop: the Desktops section.
    let onConnectHost: (StoredHost) -> Void
    let showHosts: () -> Void
    /// The shelf's wake offer (`LibraryView.wake`).
    var wake: ((StoredHost, @escaping () -> Void) -> Void)? = nil
    #if DEBUG
    /// Shot harness: a canned catalog in place of the fetch.
    var shotPhase: ShotLibraryPhase?
    #endif
    @ObservedObject private var presets = PresetStore.shared
    @AppStorage(DefaultsKey.libraryShelf) private var shelfID = ""
    @AppStorage(DefaultsKey.defaultHost) private var defaultHostID = ""

    /// Every shelf there is: each paired host, then each preset pinned to it.
    private var shelves: [LibraryTarget] {
        LibraryTarget.shelves(of: store.hosts, presets: presets)
    }

    /// The remembered shelf, else the default host's, else the first one.
    private var shelf: LibraryTarget? {
        let all = shelves
        if let remembered = all.first(where: { $0.id == shelfID }) { return remembered }
        if let host = StartScreen.defaultHost(id: defaultHostID, hosts: store.hosts).host,
           let fallback = all.first(where: { $0.id == LibraryTarget(host: host).id }) {
            return fallback
        }
        return all.first
    }

    var body: some View {
        NavigationStack {
            if let shelf {
                library(shelf)
                    .id(shelf.id)
            } else {
                LibraryNoHostView(showHosts: showHosts)
            }
        }
        #if os(macOS)
        // The host grid's floor, so the window can't squeeze the Library past it.
        .frame(minWidth: 480, minHeight: 360)
        #endif
    }

    private func library(_ shelf: LibraryTarget) -> LibraryView {
        #if DEBUG
        LibraryView(
            store: store, target: shelf, onLaunch: { onLaunch(shelf, $0) },
            onConnect: { onConnectShelf(shelf) }, inTab: true, onConnectHost: onConnectHost,
            tabHeader: filter(current: shelf), wake: wake, shotPhase: shotPhase)
        #else
        LibraryView(
            store: store, target: shelf, onLaunch: { onLaunch(shelf, $0) },
            onConnect: { onConnectShelf(shelf) }, inTab: true, onConnectHost: onConnectHost,
            tabHeader: filter(current: shelf), wake: wake)
        #endif
    }

    /// The host filter, when there is more than one shelf to pick. A pick opens the shelf at its
    /// top: its remembered title is for coming back from a stream, not for switching hosts.
    private func filter(current: LibraryTarget) -> AnyView? {
        let all = shelves
        guard all.count > 1 else { return nil }
        return AnyView(ShelfFilter(shelves: all, current: current.id) { id in
            if let picked = all.first(where: { $0.id == id }) {
                LibraryScrollMemory.forget(hostID: picked.host.id.uuidString)
            }
            shelfID = id
        })
    }
}

/// The Library's host filter: one chip per shelf, the current one filled. It took over from a title
/// menu that hid the choice.
private struct ShelfFilter: View {
    let shelves: [LibraryTarget]
    let current: String
    let pick: (String) -> Void
    @ObservedObject private var presets = PresetStore.shared

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: chipSpacing) {
                    ForEach(shelves) { shelf in
                        chip(shelf).id(shelf.id)
                    }
                }
                .padding(.horizontal)
                .padding(.vertical, chipLift)
            }
            // The row is rebuilt with each shelf, so bring the current chip back into view.
            .onAppear { proxy.scrollTo(current) }
        }
        #if os(tvOS)
        // A full-width target, so a move down from the tab bar's row lands on a chip.
        .focusSection()
        #endif
    }

    #if os(tvOS)
    /// The system's capsule buttons, which draw the focus a remote needs: the pick is the
    /// prominent one, since a tinted bordered button paints its label in the tint.
    @ViewBuilder private func chip(_ shelf: LibraryTarget) -> some View {
        let button = Button { pick(shelf.id) } label: {
            HStack(spacing: 10) {
                if let mark = osIconImage(for: shelf.host.osChain) {
                    mark.resizable().scaledToFit().frame(width: 28, height: 28)
                }
                Text(shelf.title(in: presets))
                    .lineLimit(1)
            }
        }
        .buttonBorderShape(.capsule)
        if shelf.id == current {
            button.buttonStyle(.borderedProminent).tint(Color.brand)
                .accessibilityAddTraits(.isSelected)
        } else {
            button.buttonStyle(.bordered)
        }
    }

    private let chipSpacing: CGFloat = 24
    /// Room for the focused chip to grow without the scroll view clipping it.
    private let chipLift: CGFloat = 16
    #else
    private func chip(_ shelf: LibraryTarget) -> some View {
        let on = shelf.id == current
        return Button { pick(shelf.id) } label: {
            HStack(spacing: 6) {
                if let mark = osIconImage(for: shelf.host.osChain) {
                    mark.resizable().scaledToFit().frame(width: 14, height: 14)
                }
                Text(shelf.title(in: presets))
                    .lineLimit(1)
            }
            .font(.geist(13, .semibold, relativeTo: .subheadline))
            .foregroundStyle(on ? Color.white : Color.primary)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background(Capsule().fill(on ? AnyShapeStyle(Color.brand) : AnyShapeStyle(.regularMaterial)))
            .overlay { if !on { Capsule().strokeBorder(.quaternary, lineWidth: 1) } }
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(on ? .isSelected : [])
    }

    private let chipSpacing: CGFloat = 8
    private let chipLift: CGFloat = 2
    #endif
}
