// Multi-channel input → mono fold (MicChannelPlan.fold): the fix for a mic on one channel of
// a multi-channel interface. AVAudioConverter's default N→stereo downmix grabs channels 0/1 — dead
// silence when the mic sits higher up — so we fold ourselves. This pins the fiddly bits (the
// interleaved stride, channel pinning, the sum-clamp) against regressions without needing hardware.

#if !os(tvOS)
import XCTest

@testable import PunktfunkKit

final class AudioChannelFoldTests: XCTestCase {
    /// Drive `MicChannelPlan.fold` over channel data expressed as `[[Float]]`, mirroring the two
    /// `floatChannelData` layouts:
    ///   - deinterleaved: each inner array is one channel (all `frames` long).
    ///   - interleaved: a single inner array already interleaved (c0f0, c1f0, …), with the real
    ///     channel count passed separately.
    /// `pinned` is 0-based, as the fold indexes; the plan's pick is 1-based.
    private func fold(
        _ planes: [[Float]], frames: Int, channels: Int, interleaved: Bool, pinned: Int?,
        skip: [Int] = []
    ) -> [Float] {
        // One C buffer per plane + a table of pointers to them — the shape of floatChannelData.
        let buffers: [UnsafeMutablePointer<Float>] = planes.map { plane in
            let p = UnsafeMutablePointer<Float>.allocate(capacity: plane.count)
            for i in 0..<plane.count { p[i] = plane[i] }
            return p
        }
        let table = UnsafeMutablePointer<UnsafeMutablePointer<Float>>.allocate(
            capacity: buffers.count)
        for (i, b) in buffers.enumerated() { table[i] = b }
        let out = UnsafeMutablePointer<Float>.allocate(capacity: frames)
        defer {
            buffers.forEach { $0.deallocate() }
            table.deallocate()
            out.deallocate()
        }
        MicChannelPlan(pick: pinned.map { $0 + 1 } ?? 0, loopback: skip).fold(
            input: table, frames: frames, channels: channels, interleaved: interleaved, out: out)
        return (0..<frames).map { out[$0] }
    }

    // A pinned channel is copied verbatim — the exact fix: mic on a HIGH channel, not 0/1.
    func testPinsHigherChannelDeinterleaved() {
        let result = fold(
            [[0, 0, 0], [0, 0, 0], [0.1, 0.2, 0.3], [0, 0, 0]],
            frames: 3, channels: 4, interleaved: false, pinned: 2)
        XCTAssertEqual(result, [0.1, 0.2, 0.3])
    }

    // Same signal, interleaved layout: [c0f0,c1f0,c2f0,c3f0, c0f1,…]. Guards the `i*ch + c` stride.
    func testPinsHigherChannelInterleaved() {
        let interleaved: [Float] = [
            0, 0, 0.1, 0,
            0, 0, 0.2, 0,
            0, 0, 0.3, 0,
        ]
        let result = fold([interleaved], frames: 3, channels: 4, interleaved: true, pinned: 2)
        XCTAssertEqual(result, [0.1, 0.2, 0.3])
    }

    // Auto (pinned: nil): a lone hot channel amid silence passes at FULL level, never attenuated.
    func testAutoSumsAllChannelsSoALoneMicSurvives() {
        let result = fold(
            [[0, 0], [0.4, -0.4], [0, 0]],
            frames: 2, channels: 3, interleaved: false, pinned: nil)
        XCTAssertEqual(result, [0.4, -0.4])
    }

    // Two simultaneously-hot channels sum past the unit range → clamped, never wraps/overflows.
    func testAutoSumClampsToUnitRange() {
        let result = fold(
            [[0.8, -0.8], [0.9, -0.9]],
            frames: 2, channels: 2, interleaved: false, pinned: nil)
        XCTAssertEqual(result, [1.0, -1.0])
    }

    // A plain mono device is passed through untouched (no clamp, no attenuation).
    func testMonoIsIdentity() {
        let result = fold(
            [[0.25, -0.5, 0.75]], frames: 3, channels: 1, interleaved: false, pinned: nil)
        XCTAssertEqual(result, [0.25, -0.5, 0.75])
    }

    // Scarlett Solo 4th Gen: Input 1, Input 2, Loopback 1, Loopback 2. The loopback pair carries
    // the stream the Mac plays; mixing it in sends the game back up as the mic.
    func testAutoLeavesLoopbackInputsOut() {
        let interleaved: [Float] = [
            0.2, 0, 0.5, 0.5,
            -0.2, 0, -0.5, -0.5,
        ]
        let result = fold(
            [interleaved], frames: 2, channels: 4, interleaved: true, pinned: nil, skip: [2, 3])
        XCTAssertEqual(result, [0.2, -0.2])
    }

    // A device whose every input is named loopback still sends something; a pick still wins.
    func testLoopbackSkipYieldsToAllSkippedAndToAPin() {
        let planes: [[Float]] = [[0.25, 0.25], [0.5, 0.5]]
        XCTAssertEqual(
            fold(planes, frames: 2, channels: 2, interleaved: false, pinned: nil, skip: [0, 1]),
            [0.75, 0.75])
        XCTAssertEqual(
            fold(planes, frames: 2, channels: 2, interleaved: false, pinned: 1, skip: [1]),
            [0.5, 0.5])
    }

    // Belt-and-suspenders: an out-of-range pin (the tap already guards, but the setting is
    // persisted) is ignored by the plan's own range check, which sums instead of reading past
    // the buffer.
    func testOutOfRangePinFallsBackToSum() {
        let result = fold(
            [[0, 0], [0.3, 0.3]],
            frames: 2, channels: 2, interleaved: false, pinned: 2)
        XCTAssertEqual(result, [0.3, 0.3])
    }

    // The log names the channels the fold takes, from the same rule.
    func testTheLabelNamesWhatTheFoldTakes() {
        let rows: [(plan: MicChannelPlan, channels: Int, label: String)] = [
            (MicChannelPlan(pick: 2, loopback: []), 4, "channel 2/4"),
            (MicChannelPlan(pick: 0, loopback: [2, 3]), 4, "mix 2/4ch→mono"),
            (MicChannelPlan(pick: 0, loopback: [0, 1]), 2, "mix 2/2ch→mono"),
            (MicChannelPlan(pick: 5, loopback: [3]), 4, "mix 3/4ch→mono"),
            (MicChannelPlan(pick: 0, loopback: []), 1, "mono"),
            (MicChannelPlan(pick: 1, loopback: []), 1, "channel 1/1"),
        ]
        for row in rows {
            XCTAssertEqual(
                row.plan.label(channels: row.channels), row.label, "\(row.plan) on \(row.channels)")
        }
    }
}
#endif
