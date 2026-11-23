// The C ABI punktfunk-host calls, version 1: Sources/CPunktfunkHostUI/include/pf_host_ui.h.
// Both entries run on the process's main thread, before and instead of the host's own AppKit
// loop; the host's work already runs on its own thread when `pf_host_ui_run` is called.
import AppKit
import CPunktfunkHostUI

extension HostContext {
    /// Nil for a null pointer or another ABI version.
    init?(_ raw: UnsafePointer<pf_host_ui_context>?) {
        guard let c = raw?.pointee, c.abi_version == UInt32(PF_HOST_UI_ABI_VERSION),
              let dir = c.config_dir, let version = c.version
        else { return nil }
        self.init(configDir: URL(fileURLWithPath: String(cString: dir)),
                  version: String(cString: version),
                  launchedByLoginAgent: c.launched_by_login_agent == 1,
                  hostExecutable: Bundle.main.executableURL)
    }
}

@_cdecl("pf_host_ui_abi_version")
public func hostUIABIVersion() -> UInt32 {
    UInt32(PF_HOST_UI_ABI_VERSION)
}

@_cdecl("pf_host_ui_prelaunch")
public func hostUIPrelaunch(_ raw: UnsafePointer<pf_host_ui_context>?) -> Int32 {
    guard let context = HostContext(raw), !context.launchedByLoginAgent else { return 0 }
    return LoginItem.handOffToAgent() ? 1 : 0
}

@_cdecl("pf_host_ui_run")
public func hostUIRun(_ raw: UnsafePointer<pf_host_ui_context>?) {
    let context = HostContext(raw)
    MainActor.assumeIsolated {
        guard let context else {
            // A context this library can't read: serve without a UI, as with no library.
            NSApplication.shared.setActivationPolicy(.prohibited)
            NSApplication.shared.run()
            exit(0)
        }
        HostUIApp.run(HostStore(context: context, live: true))
    }
}
