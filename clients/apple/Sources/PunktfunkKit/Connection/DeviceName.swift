// The name this device tells a host it is — the label an operator approves in the web console.

import Foundation
#if canImport(UIKit)
    import UIKit
#endif

/// The name the USER knows this device by: "Enrico's iPad", "Wohnzimmer UG", "Enricos MacBook Pro".
///
/// The host shows it in its pending-approval list — the web console's outstanding-pairings view and
/// the dialog that approves a knock — and files the device under it in the trust store. It is the
/// ONLY thing distinguishing one waiting device from another there, so it must come from the OS
/// name the user set, not from a placeholder.
///
/// The core's own default is the OS hostname, not the name the user set.
/// `PunktfunkConnection.init` passes this as `PunktfunkConnectOpts.device_name` by default.
public enum DeviceName {
    /// This device's user-facing name, never empty.
    public static var current: String {
        #if os(macOS)
            let name = (Host.current().localizedName ?? "")
                .trimmingCharacters(in: .whitespacesAndNewlines)
            return name.isEmpty ? (hostName ?? kind) : name
        #else
            let name = UIDevice.current.name.trimmingCharacters(in: .whitespacesAndNewlines)
            // iOS/tvOS 16+ answer `name` with the MODEL ("iPad") unless the app holds the
            // user-assigned-device-name entitlement — which turns a household's three iPads into
            // three identical rows in the host's approval list. The hostname is not behind that
            // gate on every OS version, and when the user has named the device it carries that
            // name ("Enricos-iPad"), so prefer it whenever `name` came back generic.
            if name.isEmpty || name == kind {
                if let host = hostName { return host }
            }
            return name.isEmpty ? kind : name
        #endif
    }

    /// The OS hostname without its mDNS `.local` suffix — nil when it is unset or the placeholder
    /// every unconfigured device reports, which would name nothing.
    private static var hostName: String? {
        let host = ProcessInfo.processInfo.hostName
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let bare = host.hasSuffix(".local") ? String(host.dropLast(6)) : host
        guard !bare.isEmpty, bare.caseInsensitiveCompare("localhost") != .orderedSame else {
            return nil
        }
        return bare
    }

    /// What to call the device when the OS has no name for it — the product, which at least tells
    /// an operator which of the pending rows is the Apple TV. (iOS/tvOS 16+ answer
    /// `UIDevice.current.name` with exactly this unless the app holds the user-assigned-name
    /// entitlement, so the two agree more often than not.)
    public static var kind: String {
        #if os(macOS)
            return "Mac"
        #elseif os(tvOS)
            return "Apple TV"
        #else
            return UIDevice.current.model // "iPad" / "iPhone"
        #endif
    }
}
