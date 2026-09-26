import XCTest

#if canImport(Metal)
import Metal
import PunktfunkCore

/// The console's C ABI end to end, the way the Swift host drives it: built over this process's
/// Metal device and queue, drawing into a texture Swift owns.
final class ConsoleABITests: XCTestCase {
    private var device: MTLDevice!
    private var queue: MTLCommandQueue!
    private var console: OpaquePointer!

    override func setUpWithError() throws {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        self.device = device
        self.queue = queue
        console = try XCTUnwrap(
            punktfunk_console_new(
                #"{"device_name": "Test", "gpu_cache_bytes": 0, "settings": {}}"#,
                Unmanaged.passUnretained(device).toOpaque(),
                Unmanaged.passUnretained(queue).toOpaque()))
    }

    override func tearDown() {
        punktfunk_console_free(console)
        console = nil
    }

    /// Home, drawn into an offscreen BGRA texture and read back after Skia's submit.
    func testDrawsHomeIntoATexture() throws {
        let (width, height) = (640, 360)
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .bgra8Unorm, width: width, height: height, mipmapped: false)
        desc.usage = [.renderTarget, .shaderRead]
        desc.storageMode = .private
        let texture = try XCTUnwrap(device.makeTexture(descriptor: desc))
        XCTAssertTrue(
            punktfunk_console_frame(
                console, Unmanaged.passUnretained(texture).toOpaque(), UInt32(width),
                UInt32(height), PunktfunkInsets(), 0))

        // Skia's work is on `queue`; a blit committed after it reads the finished frame.
        let bytesPerRow = width * 4
        let buffer = try XCTUnwrap(
            device.makeBuffer(length: bytesPerRow * height, options: .storageModeShared))
        let commands = try XCTUnwrap(queue.makeCommandBuffer())
        let blit = try XCTUnwrap(commands.makeBlitCommandEncoder())
        blit.copy(
            from: texture, sourceSlice: 0, sourceLevel: 0, sourceOrigin: MTLOrigin(),
            sourceSize: MTLSize(width: width, height: height, depth: 1), to: buffer,
            destinationOffset: 0, destinationBytesPerRow: bytesPerRow,
            destinationBytesPerImage: bytesPerRow * height)
        blit.endEncoding()
        commands.commit()
        commands.waitUntilCompleted()
        let pixels = UnsafeBufferPointer(
            start: buffer.contents().bindMemory(to: UInt32.self, capacity: width * height),
            count: width * height)
        XCTAssertGreaterThan(Set(pixels).count, 100, "Home paints a backdrop and text, not a flat fill")
    }

    /// What a TV binds its Menu button on: on a tab the press is the system's, and on a card
    /// or a screen deeper it is the console's. Screens arrive and leave on a spring, so this
    /// runs the frames that carry it, the way the display link does.
    func testRootIsWhereBackLeaves() throws {
        let texture = try offscreen(width: 320, height: 180)
        XCTAssertFalse(punktfunk_console_at_root(console), "a focused card is under its tab")
        // With no hosts, focus starts on Add Host; OK opens it, so Back has somewhere to go.
        XCTAssertTrue(punktfunk_console_menu(console, 4, 1))
        draw(texture, frames: 20)
        XCTAssertFalse(punktfunk_console_at_root(console), "a pushed screen is not the root")
        // Backing out can take more than one press — a field may claim the first — so what
        // matters is that Back stays the console's until it is home again.
        for _ in 0..<4 where !punktfunk_console_at_root(console) {
            XCTAssertTrue(punktfunk_console_menu(console, 5, 1), "Back is the console's here")
            draw(texture, frames: 20)
        }
        XCTAssertTrue(punktfunk_console_at_root(console), "Back walks back to the root")
    }

    private func offscreen(width: Int, height: Int) throws -> MTLTexture {
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .bgra8Unorm, width: width, height: height, mipmapped: false)
        desc.usage = [.renderTarget, .shaderRead]
        desc.storageMode = .private
        return try XCTUnwrap(device.makeTexture(descriptor: desc))
    }

    /// Frames at roughly a display's cadence: the springs advance on the clock, not on calls.
    private func draw(_ texture: MTLTexture, frames: Int) {
        for _ in 0..<frames {
            _ = punktfunk_console_frame(
                console, Unmanaged.passUnretained(texture).toOpaque(),
                UInt32(texture.width), UInt32(texture.height), PunktfunkInsets(), 0)
            Thread.sleep(forTimeInterval: 1.0 / 60)
        }
    }

    /// A pad's Back on a card climbs to its tab, where the press is the system's: an Apple app
    /// cannot close itself, so the console raises no Quit and `at_root` hands Menu to tvOS.
    func testBackClimbsToTheTabAndRaisesNoQuit() {
        XCTAssertTrue(punktfunk_console_menu(console, 5, 1), "Back on a card is the console's")
        XCTAssertTrue(punktfunk_console_at_root(console), "and lands on the tab")
        XCTAssertTrue(punktfunk_console_menu(console, 5, 1))
        var events: [String] = []
        while let raw = punktfunk_console_next_event(console) {
            events.append(String(cString: raw))
            punktfunk_console_string_free(raw)
        }
        XCTAssertFalse(events.contains(#"{"action":"Quit"}"#), "events: \(events)")
    }
}
#endif
