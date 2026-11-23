// The files the host leaves in its config directory for local tools: the management endpoint,
// the two bearer tokens, the console password and its TLS identity. Read per request, never
// cached: the host mints new tokens at every start and can switch certificates while the UI runs.
import CryptoKit
import Foundation

struct HostFiles: Sendable {
    let dir: URL

    /// The default management port when `mgmt-endpoint` is absent or unreadable.
    static let defaultPort: UInt16 = 47990

    /// `host.env`'s `KEY=VALUE` grammar: `#` comments and lines without `=` skipped, both sides
    /// trimmed, surrounding quotes stripped, a later line wins.
    static func value(_ text: String, key: String) -> String? {
        var found: String?
        for raw in text.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            guard !line.hasPrefix("#"), let eq = line.firstIndex(of: "=") else { continue }
            guard line[..<eq].trimmingCharacters(in: .whitespaces) == key else { continue }
            found = line[line.index(after: eq)...].trimmingCharacters(in: .whitespaces)
                .trimmingCharacters(in: CharacterSet(charactersIn: "\""))
        }
        return found.flatMap { $0.isEmpty ? nil : $0 }
    }

    /// The port in `PUNKTFUNK_MGMT_URL=https://127.0.0.1:<port>`; nil for a blank or odd line.
    static func port(endpoint text: String) -> UInt16? {
        guard let line = text.split(whereSeparator: \.isNewline)
            .map({ $0.trimmingCharacters(in: .whitespaces) }).first(where: { !$0.isEmpty })
        else { return nil }
        let value = line.firstIndex(of: "=").map { String(line[line.index(after: $0)...]) } ?? line
        var trimmed = value.trimmingCharacters(in: .whitespaces)
        while trimmed.hasSuffix("/") { trimmed.removeLast() }
        guard let colon = trimmed.lastIndex(of: ":") else { return nil }
        return UInt16(trimmed[trimmed.index(after: colon)...])
    }

    /// The DER of the first certificate in a PEM text.
    static func der(pem text: String) -> Data? {
        let begin = "-----BEGIN CERTIFICATE-----", end = "-----END CERTIFICATE-----"
        guard let start = text.range(of: begin), let stop = text.range(of: end, range: start.upperBound..<text.endIndex)
        else { return nil }
        return Data(base64Encoded: String(text[start.upperBound..<stop.lowerBound]), options: .ignoreUnknownCharacters)
    }

    /// Accept the server only when its leaf is the host's own certificate.
    static func trusts(serverDER: Data, pin: Data?) -> Bool {
        guard let pin else { return false }
        return Data(SHA256.hash(data: serverDER)) == pin
    }

    func read(_ name: String) -> String? {
        try? String(contentsOf: dir.appendingPathComponent(name), encoding: .utf8)
    }

    var port: UInt16 { read("mgmt-endpoint").flatMap(Self.port(endpoint:)) ?? Self.defaultPort }
    var trayToken: String? { read("tray-token").flatMap { Self.value($0, key: "PUNKTFUNK_TRAY_TOKEN") } }
    var mgmtToken: String? { read("mgmt-token").flatMap { Self.value($0, key: "PUNKTFUNK_MGMT_TOKEN") } }
    /// The console's login password. The console swaps the line for a hash at the first sign-in
    /// with it, and this reads nil from then on.
    var consolePassword: String? { read("web-password").flatMap { Self.value($0, key: "PUNKTFUNK_UI_PASSWORD") } }

    /// SHA-256 of the identity certificate: `native-cert.pem`, else the GameStream `cert.pem`.
    var pin: Data? {
        guard let der = (read("native-cert.pem") ?? read("cert.pem")).flatMap(Self.der(pem:)) else { return nil }
        return Data(SHA256.hash(data: der))
    }

    var logsDir: URL { dir.appendingPathComponent("logs", isDirectory: true) }
}
