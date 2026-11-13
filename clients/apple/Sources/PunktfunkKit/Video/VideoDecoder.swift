// Stage-2 presenter, decode half: explicit VideoToolbox decode of the host's AUs (H.264 /
// HEVC / AV1 — whatever the Welcome resolved).
//
// Stage-1 hands compressed samples to AVSampleBufferDisplayLayer, which decodes AND presents
// internally with no per-frame callback — so neither decode-completion nor present can be
// stamped, and frames can't be hand-paced. Here we drive VTDecompressionSession ourselves: the
// output callback delivers a decoded CVPixelBuffer, we stamp decode-completion and hand the frame
// to `onDecoded`. Stage2Pipeline's header describes where it goes from there.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// A decoded frame's pixels — which present path they take. VideoToolbox codecs deliver a
/// biplanar `CVPixelBuffer` (NV12/P010/444v/x444); the PyroWave Metal decoder delivers three
/// separate R8 plane textures straight off its compute pass (there is no CVPixelBuffer — the
/// planes never leave the GPU).
public enum ReadyImage: @unchecked Sendable {
    /// 8-bit NV12 / 4:4:4 biplanar (SDR) or 10-bit P010 / x444 (HDR), Metal-compatible.
    /// `isHDR` = the stream is BT.2020 PQ and the presenter must configure EDR output.
    case video(CVPixelBuffer, isHDR: Bool)
    #if canImport(Metal)
    /// PyroWave planar output (Y full-res + Cb/Cr half-res, 8-bit SDR) with its precomputed
    /// CSC rows — presented by `MetalVideoPresenter.renderPlanar`.
    case planar(WaveletPlanes)
    #endif
}

/// One decoded frame waiting to be presented. Owns its image (a retained `CVPixelBuffer`, or
/// the PyroWave ring textures) until shown.
public struct ReadyFrame: @unchecked Sendable {
    /// Host capture clock (the AU's pts), in nanoseconds.
    public let ptsNs: UInt64
    /// Client `CLOCK_REALTIME` instant the AU left `nextAU` (`AccessUnit.pulledNs`, threaded
    /// through the decode via the frame refcon), in nanoseconds — the decode stage's start
    /// point. (Named for its historical role; since the ABI v9 receipt split the true
    /// reassembly receipt lives on `AccessUnit.receivedNs`, and receipt→pull is the HUD's own
    /// client-queue term.) 0 when unknown (a caller that didn't stamp) — the decode-stage meter
    /// then drops the sample via its sanity guard.
    public let receivedNs: Int64
    /// Client `CLOCK_REALTIME` instant decode completed, in nanoseconds.
    public let decodedNs: Int64
    /// The decoded image and which present path it takes.
    public let image: ReadyImage
    /// The AU's wire `user_flags` (`AccessUnit.flags`), threaded through the decode via the frame
    /// context so the re-anchor gate can classify this decoded frame (IDR / RFI anchor / recovery
    /// mark) at present time — the async decode callback has no other access to it. 0 when unknown.
    public let flags: UInt32
    /// When this frame is DUE on the SOURCE's cadence, in `CACurrentMediaTime` seconds — the
    /// domain the present path schedules against (`presentAtMediaTime`, `VsyncClock`). Stamped
    /// where the frame enters the ready store, by the pipeline's `CadenceClock`; `nil` under the
    /// latency intent, which has no clock and presents on arrival. May be in the PAST: that is a
    /// late frame, and the contract is "already due ⇒ present at the next opportunity".
    public var dueMediaTime: CFTimeInterval?

    /// The VideoToolbox path's buffer; nil for a PyroWave planar frame. (Kept as the accessor
    /// the decode round-trip tests assert against.)
    public var pixelBuffer: CVPixelBuffer? {
        if case .video(let buffer, _) = image { return buffer }
        return nil
    }

    /// Whether this frame presents on the HDR path. PyroWave planar frames are 8-bit SDR by
    /// contract.
    public var isHDR: Bool {
        if case .video(_, let hdr) = image { return hdr }
        return false
    }
}

/// Per-frame context threaded through the VideoToolbox frame refcon: the AU's receipt instant (for
/// the decode-stage meter) and its wire `user_flags` (for the re-anchor gate). Retained across the
/// async decode and reclaimed exactly once — by the output callback for every frame VideoToolbox
/// accepts, or by `decode`'s error branch for a frame `DecodeFrame` rejected outright (the callback
/// then never fires). A tiny per-frame allocation, the price of smuggling two values (a 64-bit
/// instant plus the flags) through the single `void*` a bit-pattern scalar can't hold.
private final class FrameContext {
    let receivedNs: Int64
    let flags: UInt32
    init(receivedNs: Int64, flags: UInt32) {
        self.receivedNs = receivedNs
        self.flags = flags
    }
}

