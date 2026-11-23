import CPunktfunkHostUI
import Foundation
import Testing
@testable import PunktfunkHostUI

private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase
    return try decoder.decode(type, from: Data(json.utf8))
}

@Suite struct ModelTests {
    /// Unknown fields (`conflicts`, `games`, `paired_clients`) are ignored; `kept_displays` may be absent.
    @Test func summaryDecodes() throws {
        let s = try decode(LocalSummary.self, """
        {"version":"0.48.0","video_streaming":true,"audio_streaming":true,
         "session":{"width":2560,"height":1440,"fps":120,"capture":null},"client_name":"deck",
         "paired_clients":1,"native_paired_clients":2,"pin_pending":false,"pending_approvals":1,
         "conflicts":["Sunshine (installed)"],"games":["Hades"]}
        """)
        #expect(s.session == .init(width: 2560, height: 1440, fps: 120))
        #expect(s.clientName == "deck")
        #expect(s.pendingApprovals == 1)
        #expect(s.keptDisplays == nil)
        #expect(HostState.running(s).keptDisplays == 0)
    }

    @Test func statusAndPairingDecode() throws {
        let status = try decode(RuntimeStatus.self, """
        {"video_streaming":true,"audio_streaming":true,"pin_pending":false,"paired_clients":0,
         "native_paired_clients":1,"active_sessions":1,"games":[],"session_id":7,
         "sessions":[{"id":7,"plane":"native","phase":"streaming","client":"9f3a","client_name":null,
                      "mode":"1920x1080@60","hdr":false,"join":false,"muted":false,"pads":[],"uptime_s":3}],
         "stream":{"width":1920,"height":1080,"fps":60,"bitrate_kbps":20000,"packet_size":1200,"min_fec":0,"codec":"hevc"}}
        """)
        #expect(status.sessions.first?.id == 7)
        #expect(status.stream?.codec == "hevc")
        let pair = try decode(NativePairStatus.self, #"{"enabled":true,"armed":true,"paired_clients":2,"pin":"1234","expires_in_secs":90}"#)
        #expect(pair.pin == "1234" && pair.expiresInSecs == 90)
        let pending = try decode([PendingDevice].self, """
        [{"id":3,"name":"ipad","fingerprint":"ab","age_secs":5,"source":"wan","relayed":true,
          "unfamiliar_origin":false,"until_disconnect":false,"origin":null}]
        """)
        #expect(pending.first?.relayed == true)
        let update = try decode(UpdateStatus.self, """
        {"install_kind":"source","channel":"stable","current_version":"0.48.0","apply":"notify","channel_hint":"",
         "check_disabled":false,"available":true,"not_published":false,
         "manifest":{"version":"0.49.0","serial":1,"published_at":"","notes_url":"https://x","stale":false}}
        """)
        #expect(update.manifest?.notesUrl == "https://x")
    }

    /// The tray's mapping, without a service manager: starting until the grace runs out.
    @Test func hostStateMapping() {
        let now = Date()
        let s = Preview.summary()
        #expect(HostState.map(s, unreachableSince: nil, now: now) == .running(s))
        #expect(HostState.map(nil, unreachableSince: nil, now: now) == .starting)
        #expect(HostState.map(nil, unreachableSince: now.addingTimeInterval(-14), now: now) == .starting)
        #expect(HostState.map(nil, unreachableSince: now.addingTimeInterval(-15), now: now) == .unavailable)
    }

    @Test func streamingAndAttention() {
        var s = Preview.summary(streaming: true)
        #expect(HostState.running(s).isStreaming)
        s.videoStreaming = false
        #expect(HostState.running(s).isStreaming, "a live session streams without the flag")
        #expect(!HostState.running(Preview.summary()).isStreaming)
        #expect(HostState.running(Preview.summary(pending: 1)).pairingAttention)
        #expect(!HostState.unavailable.pairingAttention)
    }

    @Test func headlinesFollowTheTray() {
        #expect(HostState.running(Preview.summary()).headline == "Punktfunk host 0.48.0 — idle")
        #expect(HostState.running(Preview.summary(streaming: true)).headline == "Punktfunk host 0.48.0 — streaming 2560×1440@120")
        #expect(HostState.running(Preview.summary(kept: 2)).line == "Idle · 2 displays kept")
        #expect(HostState.unavailable.line == "Status unavailable")
    }

    @Test func glyphs() {
        #expect(GlyphState.of(.unavailable, needsAttention: true) == .init(filled: false, badge: false, struck: true))
        #expect(GlyphState.of(.running(Preview.summary(streaming: true)), needsAttention: false) == .init(filled: true, badge: false, struck: false))
        #expect(GlyphState.of(.starting, needsAttention: true) == .init(filled: false, badge: true, struck: false))
        #expect(Set(GlyphState.allCases).count == 5)
    }

    @Test func connectEdge() {
        var edge = ConnectEdge()
        let fired = [true, false, true, true].map { edge.fires(streaming: $0) }
        // A stream already running at start is not a new connection.
        #expect(fired == [false, false, true, false])
    }

    @Test func formats() {
        #expect(Format.clock(112) == "1:52")
        #expect(Format.clock(-3) == "0:00")
        #expect(Format.deviceCode("a1b2c3d4e5f60718293a") == "A1B2 C3D4 E5F6 0718")
        #expect(Format.mode("2560x1440@120")! == (2560, 1440, 120))
        #expect(Format.mode("odd") == nil)
        #expect(Format.codec("hevc") == "HEVC")
    }

    /// `GET /diagnostics` as the host sends it: the controller row's own words, nothing when
    /// the row is absent or does not apply.
    @Test func controllerVerdict() throws {
        func report(_ row: String) throws -> DiagnosticsReport {
            try decode(DiagnosticsReport.self, """
            {"ran_at_unix":1,"checks":[
              {"id":"screen_recording","status":"ok","severity":"critical","summary":"s","impact":"","params":{},"source":"startup"},
              \(row)]}
            """)
        }
        let fail = try report("""
        {"id":"virtual_controllers","status":"fail","severity":"warning","source":"startup","params":{},
         "summary":"This build can't create virtual controllers","summary_id":"diag_vc_test_summary",
         "impact":"Games see no controller from clients.","impact_id":"diag_vc_test_impact",
         "remedy":{"text":"Install it from the disk image.","relogin_required":false},"remedy_id":"diag_vc_test_remedy"}
        """)
        #expect(ControllerVerdict.of(fail) == ControllerVerdict(
            available: false, text: "This build can't create virtual controllers", impact: "Games see no controller from clients."))
        let ok = try report("""
        {"id":"virtual_controllers","status":"ok","severity":"warning","source":"startup","params":{},
         "summary":"This build can create virtual controllers.","summary_id":"diag_vc_test_ok","impact":""}
        """)
        #expect(ControllerVerdict.of(ok) == ControllerVerdict(available: true, text: "This build can create virtual controllers."))
        let na = try report("""
        {"id":"virtual_controllers","status":"inapplicable","severity":"warning","source":"startup","params":{},
         "summary":"Not on this system.","impact":""}
        """)
        #expect(ControllerVerdict.of(na) == nil)
        #expect(ControllerVerdict.of(try report(#"{"id":"other","status":"fail","severity":"info","source":"startup","params":{},"summary":"x","impact":"y"}"#)) == nil)
    }

    @Test func access() {
        #expect(Access.standard.fields.isEmpty)
        #expect(Access.guest.fields["expires_in_secs"] as? Int == 14400)
        #expect(Access.remote.fields["until_disconnect"] as? Bool == true)
    }
}

