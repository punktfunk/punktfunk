// The hand-off from an App Intent to the window that acts on its link.
//
// An intent runs in the app's process but outside any scene, and on a cold launch it can run
// before a window subscribes to the notification. So the link is held here as well as posted:
// a subscribed window takes it from the post, and a window that appears later finds it waiting.

import Foundation
import PunktfunkKit

@MainActor
enum DeepLinkInbox {
    /// The link no window has taken yet.
    private static var pending: NSURL?

    static func post(_ url: URL) {
        let link = url as NSURL
        pending = link
        NotificationCenter.default.post(name: .punktfunkOpenDeepLink, object: link)
    }

    /// True for the one caller that may act on `link`. Every window hears the post, and
    /// `LinkArbiter` says which asks first.
    static func take(_ link: NSURL) -> Bool {
        guard pending === link else { return false }
        pending = nil
        return true
    }

    /// The link that was posted to nobody, for a window that has just appeared.
    static func takePending() -> URL? {
        defer { pending = nil }
        return pending as URL?
    }
}
