import Foundation
import PunktfunkKit

/// A fresh `pair=required`/unknown host pending a trust decision: drives both the "request access
/// vs. pair with PIN" choice and the subsequent approval wait. `advertisedFingerprint` is the
/// discovered host's advertised cert (nil for a manually-typed host → trust-on-first-use).
struct ApprovalRequest {
    let host: StoredHost
    let advertisedFingerprint: Data?
    /// Tells this request's answer from a later one's.
    var token = UUID()
    /// A link's connect, run with the newly pinned host once pairing lands. Nil for every other
    /// way in: pairing alone never streams.
    var thenConnect: (@MainActor (StoredHost) -> Void)?
}