@Suite struct OnboardingTests {
    @Test func stepsAndStart() {
        #expect(Step.all(inApplications: true) == [.welcome, .permissions, .login, .done])
        #expect(Step.all(inApplications: false).contains(.move))
        let steps = Step.all(inApplications: true)
        let none = Grants(screen: false, input: false, audio: .unknown)
        #expect(Step.start(in: steps, reached: nil, finished: false, grants: none) == 0)
        #expect(steps[Step.start(in: steps, reached: nil, finished: true, grants: none)] == .permissions)
        #expect(Step.start(in: steps, reached: nil, finished: true, grants: .all) == 0)
        let moving = Step.all(inApplications: false)
        #expect(moving[Step.start(in: moving, reached: nil, finished: true, grants: .all)] == .move)
    }

    /// A grant restarts the app with every grant in: the setup still comes back where it was.
    @Test func unfinishedSetupResumes() {
        let steps = Step.all(inApplications: true)
        #expect(steps[Step.start(in: steps, reached: .permissions, finished: false, grants: .all)] == .permissions)
        #expect(steps[Step.start(in: steps, reached: .login, finished: false, grants: .all)] == .login)
        // The move reopens the app from Applications, where its page no longer exists.
        #expect(steps[Step.start(in: steps, reached: .move, finished: false, grants: .all)] == .permissions)
    }

    /// The open approval is keyed by the device, not by its row: a refresh ages the row.
    @Test @MainActor func approvalSurvivesARefresh() {
        let store = Preview.store("approve")
        var aged = Preview.lanDevice
        aged.ageSecs += 2
        store.pending = [aged, Preview.wanDevice]
        #expect(store.isApproving(aged))
        #expect(!store.isApproving(Preview.wanDevice))
    }

