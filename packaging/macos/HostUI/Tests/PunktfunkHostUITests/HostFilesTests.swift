import CryptoKit
import Foundation
import Testing
@testable import PunktfunkHostUI

@Suite struct HostFilesTests {
    @Test func envFileGrammar() {
        let text = """
        # a comment
        PUNKTFUNK_TRAY_TOKEN = "first"
        OTHER=x
        PUNKTFUNK_TRAY_TOKEN=second
        """
        #expect(HostFiles.value(text, key: "PUNKTFUNK_TRAY_TOKEN") == "second")
        #expect(HostFiles.value(text, key: "OTHER") == "x")
        #expect(HostFiles.value(text, key: "MISSING") == nil)
        #expect(HostFiles.value("PUNKTFUNK_MGMT_TOKEN=\n", key: "PUNKTFUNK_MGMT_TOKEN") == nil)
    }

    @Test func endpointPort() {
        #expect(HostFiles.port(endpoint: "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47995\n") == 47995)
        #expect(HostFiles.port(endpoint: "https://127.0.0.1:48000/") == 48000)
        #expect(HostFiles.port(endpoint: "\n") == nil)
        #expect(HostFiles.port(endpoint: "PUNKTFUNK_MGMT_URL=\n") == nil)
    }

    @Test func filesInADirectory() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("hostui-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let files = HostFiles(dir: dir)
        #expect(files.port == HostFiles.defaultPort)
        #expect(files.trayToken == nil)
        #expect(files.pin == nil)
        try "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47991\n".write(to: dir.appendingPathComponent("mgmt-endpoint"), atomically: true, encoding: .utf8)
        try "PUNKTFUNK_TRAY_TOKEN=abc\n".write(to: dir.appendingPathComponent("tray-token"), atomically: true, encoding: .utf8)
        try "PUNKTFUNK_MGMT_TOKEN=def\n".write(to: dir.appendingPathComponent("mgmt-token"), atomically: true, encoding: .utf8)
        #expect(files.port == 47991)
        #expect(files.trayToken == "abc")
        #expect(files.mgmtToken == "def")
    }

    /// The pin is the SHA-256 of the certificate's DER; native-cert.pem wins over cert.pem.
    @Test func pinning() throws {
        let der = Data((0..<64).map { UInt8($0) })
        let pem = "-----BEGIN CERTIFICATE-----\n\(der.base64EncodedString(options: .lineLength64Characters))\n-----END CERTIFICATE-----\n"
        #expect(HostFiles.der(pem: pem) == der)
        #expect(HostFiles.der(pem: "no certificate") == nil)

        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("hostui-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".write(to: dir.appendingPathComponent("cert.pem"), atomically: true, encoding: .utf8)
        try pem.write(to: dir.appendingPathComponent("native-cert.pem"), atomically: true, encoding: .utf8)
        let pin = HostFiles(dir: dir).pin
        #expect(pin == Data(SHA256.hash(data: der)))

        #expect(HostFiles.trusts(serverDER: der, pin: pin))
        #expect(!HostFiles.trusts(serverDER: der + Data([0]), pin: pin))
        #expect(!HostFiles.trusts(serverDER: der, pin: nil), "no pin accepts nothing")
    }
}
