// AV1 reference concealment for VideoToolbox (`punktfunk_av1_concealer_*`, ABI v69).
//
// VideoToolbox refuses an AV1 frame whose reference slot holds an older picture than the one it
// names. An error-resilient wave start codes the order hint it expects in every slot, so the
// core's concealer rewrites a stale one to what the decoder holds and the wave heals without a
// keyframe. `verdict` judges a wave start before the admission rule; `conceal` folds only what
// the decoder is fed, in order.

import Foundation
import PunktfunkCore

final class Av1Concealer {
    /// The same three outcomes as the HEVC concealer's.
    typealias Outcome = HevcConcealer.Outcome

    private let ptr: OpaquePointer

    init() {
        ptr = punktfunk_av1_concealer_new()
    }

    deinit { punktfunk_av1_concealer_free(ptr) }

    /// The admission rule's `concealed` for one AU with wire `flags`: `.decodable` for an
    /// error-resilient wave start, `.none` for every other AU. Leaves the concealer as it was.
    func verdict(_ au: Data, flags: UInt32) -> AUPumpState.Concealment {
        var verdict = UInt32(PUNKTFUNK_CONCEALED_NONE)
        let status = au.withUnsafeBytes { raw in
            punktfunk_av1_concealer_verdict(
                ptr, raw.bindMemory(to: UInt8.self).baseAddress, UInt(raw.count), flags, &verdict)
        }
        guard status == PUNKTFUNK_STATUS_OK.rawValue else { return .none }
        return verdict == UInt32(PUNKTFUNK_CONCEALED_DECODABLE) ? .decodable : .none
    }

    /// Fold one AU the decoder is about to get.
    func conceal(_ au: Data) -> Outcome {
        var kind = PUNKTFUNK_CONCEALMENT_INTACT
        var buf: UnsafeMutablePointer<UInt8>?
        var len: UInt = 0
        let status = au.withUnsafeBytes { raw in
            punktfunk_av1_concealer_conceal(
                ptr, raw.bindMemory(to: UInt8.self).baseAddress, UInt(raw.count), &kind, &buf,
                &len)
        }
        guard status == PUNKTFUNK_STATUS_OK.rawValue else { return .intact }
        switch kind.rawValue {
        case PUNKTFUNK_CONCEALMENT_REWRITTEN.rawValue:
            guard let buf else { return .intact }
            let data = Data(bytes: buf, count: Int(len))
            punktfunk_h265_concealer_release(buf, len)
            return .rewritten(data)
        case PUNKTFUNK_CONCEALMENT_UNRECOVERABLE.rawValue:
            return .unrecoverable
        default:
            return .intact
        }
    }
}