    @Test @MainActor func reopenIsOfferedAfterAGrantWasAsked() {
        let model = Preview.onboarding("setup-permissions")
        #expect(!model.needsReopen)
        model.screenAsked = true
        #expect(model.needsReopen)
        model.grants.screen = true
        #expect(!model.needsReopen, "an allowed grant needs no restart")
    }

    @Test func consolePasswordReadsUntilHashed() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("pf-hostui-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let files = HostFiles(dir: dir)
        #expect(files.consolePassword == nil)
        try "PUNKTFUNK_UI_PASSWORD=preview-preview-0000\n".write(to: dir.appendingPathComponent("web-password"), atomically: true, encoding: .utf8)
        #expect(files.consolePassword == "preview-preview-0000")
        try "PUNKTFUNK_UI_PASSWORD_HASH=$argon2id$x\n".write(to: dir.appendingPathComponent("web-password"), atomically: true, encoding: .utf8)
        #expect(files.consolePassword == nil)
    }

    /// The mark's three circles: the cutter sits on the front disc's edge, toward the back one.
    @Test func markGeometry() {
        let g = MarkGeometry(side: 30)
        #expect(g.radius == 10)
        #expect(g.back == CGPoint(x: 10, y: 20) && g.front == CGPoint(x: 20, y: 10))
        let d = hypot(g.cutter.x - g.front.x, g.cutter.y - g.front.y)
        #expect(abs(d - g.radius) < 1e-9)
        #expect(g.cutter.x < g.front.x && g.cutter.y > g.front.y)
    }

    @Test func grantButton() {
        #expect(GrantAction.of(granted: false, asked: false) == .ask)
        #expect(GrantAction.of(granted: false, asked: true) == .openSettings)
        #expect(GrantAction.of(granted: true, asked: true) == GrantAction.none)
    }

    @Test func finishing() {
        #expect(Finish.of(want: true, isOn: false, available: true, grantAskedThisRun: true) == .register)
        #expect(Finish.of(want: true, isOn: true, available: true, grantAskedThisRun: true) == .restart)
        #expect(Finish.of(want: false, isOn: true, available: true, grantAskedThisRun: false) == .unregister)
        #expect(Finish.of(want: true, isOn: false, available: false, grantAskedThisRun: false) == .close)
    }

    /// A hand-over's note opens the panel once, and only while it is fresh.
    @Test func handOverNote() {
        let note = FileManager.default.temporaryDirectory.appendingPathComponent("pf-handover-\(UUID().uuidString)")
        #expect(!HandOver.take(note))
        HandOver.mark(note)
        #expect(HandOver.take(note))
        #expect(!HandOver.take(note), "the note is used up")
        HandOver.mark(note)
        #expect(!HandOver.take(note, now: .now + HandOver.window + 1))
        #expect(!FileManager.default.fileExists(atPath: note.path))
    }

    /// A translocated launch names no folder: its path is macOS's, not the reader's.
    @Test func translocatedFolderHasNoName() {
        let moved = URL(fileURLWithPath: "/private/var/folders/x3/T/AppTranslocation/0FB6770B/d/Punktfunk Host.app")
        #expect(Relocation.folderName(of: moved) == nil)
        #expect(Relocation.folderName(of: URL(fileURLWithPath: "/tmp/Punktfunk Host.app")) != nil)
    }

    @Test func requiredGrants() {
        #expect(!Grants.all.missingRequired)
        #expect(Grants(screen: true, input: true, audio: .denied).missingRequired == false, "audio is not required")
        #expect(Grants(screen: false, input: true, audio: .allowed).missingRequired)
    }
}

@Suite struct ShippingTests {
    @Test func abiVersion() {
        #expect(hostUIABIVersion() == 1)
        "/tmp/punktfunk".withCString { dir in
            "0.48.0".withCString { version in
                var c = pf_host_ui_context(abi_version: 1, config_dir: dir, version: version, launched_by_login_agent: 1)
                let context = HostContext(&c)
                #expect(context?.configDir.path == "/tmp/punktfunk")
                #expect(context?.launchedByLoginAgent == true)
                c.abi_version = 2
                #expect(HostContext(&c) == nil, "another ABI version is refused")
            }
        }
        #expect(HostContext(nil) == nil)
    }

    /// `swift build` must carry the German table into the resource bundle.
    @Test func germanShips() throws {
        let bundle = try #require(Bundle(url: HostUIResources.bundleURL))
        #expect(bundle.localizations.contains("de"))
        let table = try #require(bundle.path(forResource: "Localizable", ofType: "strings", inDirectory: nil, forLocalization: "de"))
        let strings = try #require(NSDictionary(contentsOfFile: table) as? [String: String])
        #expect(strings["tray_mac_quit"] == "Punktfunk Host beenden")
    }
}
