// Unit tests for the game-library models — decoding the management API's GET /api/v1/library
// payload and the poster-art fallback order. (The network fetch itself isn't unit-tested; it's
// exercised live against a host.)

import XCTest
@testable import PunktfunkKit

final class LibraryClientTests: XCTestCase {
    func testDecodesLibraryPayload() throws {
        // A Steam entry (full art + launch) and a custom entry (sparse art, no launch) — the two
        // shapes the host's `GameEntry` serializes (note the host omits null fields).
        let json = """
        [
          {
            "id": "steam:570",
            "store": "steam",
            "title": "Dota 2",
            "art": {
              "portrait": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/library_600x900.jpg",
              "hero": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/library_hero.jpg",
              "logo": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/logo.png",
              "header": "https://cdn.cloudflare.steamstatic.com/steam/apps/570/header.jpg"
            },
            "launch": { "kind": "steam_appid", "value": "570" }
          },
          {
            "id": "custom:abc123",
            "store": "custom",
            "title": "Dolphin",
            "platform": "GameCube",
            "art": { "header": "https://example.com/dolphin.jpg" }
          }
        ]
        """.data(using: .utf8)!

        let games = try JSONDecoder().decode([GameEntry].self, from: json)
        XCTAssertEqual(games.count, 2)

        let steam = games[0]
        XCTAssertEqual(steam.id, "steam:570")
        XCTAssertEqual(steam.art.posterCandidates.count, 3)

        let custom = games[1]
        XCTAssertEqual(custom.store, "custom")
        XCTAssertNil(custom.art.portrait)
        // `platform` is the host's flattened `GameMeta.platform`: present on a rom-manager entry,
        // absent (nil) on a store-front title — the field the library's collections group by.
        XCTAssertEqual(custom.platform, "GameCube")
        XCTAssertNil(steam.platform)
    }

