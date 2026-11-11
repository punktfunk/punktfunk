// A CJK keyboard types a host address in full-width forms; the core dials it in ASCII.

import XCTest

@testable import PunktfunkShared

final class AsciiAddressTests: XCTestCase {
    func testFullWidthIPv4MapsToAscii() {
        XCTAssertEqual("１９２．１６８．１．５".withASCIIAddress, "192.168.1.5")
        XCTAssertEqual("192。168。1。5".withASCIIAddress, "192.168.1.5")
    }

    func testFullWidthPunctuationAndLettersMapToAscii() {
        XCTAssertEqual("ｎｕｃ．ｌｏｃａｌ：９７７７".withASCIIAddress, "nuc.local:9777")
        XCTAssertEqual("［ｆｄ７ａ：：１］".withASCIIAddress, "[fd7a::1]")
        XCTAssertEqual("～".withASCIIAddress, "~")
    }

    func testAsciiAndIdnStayAsTheyWere() {
        XCTAssertEqual("192.168.1.5:9777".withASCIIAddress, "192.168.1.5:9777")
        XCTAssertEqual("".withASCIIAddress, "")
        XCTAssertEqual("bücher.example".withASCIIAddress, "bücher.example")
        XCTAssertEqual("日本語.jp".withASCIIAddress, "日本語.jp")
        XCTAssertEqual("пример．рф".withASCIIAddress, "пример.рф")
    }
}