/// The C output callback can't capture context, so VideoToolbox hands it the refcon we set at
/// session creation — a pointer back to the owning `VideoDecoder`. The per-frame refcon is the
/// retained `FrameContext` set at submit; reclaim it here (balancing `passRetained`) and unpack the
/// AU's receipt instant (for the decode stage) and wire flags (for the re-anchor gate).
private let decoderOutputCallback: VTDecompressionOutputCallback = {
    refcon, frameRefcon, status, infoFlags, imageBuffer, pts, _ in
    guard let refcon else { return }
    let ctx = frameRefcon.map { Unmanaged<FrameContext>.fromOpaque($0).takeRetainedValue() }
    Unmanaged<VideoDecoder>.fromOpaque(refcon)
        .takeUnretainedValue()
        .handleDecoded(
            status: status, imageBuffer: imageBuffer, pts: pts,
            receivedNs: ctx?.receivedNs ?? 0, flags: ctx?.flags ?? 0,
            dropped: infoFlags.contains(.frameDropped))
}

/// Owns a `VTDecompressionSession` rebuilt whenever the format description changes (every IDR /
/// mode change, the same trigger stage-1 uses). Thread-safe: `decode` runs on the pump thread,
/// the output callback on a VT-managed thread; the only shared mutable state is the session +
/// format, guarded by `lock`. `@unchecked Sendable` — the lock enforces the contract.
public final class VideoDecoder: @unchecked Sendable {
    private let lock = NSLock()
    private var session: VTDecompressionSession?
    private var format: CMVideoFormatDescription?

    /// Called on the VT thread for each successfully decoded frame — stamp + enqueue, don't block.
    private let onDecoded: @Sendable (ReadyFrame) -> Void
    /// Called on the VT thread when a frame fails to decode (bad data / decoder reset) so the
    /// pump can re-gate on the next IDR.
    private let onDecodeError: @Sendable (OSStatus) -> Void

    /// Whether the stream is full-chroma 4:4:4 (`connection.isChroma444`). Selects the 4:4:4
    /// decode pixel format (orthogonal to bit depth / HDR). Read inside `createSessionLocked`
    /// under `lock`.
    private var chroma444 = false

    /// The stream's codec (`connection.videoCodec`). Drives the bitstream framing (H.264/HEVC NAL
    /// parsing vs AV1 OBU repack). Read under `lock`.
    private var codec: VideoCodec = .hevc

    /// The stream's encode depth (`connection.bitDepth`). 10 asks the decoder for a 10-bit buffer
    /// whether or not the stream is HDR: 10-bit SDR is Main10 under BT.709. Read inside
    /// `createSessionLocked` under `lock`.
    private var bitDepth: UInt8 = 8

    /// Where the core assembles the AUs, for samples over the AU itself. PUMP THREAD, which is
    /// the one thread that calls `decode`.
    var accessUnitMemory: AccessUnitSlotPool?

    public init(
        onDecoded: @escaping @Sendable (ReadyFrame) -> Void,
        onDecodeError: @escaping @Sendable (OSStatus) -> Void = { _ in }
    ) {
        self.onDecoded = onDecoded
        self.onDecodeError = onDecodeError
    }

    deinit { teardown() }

    /// Select the chroma subsampling of the decode output (4:2:0 vs full-chroma 4:4:4) from
    /// `connection.isChroma444`, at session start and on each new-format AU. Takes effect on the
    /// next session (re)build. Thread-safe.
    public func setChroma444(_ on: Bool) {
        lock.lock()
        chroma444 = on
        lock.unlock()
    }

    /// Select the codec (H.264 / HEVC / AV1) from `connection.videoCodec`, at session start and on
    /// each new-format AU. Thread-safe.
    public func setCodec(_ c: VideoCodec) {
        lock.lock()
        codec = c
        lock.unlock()
    }

    /// Select the decode bit depth from `connection.bitDepth` (8 or 10), at session start and on
    /// each new-format AU. Takes effect on the next session (re)build. Thread-safe.
    public func setBitDepth(_ bits: UInt8) {
        lock.lock()
        bitDepth = bits
        lock.unlock()
    }

