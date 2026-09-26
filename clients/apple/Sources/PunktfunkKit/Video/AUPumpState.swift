// The per-access-unit bookkeeping both VideoToolbox pumps do: the straggler filter, the format
// and decoded-size tracking, the keyframe WANT that only an IDR's parameter sets can end, and
// which post-loss AUs never reach the decoder.
//
// No I/O: the caller reads the connection and applies what `note` returns, so the rules are
// testable without a host. Which post-loss AUs reach VideoToolbox is the core's receiver rule
// (`punktfunk_au_admission_*`), strict: one reference-damaged delta can make VideoToolbox
// refuse every later non-IDR AU, the anchor too. What stays on glass is `ReanchorGate`'s.

import CoreMedia
import Foundation
import PunktfunkCore

struct AUPumpState {
    /// The live format description. Only an IDR's parameter sets can set it.
    private(set) var format: CMVideoFormatDescription?
    /// The last size reported upward, so a loss-recovery IDR at the same size stays quiet.
    private var lastDims: CMVideoDimensions?
    /// Newest submitted frame index, for the straggler filter.
    private var newestIndex: UInt32?
    /// Persistent WANT for the two states only parameter sets can end: no decodable format yet,
    /// or a decoder reset. The caller re-asks (throttled) while it is true.
    private(set) var awaitingIDR = false
    /// The core's receiver rule. A copy of this state shares it; each pump owns one.
    private let admission = Admission()

    /// What the caller should do about this access unit.
    struct Step: Equatable {
        /// Arrived behind one already submitted: decoding it rewinds the reference buffer, so
        /// skip it entirely.
        var straggler = false
        /// The decoded size changed — report it (a new-mode IDR, not a same-size recovery one).
        var newSize: Size?
        /// This AU ended a recovery the pump was waiting on.
        var resumed = false
        /// The wait for a decodable format began with this AU (log once, not per AU).
        var startedFormatWait = false
        /// Do not hand this AU to the decoder: it references a lost picture.
        var withhold = false
        /// Ask the host for a keyframe (the caller throttles).
        var askKeyframe = false

        struct Size: Equatable {
            var width: Int
            var height: Int
        }
    }

    /// What the codec's concealer made of an AU. `.none` for a codec without one: loss then
    /// falls back to withholding until the re-anchor.
    enum Concealment: Equatable {
        case none
        /// Every current reference names a picture the decoder holds (as received or rewritten).
        case decodable
        /// Nothing can stand in: off the decoder, and ask for an IDR.
        case unrecoverable

        var core: UInt32 {
            switch self {
            case .none: UInt32(PUNKTFUNK_CONCEALED_NONE)
            case .decodable: UInt32(PUNKTFUNK_CONCEALED_DECODABLE)
            case .unrecoverable: UInt32(PUNKTFUNK_CONCEALED_UNRECOVERABLE)
            }
        }
    }

    /// Whether `note` skips this index as a straggler. Work the decoder never sees must not
    /// happen on one — the concealer mirrors the decoder's DPB.
    func isStraggler(frameIndex: UInt32) -> Bool {
        // Wraparound-safe: the index is a 32-bit counter, so compare the difference as signed.
        newestIndex.map { Int32(bitPattern: frameIndex &- $0) <= 0 } ?? false
    }

    /// Fold one access unit in. `idrFormat` is what the codec made of its parameter sets, or nil
    /// for a delta frame; `lossAhead` says a frame-index gap precedes this AU; `flags` are its
    /// wire flags (`AccessUnit.flags`); `concealed` is the concealer's verdict on it. Parsed
    /// parameter sets re-anchor like the wire IDR bit.
    mutating func note(
        frameIndex: UInt32, idrFormat: CMVideoFormatDescription?, lossAhead: Bool = false,
        flags: UInt32 = 0, concealed: Concealment = .none
    ) -> Step {
        var step = Step()
        if isStraggler(frameIndex: frameIndex) {
            step.straggler = true
            return step
        }
        newestIndex = frameIndex

        let idr = idrFormat != nil ? PunktfunkConnection.flagSOF : 0
        _ = punktfunk_au_admission_note(
            admission.ptr, frameIndex, lossAhead ? 1 : 0, flags | idr, true, concealed.core,
            &step.withhold, &step.askKeyframe)

        if let f = idrFormat {
            format = f // refreshed on every IDR, mode changes included
            let dims = CMVideoFormatDescriptionGetDimensions(f)
            if lastDims?.width != dims.width || lastDims?.height != dims.height {
                lastDims = dims
                step.newSize = .init(width: Int(dims.width), height: Int(dims.height))
            }
            if awaitingIDR { step.resumed = true }
            awaitingIDR = false
        }

        if format == nil {
            // Nothing decodable yet: the opening IDR's parameter sets never arrived or never
            // parsed, and under the host's infinite GOP nothing re-delivers them unless we ASK.
            // Without this every AU is dropped silently, forever.
            step.startedFormatWait = !awaitingIDR
            awaitingIDR = true
        }
        return step
    }

    /// A wedged decoder or a reset: drop the format and wait for the next parameter sets.
    mutating func requireIDR() {
        format = nil
        awaitingIDR = true
    }
}

/// The core rule's C handle, freed once with the last state that holds it.
private final class Admission {
    let ptr: OpaquePointer = punktfunk_au_admission_new()
    deinit { punktfunk_au_admission_free(ptr) }
}
