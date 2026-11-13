// Shot harness for "Hide for me": the Library as a locked profile's person sees it, with the
// title's menu or the sort menu open. A menu has no public call that opens it, so the scene opens
// it through UIKit's own, which no release build carries.

#if DEBUG && (os(iOS) || os(tvOS))
import PunktfunkKit
import SwiftUI
import UIKit

struct ShotHideForMe: View {
    enum Opens { case title, sortMenu }
    let opens: Opens

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
                shotHides: Self.kid.displayName)
        }
        .task {
            try? await Task.sleep(for: .seconds(2))
            switch opens {
            case .title:
                // The top-left poster on screen.
                let rects = ShotMock.games.compactMap { TileFrames.rect($0.id) }.filter { $0.minY > 0 }
                guard let rect = rects.min(by: { ($0.minY, $0.minX) < ($1.minY, $1.minX) })
                else { return }
                ShotMenu.open(at: CGPoint(x: rect.midX, y: rect.midY))
            case .sortMenu:
                ShotMenu.openMenuButton()
            }
        }
    }
}

/// Opens the menu UIKit holds for a point or a menu button, as a long-press or a tap would.
@MainActor
enum ShotMenu {
    static func open(at point: CGPoint) {
        guard let window else { return }
        var view = window.hitTest(point, with: nil)
        while let v = view, !present(v, at: v.convert(point, from: window)) { view = v.superview }
    }

    /// The first button on screen that holds a menu: the Library's sort menu.
    static func openMenuButton() {
        guard let window else { return }
        func find(_ v: UIView) -> UIView? {
            if v is UIControl, !(v is UITextField),
               v.interactions.contains(where: { $0 is UIContextMenuInteraction }) {
                return v
            }
            return v.subviews.lazy.compactMap(find).first
        }
        guard let control = find(window) else { return }
        _ = present(control, at: CGPoint(x: control.bounds.midX, y: control.bounds.midY))
        // A TV's menu runs past the screen: show its foot, where the entry is.
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
            func scroll(_ v: UIView, inMenu: Bool) {
                let menu = inMenu || "\(type(of: v))".contains("ContextMenu")
                if menu, let list = v as? UIScrollView {
                    let foot = list.contentSize.height - list.bounds.height + list.adjustedContentInset.bottom
                    list.setContentOffset(CGPoint(x: 0, y: max(0, foot)), animated: false)
                }
                v.subviews.forEach { scroll($0, inMenu: menu) }
            }
            (window.windowScene?.windows ?? [window]).forEach { scroll($0, inMenu: false) }
        }
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
