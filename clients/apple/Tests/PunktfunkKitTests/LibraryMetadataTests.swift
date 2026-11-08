// The metadata the title sheet shows, and art a plugin inlined as a `data:` URL.

import XCTest
@testable import PunktfunkKit

final class LibraryMetadataTests: XCTestCase {
    func testDecodesDescriptionTagsAndPlayers() throws {
        let json = Data("""
        [{"id":"rom-manager:1","store":"rom-manager","title":"Mario Kart","art":{},
          "description":"Race.","tags":["co-op"],"players":4,"release_year":1992}]
        """.utf8)
        let game = try XCTUnwrap(try JSONDecoder().decode([GameEntry].self, from: json).first)
        XCTAssertEqual(game.description, "Race.")
        XCTAssertEqual(game.tags, ["co-op"])
        XCTAssertEqual(game.players, 4)
        XCTAssertEqual(game.releaseYear, 1992)
    }

    func testAnOlderHostOmitsThemAll() throws {
        let json = Data(#"[{"id":"steam:1","store":"steam","title":"T","art":{}}]"#.utf8)
        let game = try XCTUnwrap(try JSONDecoder().decode([GameEntry].self, from: json).first)
        XCTAssertNil(game.description)
        XCTAssertNil(game.tags)
        XCTAssertNil(game.players)
    }

    func testDecodesTheHostsOverrides() throws {
        let json = Data("""
        [{"id":"steam:1","store":"steam","title":"T","art":{},
          "overrides":{"name":"1080p60 DualSense","lines":1080,"hz":60,"hdr":false,
                       "gamepad":"dualsense","beyond_screen":true,"future":1}}]
        """.utf8)
        let game = try XCTUnwrap(try JSONDecoder().decode([GameEntry].self, from: json).first)
        XCTAssertEqual(game.overrides, GameOverrides(
            name: "1080p60 DualSense", lines: 1080, hz: 60, hdr: false, gamepad: "dualsense",
            beyondScreen: true))
        // The library cache writes the entry back; it must read the same.
        let back = try JSONDecoder().decode(GameEntry.self, from: JSONEncoder().encode(game))
        XCTAssertEqual(back.overrides, game.overrides)
    }

    func testOverridesAbsentOrPartial() throws {
        let json = Data("""
        [{"id":"steam:1","store":"steam","title":"T","art":{}},
         {"id":"steam:2","store":"steam","title":"U","art":{},"overrides":{"lines":"x","hz":60}}]
        """.utf8)
        let games = try JSONDecoder().decode([GameEntry].self, from: json)
        XCTAssertNil(games[0].overrides)
        XCTAssertEqual(games[1].overrides, GameOverrides(hz: 60), "a mistyped field reads unset")
        XCTAssertEqual(games[1].overrides?.beyondScreen, false)
    }

    func testTheHostLineNamesTheSetElseItsValues() {
        XCTAssertNil(GameOverrides().hostLine)
        XCTAssertNil(GameOverrides(name: "4K").hostLine, "a name alone sets nothing")
        XCTAssertEqual(
            GameOverrides(name: "1080p60 DualSense", lines: 1080).hostLine,
            "Host: 1080p60 DualSense")
        XCTAssertEqual(
            GameOverrides(lines: 1080, hz: 60, hdr: false, gamepad: "dualsense").hostLine,
            "Host: 1080p60, DualSense, HDR off")
        XCTAssertEqual(GameOverrides(hz: 120).hostLine, "Host: 120 Hz")
        XCTAssertEqual(GameOverrides(gamepad: "xbox").hostLine, "Host: Xbox 360")
    }

    func testInlineArtDecodesBase64AndRefusesTheRest() throws {
        let png = Data([0x89, 0x50, 0x4E, 0x47])
        let url = try XCTUnwrap(URL(string: "data:image/png;base64,\(png.base64EncodedString())"))
        XCTAssertEqual(try LibraryArtLoader.inlineBytes(url), png)
        let plain = try XCTUnwrap(URL(string: "data:text/plain,hello"))
        XCTAssertThrowsError(try LibraryArtLoader.inlineBytes(plain))
    }
}