    /// Submit one AU for asynchronous decode, (re)creating the session if `format` changed. The
    /// caller resolves `format` from the keyframe exactly as stage-1 does
    /// (`VideoCodec.formatDescription(fromKeyframe:)`). Returns false if the session couldn't be
    /// created or the frame couldn't be submitted.
    @discardableResult
    public func decode(au: AccessUnit, format newFormat: CMVideoFormatDescription) -> Bool {
        lock.lock()
        let needsNew: Bool = {
            guard let session, let format else { return true }
            if CMFormatDescriptionEqual(format, otherFormatDescription: newFormat) { return false }
            // The output pixel format is chosen from the format's HDR-ness at session creation, so
            // an SDR→HDR flip the live session would otherwise "accept" must still rebuild — it
            // would keep emitting 8-bit NV12 for a PQ stream and the presenter would stay SDR.
            if Self.isHDRFormat(newFormat) != Self.isHDRFormat(format) { return true }
            // A new desc that the live session can still accept (rare for HEVC) avoids a rebuild.
            return !VTDecompressionSessionCanAcceptFormatDescription(session, formatDescription: newFormat)
        }()
        if needsNew, !createSessionLocked(format: newFormat) {
            lock.unlock()
            return false
        }
        // Adopt it on the accept path too: leaving `format` stale re-runs the compare and the
        // accept probe on every single AU.
        format = newFormat
        // Submit WHILE holding the lock so a concurrent reset()/teardown (main thread) can't
        // invalidate the session between here and DecodeFrame. The VT output callback takes the
        // ring lock, not this one, so there's no re-entrancy. DecodeFrame is async — non-blocking.
        guard let session,
              let sample = codec.sampleBuffer(au: au, format: newFormat, in: accessUnitMemory)
        else { lock.unlock(); return false }
        var infoOut = VTDecodeInfoFlags()
        // The AU's receipt instant + wire flags ride through as a retained context; the output
        // callback reclaims it. Retain immediately before submit so no early return can leak it.
        // The decode stage starts at the PULL (the AU leaving nextAU), not the reassembly
        // receipt: both consumers — the decode-stage meter and the ABR decode signal — are
        // specified from the pull, and the receipt→pull wait is the HUD's separate client-queue
        // term (see AccessUnit.pulledNs).
        let ctx = FrameContext(receivedNs: au.pulledNs, flags: au.flags)
        let refcon = Unmanaged.passRetained(ctx).toOpaque()
        let status = VTDecompressionSessionDecodeFrame(
            session,
            sampleBuffer: sample,
            flags: [._EnableAsynchronousDecompression],
            frameRefcon: refcon,
            infoFlagsOut: &infoOut)
        lock.unlock()
        if status != noErr {
            // DecodeFrame rejected the frame outright — the output callback will NOT fire, so
            // reclaim the context here (balancing passRetained) to avoid leaking it.
            Unmanaged<FrameContext>.fromOpaque(refcon).release()
            onDecodeError(status)
            return false
        }
        return true
    }

    /// Drop the session — the next `decode` rebuilds it. Used on stop and to recover from a
    /// wedged decoder (re-gates on the next in-band parameter sets, like stage-1's flush).
    public func reset() {
        lock.lock()
        teardownLocked()
        lock.unlock()
    }

    private func teardown() {
        lock.lock()
        teardownLocked()
        lock.unlock()
    }

    private func teardownLocked() {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        session = nil
        format = nil
    }

    /// True when `newFormat` carries a PQ (SMPTE ST 2084) or HLG transfer function — i.e. the host
    /// is sending HDR (BT.2020). VideoToolbox populates the transfer-function extension from the
    /// HEVC VUI, so this picks the decode bit depth (10-bit P010/x444 vs 8-bit NV12/444v) from the
    /// stream — and can flip mid-session (a game entering HDR re-inits the host encoder). The
    /// presenter follows the decoded frame's resulting `isHDR`, not the Welcome's latched flag
    /// (`render` reconciles the layer per frame via the idempotent `configure(hdr:)`).
    static func isHDRFormat(_ format: CMVideoFormatDescription) -> Bool {
        guard
            let tf = CMFormatDescriptionGetExtension(
                format, extensionKey: kCMFormatDescriptionExtension_TransferFunction)
        else { return false }
        let s = tf as? String
        return s == (kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ as String)
            || s == (kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG as String)
    }

    /// PQ/HLG verdict from a decoded buffer's own transfer attachment, or `nil` when it carries
    /// none. Same rule as `isHDRFormat`, read per frame on the VT thread where the session's
    /// format is not ours to touch.
    static func transferIsHDR(_ buffer: CVImageBuffer) -> Bool? {
        guard let att = CVBufferCopyAttachment(buffer, kCVImageBufferTransferFunctionKey, nil),
            CFGetTypeID(att) == CFStringGetTypeID()
        else { return nil }
        let s = unsafeDowncast(att, to: CFString.self)
        return CFEqual(s, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ)
            || CFEqual(s, kCVImageBufferTransferFunction_ITU_R_2100_HLG)
    }

