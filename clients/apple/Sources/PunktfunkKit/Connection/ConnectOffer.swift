// What one connect offers the host, resolved from the session's settings and this device's
// decoders, and what the user reads when the dial fails. The host answers in `Welcome`; a live
// session reads that answer from the connection, never the offer.

import Foundation

public struct ConnectOffer: Equatable, Sendable {
    public var width: UInt32
    public var height: UInt32
    public var hz: UInt32
    public var compositor: PunktfunkConnection.Compositor
    /// 0 = Automatic.
    public var bitrateKbps: UInt32
    public var audioChannels: UInt8
    /// The format asked for. Only the host's gate knows the datagram size, so it may decline it.
    public var audioRateHz: UInt32
    public var audioBits: UInt8
    /// HDR is on and this display can show it.
    public var hdr: Bool
    public var tenBit: Bool
    /// The 4:4:4 setting, before `videoCaps` gates it on a real-time decoder.
    public var want444: Bool
    public var videoCaps: UInt8
    public var videoCodecs: UInt8
    public var preferredCodec: UInt8
    public var clientCaps: UInt8
    public var videoFit: UInt8

    #if os(macOS)
    public static let onMac = true
    #else
    public static let onMac = false
    #endif

    /// The offer for `effective` at `mode`. The decoder probes are lazy: PyroWave's runs only for
    /// the PyroWave codec, the 10-bit 4:4:4 one only for a 10-bit offer. The caller keeps them off
    /// the main actor. `mac` picks the client caps: the Mac draws the host cursor itself in the
    /// desktop mouse model.
    public static func resolve(
        _ effective: EffectiveSettings,
        mode: (width: UInt32, height: UInt32, hz: UInt32),
        displayHDR: Bool,
        pyroWave pyroWaveDecodes: @autoclosure () -> Bool,
        av1: @autoclosure () -> Bool,
        hw444EightBit: @autoclosure () -> Bool,
        hw444TenBit: @autoclosure () -> Bool,
        mac: Bool = onMac
    ) -> ConnectOffer {
        let preferredCodec = PunktfunkConnection.codecByte(effective.codec)
        // PyroWave is a pure opt-in, gated on the Metal probe: a device that fails it falls back
        // to H.26x and keeps the user's rate. PyroWave itself always sends Automatic (0), so the
        // host pins its per-mode rate under the operator's ceiling.
        let pyroWave = preferredCodec == PunktfunkConnection.codecPyroWave && pyroWaveDecodes()
        let hdr = effective.hdrEnabled && displayHDR
        // HDR carries the depth; `tenBitSdr` adds it where HDR is off or the display can't show it.
        let tenBit = hdr || effective.tenBitSdr
        // 4:4:4 needs a real-time decoder: PyroWave's Metal one, or hardware HEVC 4:4:4 at both
        // depths when 10-bit is asked for, since the host may still send 8-bit.
        let decode444 = pyroWave || (hw444EightBit() && (!tenBit || hw444TenBit()))
        // VideoToolbox has no software AV1 decoder, so AV1 is offered only on hardware.
        var videoCodecs = PunktfunkConnection.codecH264 | PunktfunkConnection.codecHEVC
        if av1() { videoCodecs |= PunktfunkConnection.codecAV1 }
        if pyroWave { videoCodecs |= PunktfunkConnection.codecPyroWave }
        let desktopMouse = (MouseInputMode(rawValue: effective.mouseMode) ?? .capture) == .desktop
        let presentCaps: UInt8 = mac && desktopMouse ? PunktfunkConnection.clientCapCursor : 0
        let (audioRateHz, audioBits) = effective.audioFormatChoice.wire
        return ConnectOffer(
            width: mode.width, height: mode.height, hz: mode.hz,
            compositor: PunktfunkConnection.Compositor(
                rawValue: UInt32(clamping: effective.compositor)) ?? .auto,
            bitrateKbps: pyroWave ? 0 : UInt32(clamping: effective.bitrateKbps),
            audioChannels: UInt8(clamping: effective.audioChannels),
            audioRateHz: audioRateHz, audioBits: audioBits,
            hdr: hdr, tenBit: tenBit, want444: effective.enable444,
            videoCaps: PunktfunkConnection.videoCaps(
                tenBit: tenBit, hdr: hdr, chroma444: effective.enable444 && decode444),
            videoCodecs: videoCodecs, preferredCodec: preferredCodec,
            clientCaps: presentCaps
                | (effective.keepHostAudio ? PunktfunkConnection.clientCapKeepHostAudio : 0),
            videoFit: VideoFit(name: effective.videoFit).wire)
    }

    /// What the user reads when a dial to `hostName` fails; nil when the caller takes the failure
    /// over (`callerRecovers`, the wake wait). A host that answered states its own reason and is
    /// never woken. A delegated-approval dial always explains the approval step.
    public static func failureMessage(
        _ error: Error, hostName: String, pinned: Bool, requestAccess: Bool,
        callerRecovers: Bool
    ) -> String? {
        if case PunktfunkClientError.rejected(let rejection) = error {
            return "\(hostName): \(rejection.userMessage)"
        }
        if callerRecovers, !requestAccess { return nil }
        if requestAccess {
            return "\(hostName) didn't let this device in. "
                + "Approve it in the host's web console (port 47992 → Pairing), then "
                + "request access again — the request expires after a few minutes."
        }
        return pinned
            ? "Couldn't reach \(hostName) — it may be asleep, or its "
                + "identity changed since you paired. Pair with it again from "
                + "its host card."
            : "Couldn't reach \(hostName) — it may be asleep, or not "
                + "paired yet. Wake it, or pair with it from its host card."
    }
}
