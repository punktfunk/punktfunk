// Canned states and the offscreen renderer behind HostUIPreview. A render hosts each view in a
// window that is never ordered on screen and draws it into a bitmap, so nothing shows; AppKit
// controls draw there as in an inactive window.
import AppKit
import SwiftUI

public enum Preview {
    public static let scenarios = [
        "starting", "idle", "streaming", "streaming-confirm", "pairing", "pending", "approve",
        "attention", "release", "unavailable", "elsewhere", "error",
    ]
    public static let setupPages = [
        "setup-welcome", "setup-move", "setup-screen", "setup-screen-asked", "setup-input",
        "setup-audio", "setup-login", "setup-done", "setup-done-unsigned",
    ]

    /// The host's controller row, its English left to the host messages the bundle carries.
    static func controllerRow(ok: Bool) -> DiagnosticsReport {
        let summary = ok ? "diag_virtual_controllers_ok_summary" : "diag_virtual_controllers_summary"
        let impact = ok ? "" : "diag_virtual_controllers_impact"
        return DiagnosticsReport(checks: [.init(id: "virtual_controllers", status: ok ? "ok" : "fail", summary: summary,
                                                summaryId: summary, impact: impact, impactId: impact)])
    }

    static let lanDevice = PendingDevice(id: 3, name: "kitchen-ipad", fingerprint: "a1b2c3d4e5f60718293a4b5c6d7e8f90",
                                         ageSecs: 40, source: "lan", relayed: false, unfamiliarOrigin: false)
    static let wanDevice = PendingDevice(id: 4, name: "lenas-phone", fingerprint: "0f1e2d3c4b5a69788796a5b4c3d2e1f0",
                                         ageSecs: 190, source: "wan", relayed: false, unfamiliarOrigin: false)

    static func summary(streaming: Bool = false, kept: Int = 0, pending: Int = 0) -> LocalSummary {
        LocalSummary(version: "0.48.0", videoStreaming: streaming, audioStreaming: streaming,
                     session: streaming ? .init(width: 2560, height: 1440, fps: 120) : nil,
                     clientName: streaming ? "studio-deck" : nil, pinPending: false,
                     pendingApprovals: pending, keptDisplays: kept)
    }

    /// A store holding one canned state; its actions do nothing.
    @MainActor public static func store(_ name: String) -> HostStore {
        let s = HostStore(context: HostContext(configDir: URL(fileURLWithPath: "/nonexistent"), version: "0.48.0"), live: false)
        s.state = .running(summary())
        s.login = .on
        switch name {
        case "starting": s.state = .starting
        case "streaming", "streaming-confirm":
            s.state = .running(summary(streaming: true))
            s.runtime = RuntimeStatus(activeSessions: 1, sessions: [
                .init(id: 7, client: "9f3a", clientName: "studio-deck", mode: "2560x1440@120", hdr: true, phase: "streaming"),
            ], sessionId: 7, stream: .init(bitrateKbps: 45000, codec: "hevc"))
            if name == "streaming-confirm" { s.confirm = .endSession(id: 7, name: "studio-deck") }
        case "pairing":
            s.pairing = NativePairStatus(enabled: true, armed: true, pin: "4815", expiresInSecs: 112)
            s.pairDeadline = .now.addingTimeInterval(112)
        case "pending", "approve":
            s.state = .running(summary(pending: 2))
            s.pending = [lanDevice, wanDevice]
            if name == "approve" { s.confirm = .approve(lanDevice) }
        case "attention", "release":
            s.state = .running(summary(kept: 2))
            s.grants = Grants(screen: false, input: true, audio: .unknown)
            s.update = UpdateStatus(available: true, manifest: .init(version: "0.49.0", notesUrl: "https://punktfunk.io"))
            s.consoleUp = false
            s.login = .needsApproval
            if name == "release" { s.confirm = .release }
        case "unavailable":
            s.state = .unavailable
            s.consoleUp = false
        case "elsewhere":
            s.inApplications = false
            s.login = .off
        case "error":
            s.error = L10n.trayMacPairFailed(why: URLError(.timedOut).localizedDescription)
        default: break
        }
        return s
    }