    /// Replace the session for `newFormat` while `lock` is held.
    ///
    /// Chroma and HDR signaling choose the biplanar output format. Every output is both Metal-
    /// compatible and IOSurface-backed, allowing either renderer to consume the same decoded image
    /// without a copy. Hardware-only formats fail session creation rather than silently falling back
    /// to a decoder that cannot sustain the stream.
    ///
    /// Returns false if VideoToolbox refuses the requested configuration.
    private func createSessionLocked(format newFormat: CMVideoFormatDescription) -> Bool {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        session = nil
        format = nil

        // Decode pixel format is a 2×2 of (chroma, depth), both biplanar so the presenter binds
        // plane 0 = luma, plane 1 = interleaved chroma uniformly — 4:4:4 just delivers a full-size
        // chroma plane. Depth is its own axis: HDR is always 10-bit, and a 10-bit SDR session
        // (Main10 under BT.709) takes the same P010 / `x444` buffer without being HDR.
        let tenBit = Self.isHDRFormat(newFormat) || bitDepth >= 10
        let pixelFormat: OSType = {
            switch (chroma444, tenBit) {
            case (false, false): return kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange // NV12
            case (false, true): return kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange // P010
            case (true, false): return kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange // 444v
            case (true, true): return kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange // x444
            }
        }()
        let imageAttrs: [CFString: Any] = [
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
            kCVPixelBufferPixelFormatTypeKey: pixelFormat,
        ]
        var callback = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: decoderOutputCallback,
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque())
        // 4:4:4 and AV1 sessions REQUIRE a hardware decoder: both are only advertised when the
        // hardware gate passed (the 4:4:4 probe / `AV1.hardwareDecodeSupported`), so a
        // hardware-incapable mode (e.g. a resolution past a HW ceiling) must fail HERE,
        // synchronously, letting the pump's backstop end the session — rather than silently
        // falling back to a software decoder far too slow for a real-time stream. 4:2:0
        // H.264/HEVC keeps the software fallback (nil spec) as a robustness net.
        let spec: CFDictionary? =
            chroma444 || codec == .av1
            ? [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true] as CFDictionary
            : nil
        var newSession: VTDecompressionSession?
        let status = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: newFormat,
            decoderSpecification: spec,
            imageBufferAttributes: imageAttrs as CFDictionary,
            outputCallback: &callback,
            decompressionSessionOut: &newSession)
        guard status == noErr, let newSession else { return false }
        // Real-time hint: schedule this session for live-streaming latency rather than
        // throughput/efficiency. Best-effort — decoders that don't support the property
        // return an error, which is fine to ignore.
        VTSessionSetProperty(
            newSession, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        session = newSession
        format = newFormat
        return true
    }

    /// VT thread. Stamp decode-completion and enqueue, or report the error. `receivedNs` is the
    /// AU's receipt instant and `flags` its wire `user_flags`, both threaded through the frame refcon
    /// (0 = unknown).
    fileprivate func handleDecoded(
        status: OSStatus, imageBuffer: CVImageBuffer?, pts: CMTime, receivedNs: Int64,
        flags: UInt32, dropped: Bool
    ) {
        guard status == noErr, let imageBuffer else {
            // A frame VideoToolbox chose to DROP under load reports success with no image and the
            // dropped flag. Reporting it as a decode error armed the display freeze and asked the
            // host for keyframes, which is the opposite of what an overloaded decoder needs.
            if dropped, status == noErr { return }
            onDecodeError(status)
            return
        }
        let decodedNs = realtimeNowNs()
        // pts was stamped at timescale 1e9 (AnnexB.sampleBuffer); normalize defensively.
        let p = CMTimeConvertScale(pts, timescale: 1_000_000_000, method: .default)
        let ptsNs = p.value > 0 ? UInt64(p.value) : 0
        // HDR is the transfer function, never the depth: 10-bit SDR decodes to P010 too, and
        // calling that HDR paints the layer PQ and blows the picture out. A buffer carrying no
        // transfer attachment falls back to depth, as this did before.
        let fmt = CVPixelBufferGetPixelFormatType(imageBuffer)
        let tenBitBuffer =
            fmt == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            || fmt == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
            || fmt == kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange
            || fmt == kCVPixelFormatType_444YpCbCr10BiPlanarFullRange
        let isHDR = Self.transferIsHDR(imageBuffer) ?? tenBitBuffer
        onDecoded(
            ReadyFrame(
                ptsNs: ptsNs, receivedNs: receivedNs, decodedNs: decodedNs,
                image: .video(imageBuffer, isHDR: isHDR), flags: flags))
    }
}
