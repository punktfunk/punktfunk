// A CJK keyboard types full-width digits; a PIN or a port has to reach the host in ASCII.

import XCTest

@testable import PunktfunkShared

final class AsciiDigitsTests: XCTestCase {
    func testAsciiStaysAsItWas() {
        XCTAssertEqual("0123456789".asciiDigits, "0123456789")
        XCTAssertEqual("0123456789".withASCIIDigits, "0123456789")
        XCTAssertEqual("".asciiDigits, "")
    }

    func testFullWidthAndArabicIndicDigitsMapToAscii() {
        XCTAssertEqual("０１２３４５６７８９".asciiDigits, "0123456789")
        XCTAssertEqual("４８２１".asciiDigits, "4821")
        XCTAssertEqual("٠١٢٣".asciiDigits, "0123")
        XCTAssertEqual("4８".asciiDigits, "48")
    }

    func testEverythingElseIsDropped() {
        XCTAssertEqual(" 1a 2-".asciiDigits, "12")
        XCTAssertEqual("ＡＢ　:".asciiDigits, "")
        // A number value is not a digit: `一` is one, `十` is ten, `②` is circled.
        XCTAssertEqual("一十②²½".asciiDigits, "")
    }

    func testWithASCIIDigitsKeepsWhatIsNotADigit() {
        XCTAssertEqual("９７a 一".withASCIIDigits, "97a 一")
        XCTAssertEqual("ＡＢ".withASCIIDigits, "ＡＢ")
    }
}
