// The host's loopback management API, as the Windows and Linux trays use it: the summary with
// the tray token, everything else with the admin token. TLS is pinned to the host's identity
// certificate and nothing else is accepted. The shapes are api/openapi.json's; fields the UI
// does not read are left out and ignored.
import Foundation

struct LocalSummary: Decodable, Equatable, Sendable {
    struct Session: Decodable, Equatable, Sendable {
        var width: Int, height: Int, fps: Int
    }

    var version: String
    var videoStreaming: Bool
    var audioStreaming: Bool
    var session: Session?
    var clientName: String?
    var pinPending: Bool
    var pendingApprovals: Int
    var keptDisplays: Int?
}

struct RuntimeStatus: Decodable, Equatable, Sendable {
    struct Row: Decodable, Equatable, Sendable, Identifiable {
        var id: Int?
        var client: String
        var clientName: String?
        var mode: String
        var hdr: Bool
        var phase: String
    }

    struct Stream: Decodable, Equatable, Sendable {
        var bitrateKbps: Int
        var codec: String
    }

    var activeSessions: Int
    var sessions: [Row]
    var sessionId: Int?
    var stream: Stream?
}

struct NativePairStatus: Decodable, Equatable, Sendable {
    var enabled: Bool
    var armed: Bool
    var pin: String?
    var expiresInSecs: Int?
}

struct PendingDevice: Decodable, Equatable, Sendable, Identifiable {
    var id: Int
    var name: String
    var fingerprint: String
    var ageSecs: Int
    var source: String
    var relayed: Bool
    var unfamiliarOrigin: Bool
}

struct UpdateStatus: Decodable, Equatable, Sendable {
    struct Manifest: Decodable, Equatable, Sendable {
        var version: String
        var notesUrl: String
    }

    var available: Bool
    var manifest: Manifest?
}

struct DiagnosticsReport: Decodable, Equatable, Sendable {
    struct Check: Decodable, Equatable, Sendable {
        var id: String
        var status: String
        var summary: String
        var summaryId: String?
        var impact: String
        var impactId: String?
    }

    var checks: [Check]
}

/// The host's `virtual_controllers` verdict, worded by the row itself.
struct ControllerVerdict: Equatable {
    var available: Bool
    var text: String
    var impact: String?

    /// Nil when the row is absent or does not apply.
    static func of(_ report: DiagnosticsReport) -> ControllerVerdict? {
        guard let row = report.checks.first(where: { $0.id == "virtual_controllers" }) else { return nil }
        let text = words(row.summaryId, row.summary)
        switch row.status {
        case "ok": return ControllerVerdict(available: true, text: text)
        case "warn", "fail":
            return ControllerVerdict(available: false, text: text, impact: row.impact.isEmpty ? nil : words(row.impactId, row.impact))
        default: return nil
        }
    }

    /// The row's `host` message in the person's language, else the host's English.
    private static func words(_ id: String?, _ english: String) -> String {
        guard let id else { return english }
        return Bundle(url: HostUIResources.bundleURL)?.localizedString(forKey: id, value: english, table: nil) ?? english
    }
}

/// Why a call did not work, worded for the person: the host's own English sentence when it
/// sent one.
struct HostError: Error, Equatable {
    var reason: String
}

/// The access an arm or an approve grants, as the web console sends it (`grantFields`).
enum Access: Sendable {
    /// Full control until unpaired; a device paired before keeps what it had.
    case standard
    /// A guest: controller only, for `guestSeconds`.
    case guest
    /// A device that knocked from outside: controller only, until its last session ends.
    case remote

    static let controller = 0x01
    static let guestSeconds = 4 * 3600

    var fields: [String: Any] {
        switch self {
        case .standard: [:]
        case .guest: ["grants": Self.controller, "expires_in_secs": Self.guestSeconds]
        case .remote: ["grants": Self.controller, "until_disconnect": true]
        }
    }
}

struct HostAPI: Sendable {
    let files: HostFiles
    private let session: URLSession

    init(files: HostFiles) {
        self.files = files
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = 3
        config.connectionProxyDictionary = [:]
        session = URLSession(configuration: config, delegate: PinnedTrust(files: files), delegateQueue: nil)
    }

    private var base: String { "https://127.0.0.1:\(files.port)" }

    func summary() async throws -> LocalSummary {
        try await call("GET", "/api/v1/local/summary", token: files.trayToken)
    }

