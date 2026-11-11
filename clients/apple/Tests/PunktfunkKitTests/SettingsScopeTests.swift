// The settings surface's layers: what a row shows in each scope and where its write goes.

import XCTest

@testable import PunktfunkShared

final class SettingsScopeTests: XCTestCase {
    private let bitrate = SettingsField(
        name: "bitrate_kbps", key: DefaultsKey.bitrateKbps, overlay: \.bitrateKbps,
        effective: \.bitrateKbps)

    private func catalog(_ presets: StreamPreset...) -> PresetCatalog {
        PresetCatalog(presets: presets)
    }

    private func game(bitrate kbps: Int? = nil) -> StreamPreset {
        var preset = StreamPreset(name: "Game", id: "game")
        preset.overrides.bitrateKbps = kbps
        return preset
    }

    private var globals: EffectiveSettings {
        var g = EffectiveSettings()
        g.bitrateKbps = 40_000
        g.codec = "hevc"
        return g
    }

    func testDefaultsShowTheGlobals() {
        let shown = SettingsScope.resolve(
            globals: globals, scope: .defaults, catalog: catalog(game(bitrate: 80_000)))
        XCTAssertEqual(shown, globals)
    }

    func testAPresetShowsItsOverridesOverTheLiveGlobals() {
        let shown = SettingsScope.resolve(
            globals: globals, scope: .preset("game"), catalog: catalog(game(bitrate: 80_000)))
        XCTAssertEqual(shown.bitrateKbps, 80_000)
        XCTAssertEqual(shown.codec, "hevc", "a row the preset leaves alone reads the global")
    }

    func testAPinEqualToTheGlobalStillShows() {
        let shown = SettingsScope.resolve(
            globals: globals, scope: .preset("game"), catalog: catalog(game(bitrate: 40_000)))
        XCTAssertEqual(shown.bitrateKbps, 40_000)
    }

    func testAScopeWhosePresetIsGoneShowsTheGlobals() {
        let shown = SettingsScope.resolve(
            globals: globals, scope: .preset("deleted"), catalog: catalog(game(bitrate: 80_000)))
        XCTAssertEqual(shown, globals)
    }

    func testDefaultsWriteTheFieldsKey() {
        XCTAssertEqual(
            SettingsScope.route(scope: .defaults, field: bitrate),
            .defaults(DefaultsKey.bitrateKbps))
    }

    func testAPresetWritesAnOverride() {
        XCTAssertEqual(SettingsScope.route(scope: .preset("game"), field: bitrate), .override("game"))
    }
}
