// Steers the system's iPad pointer-lock resolution down to a chosen "anchor" view controller.
//
// `UIViewController.prefersPointerLocked` is resolved the same way as the status bar: the system
// walks DOWN from the window's root view controller through `childViewControllerForPointerLock`.
// SwiftUI's hosting / container view controllers do NOT forward that query to their children, so a
// `UIViewControllerRepresentable` controller buried in the SwiftUI tree (our StreamViewController)
// is never consulted — its `prefersPointerLocked = true` is silently ignored and a Magic Keyboard
// trackpad / mouse falls through to the absolute-pointer path instead of being captured.
//
// Swizzling the DEFAULT implementation isn't enough: the controllers that break the chain
// (UIHostingController and SwiftUI's internal containers) provide their OWN implementation of the
// property, so a base-class swizzle never runs for them. Instead we walk UP the LIVE `parent`
// chain from the anchor to the window root and, on each real ancestor, force
// `childViewControllerForPointerLock` to return the next controller toward the anchor. Each forced
// value is a genuine direct child (we follow the actual containment chain), so the system's
// downward walk reaches the anchor and reads its `prefersPointerLocked`.
//
// The forcing is per-INSTANCE — an associated object — gated behind a one-time per-CLASS IMP
// swizzle. Only the specific controllers in the anchor's chain are affected; every other instance
// of those classes keeps its original behavior (associated object nil → original IMP). The forced
// values are cleared on disengage so the long-lived SwiftUI parents don't retain a stale controller
// across sessions. Only the PUBLIC `childViewControllerForPointerLock` selector is touched
// (App-Store-safe; no private API).

#if os(iOS)
import ObjectiveC
import UIKit

enum PointerLockChain {
    private static var forcedChildKey: UInt8 = 0
    /// Classes whose `childViewControllerForPointerLock` we've already IMP-swizzled (keyed by the
    /// class object). Main-thread only — pointer-lock resolution and capture toggles are all main.
    private static var swizzledClasses = Set<ObjectIdentifier>()
    /// Ancestors we've stamped with a forced child this engagement, held weakly so a deallocated
    /// SwiftUI controller drops out on its own (no dangling). disengage() clears every one — even
    /// if the live `parent` chain has since broken — so a stamped parent can never retain a stale
    /// controller subtree across sessions. One anchor is ever engaged at a time.
    private static let stampedParents = NSHashTable<UIViewController>.weakObjects()

    private static func forcedChild(of vc: UIViewController) -> UIViewController? {
        objc_getAssociatedObject(vc, &forcedChildKey) as? UIViewController
    }

    private static func setForcedChild(_ child: UIViewController?, on vc: UIViewController) {
        // RETAIN: while steering, the parent must keep the toward-anchor child alive. It's also
        // already a strong child of `vc` via UIKit containment, so this adds no cycle (the reverse
        // `.parent` link is weak), and disengage() always clears it — so it can't outlive a session.
        objc_setAssociatedObject(vc, &forcedChildKey, child, .OBJC_ASSOCIATION_RETAIN_NONATOMIC)
    }

    /// Ensure `cls`'s `childViewControllerForPointerLock` getter consults the per-instance forced
    /// child first, falling back to the class's original implementation. Idempotent per class.
    private static func ensureSwizzled(_ cls: AnyClass) {
        let id = ObjectIdentifier(cls)
        guard !swizzledClasses.contains(id) else { return }
        swizzledClasses.insert(id)
        let selector = #selector(getter: UIViewController.childViewControllerForPointerLock)
        guard let method = class_getInstanceMethod(cls, selector) else { return }
        let originalIMP = method_getImplementation(method)
        typealias OriginalFn = @convention(c) (AnyObject, Selector) -> UIViewController?
        let original = unsafeBitCast(originalIMP, to: OriginalFn.self)
        let forwarding: @convention(block) (UIViewController) -> UIViewController? = { vc in
            if let forced = forcedChild(of: vc) { return forced }
            return original(vc, selector)
        }
        method_setImplementation(method, imp_implementationWithBlock(forwarding))
    }

    /// Force every ancestor of `anchor` to forward pointer-lock resolution toward it, then ask the
    /// system to re-resolve. No-op when `anchor` isn't in a view-controller hierarchy yet (it
    /// re-runs from the anchor's appearance/parent callbacks once it is).
    static func engage(_ anchor: UIViewController) {
        disengage(anchor) // clear any prior engagement first (reparent / re-anchor)
        var child = anchor
        while let parent = child.parent {
            ensureSwizzled(object_getClass(parent)!)
            setForcedChild(child, on: parent)
            stampedParents.add(parent)
            child = parent
        }
        anchor.setNeedsUpdateOfPrefersPointerLocked()
    }

    /// Whether the system's downward walk can reach `anchor` at all: the containment chain must
    /// end at the window's root or at a presented controller. A diagnostic, not a gate.
    static func reachesScene(from anchor: UIViewController) -> Bool {
        var top = anchor
        while let parent = top.parent { top = parent }
        return top === anchor.view.window?.rootViewController || top.presentingViewController != nil
    }

    /// Clear the forced forwarding on every stamped ancestor (so the SwiftUI parents stop retaining
    /// the anchor's subtree) and re-resolve to drop the lock. The ancestors re-resolve too: a
    /// session ends after its controller has left the window, where the anchor's own request never
    /// reaches the scene, and the lock would keep the pointer hidden on the hosts screen.
    static func disengage(_ anchor: UIViewController) {
        let parents = stampedParents.allObjects
        for parent in parents {
            setForcedChild(nil, on: parent)
        }
        stampedParents.removeAllObjects()
        anchor.setNeedsUpdateOfPrefersPointerLocked()
        for parent in parents {
            parent.setNeedsUpdateOfPrefersPointerLocked()
        }
    }
}
#endif