    func status() async throws -> RuntimeStatus { try await call("GET", "/api/v1/status") }
    func pairing() async throws -> NativePairStatus { try await call("GET", "/api/v1/native/pair") }
    func pending() async throws -> [PendingDevice] { try await call("GET", "/api/v1/native/pending") }
    func update() async throws -> UpdateStatus { try await call("GET", "/api/v1/update/status") }
    func diagnostics() async throws -> DiagnosticsReport { try await call("GET", "/api/v1/diagnostics") }

    /// A two-minute window, as the web console arms it; `boundTo` limits it to one fingerprint.
    func arm(boundTo fingerprint: String?, access: Access) async throws -> NativePairStatus {
        var body = access.fields
        body["ttl_secs"] = 120
        if let fingerprint { body["fingerprint"] = fingerprint }
        return try await call("POST", "/api/v1/native/pair/arm", body: body)
    }

    func disarm() async throws { try await send("DELETE", "/api/v1/native/pair") }
    func approve(_ id: Int, access: Access) async throws {
        try await send("POST", "/api/v1/native/pending/\(id)/approve", body: access.fields)
    }

    func deny(_ id: Int) async throws { try await send("POST", "/api/v1/native/pending/\(id)/deny") }
    func endSession(_ id: Int) async throws { try await send("DELETE", "/api/v1/session/\(id)") }
    func releaseDisplays() async throws { try await send("POST", "/api/v1/display/release", body: [:]) }
    func restart() async throws { try await send("POST", "/api/v1/actions/host.restart") }

    private func call<T: Decodable>(_ method: String, _ path: String, token: String? = nil, body: [String: Any]? = nil) async throws -> T {
        let data = try await send(method, path, token: token, body: body)
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            return try decoder.decode(T.self, from: data)
        } catch {
            throw HostError(reason: error.localizedDescription)
        }
    }

    @discardableResult
    private func send(_ method: String, _ path: String, token: String? = nil, body: [String: Any]? = nil) async throws -> Data {
        guard let url = URL(string: base + path), let token = token ?? files.mgmtToken else {
            throw HostError(reason: URLError(.userAuthenticationRequired).localizedDescription)
        }
        var req = URLRequest(url: url)
        req.httpMethod = method
        req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        if let body {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try JSONSerialization.data(withJSONObject: body)
        }
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            throw HostError(reason: error.localizedDescription)
        }
        let code = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard (200..<300).contains(code) else {
            let sentence = (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["error"] as? String
            throw HostError(reason: sentence ?? HTTPURLResponse.localizedString(forStatusCode: code))
        }
        return data
    }
}

/// Accepts the host's own certificate, compared by the SHA-256 of its DER, and nothing else.
private final class PinnedTrust: NSObject, URLSessionDelegate, Sendable {
    let files: HostFiles
    init(files: HostFiles) { self.files = files }

    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge) async
        -> (URLSession.AuthChallengeDisposition, URLCredential?)
    {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              let trust = challenge.protectionSpace.serverTrust,
              let leaf = (SecTrustCopyCertificateChain(trust) as? [SecCertificate])?.first,
              HostFiles.trusts(serverDER: SecCertificateCopyData(leaf) as Data, pin: files.pin)
        else { return (.cancelAuthenticationChallenge, nil) }
        return (.useCredential, URLCredential(trust: trust))
    }
}

/// Whether the web console answers on loopback. Any HTTP answer counts; it sends nothing secret,
/// so the console's certificate is not checked.
struct ConsoleProbe: Sendable {
    static let port = 47992
    private let session: URLSession

    init() {
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = 2
        config.connectionProxyDictionary = [:]
        session = URLSession(configuration: config, delegate: AnyTrust(), delegateQueue: nil)
    }

    func responds() async -> Bool {
        var req = URLRequest(url: URL(string: "https://127.0.0.1:\(Self.port)/login")!)
        req.httpMethod = "HEAD"
        return (try? await session.data(for: req)) != nil
    }
}

private final class AnyTrust: NSObject, URLSessionDelegate, Sendable {
    func urlSession(_ session: URLSession, didReceive challenge: URLAuthenticationChallenge) async
        -> (URLSession.AuthChallengeDisposition, URLCredential?)
    {
        guard let trust = challenge.protectionSpace.serverTrust else { return (.performDefaultHandling, nil) }
        return (.useCredential, URLCredential(trust: trust))
    }
}