    @MainActor static func onboarding(_ name: String) -> Onboarding {
        let s = store("idle")
        s.grants = Grants(screen: false, input: true, audio: .unknown)
        s.login = .off
        s.inApplications = name != "setup-welcome" && name != "setup-move"
        let model = Onboarding(store: s, firstLaunch: true)
        let page: Step = switch name {
        case "setup-move": .move
        case "setup-screen", "setup-screen-asked": .screen
        case "setup-input": .input
        case "setup-audio": .audio
        case "setup-login": .login
        case "setup-done", "setup-done-unsigned": .done
        default: .welcome
        }
        model.index = model.steps.firstIndex(of: page) ?? 0
        model.screenAsked = name == "setup-screen-asked"
        if page == .done { model.controllers = ControllerVerdict.of(controllerRow(ok: name == "setup-done")) }
        return model
    }

    /// Writes `<light|dark>-<name>.png` for every scenario, setup page and the glyphs into
    /// `dir`, in the process's language.
    @MainActor public static func render(to dir: URL) throws {
        NSApplication.shared.setActivationPolicy(.prohibited)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        for dark in [false, true] {
            let prefix = dark ? "dark-" : "light-"
            for name in scenarios {
                try snapshot(PanelView(store: store(name)), dark: dark, to: dir.appendingPathComponent(prefix + name + ".png"))
            }
            for name in setupPages {
                try snapshot(OnboardingView(model: onboarding(name)), dark: dark, to: dir.appendingPathComponent(prefix + name + ".png"))
            }
            try snapshot(GlyphSheet(), dark: dark, to: dir.appendingPathComponent(prefix + "glyphs.png"))
        }
    }

    @MainActor private static func snapshot(_ view: some View, dark: Bool, to url: URL) throws {
        let host = NSHostingView(rootView: view.background(Color(nsColor: .windowBackgroundColor)))
        let appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        host.appearance = appearance
        let window = NSWindow(contentRect: .zero, styleMask: [.borderless], backing: .buffered, defer: true)
        window.appearance = appearance
        window.contentView = host
        let size = host.fittingSize
        window.setContentSize(size)
        host.frame = NSRect(origin: .zero, size: size)
        host.layoutSubtreeIfNeeded()
        guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size.width * 2),
                                         pixelsHigh: Int(size.height * 2), bitsPerSample: 8, samplesPerPixel: 4,
                                         hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                         bytesPerRow: 0, bitsPerPixel: 0)
        else { return }
        rep.size = size
        host.cacheDisplay(in: host.bounds, to: rep)
        try rep.representation(using: .png, properties: [:])?.write(to: url)
    }

    /// Runs the app on a canned state, without the host. Its actions do nothing.
    @MainActor public static func runCanned(_ name: String) -> Never {
        HostUIApp.run(store(name))
    }

    /// Runs the app against the management port in `configDir`, without the host process:
    /// the grants read are this process's own.
    @MainActor public static func runLive(configDir: URL, hostExecutable: URL?) -> Never {
        let context = HostContext(configDir: configDir, version: "dev", hostExecutable: hostExecutable)
        HostUIApp.run(HostStore(context: context, live: true))
    }
}

/// Every glyph state at menu bar size, then its 36 px (Retina) and 18 px rasters enlarged
/// pixel for pixel.
private struct GlyphSheet: View {
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            row(px: nil)
            row(px: 36)
            row(px: 18)
            BrandMark(size: 64)
        }
        .padding(16)
    }

    private func row(px: Int?) -> some View {
        HStack(spacing: px == nil ? 16 : 24) {
            ForEach(GlyphState.allCases, id: \.self) { state in
                Image(nsImage: px.map { raster(Glyph.image(state), px: $0) } ?? Glyph.image(state))
                    .renderingMode(.template)
                    .resizable()
                    .interpolation(.none)
                    .frame(width: px == nil ? Glyph.size : 72, height: px == nil ? Glyph.size : 72)
                    .foregroundStyle(.primary)
            }
        }
    }

    private func raster(_ image: NSImage, px: Int) -> NSImage {
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                                   samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                   bytesPerRow: 0, bitsPerPixel: 0)!
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        image.draw(in: NSRect(x: 0, y: 0, width: px, height: px))
        NSGraphicsContext.restoreGraphicsState()
        let out = NSImage(size: NSSize(width: px, height: px))
        out.addRepresentation(rep)
        out.isTemplate = true
        return out
    }
}
