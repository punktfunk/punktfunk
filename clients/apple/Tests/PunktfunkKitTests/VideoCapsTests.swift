// The video-capability byte, which is a wire promise: every bit says the decoder and the
// present path can take what the host is then free to send.
//
// Depth and HDR are separate asks. The pair that must not drift is "HDR without the depth":
// the host would upgrade the stream and hand the decoder Main10 the client never claimed.

import XCTest

@testable import PunktfunkKit

final class VideoCapsTests: XCTestCase {
    func testDepthAndHDRAreSeparateAsks() {
        XCTAssertEqual(
            PunktfunkConnection.videoCaps(tenBit: false, hdr: false, chroma444: false), 0,
            "nothing advertised is the 8-bit BT.709 SDR stream")

        // `ten_bit_sdr`: Main10 for a desktop, and no HDR promise with it.
        XCTAssertEqual(
            PunktfunkConnection.videoCaps(tenBit: true, hdr: false, chroma444: false),
            PunktfunkConnection.videoCap10Bit)

        // HDR carries the depth even when the ten-bit ask was off — the two cannot separate
        // in that direction, because HDR10 is Main10.
        XCTAssertEqual(
            PunktfunkConnection.videoCaps(tenBit: false, hdr: true, chroma444: false),
            PunktfunkConnection.videoCap10Bit | PunktfunkConnection.videoCapHDR)

        XCTAssertEqual(
            PunktfunkConnection.videoCaps(tenBit: true, hdr: true, chroma444: true),
            PunktfunkConnection.videoCap10Bit | PunktfunkConnection.videoCapHDR
                | PunktfunkConnection.videoCap444)

        // 4:4:4 rides on whatever depth was asked for, and never implies one.
        XCTAssertEqual(
            PunktfunkConnection.videoCaps(tenBit: false, hdr: false, chroma444: true),
            PunktfunkConnection.videoCap444)
    }
}
