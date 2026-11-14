import XCTest

#if canImport(Metal)
import CoreVideo
import Metal
import QuartzCore
@testable import PunktfunkKit

final class MetalPresenterTests: XCTestCase {
    /// `MetalVideoPresenter.make()` compiles the runtime Metal shaders (the BT.709/BT.2020 YUV→RGB
    /// fragment shaders plus the Catmull-Rom luma sampler). A `nil` result on a GPU-equipped host
    /// means a shader failed to compile — this catches a malformed shader before it silently
    /// degrades stage-2 to a stage-1 fallback on device.
    func testPresenterInitCompilesShaders() throws {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device available in this environment")
        }
        XCTAssertNotNil(
            MetalVideoPresenter.make(),
            "stage-2 Metal shaders failed to compile (presenter init returned nil)")
    }

    /// The launch warm-up builds the shared pipelines off the main thread; `make()` then uses them.
    func testWarmBuildsThePipelinesOffTheMainThread() throws {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device available in this environment")
        }
        let warmed = expectation(description: "warmed")
        DispatchQueue.global(qos: .utility).async {
            XCTAssertFalse(Thread.isMainThread)
            MetalVideoPresenter.warm()
            warmed.fulfill()
        }
        wait(for: [warmed], timeout: 60)
        XCTAssertNotNil(MetalVideoPresenter.make(), "the warmed pipelines build a presenter")
    }

    /// Linear always takes half-float; the env lever picks the PQ drawable over the device rule.
    func testHDRDrawableFormat() {
        XCTAssertEqual(hdrDrawableFormat(linear: true, env: "10", halfFloatPQ: false), .rgba16Float)
        XCTAssertEqual(hdrDrawableFormat(linear: false, env: nil, halfFloatPQ: false), .bgr10a2Unorm)
        XCTAssertEqual(
            hdrDrawableFormat(linear: false, env: nil, halfFloatPQ: true), .rgba16Float,
            "an Apple TV older than the A15")
        XCTAssertEqual(hdrDrawableFormat(linear: false, env: "10", halfFloatPQ: true), .bgr10a2Unorm)
        XCTAssertEqual(hdrDrawableFormat(linear: false, env: "16", halfFloatPQ: false), .rgba16Float)
    }

    /// The layer colour space HDR takes under the session's `HdrMode`.
    private var hdrSpace: CFString {
        HdrMode.current == .linear ? CGColorSpace.extendedLinearDisplayP3 : CGColorSpace.itur_2100_PQ
    }

    /// `configure(hdr:)`: PQ mode tags BT.2100 PQ with EDR and a reference-white anchor
    /// (`edrMetadata`; without it PQ renders too bright). Linear mode tags extended-linear P3 with
    /// EDR and no metadata, on half-float. SDR is the plain 8-bit path, EDR off, no metadata, and a
    /// mid-session flip back must restore it fully.
    func testConfigureHDRSetsEDRAnchor() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: true)
        XCTAssertEqual(presenter.layer.pixelFormat, hdrDrawable, "HDR uses the HDR drawable")
        XCTAssertEqual(presenter.layer.colorspace?.name, hdrSpace)
        XCTAssertTrue(
            presenter.layer.wantsExtendedDynamicRangeContent, "EDR must be requested on all platforms")
        if HdrMode.current == .linear {
            XCTAssertEqual(hdrDrawable, .rgba16Float, "linear EDR values exceed 1")
            XCTAssertNil(presenter.layer.edrMetadata, "linear leaves the system nothing to tone-map")
        } else {
            XCTAssertNotNil(
                presenter.layer.edrMetadata, "PQ must anchor reference white via edrMetadata")
        }
        presenter.layer.drawableSize = CGSize(width: 64, height: 64)
        if let drawable = presenter.layer.nextDrawable() {
            XCTAssertEqual(
                drawable.texture.pixelFormat, hdrDrawable, "the layer must vend the HDR format")
        }

        // Mid-session HDR→SDR flip: the 8-bit path, EDR off, no metadata.
        presenter.configure(hdr: false)
        XCTAssertEqual(presenter.layer.pixelFormat, .bgra8Unorm, "SDR uses the plain 8-bit drawable")
        XCTAssertFalse(presenter.layer.wantsExtendedDynamicRangeContent)
        XCTAssertNil(presenter.layer.edrMetadata)
    }

    /// The shader filters luma only while it resamples: 1:1 on the pixel grid takes one tap.
    func testLumaIsOnGridOnlyAtOneToOneOnWholeTexels() {
        let target = CGSize(width: 2752, height: 2064)
        XCTAssertTrue(MetalVideoPresenter.lumaOnGrid(
            target: target, crop: CGRect(x: 0, y: 0, width: 2752, height: 2064)))
        XCTAssertFalse(MetalVideoPresenter.lumaOnGrid(
            target: target, crop: CGRect(x: 0, y: 0, width: 2560, height: 1920)), "upscale")
        XCTAssertFalse(MetalVideoPresenter.lumaOnGrid(
            target: target, crop: CGRect(x: 0.5, y: 0, width: 2752, height: 2064)), "half-texel")
        XCTAssertTrue(MetalVideoPresenter.lumaOnGrid(
            target: target, crop: CGRect(x: 8, y: 4, width: 2752, height: 2064)), "whole-texel crop")
    }

    /// Deadline pacing: once the link vends, layer writes wait for the link thread to apply them
    /// between vends, in order.
    func testDeferredLayerWritesWaitForTheLink() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: false)
        presenter.deferLayerWrites()
        presenter.reconcileLayer(decodedSize: CGSize(width: 64, height: 32), isHDR: true)
        XCTAssertEqual(presenter.layer.pixelFormat, .bgra8Unorm, "a queued write leaves the layer")
        XCTAssertNil(presenter.layer.edrMetadata)
        presenter.applyLayerWrites()
        XCTAssertEqual(presenter.layer.pixelFormat, hdrDrawable)
        XCTAssertEqual(presenter.layer.colorspace?.name, hdrSpace)
        XCTAssertEqual(presenter.layer.drawableSize, CGSize(width: 64, height: 32))
    }

    /// A 10-bit SDR session presents through the 10-bit SDR drawable: sRGB-tagged like 8-bit SDR,
    /// no EDR, no metadata — and the layer must actually vend that format, which is the one thing
    /// the SDK's stale "two supported values" comment cannot tell us. HDR outranks the depth flag,
    /// and an 8-bit session keeps the 8-bit drawable.
    func testConfigureTenBitSDRWidensTheDrawable() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: false, tenBitSDR: true)
        XCTAssertEqual(
            presenter.layer.pixelFormat, .bgr10a2Unorm, "10-bit SDR uses the 10-bit unorm drawable")
        XCTAssertNotNil(presenter.layer.colorspace, "10-bit SDR stays sRGB-tagged like 8-bit SDR")
        XCTAssertFalse(presenter.layer.wantsExtendedDynamicRangeContent, "10-bit SDR is not EDR")
        XCTAssertNil(presenter.layer.edrMetadata)
        presenter.layer.drawableSize = CGSize(width: 64, height: 64)
        if let drawable = presenter.layer.nextDrawable() {
            XCTAssertEqual(
                drawable.texture.pixelFormat, .bgr10a2Unorm,
                "the layer must vend the 10-bit format rather than fall back")
        }
        presenter.configure(hdr: true, tenBitSDR: true)
        XCTAssertTrue(presenter.layer.wantsExtendedDynamicRangeContent, "HDR outranks the depth flag")
        XCTAssertEqual(presenter.layer.colorspace?.name, hdrSpace, "the HDR tag, not the SDR one")
        presenter.configure(hdr: false, tenBitSDR: false)
        XCTAssertEqual(
            presenter.layer.pixelFormat, .bgra8Unorm, "an 8-bit session keeps the 8-bit drawable")
    }

    /// `render` of a 10-bit SDR buffer reconciles the layer to the 10-bit drawable by itself — the
    /// per-frame path, not only the session-start configure — and presents without trapping.
    func testRenderTenBitSDRFrameWidensTheDrawable() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: false)
        XCTAssertEqual(presenter.layer.pixelFormat, .bgra8Unorm)
        var pb: CVPixelBuffer?
        let attrs: [CFString: Any] = [kCVPixelBufferMetalCompatibilityKey: true]
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault, 256, 256, kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            attrs as CFDictionary, &pb)
        guard status == kCVReturnSuccess, let pixelBuffer = pb else {
            throw XCTSkip("couldn't allocate a P010 test pixel buffer")
        }
        XCTAssertTrue(MetalVideoPresenter.tenBitBuffer(pixelBuffer))
        _ = presenter.render(pixelBuffer, isHDR: false)
        XCTAssertEqual(
            presenter.layer.pixelFormat, .bgr10a2Unorm,
            "a 10-bit SDR frame reconciles the layer to the 10-bit drawable")
    }

    /// The PyroWave twin: 16-bit planes without PQ are a 10-bit SDR stream, and take the 10-bit
    /// drawable; 8-bit planes keep the plain one.
    func testRenderPlanarTenBitSDRWidensTheDrawable() throws {
        guard let presenter = MetalVideoPresenter.make(),
              let device = MTLCreateSystemDefaultDevice()
        else { throw XCTSkip("no Metal device available in this environment") }
        func planes(_ format: MTLPixelFormat) throws -> WaveletPlanes {
            func plane(_ side: Int) throws -> MTLTexture {
                let d = MTLTextureDescriptor.texture2DDescriptor(
                    pixelFormat: format, width: side, height: side, mipmapped: false)
                return try XCTUnwrap(device.makeTexture(descriptor: d))
            }
            return WaveletPlanes(
                y: try plane(64), cb: try plane(32), cr: try plane(32),
                csc: CscUniform(r0: .zero, r1: .zero, r2: .zero), pq: false)
        }
        _ = presenter.renderPlanar(try planes(.r16Unorm))
        XCTAssertEqual(presenter.layer.pixelFormat, .bgr10a2Unorm)
        _ = presenter.renderPlanar(try planes(.r8Unorm))
        XCTAssertEqual(presenter.layer.pixelFormat, .bgra8Unorm)
    }

    /// `render` with a freshly-allocated NV12 buffer must present without crashing or hanging — the
    /// main-thread present path is the highest-risk part of the stage-2 rewrite. (A headless CI with no
    /// display can still allocate a drawable from a CAMetalLayer; if it can't, render returns false,
    /// which is also a valid non-crashing outcome.)
    func testRenderDoesNotCrashOnNV12Frame() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: false)
        var pb: CVPixelBuffer?
        let attrs: [CFString: Any] = [kCVPixelBufferMetalCompatibilityKey: true]
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault, 256, 256, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            attrs as CFDictionary, &pb)
        guard status == kCVReturnSuccess, let pixelBuffer = pb else {
            throw XCTSkip("couldn't allocate a test pixel buffer")
        }
        // Just asserting it returns (true or false) without trapping — the layer may have no drawable
        // source headless, so a false return is acceptable.
        _ = presenter.render(pixelBuffer, isHDR: false)
    }

    /// The decoder's pool cycles a few surfaces and now and then replaces one. The cycling ones
    /// stay cached; the replaced one leaves once nothing has touched it for `idle` calls.
    func testTheSurfaceCacheLetsGoOfASurfaceThePoolStoppedCycling() {
        var cache = RecentCache<Int, String>(capacity: 48, idle: 10)
        func frame(_ key: Int) -> [String] {
            let hit = cache.touch(key)
            var gone = cache.sweep()
            if hit == nil { gone += cache.insert("s\(key)", for: key) }
            return gone
        }
        var gone: [String] = []
        for i in 0..<12 { gone += frame(i % 4) }
        XCTAssertEqual(gone, [], "four cycling surfaces")
        // The pool swaps surface 3 for surface 9.
        for i in 0..<40 { gone += frame([0, 1, 2, 9][i % 4]) }
        XCTAssertEqual(gone, ["s3"])
        XCTAssertEqual(cache.count, 4)
    }

    func testTheSurfaceCacheEvictsTheLeastRecentPastItsCapacity() {
        var cache = RecentCache<Int, String>(capacity: 2, idle: 1_000)
        _ = cache.touch(1)
        XCTAssertEqual(cache.insert("a", for: 1), [])
        _ = cache.touch(2)
        XCTAssertEqual(cache.insert("b", for: 2), [])
        XCTAssertEqual(cache.touch(1), "a")
        _ = cache.touch(3)
        XCTAssertEqual(cache.insert("c", for: 3), ["b"], "2 is the least recently touched")
        _ = cache.touch(3)
        XCTAssertEqual(cache.insert("c2", for: 3), ["c"], "the same key replaces its entry")
        XCTAssertEqual(cache.removeAll().sorted(), ["a", "c2"])
        XCTAssertEqual(cache.count, 0)
    }

    /// A parked pass reads its uniform when the GPU runs it. With three buffers, pass 4 takes
    /// pass 1's: it parks only once pass 1 has run, however far the GPU is behind.
    func testAParkedPassWaitsForItsUniformsLastPass() {
        let free = MetalVideoPresenter.latchUniformFree
        XCTAssertTrue(free(3, 0, 3), "the first three passes share no buffer")
        XCTAssertFalse(free(4, 0, 3), "pass 1 is still queued")
        XCTAssertTrue(free(4, 1, 3))
        XCTAssertFalse(free(9, 5, 3), "pass 6 is still queued")
        XCTAssertFalse(free(1, 0, 0), "no buffer was allocated")
    }
}
#endif
