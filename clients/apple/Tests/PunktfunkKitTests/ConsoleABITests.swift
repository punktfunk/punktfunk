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

    /// A pad's Back at Home is not the console's: tvOS hands Menu to the system, and the shell
    /// says why with a Quit action.
    func testBackAtTheRootGoesToTheSystem() {
        XCTAssertFalse(punktfunk_console_menu(console, 5, 1))
        var events: [String] = []
        while let raw = punktfunk_console_next_event(console) {
            events.append(String(cString: raw))
            punktfunk_console_string_free(raw)
        }
        XCTAssertTrue(events.contains(#"{"action":"Quit"}"#), "events: \(events)")
    }
}
#endif
