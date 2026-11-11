// SessionMic: a row per fact behind the mute control, and the two reasons behind the mute.

import AVFoundation
import XCTest

@testable import PunktfunkKit

final class SessionMicTests: XCTestCase {
    private func available(
        hasMic: Bool = true, enabled: Bool = true, granted: Bool = true,
        _ status: AVAuthorizationStatus = .authorized
    ) -> Bool {
        SessionMic.available(hasMic: hasMic, enabled: enabled, granted: granted, authorization: status)
    }

    /// Every fact can take the mic away on its own.
    func testEachFactCanTakeTheMicAway() {
        XCTAssertTrue(available())
        XCTAssertFalse(available(hasMic: false))
        XCTAssertFalse(available(enabled: false))
        XCTAssertFalse(available(granted: false))
    }

    /// The OS refusing is no mic; a prompt still pending counts, since a grant starts the uplink.
    func testTheOSAnswerDecidesLast() {
        XCTAssertTrue(available(.notDetermined))
        XCTAssertFalse(available(.denied))
        XCTAssertFalse(available(.restricted))
    }

    /// tvOS has no app microphone, and nothing asks the OS where another fact says no.
    func testTheOSIsNotAskedWithoutAMicToAskFor() {
        for (hasMic, enabled, granted) in [(false, true, true), (true, false, true), (true, true, false)] {
            XCTAssertFalse(SessionMic.available(
                hasMic: hasMic, enabled: enabled, granted: granted,
                authorization: { XCTFail("asked the OS"); return .authorized }()))
        }
    }

    /// Either reason mutes, and neither clears the other.
    func testEitherReasonMutes() {
        XCTAssertFalse(SessionMic.muted(user: false, backgrounded: false))
        XCTAssertTrue(SessionMic.muted(user: true, backgrounded: false))
        XCTAssertTrue(SessionMic.muted(user: false, backgrounded: true))
        XCTAssertTrue(SessionMic.muted(user: true, backgrounded: true))
    }
}
