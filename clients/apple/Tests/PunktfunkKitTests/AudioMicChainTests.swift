// The rate-dependent half of the mic chain (SessionAudio.micChain). The tap now installs with
// `format: nil` — a non-nil format is validated against the bus and raises an Objective-C
// exception on mismatch, which Swift cannot catch, so it aborted the whole app (SIGABRT in
// AVAudioEngineGraph::InstallTapOnNode, reported against 0.31.0). With nil the tap follows
// whatever the bus emits, which means the chain has to be rebuildable at the device's real rate.
// This pins the sizing arithmetic that rebuild depends on, without an engine, device or mic grant.

#if !os(tvOS)
import AVFoundation
import XCTest

@testable import PunktfunkKit

final class AudioMicChainTests: XCTestCase {
    /// The encoder's target: 48 kHz mono float — what every chain resamples ONTO.
    private let target = AVAudioFormat(
        commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 1, interleaved: false)!

    /// A chain is built at the device's rate, mono, and resamples onto the 48 kHz encoder format.
    func testBuildsMonoChainAtDeviceRate() throws {
        let chain = try XCTUnwrap(
            SessionAudio.micChain(rate: 44_100, frames: 8192, to: target))
        XCTAssertEqual(chain.monoFormat.sampleRate, 44_100)
        XCTAssertEqual(chain.monoFormat.channelCount, 1)
        XCTAssertEqual(chain.mono.frameCapacity, 8192)
        XCTAssertEqual(chain.resampler.outputFormat.sampleRate, 48_000)
    }

    /// `staging` holds the resampled 48 kHz mono, so it must fit the UPWARD ratio — the bug this
    /// guards is a staging buffer sized for the input rate, which silently truncates every packet
    /// when the device runs below 48 kHz.
    func testStagingFitsUpwardResampleRatio() throws {
        for rate in [8_000.0, 16_000, 44_100, 48_000, 96_000] {
            let chain = try XCTUnwrap(
                SessionAudio.micChain(rate: rate, frames: 1024, to: target))
            let needed = (1024.0 * 48_000 / rate).rounded(.up)
            XCTAssertGreaterThanOrEqual(
                Double(chain.staging.frameCapacity), needed,
                "staging too small to hold 1024 frames resampled from \(rate) Hz")
        }
    }

    /// A rate the device cannot report is refused rather than producing a chain that would
    /// divide by zero in the staging arithmetic. The tap treats nil as "skip this buffer".
    func testRejectsUnusableRateAndEmptyQuantum() {
        XCTAssertNil(SessionAudio.micChain(rate: 0, frames: 8192, to: target))
        XCTAssertNil(SessionAudio.micChain(rate: -48_000, frames: 8192, to: target))
        XCTAssertNil(SessionAudio.micChain(rate: 48_000, frames: 0, to: target))
    }

    /// The rebuild path: a device that switches 48 kHz → 44.1 kHz under a live tap yields a chain
    /// at the NEW rate. Resampling by the stale ratio is what pitch-shifts the mic.
    func testRebuildFollowsNewRate() throws {
        let first = try XCTUnwrap(SessionAudio.micChain(rate: 48_000, frames: 512, to: target))
        let second = try XCTUnwrap(SessionAudio.micChain(rate: 44_100, frames: 512, to: target))
        XCTAssertEqual(first.monoFormat.sampleRate, 48_000)
        XCTAssertEqual(second.monoFormat.sampleRate, 44_100)
        XCTAssertGreaterThan(second.staging.frameCapacity, first.staging.frameCapacity)
    }

    /// The 10 s level verdict comes once, when the window fills, with the loudest sample seen.
    func testTheLevelCheckGivesOneVerdictWhenTheWindowFills() {
        var check = SessionAudio.MicLevelCheck(rate: 100) // a 1 000-sample window
        let quiet = [Float](repeating: 0, count: 600)
        let loud: [Float] = [0, -0.5, 0.25]
        quiet.withUnsafeBufferPointer { XCTAssertNil(check.note($0.baseAddress!, count: 600)) }
        loud.withUnsafeBufferPointer { XCTAssertNil(check.note($0.baseAddress!, count: 3)) }
        quiet.withUnsafeBufferPointer {
            XCTAssertEqual(check.note($0.baseAddress!, count: 600), 0.5)
        }
        quiet.withUnsafeBufferPointer { XCTAssertNil(check.note($0.baseAddress!, count: 600)) }
    }
}
#endif
