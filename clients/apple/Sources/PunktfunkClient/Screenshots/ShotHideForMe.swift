// Shot harness for "Hide for me": the Library as a locked profile's person sees it, its title
// menu open or not, and as a shelf without the action. A menu has no public call that opens it,
// so the scene opens it through UIKit's own, which no release build carries.

#if DEBUG && (os(iOS) || os(tvOS))
import PunktfunkKit
import SwiftUI
import UIKit

struct ShotHideForMe: View {
    /// Opens the top-left title's menu.
    var opensMenu = false
    /// The shelf's profile may hide titles.
    var hides = true

    static let kid = ListedProfile(id: "shot", displayName: "Kid", person: true)

    var body: some View {
        #if os(iOS)
        ShotTouchTabs(selection: .library) { Color.clear } library: { library }
        #else
        library
        #endif
    }

    private var library: some View {
        NavigationStack {
            LibraryView(
                store: ShotMock.pageStore, target: LibraryTarget(host: ShotMock.pageStore.hosts[0]),
                onLaunch: { _ in }, inTab: true,
                shotPhase: .catalog(ShotMock.games, running: []),
                shotLayout: "games,desktops,favorites,recent,launchers",
                shotHides: hides ? Self.kid.displayName : nil)
        }
        .task {
            guard opensMenu else { return }
            try? await Task.sleep(for: .seconds(2))
            // The top-left poster on screen.
            let rects = ShotMock.games.compactMap { TileFrames.rect($0.id) }.filter { $0.minY > 0 }
            guard let rect = rects.min(by: { ($0.minY, $0.minX) < ($1.minY, $1.minX) }) else { return }
            ShotMenu.open(at: CGPoint(x: rect.midX, y: rect.midY))
        }
    }
}

/// Opens the menu UIKit holds for a point, as a long-press would.
@MainActor
enum ShotMenu {
    static func open(at point: CGPoint) {
        guard let window else { return }
        var view = window.hitTest(point, with: nil)
        while let v = view, !present(v, at: v.convert(point, from: window)) { view = v.superview }
    }

    private static var window: UIWindow? {
        UIApplication.shared.connectedScenes.lazy
            .compactMap { ($0 as? UIWindowScene)?.keyWindow }.first
    }

    private static func present(_ view: UIView, at point: CGPoint) -> Bool {
        let selector = NSSelectorFromString("_presentMenuAtLocation:")
        for case let menu as UIContextMenuInteraction in view.interactions
        where menu.responds(to: selector) {
            typealias Present = @convention(c) (AnyObject, Selector, CGPoint) -> Void
            unsafeBitCast(menu.method(for: selector), to: Present.self)(menu, selector, point)
            // Opened this way the menu raises the search field's keyboard; a long-press does not.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
                UIApplication.shared.sendAction(
                    #selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
            }
            return true
        }
        return false
    }
}
#endif
