// What a connect offers the host. Every bit is a promise the decoder and the present path keep,
// so each gate here says which device answer the bit waits for.

import XCTest

@testable import PunktfunkKit

final class ConnectOfferTests: XCTestCase {
    private let mode: (width: UInt32, height: UInt32, hz: UInt32) = (2560, 1440, 120)

    private func offer(
        _ settings: EffectiveSettings, displayHDR: Bool = false, pyroWave: Bool = false,
        av1: Bool = false, hw444: (eightBit: Bool, tenBit: Bool) = (false, false),
        mac: Bool = false, probed: ((String) -> Void)? = nil
    ) -> ConnectOffer {
        func probe(_ name: String, _ answer: Bool) -> Bool {
            probed?(name)
            return answer
        }
        return ConnectOffer.resolve(
            settings, mode: mode, displayHDR: displayHDR,
            pyroWave: probe("pyrowave", pyroWave), av1: probe("av1", av1),
            hw444EightBit: probe("444-8", hw444.eightBit),
            hw444TenBit: probe("444-10", hw444.tenBit), mac: mac)
    }

    func testAPlainOfferCarriesTheSettings() {
        var s = EffectiveSettings()
        s.bitrateKbps = 40_000
        s.compositor = Int(PunktfunkConnection.Compositor.kwin.rawValue)
        s.audioChannels = 6
        s.hdrEnabled = false
        let o = offer(s)
        XCTAssertEqual([o.width, o.height, o.hz], [2560, 1440, 120])
        XCTAssertEqual(o.compositor, .kwin)
        XCTAssertEqual(o.bitrateKbps, 40_000)
        XCTAssertEqual(o.audioChannels, 6)
        XCTAssertEqual(o.audioRateHz, 48_000)
        XCTAssertEqual(o.audioBits, 16)
        XCTAssertEqual(o.videoCaps, 0)
        XCTAssertEqual(o.videoCodecs, PunktfunkConnection.codecH264 | PunktfunkConnection.codecHEVC)
        XCTAssertEqual(o.preferredCodec, 0)
        XCTAssertEqual(o.videoFit, VideoFit.fit.wire)
        XCTAssertEqual(o.clientCaps, 0)
    }

    /// HDR waits for a display that can show it; the depth rides along with it.
    func testHDRNeedsTheDisplay() {
        var s = EffectiveSettings()
        s.hdrEnabled = true
        let sdrPanel = offer(s, displayHDR: false)
        XCTAssertFalse(sdrPanel.hdr)
        XCTAssertFalse(sdrPanel.tenBit)
        XCTAssertEqual(sdrPanel.videoCaps, 0)

        let hdrPanel = offer(s, displayHDR: true)
        XCTAssertTrue(hdrPanel.hdr)
        XCTAssertTrue(hdrPanel.tenBit)
        XCTAssertEqual(
            hdrPanel.videoCaps, PunktfunkConnection.videoCap10Bit | PunktfunkConnection.videoCapHDR)

        s.hdrEnabled = false
        s.tenBitSdr = true
        XCTAssertEqual(offer(s, displayHDR: true).videoCaps, PunktfunkConnection.videoCap10Bit)
    }

    /// 4:4:4 needs hardware at every depth the host may send, and the 10-bit probe only runs for
    /// a 10-bit offer.
    func test444WaitsForAHardwareDecoderAtEveryDepth() {
        var s = EffectiveSettings()
        s.hdrEnabled = false
        s.enable444 = true
        var probed: [String] = []
        let eightBit = offer(s, hw444: (true, false), probed: { probed.append($0) })
        XCTAssertEqual(eightBit.videoCaps, PunktfunkConnection.videoCap444)
        XCTAssertFalse(probed.contains("444-10"), "an 8-bit offer never asks the 10-bit probe")

        s.tenBitSdr = true
        XCTAssertEqual(
            offer(s, hw444: (true, false)).videoCaps, PunktfunkConnection.videoCap10Bit,
            "8-bit hardware alone can't take a 10-bit 4:4:4 stream")
        XCTAssertEqual(
            offer(s, hw444: (true, true)).videoCaps,
            PunktfunkConnection.videoCap10Bit | PunktfunkConnection.videoCap444)

        s.enable444 = false
        XCTAssertEqual(offer(s, hw444: (true, true)).videoCaps, PunktfunkConnection.videoCap10Bit)
        XCTAssertFalse(offer(s, hw444: (true, true)).want444)
    }

