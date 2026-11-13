// The host cards' OS marks: template vector imagesets in Resources/OsIcons.xcassets
// (generated from the repo's assets/os-icons masters by scripts/gen-os-icons.sh —
// per-mark provenance and licensing in that directory's README), resolved from the host's
// OS-identity chain via PunktfunkShared's `osIconTokens` walk. Template rendering means
// they tint with `foregroundStyle` like an SF Symbol.

import PunktfunkShared
import SwiftUI

/// The icon tokens this client ships art for: the families a chain can land on, plus the
/// distro leaves that earn their own mark because "a Bazzite box" and "a Fedora box" are
/// different machines to the person reading the card. A distro with no mark of its own
/// still degrades to its family's and finally to Tux via the chain walk.
private let osIconTokensShipped: Set<String> = [
    "windows", "apple", "linux", "steam", "ubuntu", "fedora", "arch", "debian", "nixos",
    "opensuse", "bazzite", "cachyos", "nobara", "omarchy",
]

/// The mark for an OS-identity chain (`linux/fedora/bazzite`, ...), or nil — no view at
/// all — when the host doesn't advertise one or nothing in the chain is recognized.
public func osIconImage(for chain: String?) -> Image? {
    osIconTokens(chain)
        .first(where: osIconTokensShipped.contains)
        .map { Image("os-\($0)", bundle: .module) }
}
