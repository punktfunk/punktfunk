// AccessWarning: the expiry warnings' latches and toasts, and the capture an access edit releases.

import XCTest

@testable import PunktfunkKit

final class AccessWarningTests: XCTestCase {
    private let none = AccessWarning()
    private let five = AccessWarning(fiveMinutes: true)
    private let both = AccessWarning(fiveMinutes: true, oneMinute: true)

    /// Walk a countdown through `seconds`, returning each tick's toast.
    private func walk(_ seconds: [UInt32], from start: AccessWarning = AccessWarning())
        -> [AccessWarning.Toast?] {
        var warned = start
        return seconds.map {
            let (next, toast) = AccessWarning.step(remaining: $0, warned: warned)
            warned = next
            return toast
        }
    }

    /// A permanent grant warns of nothing and leaves the latches alone.
    func testAPermanentGrantWarnsOfNothing() {
        XCTAssertEqual(AccessWarning.step(remaining: 0, warned: none).warned, none)
        XCTAssertNil(AccessWarning.step(remaining: 0, warned: none).toast)
        XCTAssertEqual(AccessWarning.step(remaining: 0, warned: both).warned, both)
    }

    /// Each mark fires once, on the tick that crosses it, and the 5 m toast says the time left.
    func testEachMarkFiresOnce() {
        XCTAssertEqual(
            walk([400, 301, 300, 299, 61, 60, 59, 1]),
            [nil, nil, .endsIn(300), nil, nil, .endsSoon, nil, nil])
    }

    /// A countdown first seen inside a mark fires that mark's toast at once.
    func testAGrantSeenInsideAMarkWarnsAtOnce() {
        XCTAssertEqual(walk([240]), [.endsIn(240)])
        XCTAssertEqual(walk([30]), [.endsSoon])
        // Straight past both marks: one toast, and both latches set.
        XCTAssertEqual(AccessWarning.step(remaining: 30, warned: none).warned, both)
    }

    /// An edit that moves the deadline back above a mark re-arms it.
    func testAnExtendedDeadlineReArmsTheMarks() {
        XCTAssertEqual(walk([30, 600, 200, 50]), [.endsSoon, nil, .endsIn(200), .endsSoon])
        // Back between the marks: the 1 m latch clears, the 5 m one stands.
        XCTAssertEqual(AccessWarning.step(remaining: 200, warned: both).warned, five)
        XCTAssertNil(AccessWarning.step(remaining: 200, warned: both).toast)
        // Above 5 m both clear, and only from a set 5 m latch.
        XCTAssertEqual(AccessWarning.step(remaining: 400, warned: both).warned, none)
        let lone = AccessWarning(oneMinute: true)
        XCTAssertEqual(AccessWarning.step(remaining: 400, warned: lone).warned, lone)
    }

    /// A capture lets go only when the grants carry neither pointer nor keyboard.
    func testARevokedInputGrantReleasesTheCapture() {
        let pointer = PunktfunkConnection.grantPointer
        let keyboard = PunktfunkConnection.grantKeyboard
        let pad = PunktfunkConnection.grantGamepad
        XCTAssertTrue(AccessWarning.releasesCapture(captured: true, grants: 0))
        XCTAssertTrue(AccessWarning.releasesCapture(captured: true, grants: pad))
        XCTAssertFalse(AccessWarning.releasesCapture(captured: true, grants: pointer))
        XCTAssertFalse(AccessWarning.releasesCapture(captured: true, grants: keyboard))
        XCTAssertFalse(AccessWarning.releasesCapture(captured: true, grants: PunktfunkConnection.grantAll))
        XCTAssertFalse(AccessWarning.releasesCapture(captured: false, grants: 0))
    }
}