    func testAV1IsOfferedOnlyWithAHardwareDecoder() {
        let s = EffectiveSettings()
        XCTAssertEqual(offer(s, av1: false).videoCodecs & PunktfunkConnection.codecAV1, 0)
        XCTAssertEqual(
            offer(s, av1: true).videoCodecs & PunktfunkConnection.codecAV1,
            PunktfunkConnection.codecAV1)
    }

    /// PyroWave is an opt-in behind the Metal probe: past it the codec is offered and preferred,
    /// sends Automatic bitrate and takes 4:4:4 without the VideoToolbox probes. A device that fails
    /// it keeps the user's rate on H.26x.
    func testPyroWaveIsAnOptInBehindItsProbe() {
        var s = EffectiveSettings()
        s.bitrateKbps = 500_000
        s.enable444 = true
        s.hdrEnabled = false
        var probed: [String] = []
        XCTAssertEqual(offer(s, pyroWave: true, probed: { probed.append($0) }).bitrateKbps, 500_000)
        XCTAssertFalse(probed.contains("pyrowave"), "another codec never asks the Metal probe")

        s.codec = "pyrowave"
        let passed = offer(s, pyroWave: true)
        XCTAssertEqual(passed.bitrateKbps, 0)
        XCTAssertEqual(passed.preferredCodec, PunktfunkConnection.codecPyroWave)
        XCTAssertEqual(
            passed.videoCodecs & PunktfunkConnection.codecPyroWave, PunktfunkConnection.codecPyroWave)
        XCTAssertEqual(passed.videoCaps, PunktfunkConnection.videoCap444)

        let failed = offer(s, pyroWave: false)
        XCTAssertEqual(failed.bitrateKbps, 500_000)
        XCTAssertEqual(failed.videoCodecs & PunktfunkConnection.codecPyroWave, 0)
        XCTAssertEqual(failed.videoCaps, 0)
    }

    /// Only the Mac draws the host cursor, and only in the desktop mouse model. Keeping host audio
    /// is a request on every platform.
    func testClientCapsFollowThePlatform() {
        var s = EffectiveSettings()
        XCTAssertEqual(offer(s, mac: true).clientCaps, 0)
        s.mouseMode = MouseInputMode.desktop.rawValue
        XCTAssertEqual(offer(s, mac: true).clientCaps, PunktfunkConnection.clientCapCursor)
        XCTAssertEqual(offer(s, mac: false).clientCaps, 0)
        s.keepHostAudio = true
        XCTAssertEqual(offer(s, mac: false).clientCaps, PunktfunkConnection.clientCapKeepHostAudio)
    }

    // MARK: - Failure wording

    /// A host that answered is never woken, and says why itself.
    func testARejectionStatesTheHostsReason() {
        let message = ConnectOffer.failureMessage(
            PunktfunkClientError.rejected(.busy), hostName: "Desk", pinned: true,
            requestAccess: false, callerRecovers: true)
        XCTAssertEqual(message, "Desk: \(HostRejection.busy.userMessage)")
    }

    /// A plain unreachable dial goes to the caller's wake wait; an approval dial never does.
    func testUnreachableGoesToTheWakeWaitUnlessAskingForAccess() {
        XCTAssertNil(
            ConnectOffer.failureMessage(
                PunktfunkClientError.connectFailed, hostName: "Desk", pinned: false,
                requestAccess: false, callerRecovers: true))
        let approval = ConnectOffer.failureMessage(
            PunktfunkClientError.connectFailed, hostName: "Desk", pinned: false,
            requestAccess: true, callerRecovers: true)
        XCTAssertTrue(approval?.hasPrefix("Desk didn't let this device in.") == true)
    }

    /// Without a wake wait, a pinned host may have a new identity; an unpinned one may need
    /// pairing.
    func testUnreachableWordingFollowsThePin() {
        let pinned = ConnectOffer.failureMessage(
            PunktfunkClientError.connectFailed, hostName: "Desk", pinned: true,
            requestAccess: false, callerRecovers: false)
        let unpinned = ConnectOffer.failureMessage(
            PunktfunkClientError.connectFailed, hostName: "Desk", pinned: false,
            requestAccess: false, callerRecovers: false)
        XCTAssertTrue(pinned?.contains("identity changed since you paired") == true)
        XCTAssertTrue(unpinned?.contains("or not paired yet") == true)
    }
}
