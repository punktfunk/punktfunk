// Where the package's resource bundle is. In the app it is Contents/Resources; after
// `swift build` it sits beside the library and the preview; under `swift test` a few folders
// above the test binary. SwiftPM's own Bundle.module looks beside the app bundle and ends the
// process when it finds nothing, so nothing here calls it.
import Foundation

enum HostUIResources {
    static let bundleName = "PunktfunkHostUI_PunktfunkHostUI.bundle"

    static let bundleURL: URL = {
        var image = Dl_info()
        var here: URL?
        if dladdr(#dsohandle, &image) != 0, let path = image.dli_fname {
            here = URL(fileURLWithPath: String(cString: path)).deletingLastPathComponent()
        }
        var candidates = [Bundle.main.resourceURL, Bundle.main.bundleURL]
        for _ in 0..<4 {
            candidates.append(here)
            here = here?.deletingLastPathComponent()
        }
        for case let dir? in candidates {
            let url = dir.appendingPathComponent(bundleName)
            if FileManager.default.fileExists(atPath: url.path) { return url }
        }
        // No bundle: every message reads its English default.
        return Bundle.main.bundleURL
    }()
}
