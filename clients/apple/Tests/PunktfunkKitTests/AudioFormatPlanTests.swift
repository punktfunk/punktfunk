// The output format rules (`AudioFormatPlan`). Each row says what it pins.

import XCTest

@testable import PunktfunkKit

final class AudioFormatPlanTests: XCTestCase {
    func testTheMixerMeetsTheOutputAtItsWidthAndTheCaptureRate() {
        typealias Out = AudioFormatPlan.MixerOutput
        let rows: [(ch: Int, rate: Double, layout: Bool, capture: Double, want: Out, why: String)] = [
            (2, 48_000, true, 0, .engineDefault, "a stereo route keeps the engine's connection"),
            (6, 48_000, true, 0, .width(channels: 6, rate: 48_000), "5.1 is not mixed to stereo"),
            (8, 48_000, false, 0, .engineDefault, "a width with no layout cannot be declared"),
            (6, 0, true, 0, .engineDefault, "an output that reads 0 Hz has no rate to declare"),
            (2, 44_100, true, 192_000, .stereo(rate: 192_000),
             "#2569: voice processing renders at the capture rate"),
            (6, 44_100, true, 48_000, .width(channels: 6, rate: 48_000),
             "a wide route under voice processing keeps its width"),
            (0, 0, false, 96_000, .stereo(rate: 96_000),
             "#2569: an output not yet read still renders at the capture rate"),
        ]
        for row in rows {
            XCTAssertEqual(
                AudioFormatPlan.mixerOutput(
                    outputChannels: row.ch, outputRate: row.rate, hasLayout: row.layout,
                    captureRate: row.capture),
                row.want, row.why)
        }
    }

    func testTheSessionIsAskedAgainForWhatTheRouteCanCarry() {
        let rows: [(wire: Int, max: Int, current: Int, want: Int?, why: String)] = [
            (8, 6, 2, 6, "an 8-channel wire on a 5.1 AVR asks for 6"),
            (6, 2, 2, nil, "a stereo route has nothing more to give"),
            (6, 8, 6, nil, "a grant that already holds the wire is left alone"),
            (2, 8, 2, nil, "a stereo wire never asks for more"),
        ]
        for row in rows {
            XCTAssertEqual(
                AudioFormatPlan.channelReask(wire: row.wire, maximum: row.max, current: row.current),
                row.want, row.why)
        }
    }

    func testTheLogSaysWhatTheOutputOpenedAt() {
        typealias Note = AudioFormatPlan.Note
        let rows: [(rate: Int, out: Int, mixer: Int, wireRate: Int, wire: Int, want: [Note])] = [
            (48_000, 2, 2, 48_000, 2, [.negotiated]),
            (44_100, 2, 2, 48_000, 2, [.resampling(device: 44_100)]),
            (48_000, 2, 2, 48_000, 6, [.folded(output: 2)]),
            (48_000, 6, 2, 48_000, 6, [.mixerFolds(mixer: 2), .negotiated]),
            (48_000, 8, 8, 48_000, 6, [.silentTail(output: 8)]),
            (96_000, 2, 2, 48_000, 6, [.resampling(device: 96_000), .folded(output: 2)]),
            (0, 2, 2, 48_000, 2, []),
        ]
        for row in rows {
            XCTAssertEqual(
                AudioFormatPlan.notes(
                    deviceRate: row.rate, outputChannels: row.out, mixerChannels: row.mixer,
                    wireRate: row.wireRate, wireChannels: row.wire),
                row.want, "\(row)")
        }
    }
}
