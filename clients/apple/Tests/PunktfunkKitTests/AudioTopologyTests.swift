// The session's engines (`AudioTopology`) and the decisions that change them: what a start
// builds, what a permission grant does, and what a published engine displaces.

import AVFoundation
import XCTest

@testable import PunktfunkKit

final class AudioTopologyTests: XCTestCase {
    private func mic() -> MicCapture {
        MicCapture(
            engine: AVAudioEngine(), uplink: MicUplink(plan: MicChannelPlan(pick: 0, loopback: [])))
    }

    private func ids(_ engines: [AVAudioEngine]) -> [ObjectIdentifier] {
        engines.map(ObjectIdentifier.init)
    }

    func testAPartNeverLandsBesideTheCombinedEngineOrItsOwnKind() {
        let p1 = AVAudioEngine(), p2 = AVAudioEngine()
        let c1 = mic(), c2 = mic(), k = mic()
        let rows: [(
            start: AudioTopology, part: AudioTopology.Part, render: AVAudioEngine?,
            capture: AVAudioEngine?, combined: AVAudioEngine?, displaced: [AVAudioEngine],
            uplinks: Int, why: String
        )] = [
            (.none, .playback(p1), p1, nil, nil, [], 0, "playback joins an empty session"),
            (.separate(playback: p1, capture: nil), .capture(c1), p1, c1.engine, nil, [], 0,
             "the split path adds its capture"),
            (.separate(playback: p1, capture: c1), .capture(c2), p1, c2.engine, nil, [c1.engine], 1,
             "one mic engine per permission grant"),
            (.separate(playback: p1, capture: nil), .playback(p2), p2, nil, nil, [p1], 0,
             "a second playback engine displaces the first"),
            (.separate(playback: p1, capture: c1), .combined(k), k.engine, nil, k.engine,
             [c1.engine, p1], 1, "the combined engine never runs beside a pair"),
            (.combined(k), .playback(p1), p1, nil, nil, [k.engine], 1,
             "playback displaces a combined engine with its uplink"),
            (.combined(k), .capture(c1), nil, c1.engine, nil, [k.engine], 1,
             "a capture displaces a combined engine with its uplink"),
        ]
        for row in rows {
            var topology = row.start
            let displaced = topology.publish(row.part)
            XCTAssert(topology.render === row.render, row.why)
            XCTAssert(topology.capture === row.capture, row.why)
            XCTAssert(topology.combined === row.combined, row.why)
            XCTAssertEqual(ids(displaced.engines), ids(row.displaced), row.why)
            XCTAssertEqual(displaced.uplinks.count, row.uplinks, row.why)
        }
    }

    func testTheSwapToCombinedTakesOnlyPlayback() {
        let playback = AVAudioEngine(), capture = mic()
        var separate = AudioTopology.separate(playback: playback, capture: capture)
        XCTAssert(separate.takePlayback() === playback)
        XCTAssertNil(separate.render)
        XCTAssert(separate.capture === capture.engine)

        var combined = AudioTopology.combined(mic())
        XCTAssertNil(combined.takePlayback())
        XCTAssertNotNil(combined.combined)
    }

    func testAGrantStartsTheMicOnce() {
        let rows: [(topology: AudioTopology, combined: Bool, want: AudioTopology.GrantStep, why: String)] = [
            (.separate(playback: AVAudioEngine(), capture: nil), false, .addCapture,
             "split: the capture joins playback"),
            (.separate(playback: AVAudioEngine(), capture: nil), true, .swapToCombined,
             "echo cancellation: the combined engine replaces playback"),
            (.none, true, .swapToCombined, "playback that failed to start still gets the mic"),
            (.separate(playback: AVAudioEngine(), capture: mic()), false, .ignore,
             "a second grant starts no second mic"),
            (.combined(mic()), true, .ignore, "a rebuild after the grant already started the mic"),
        ]
        for row in rows {
            XCTAssertEqual(row.topology.onGrant(combined: row.combined), row.want, row.why)
        }
    }

    func testAStartAsksForTheMicOnlyWhenItIsOn() {
        let rows: [(mic: Bool, combined: Bool, permission: AudioStart.Permission, want: AudioStart)] = [
            (false, true, .notDetermined, .playback),
            (false, false, .authorized, .playback),
            (true, true, .authorized, .combined),
            (true, false, .authorized, .split),
            (true, true, .notDetermined, .askThen(combined: true)),
            (true, false, .notDetermined, .askThen(combined: false)),
            (true, true, .denied, .denied),
        ]
        for row in rows {
            XCTAssertEqual(
                AudioStart.choose(micEnabled: row.mic, combined: row.combined, permission: row.permission),
                row.want, "\(row)")
        }
    }
}
