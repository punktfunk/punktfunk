// What a session's end leads to: a redial of a host still waking, a redial of a lost session,
// a sentence, the shelf it came from, or nothing. `SessionModel.sessionEnded` reads the facts off
// the ended connection, takes one route and performs it. Pure, so `swift test` covers every
// branch (`SessionEndRouteTests`).

import Foundation
import PunktfunkShared

public enum SessionEndRoute: Equatable {
    /// Dial the same host again after this wait: it closed before it could stream.
    case wakeRedial(after: Duration)
    /// Dial the lost session again, pinned to this fingerprint.
    case lossRedial(pin: Data)
    /// Say why it ended.
    case message(Message)
    /// Back to the shelf the session started from.
    case library
    /// Nothing to say: the player quit a game that came off no shelf.
    case none

    public enum Message: Equatable {
        /// The host's typed close, in its own words when it sent any.
        case rejected(HostRejection, said: String?)
        case endedByHost
        case hostError
        case lost
        /// No verdict: an older core, or the close raced the read.
        case unknown

        public func text(host: String) -> String {
            switch self {
            case .rejected(let rejection, let said):
                return L10n.dialHostReason(host: host, reason: said ?? rejection.userMessage)
            case .endedByHost: return L10n.appleEndedByHost(host: host)
            case .hostError: return L10n.appleEndedHostError(host: host)
            case .lost: return L10n.appleEndedLost(host: host)
            case .unknown: return L10n.appleEndedUnknown(host: host)
            }
        }
    }

    /// `rejection` and `said` are the typed close and the host's sentence. `wakeDelay` is the
    /// core's wait for a host that closed before it could stream. `dialed` is the host as the
    /// session's dial had it, nil when there is no dial to repeat; `live` is the fingerprint the
    /// session saw. `shelf`: the session came off a library shelf.
    public static func decide(
        phase: SessionPhase, reason: PunktfunkConnection.SessionEndReason,
        rejection: HostRejection?, said: String?, wakeDelay: Duration?,
        dialed: StoredHost?, live: Data, shelf: Bool
    ) -> SessionEndRoute {
        if let wakeDelay, dialed != nil { return .wakeRedial(after: wakeDelay) }
        if reason == .lost, rejection == nil, let dialed,
           let pin = phase.lossRedialPin(dialed: dialed.pinnedSHA256, live: live) {
            return .lossRedial(pin: pin)
        }
        // A typed close outranks the coarse reason: an access expiry files under `.hostError`.
        if let rejection { return .message(.rejected(rejection, said: said)) }
        switch reason {
        case .gameExited: return shelf ? .library : .none
        case .hostEnded, .local: return .message(.endedByHost)
        case .hostError: return .message(.hostError)
        case .lost: return .message(.lost)
        case .none: return .message(.unknown)
        }
    }
}
