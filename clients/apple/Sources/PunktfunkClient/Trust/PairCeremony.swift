// The SPAKE2 PIN ceremony itself, with no opinion about how it's presented. Two callers run it:
// `PairSheet` (the touch/desktop Form, and tvOS's focus-engine layout) and `ConsoleModel` (the
// console's pair screen). The ceremony is the part that must not diverge between them —
// it decides what counts as a wrong PIN, what a rejection means, and which failures are worth
// telling the user apart — so it lives here once rather than being copied into the second caller.
//
// Threading: `pair()` and the identity load both BLOCK, so they run off the main actor; every
// published mutation lands back on it.

import Foundation
import PunktfunkKit
import SwiftUI

@MainActor
final class PairCeremony: ObservableObject {
    /// A ceremony is in flight — callers disable their commit action and show a spinner.
    @Published private(set) var busy = false
    /// The last failure, in user-facing terms; cleared when a new attempt starts.
    @Published var errorText: String?

    /// Dismissing the presenting screen must abandon an in-flight ceremony: the blocking `pair()`
    /// call can't be interrupted, so its completion checks this token and self-discards — a late
    /// success must NOT pin a host the user cancelled out of. A fresh token
    /// per attempt, so abandoning one attempt can't silence the next.
    private var token = Token()

    private final class Token: @unchecked Sendable {
        var cancelled = false
    }

    /// Run the ceremony. `onPaired` receives the host's now-VERIFIED fingerprint for the caller to
    /// pin; no manual fingerprint comparison is needed, because the host proved itself with the
    /// same PIN.
    func run(
        host address: String, port: UInt16, pin rawPIN: String, clientName rawName: String,
        onPaired: @escaping (Data) -> Void
    ) {
        busy = true
        errorText = nil
        let pin = rawPIN.trimmingCharacters(in: .whitespaces)
        let name = rawName.trimmingCharacters(in: .whitespaces)
        token = Token()
        let token = token
        Task.detached(priority: .userInitiated) {
            // Identity load + the ceremony both block — keep them off the main actor.
            // loadForPairing is the strict variant: the host durably trusts this
            // identity, so it must have made it into the Keychain.
            let result = Result {
                let identity = try ClientIdentityStore.shared.loadForPairing()
                return try PunktfunkKit.pair(
                    host: address, port: port, identity: identity,
                    pin: pin, name: name.isEmpty ? DeviceName.current : name)
            }
            await MainActor.run {
                guard !token.cancelled else { return } // screen dismissed mid-ceremony
                self.busy = false
                switch result {
                case .success(let fingerprint):
                    onPaired(fingerprint)
                case .failure(PunktfunkClientError.wrongPIN):
                    self.errorText = "Wrong PIN — check the host's web console (port 47992) "
                        + "and try again."
                case .failure(PunktfunkClientError.rejected(let rejection)):
                    // The host answered and said why (not armed / rate-limited / armed for
                    // another device) — show that instead of the guessing-game fallback.
                    self.errorText = rejection.userMessage
                case .failure(is ClientIdentityStore.IdentityError):
                    self.errorText = "Can't store this Mac's identity in the Keychain, so the "
                        + "pairing would not survive a relaunch. Unlock the login "
                        + "keychain and try again."
                case .failure:
                    self.errorText = "Pairing failed — the host didn't answer. Is it running, "
                        + "and is this device on the same network (no VPN, no guest-Wi-Fi "
                        + "isolation)?"
                }
            }
        }
    }

    /// The presenting screen went away — discard whatever is still in flight. Called from every
    /// dismissal path (an explicit Cancel, a swipe, B on a controller), which is why it is safe to
    /// call when nothing is running.
    func abandon() {
        token.cancelled = true
    }
}
