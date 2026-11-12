// ClientIdentity.deviceCode: the code a waiting device shows, spelled as the core and the
// host's web console spell it.

import Foundation
import XCTest

@testable import PunktfunkKit

final class DeviceCodeTests: XCTestCase {
    /// The vector `punktfunk_core::fp::device_code` is tested against.
    func testSpellsTheCoresVector() {
        var fingerprint = Data(repeating: 0, count: 32)
        fingerprint.replaceSubrange(0..<8, with: [0x4f, 0x2a, 0x91, 0xc3, 0x07, 0xbe, 0xd5, 0x12])
        XCTAssertEqual(ClientIdentity.deviceCode(of: fingerprint), "4F2A 91C3 07BE D512")
    }
}
