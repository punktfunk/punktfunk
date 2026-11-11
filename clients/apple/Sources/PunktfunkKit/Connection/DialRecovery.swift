// What a failed dial leads to: the caller's profile handlers, a redial of a host still waking,
// an alert, or the caller's own recovery (the wake wait). `SessionModel.connect` takes one
// recovery for a failed dial and performs it; every redial dials `StoredHost.redialed`. Pure, so
// `swift test` covers every branch (`DialRecoveryTests`).

import Foundation
import PunktfunkShared

public indirect enum DialRecovery: Equatable {
    /// The host doesn't list the profile: the caller's handler may dial again, and `otherwise`
    /// runs when it doesn't.
    case profileUnknown(otherwise: DialRecovery)
    /// The profile is locked, in the host's words: the caller's handler may prompt.
    case profileLocked(said: String?, otherwise: DialRecovery)
    /// Dial the same host again after this wait: it answered before it could stream.
    case wakeRedial(after: Duration)
    case alert(String)
    /// The caller recovers (the wake wait), and its overlay explains it.
    case handBack

    /// The launch hold stays up through this recovery: a wake redial holds from the tap to the
    /// game. Every other one drops it before it runs.
    public var keepsLaunchHold: Bool {
        if case .wakeRedial = self { return true }
        return false
    }

    /// `wakeDelay` is the core's wait for a host still waking, nil without a dial to repeat. The
    /// rest feed `ConnectOffer.failureMessage`; `callerRecovers` says the caller has a recovery.
    public static func decide(
        _ error: Error, wakeDelay: Duration?, hostName: String, pinned: Bool,
        requestAccess: Bool, callerRecovers: Bool
    ) -> DialRecovery {
        let rest: DialRecovery
        if let wakeDelay {
            rest = .wakeRedial(after: wakeDelay)
        } else if let message = ConnectOffer.failureMessage(
            error, hostName: hostName, pinned: pinned, requestAccess: requestAccess,
            callerRecovers: callerRecovers)
        {
            rest = .alert(message)
        } else {
            rest = .handBack
        }
        switch error {
        case PunktfunkClientError.rejected(.profileUnknown, _):
            return .profileUnknown(otherwise: rest)
        case PunktfunkClientError.rejected(.profileLocked, let said):
            return .profileLocked(said: said, otherwise: rest)
        default:
            return rest
        }
    }
}

public extension StoredHost {
    /// The record a redial of this dial dials: the store's `current` one, so an address or a port
    /// that moved since the first dial is followed, else this copy. `pin` replaces the record's
    /// pin: a lost session's redial keeps the pin that session proved.
    func redialed(current: StoredHost?, pin: Data? = nil) -> StoredHost {
        var host = current ?? self
        if let pin { host.pinnedSHA256 = pin }
        return host
    }
}
