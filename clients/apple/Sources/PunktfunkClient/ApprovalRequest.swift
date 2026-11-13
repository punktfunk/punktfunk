import Foundation
import PunktfunkKit

/// A fresh `pair=required`/unknown host pending a trust decision: drives both the "request access
/// vs. pair with PIN" choice and the subsequent approval wait. `advertisedFingerprint` is the
/// discovered host's advertised cert. A typed host has none, and is offered the PIN only.
struct ApprovalRequest {
    let host: StoredHost
    let advertisedFingerprint: Data?
    /// Tells this request's answer from a later one's.
    var token = UUID()
    /// The dial itself raised this wait: the host parked a plain connect. It ends with that dial.
    var viaDial = false
    /// A link's connect, run with the newly pinned host once pairing lands. Nil for every other
    /// way in: pairing alone never streams.
    var thenConnect: (@MainActor (StoredHost) -> Void)?
}