    private func page(_ ids: [String], next: String?) -> Data {
        let items = ids.map { #"{"id":"\#($0)","store":"custom","title":"\#($0)","art":{},"hidden":false}"# }
        let cursor = next.map { #","next_cursor":"\#($0)""# } ?? ""
        return Data(#"{"items":[\#(items.joined(separator: ","))],"total":4,"platforms":[]\#(cursor)}"#.utf8)
    }

    func testWalkFollowsTheCursorToTheLastPage() async throws {
        var asked: [String?] = []
        let games = try await LibraryClient.walkPages { cursor in
            asked.append(cursor)
            switch cursor {
            case nil: return self.page(["a", "b"], next: "c1")
            case "c1": return self.page(["c"], next: "c2")
            default: return self.page(["d"], next: nil)
            }
        }
        XCTAssertEqual(games.map(\.id), ["a", "b", "c", "d"])
        XCTAssertEqual(asked, [nil, "c1", "c2"])
    }

    func testWalkEndsOnACursorThatDoesNotMove() async throws {
        var calls = 0
        let games = try await LibraryClient.walkPages { _ in
            calls += 1
            return self.page(["a"], next: "stuck")
        }
        XCTAssertEqual(calls, 2)
        XCTAssertEqual(games.count, 2)
    }

    func testAFailedPageFailsTheWalk() async {
        do {
            _ = try await LibraryClient.walkPages { cursor in
                if cursor != nil { throw LibraryError.http(500) }
                return self.page(["a"], next: "c1")
            }
            XCTFail("the second page failed")
        } catch LibraryError.http(500) {
        } catch {
            XCTFail("unexpected \(error)")
        }
    }

    func testAPictureMustBeAPathOnTheHost() {
        let base = "https://10.0.0.2:47990"
        XCTAssertEqual(
            LibraryClient.pictureURL("/kid?v=1", base: base)?.absoluteString, base + "/kid?v=1")
        XCTAssertNil(LibraryClient.pictureURL("https://elsewhere.example/x.png", base: base))
        XCTAssertNil(LibraryClient.pictureURL(nil, base: base))
    }

    /// `clients/shared/library-profile-vectors.json`, the rule Rust and Kotlin replay too.
    func testALibraryFetchFollowsTheSharedProfileVectors() async throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/library-profile-vectors.json")
        let doc = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any]
        let cases = try XCTUnwrap(doc?["cases"] as? [[String: Any]])
        XCTAssertFalse(cases.isEmpty)
        for c in cases {
            let name = c["name"] as? String ?? ""
            let host = c["host"] as? [String: [String: Any]] ?? [:]
            let expect = c["expect"] as? [String: Any] ?? [:]
            var asked: [String] = []
            let shown = try? await LibraryClient.catalog(as: c["pick"] as? String) { path in
                asked.append(path)
                guard let answer = host[path], let status = answer["status"] as? Int else {
                    return HTTPResponse(status: 404, headers: [:], body: Data())
                }
                let body = try JSONSerialization.data(withJSONObject: answer["body"] as Any)
                return HTTPResponse(status: status, headers: [:], body: body)
            }
            XCTAssertEqual(asked, expect["asked"] as? [String], "\(name): asked")
            XCTAssertEqual(shown?.profile, expect["as"] as? String, "\(name): as")
            XCTAssertEqual(shown?.games.map(\.id), expect["ids"] as? [String], "\(name): ids")
        }
    }

    func testACursorIsEncodedIntoThePagePath() {
        XCTAssertEqual(LibraryClient.pagePath(cursor: nil), "/api/v1/library/page?limit=200")
        XCTAssertEqual(
            LibraryClient.pagePath(cursor: "a+b="), "/api/v1/library/page?limit=200&cursor=a%2Bb%3D")
    }

    /// An id stays one segment: a `/` or `?` in it can't reach another route.
    func testAnIdIsOnePathSegment() {
        XCTAssertEqual(LibraryClient.pathSegment("steam:570"), "steam:570")
        XCTAssertEqual(LibraryClient.pathSegment("../a b/c?d"), "..%2Fa%20b%2Fc%3Fd")
    }

    func testPosterCandidatesPreferPortraitThenHeader() {
        let full = Artwork(
            portrait: "https://x/p.jpg", hero: "https://x/hero.jpg", header: "https://x/h.jpg")
        XCTAssertEqual(full.posterCandidates.map(\.absoluteString),
                       ["https://x/p.jpg", "https://x/h.jpg", "https://x/hero.jpg"])

        // No portrait → header leads; absent fields are skipped, not nil-padded.
        let sparse = Artwork(portrait: nil, hero: nil, header: "https://x/h.jpg")
        XCTAssertEqual(sparse.posterCandidates.map(\.absoluteString), ["https://x/h.jpg"])

        XCTAssertTrue(Artwork().posterCandidates.isEmpty)
    }

    /// Host art stays the path the host sent, so a catalog cached before the host moved still
    /// finds its posters: the loader asks the host where it is now, under the same cache key.
    func testHostArtPathKeysToTheMachineAtAnyAddress() throws {
        let json = #"{"id":"steam:570","store":"steam","title":"Dota 2","art":{"#
            + #""portrait":"/api/v1/library/art/steam:570/portrait","#
            + #""hero":"https://cdn.example.com/h.jpg"}}"#
        let art = try JSONDecoder().decode(GameEntry.self, from: Data(json.utf8))
            .art.posterCandidates
        for address in ["192.168.1.70", "100.64.1.2"] {
            XCTAssertTrue(LibraryArtLoader.isHostOrigin(art[0], address: address, port: 47990))
            XCTAssertEqual(
                LibraryArtLoader.cacheKey(
                    for: art[0], hostAddress: address, hostPort: 47990, pin: Data([0xDE, 0xAD])),
                "dead|/api/v1/library/art/steam:570/portrait")
            XCTAssertFalse(LibraryArtLoader.isHostOrigin(art[1], address: address, port: 47990))
        }
        XCTAssertEqual(
            LibraryArtLoader.requestPath(art[0]), "/api/v1/library/art/steam:570/portrait")
    }

    // MARK: - HTTP response parsing (MgmtTransport)

    // The management API is reached over Network.framework rather than URLSession (ATS cannot be
    // relaxed for the arbitrary addresses a host lives at — see MgmtTransport), so we parse HTTP
    // ourselves. These cover the framings hyper actually emits, plus the failure modes where
    // getting it wrong would be silent.

    private func raw(_ text: String) -> Data { Data(text.utf8) }

    func testParsesContentLengthFramedJSON() throws {
        let body = #"[{"id":"steam:570"}]"#
        let response = try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                + "Content-Length: \(body.utf8.count)\r\n\r\n\(body)"))
        XCTAssertEqual(response.status, 200)
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), body)
        // Field names are case-insensitive per RFC 9110.
        XCTAssertEqual(response.header("CONTENT-TYPE"), "application/json")
    }

    func testParsesChunkedBody() throws {
        // How hyper streams the art proxy.
        let response = try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
                + "5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "hello world")
    }

    func testChunkExtensionsAndTrailersAreIgnored() throws {
        let response = try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
                + "3;foo=bar\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\n"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "abc")
    }

    func testUnauthorizedStatusSurvives() throws {
        // What an unpaired certificate gets from the host — the status is the whole signal.
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n"))
        XCTAssertEqual(response.status, 401)
        XCTAssertTrue(response.body.isEmpty)
    }

    /// A 204 has no framing header and no body: it ends at its header block. Read as framed by
    /// close, a kept-alive connection waits out the request timeout for a hang-up.
    func testANoContentResponseEndsAtItsHeaders() throws {
        let answer = raw("HTTP/1.1 204 No Content\r\ndate: Fri, 13 Nov 2026 10:00:00 GMT\r\n\r\n")
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: answer), answer.count)
        let response = try HTTPResponseParser.parse(answer)
        XCTAssertEqual(response.status, 204)
        XCTAssertTrue(response.body.isEmpty)
        // What follows is the next response's.
        XCTAssertEqual(
            try HTTPResponseParser.messageLength(in: answer + raw("HTTP/1.1 200")), answer.count)
    }

    func testBodyRunsToEOFWithoutFramingHeaders() throws {
        let response = try HTTPResponseParser.parse(raw("HTTP/1.1 200 OK\r\n\r\nraw-to-eof"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "raw-to-eof")
    }

    func testTruncatedBodyThrowsRatherThanReturningPartialJSON() {
        // The one that matters: a body cut short must NOT come back as success. A clipped JSON
        // array would decode to fewer games — "this host has no games" — instead of an error.
        XCTAssertThrowsError(
            try HTTPResponseParser.parse(raw("HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\nshort")))
    }

    func testOverLongBodyIsClippedToContentLength() throws {
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcdef"))
        XCTAssertEqual(String(decoding: response.body, as: UTF8.self), "abc")
    }

    func testMalformedResponsesThrow() {
        // Header block never terminated (peer hung up), a non-HTTP greeting, and a chunked stream
        // cut mid-chunk.
        XCTAssertThrowsError(
            try HTTPResponseParser.parse(raw("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n")))
        XCTAssertThrowsError(try HTTPResponseParser.parse(raw("NOT-HTTP\r\n\r\n")))
        XCTAssertThrowsError(try HTTPResponseParser.parse(raw(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nabc")))
    }

    func testMultiWordReasonPhraseParses() throws {
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"))
        XCTAssertEqual(response.status, 404)
    }

    func testBinaryBodySurvivesByteForByte() throws {
        // Posters are PNG/JPEG: the body must never be round-tripped through a String.
        let png: [UInt8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF, 0x0D, 0x0A]
        var message = raw("HTTP/1.1 200 OK\r\nContent-Length: \(png.count)\r\n\r\n")
        message.append(contentsOf: png)
        let response = try HTTPResponseParser.parse(message)
        XCTAssertEqual([UInt8](response.body), png)
    }

    // MARK: - Message framing (keep-alive)

    // Connections are pooled and reused, so a response has to be delimited WITHOUT waiting for
    // the peer to hang up. Getting this wrong either truncates a response or bleeds one response
    // into the next, and both would be silent.

    func testMessageLengthDelimitsContentLengthFraming() throws {
        let complete = raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: complete), complete.count)
        XCTAssertNil(try HTTPResponseParser.messageLength(
            in: raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel")))
    }

    func testMessageLengthDelimitsChunkedFraming() throws {
        let complete = raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n")
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: complete), complete.count)
        // Mid-chunk, and terminal chunk without its closing blank line.
        XCTAssertNil(try HTTPResponseParser.messageLength(
            in: raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel")))
        XCTAssertNil(try HTTPResponseParser.messageLength(
            in: raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n")))
        // Trailers belong to the message and must be consumed with it.
        let trailered = raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
            + "3\r\nabc\r\n0\r\nX-Trailer: 1\r\n\r\n")
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: trailered), trailered.count)
    }

    func testAChunkOfIntMaxNeitherTrapsNorFits() throws {
        let huge = raw("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7fffffffffffffff\r\nx")
        XCTAssertNil(try HTTPResponseParser.messageLength(in: huge))
        XCTAssertThrowsError(try HTTPResponseParser.parse(huge))
    }

    func testMessageLengthIsNilWithoutFraming() throws {
        // No Content-Length and not chunked ⇒ the body runs to EOF and the connection can't be
        // reused. Partial header blocks are likewise "not yet".
        XCTAssertNil(try HTTPResponseParser.messageLength(in: raw("HTTP/1.1 200 OK\r\n\r\nto-eof")))
        XCTAssertNil(try HTTPResponseParser.messageLength(in: raw("HTTP/1.1 200 OK\r\nContent-Len")))
    }

    func testBackToBackResponsesSplitExactly() throws {
        // The reuse case that matters: two responses arriving in one read.
        let first = raw("HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
        let second = raw("HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nbye")
        let both = first + second
        XCTAssertEqual(try HTTPResponseParser.messageLength(in: both), first.count)
        XCTAssertEqual(
            String(decoding: try HTTPResponseParser.parse(both.prefix(first.count)).body,
                   as: UTF8.self), "hello")
        XCTAssertEqual(
            String(decoding: try HTTPResponseParser.parse(both.dropFirst(first.count)).body,
                   as: UTF8.self), "bye")
    }

    func testConnectionCloseIsDetected() throws {
        let response = try HTTPResponseParser.parse(
            raw("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"))
        XCTAssertTrue(response.wantsClose)
        // HTTP/1.1 keeps the connection open unless told otherwise.
        XCTAssertFalse(try HTTPResponseParser.parse(
            raw("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")).wantsClose)
    }

    // MARK: - Art cache

    private func temporaryCacheDirectory() -> URL {
        URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("pf-art-test-\(UUID().uuidString)", isDirectory: true)
    }

    func testArtCacheRoundTripsBinaryAndSeparatesKeys() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = ArtCache(directory: directory)

        let png = Data([0x89, 0x50, 0x4E, 0x47, 0x00, 0xFF, 0x0D, 0x0A])
        let portrait = URL(string: "https://100.64.1.2:47990/api/v1/library/art/steam:570/portrait")!
        let header = URL(string: "https://100.64.1.2:47990/api/v1/library/art/steam:570/header")!

        var hit = await cache.data(forKey: portrait.absoluteString)
        XCTAssertNil(hit, "cold cache must miss")
        await cache.store(png, forKey: portrait.absoluteString)
        hit = await cache.data(forKey: portrait.absoluteString)
        XCTAssertEqual(hit, png, "posters are binary; the body must survive byte-for-byte")
        // Sibling art of the same title must not collide.
        let sibling = await cache.data(forKey: header.absoluteString)
        XCTAssertNil(sibling)
    }

    func testArtCacheRefusesEmptyData() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = ArtCache(directory: directory)

        let empty = URL(string: "https://cdn.example.com/empty.jpg")!
        await cache.store(Data(), forKey: empty.absoluteString)
        let emptyHit = await cache.data(forKey: empty.absoluteString)
        XCTAssertNil(emptyHit, "an empty body is not art")
    }

    func testInlineBytesDecodesBase64AndRefusesTheRest() throws {
        let payload = Data([0x89, 0x50, 0x4E, 0x47])
        let url = URL(string: "data:image/png;base64,\(payload.base64EncodedString())")!
        XCTAssertEqual(try LibraryArtLoader.inlineBytes(url), payload)
        // No ;base64 marker — a charset/plaintext data URL is not the form the kit emits.
        XCTAssertThrowsError(try LibraryArtLoader.inlineBytes(
            URL(string: "data:image/png,\(payload.base64EncodedString())")!))
        // A body over the transport ceiling is refused even when it decodes.
        let huge = Data(repeating: 0x41, count: MgmtTransport.maxResponseBytes + 1)
        XCTAssertThrowsError(try LibraryArtLoader.inlineBytes(
            URL(string: "data:image/png;base64,\(huge.base64EncodedString())")!))
    }

    func testCacheKeyPinsHostArtToTheMachineNotItsAddress() {
        let pin = Data([0xDE, 0xAD])
        let art = URL(
            string: "https://192.168.1.70:47990/api/v1/library/art/steam:570/portrait")!
        let key = LibraryArtLoader.cacheKey(
            for: art, hostAddress: "192.168.1.70", hostPort: 47990, pin: pin)
        XCTAssertEqual(key, "dead|/api/v1/library/art/steam:570/portrait")

        // The same host answering at a NEW address must keep its cache entries.
        let moved = URL(
            string: "https://100.64.1.2:47990/api/v1/library/art/steam:570/portrait")!
        XCTAssertEqual(
            LibraryArtLoader.cacheKey(
                for: moved, hostAddress: "100.64.1.2", hostPort: 47990, pin: pin),
            key)

        // A different machine answering the SAME address must not inherit them.
        XCTAssertNotEqual(
            LibraryArtLoader.cacheKey(
                for: art, hostAddress: "192.168.1.70", hostPort: 47990,
                pin: Data([0xBE, 0xEF])),
            key)

        // A query is part of the path — art endpoints that take one don't collide.
        let queried = URL(
            string: "https://192.168.1.70:47990/api/v1/library/art/steam:570/portrait?v=2")!
        XCTAssertEqual(
            LibraryArtLoader.requestPath(queried),
            "/api/v1/library/art/steam:570/portrait?v=2")

        // CDN art is keyed by its URL — the URL already names its origin.
        let cdn = URL(string: "https://cdn.example.com/hero.jpg")!
        XCTAssertEqual(
            LibraryArtLoader.cacheKey(
                for: cdn, hostAddress: "192.168.1.70", hostPort: 47990, pin: pin),
            cdn.absoluteString)
    }

    /// Age-out reads the file mtime, so stamp it instead of sleeping past `maxAge`.
    func testArtCacheAgesEntriesOut() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = ArtCache(directory: directory, maxAge: 1)
        let url = URL(string: "https://cdn.example.com/stale.jpg")!

        await cache.store(Data("stale".utf8), forKey: url.absoluteString)
        let fresh = await cache.data(forKey: url.absoluteString)
        XCTAssertNotNil(fresh)
        ageFiles(in: directory, by: 10)
        let expired = await cache.data(forKey: url.absoluteString)
        XCTAssertNil(expired)
    }

    func testArtCacheEvictsLeastRecentlyUsedOverBudget() async throws {
        let directory = temporaryCacheDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        // Budget holds three of these; the fourth store must evict.
        let cache = ArtCache(directory: directory, maxBytes: 300)
        let blob = Data(repeating: 0x41, count: 100)
        var urls: [URL] = []
        for i in 0..<4 {
            let url = URL(string: "https://cdn.example.com/blob\(i).jpg")!
            urls.append(url)
            await cache.store(blob, forKey: url.absoluteString)
            // Back-date what is already stored, so each blob lands newer than the last.
            if i < 3 { ageFiles(in: directory, by: 1) }
        }
        let evicted = await cache.data(forKey: urls[0].absoluteString)
        XCTAssertNil(evicted, "the oldest entry should have been evicted")
        let newest = await cache.data(forKey: urls[3].absoluteString)
        XCTAssertEqual(newest, blob)
    }

    /// Move every cached file's mtime `seconds` into the past.
    private func ageFiles(in directory: URL, by seconds: TimeInterval) {
        let files = (try? FileManager.default.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
        for file in files {
            let values = try? file.resourceValues(forKeys: [.contentModificationDateKey])
            let date = values?.contentModificationDate ?? Date()
            try? FileManager.default.setAttributes(
                [.modificationDate: date.addingTimeInterval(-seconds)], ofItemAtPath: file.path)
        }
    }

    func testBaseURLBracketsIPv6Only() {
        XCTAssertEqual(LibraryClient.baseURL(address: "192.168.1.70", port: 47990),
                       "https://192.168.1.70:47990")
        XCTAssertEqual(LibraryClient.baseURL(address: "100.101.102.103", port: 47990),
                       "https://100.101.102.103:47990")
        XCTAssertEqual(LibraryClient.baseURL(address: "fd7a:115c::1", port: 47990),
                       "https://[fd7a:115c::1]:47990")
        // An address the user pasted already bracketed must not end up double-bracketed.
        XCTAssertEqual(LibraryClient.baseURL(address: "[fd7a:115c::1]", port: 47990),
                       "https://[fd7a:115c::1]:47990")
    }

    // MARK: - Management call preamble

    func testMgmtTargetNamesWhatIsMissing() {
        let unpaired = StoredHost(name: "Desk", address: "192.168.1.70")
        let identity = ClientIdentity(certPEM: "", keyPEM: "")
        XCTAssertEqual(MgmtTarget.make(host: unpaired, identity: nil).failure, .identity)
        XCTAssertEqual(MgmtTarget.make(host: unpaired, identity: identity).failure, .pairing("Desk"))
        var paired = unpaired
        paired.pinnedSHA256 = Data(repeating: 1, count: 32)
        XCTAssertEqual(MgmtTarget.make(host: paired, identity: identity).failure, .keychain)
    }

    func testARefusedWakeReadsAsTheHostsSentence() {
        func woken(_ code: Int) -> String {
            do {
                try LibraryClient.expectWoken(HTTPResponse(status: code, headers: [:], body: Data()))
                return "ok"
            } catch {
                return LibraryClient.wakeFailedLine("Kid", error)
            }
        }
        XCTAssertEqual(woken(200), "ok")
        XCTAssertEqual(woken(202), "ok")
        XCTAssertEqual(woken(403), "This device isn't allowed to start another profile's seat.")
        XCTAssertEqual(woken(401), "Couldn't get Kid's desktop ready. Try again in a moment.")
        XCTAssertEqual(woken(500), "Couldn't get Kid's desktop ready. Try again in a moment.")
    }

    func testExpectOKFoldsTheCertificateRefusals() {
        func status(_ code: Int, _ ok: Set<Int> = [200]) -> String {
            let response = HTTPResponse(status: code, headers: [:], body: Data())
            do {
                try LibraryClient.expectOK(response, ok)
                return "ok"
            } catch {
                return "\(error)"
            }
        }
        XCTAssertEqual(status(200), "ok")
        XCTAssertEqual(status(202, [200, 202]), "ok")
        XCTAssertEqual(status(202), "http(202)")
        XCTAssertEqual(status(401), "unauthorized")
        XCTAssertEqual(status(403), "unauthorized")
        XCTAssertEqual(status(500), "http(500)")
    }

    /// A cancelled caller is not an unreachable host.
    func testACancelledRequestStaysCancelled() {
        XCTAssertTrue(LibraryClient.transportFailure(CancellationError()) is CancellationError)
        XCTAssertEqual(
            "\(LibraryClient.transportFailure(MgmtTransportError.timedOut))",
            #"unreachable("timed out")"#)
    }

    /// The host refuses an install in its `ApiError` envelope; the player reads its sentence.
    func testInstallRefusalCarriesTheHostSentence() {
        let body = Data(#"{"error":"Not enough space on the host."}"#.utf8)
        XCTAssertEqual(
            InstallOutcome.from(status: 409, message: LibraryClient.hostReason(body)),
            .refused("Not enough space on the host."))
        XCTAssertNil(LibraryClient.hostReason(Data(#"{"error":""}"#.utf8)))
        XCTAssertNil(LibraryClient.hostReason(Data()))
    }
}

private extension Result {
    var failure: Failure? {
        if case .failure(let error) = self { return error }
        return nil
    }
}
