// What a `punktfunk://` link should DO, decided once for both routes.
//
// Connect and browse each ran the same three checks side by side — resolve a `preset=` reference,
// refuse a fingerprint that disagrees with the saved record, refuse to preempt a live session —
// and had already drifted: the same unsaved host produced two different sentences depending on
// which route the user came in on.
//
// A value, so the rules are testable without a window. The caller supplies what it knows and
// performs the outcome; nothing here touches the store, the session or the view.

import Foundation

public enum DeepLinkRouter {
    /// What the caller should do about a link.
    public enum Outcome: Equatable, Sendable {
        /// Refuse, and say this.
        case notice(String)
        /// A host named by something GUESSABLE (its label or address): the same action, one tap
        /// later, so a link cannot dial on its own say-so.
        case confirm(StoredHost, PresetSelection)
        /// Named by its unguessable record id, or already confirmed: go.
        case proceed(StoredHost, PresetSelection)
        /// The link points at the session already running. The open foregrounded the app, which
        /// is all "focus it" can mean mid-stream.
        case alreadyHere
    }

    /// The session state the guards need, so the router stays free of the model.
    public struct SessionState: Equatable, Sendable {
        public var isIdle: Bool
        public var activeHostID: UUID?
        public var activeHostName: String?

        public init(isIdle: Bool, activeHostID: UUID? = nil, activeHostName: String? = nil) {
            self.isIdle = isIdle
            self.activeHostID = activeHostID
            self.activeHostName = activeHostName
        }
    }

    /// `browse` picks the wording for an unsaved host — a library needs a paired identity, so
    /// there is nothing to show before the host is saved, where a connect can at least name the
    /// address the link pointed at.
    public static func resolve(
        link: DeepLink,
        hosts: [StoredHost],
        catalog: PresetCatalog,
        session: SessionState,
        browse: Bool
    ) -> Outcome {
        // The preset FIRST: an unknown or ambiguous reference must refuse, never quietly degrade
        // to the host's own binding, which is a different preset wearing the same host's name.
        var selection = PresetSelection.inherit
        if let reference = link.preset {
            let (preset, resolution) = catalog.resolve(reference)
            switch resolution {
            case .found:
                selection = .preset(preset?.id ?? "")
            case .notFound:
                return .notice(L10n.linkUnknownPreset(preset: reference))
            case .ambiguous:
                return .notice(L10n.linkAmbiguousPreset(preset: reference))
            }
        }

        let resolution = link.resolveHost(in: hosts)
        switch resolution {
        case .known(let host), .confirm(let host):
            guard !link.pinConflict(with: host) else {
                return .notice(L10n.linkPinConflict(host: host.displayName))
            }
            guard session.isIdle else {
                guard session.activeHostID == host.id else {
                    return .notice(String(localized: L10n.linkSessionRunning))
                }
                return .alreadyHere
            }
            if case .confirm = resolution {
                return .confirm(host, selection)
            }
            return .proceed(host, selection)

        case .unknown(let address, let port, let name, let fingerprint):
            // Never a silent connect: an unsaved host is a trust decision, and a link is not
            // where it gets made. This only NAMES what the link pointed at.
            guard session.isIdle || browse else {
                return .notice(String(localized: L10n.linkSessionRunning))
            }
            if browse {
                return .notice(L10n.appleLinkUnsavedBrowse(host: name ?? address))
            }
            let target = "\(address):\(String(port))"
            return .notice(
                fingerprint == nil
                    ? L10n.appleLinkUnsavedConnect(host: name ?? address, address: target)
                    : L10n.appleLinkUnsavedConnectFp(host: name ?? address, address: target))

        case .ambiguous:
            return .notice(L10n.linkAmbiguousHost(name: link.hostRef))

        case .unresolvable:
            return .notice(String(localized: L10n.linkHostNotSaved))
        }
    }
}
