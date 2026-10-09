// The console's settings document (`pf_client_core::trust::Settings`) as this client reads and
// writes it.
//
// The console owns keys this app has no field for — `library_sort`, `reduce_motion`, the ring's
// own bookkeeping — so its last saved document is the BASE and every key the app owns is written
// over it. That way the touch UI's edits win and the console's own keys survive the round trip.
// `Settings` is `#[serde(default)]`, so a partial document is fine.
//
// The names on the left are the cross-client keys; most already have a `SettingsFields` entry,
// which is the one place that knows a key's `UserDefaults` twin. Two are stored here as the
// wire integer the app sends and as a name in the console, so they carry a table.

import Foundation
import PunktfunkShared

public enum ConsoleSettings {
    /// Where the console's own document is kept, whole, between sessions.
    public static let documentKey = "punktfunk.consoleSettings"

    /// What the console is handed: the base document with every key this app owns over it.
    public static func document(_ defaults: UserDefaults = .standard) -> [String: Any] {
        var j = base(defaults)
        for field in fields { field.write(&j, defaults) }
        j["compositor"] = compositorName(
            defaults.object(forKey: DefaultsKey.compositor) as? Int ?? SettingDefault.compositor)
        j["gamepad"] = padTypeName(
            defaults.object(forKey: DefaultsKey.gamepadType) as? Int ?? SettingDefault.gamepadType)
        // Derived, never stored: the overlay is off when its tier is.
        j["show_stats"] = EffectiveSettings.storedStatsVerbosity(defaults) != "off"
        // The OS answers this one, so the shell shows no row for it (`platform.rs`).
        j["reduce_motion"] = reduceMotion
        j["default_host"] = (defaults.string(forKey: DefaultsKey.defaultHost)).flatMap {
            $0.isEmpty ? nil : $0
        } ?? NSNull()
        return j
    }

    public static func json(_ defaults: UserDefaults = .standard) -> String {
        let data = try? JSONSerialization.data(withJSONObject: document(defaults))
        return data.flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
    }

    /// The console saved a document: keep it whole as the next base, and fold every key this
    /// app owns back into its own defaults. An unknown value is left alone rather than stored.
    public static func apply(_ j: [String: Any], _ defaults: UserDefaults = .standard) {
        defaults.set(j, forKey: documentKey)
        for field in fields { field.read(j, defaults) }
        if let name = j["compositor"] as? String, let tag = compositorTag(name) {
            defaults.set(tag, forKey: DefaultsKey.compositor)
        }
        if let name = j["gamepad"] as? String, let tag = padTypeTag(name) {
            defaults.set(tag, forKey: DefaultsKey.gamepadType)
        }
        if let host = j["default_host"] as? String {
            defaults.set(host, forKey: DefaultsKey.defaultHost)
        } else if j["default_host"] is NSNull {
            defaults.set("", forKey: DefaultsKey.defaultHost)
        }
    }

    private static func base(_ defaults: UserDefaults) -> [String: Any] {
        defaults.dictionary(forKey: documentKey) ?? [:]
    }

    private static var reduceMotion: Bool {
        #if canImport(UIKit)
        return UIAccessibility.isReduceMotionEnabled
        #elseif canImport(AppKit)
        return NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        #else
        return false
        #endif
    }

    // MARK: - the table

    /// One key the app owns, in both directions.
    private struct Field {
        let write: (inout [String: Any], UserDefaults) -> Void
        let read: ([String: Any], UserDefaults) -> Void

        static func bool(_ json: String, _ key: String, _ fallback: Bool) -> Field {
            Field(
                write: { j, d in j[json] = d.object(forKey: key) as? Bool ?? fallback },
                read: { j, d in if let v = j[json] as? Bool { d.set(v, forKey: key) } })
        }

        static func int(_ json: String, _ key: String, _ fallback: Int) -> Field {
            Field(
                write: { j, d in j[json] = d.object(forKey: key) as? Int ?? fallback },
                read: { j, d in if let v = j[json] as? Int { d.set(v, forKey: key) } })
        }

        static func double(_ json: String, _ key: String, _ fallback: Double) -> Field {
            Field(
                write: { j, d in j[json] = d.object(forKey: key) as? Double ?? fallback },
                read: { j, d in if let v = j[json] as? Double { d.set(v, forKey: key) } })
        }

        static func string(_ json: String, _ key: String, _ fallback: String) -> Field {
            Field(
                write: { j, d in j[json] = d.string(forKey: key) ?? fallback },
                read: { j, d in if let v = j[json] as? String { d.set(v, forKey: key) } })
        }
    }

    /// Defaults are `SettingDefault`'s, which the app's `@AppStorage` reads too: a key nobody has
    /// written yet must read as what the touch UI shows, or the first console frame would offer
    /// to change a setting the player never set.
    private static let fields: [Field] = [
        .int("width", DefaultsKey.streamWidth, SettingDefault.streamWidth),
        .int("height", DefaultsKey.streamHeight, SettingDefault.streamHeight),
        .int("refresh_hz", DefaultsKey.streamHz, SettingDefault.streamHz),
        .bool("match_window", DefaultsKey.matchWindow, SettingDefault.matchWindow),
        .int("bitrate_kbps", DefaultsKey.bitrateKbps, SettingDefault.bitrateKbps),
        .double("render_scale", DefaultsKey.renderScale, SettingDefault.renderScale),
        .string("video_fit", DefaultsKey.videoFit, SettingDefault.videoFit),
        .string("codec", DefaultsKey.codec, SettingDefault.codec),
        .bool("hdr_enabled", DefaultsKey.hdrEnabled, SettingDefault.hdrEnabled),
        .bool("enable_444", DefaultsKey.enable444, SettingDefault.enable444),
        .bool("ten_bit_sdr", DefaultsKey.tenBitSdr, SettingDefault.tenBitSdr),
        .int("audio_channels", DefaultsKey.audioChannels, SettingDefault.audioChannels),
        .string("audio_format", DefaultsKey.audioFormat, SettingDefault.audioFormat),
        .bool("mic_enabled", DefaultsKey.micEnabled, SettingDefault.micEnabled),
        .bool("echo_cancel", DefaultsKey.echoCancel, SettingDefault.echoCancel),
        .bool("keep_host_audio", DefaultsKey.keepHostAudio, SettingDefault.keepHostAudio),
        .string("speaker_device", DefaultsKey.speakerUID, SettingDefault.speakerUID),
        .string("mic_device", DefaultsKey.micUID, SettingDefault.micUID),
        .string("touch_mode", DefaultsKey.touchMode, SettingDefault.touchMode),
        .string("mouse_mode", DefaultsKey.mouseMode, SettingDefault.mouseMode),
        .bool("invert_scroll", DefaultsKey.invertScroll, SettingDefault.invertScroll),
        .string("overlay_actions", DefaultsKey.overlayActions, SettingDefault.overlayActions),
        .bool("inhibit_shortcuts", DefaultsKey.inhibitShortcuts, SettingDefault.inhibitShortcuts),
        .bool(
            "gamepad_forwarding", DefaultsKey.gamepadForwarding,
            SettingDefault.gamepadForwarding),
        .bool("pad_rumble", DefaultsKey.padRumble, SettingDefault.padRumble),
        .string("system_buttons", DefaultsKey.systemButtons, SettingDefault.systemButtons),
        .string("guide_gesture", DefaultsKey.guideGesture, SettingDefault.guideGesture),
        // The tier the stream reads: an install from before the tiers keeps its overlay off.
        Field(
            write: { j, d in j["stats_verbosity"] = EffectiveSettings.storedStatsVerbosity(d) },
            read: { j, d in
                if let v = j["stats_verbosity"] as? String {
                    d.set(v, forKey: DefaultsKey.statsVerbosity)
                }
            }),
        .bool("advanced_stats", DefaultsKey.advancedStats, SettingDefault.advancedStats),
        .bool(
            "fullscreen_on_stream", DefaultsKey.fullscreenWhileStreaming,
            SettingDefault.fullscreenWhileStreaming),
        .bool("fullscreen_always", DefaultsKey.fullscreenAlways, SettingDefault.fullscreenAlways),
        .string("present_priority", DefaultsKey.presentPriority, SettingDefault.presentPriority),
        .int("smooth_buffer", DefaultsKey.smoothBuffer, SettingDefault.smoothBuffer),
        .bool("vsync", DefaultsKey.vsync, SettingDefault.vsync),
        .bool("allow_vrr", DefaultsKey.allowVRR, SettingDefault.allowVRR),
        .string("ui_palette", DefaultsKey.uiPalette, SettingDefault.uiPalette),
        .string("library_sort", DefaultsKey.librarySort, SettingDefault.librarySort),
        .string("library_sections", DefaultsKey.librarySections, SettingDefault.librarySections),
        // Unset stays unset: the console's own default is the Games tab's grid.
        .string("library_view", DefaultsKey.libraryView, ""),
        .string("start_in", DefaultsKey.startIn, SettingDefault.startIn),
        .bool("auto_wake", DefaultsKey.autoWake, SettingDefault.autoWake),
        // The console's own off switch lands on the touch, TV or Mac UI.
        .bool("gamepad_ui_enabled", DefaultsKey.gamepadUIEnabled, SettingDefault.gamepadUIEnabled),
        .bool(
            "background_keep_alive", DefaultsKey.backgroundKeepAlive,
            SettingDefault.backgroundKeepAlive),
        .int(
            "background_timeout_minutes", DefaultsKey.backgroundTimeoutMinutes,
            SettingDefault.backgroundTimeoutMinutes),
        .string("hud_placement", DefaultsKey.hudPlacement, SettingDefault.hudPlacement),
        .int("stats_scale_pct", DefaultsKey.statsScalePct, SettingDefault.statsScalePct),
        .bool("exit_hint", DefaultsKey.exitHint, SettingDefault.exitHint),
        .bool("show_advanced", DefaultsKey.showAdvanced, SettingDefault.showAdvanced),
        .string("host_sort", DefaultsKey.hostSort, SettingDefault.hostSort),
        .string("host_grouping", DefaultsKey.hostGrouping, SettingDefault.hostGrouping),
        .string("gamepad_ui_mode", DefaultsKey.gamepadUIMode, GamepadUIEnvironment.modeWhenConnected),
        // `Settings::extra` (flattened, so plain top-level keys). The `android.` prefix is
        // where these were first written; the console reads the same names here.
        .bool("android.rumble_on_phone", DefaultsKey.rumbleOnDevice, SettingDefault.rumbleOnDevice),
        .bool("android.gyro_on_phone", DefaultsKey.gyroFromDevice, SettingDefault.gyroFromDevice),
        .bool("android.sc2_capture", DefaultsKey.sc2Capture, SettingDefault.sc2Capture),
    ]

    // MARK: - the two that differ

    /// The compositors and pad kinds the console names. The stored value is the wire value, the
    /// console's word is the host's name; anything else reads as "auto".
    private static let compositors: [PunktfunkConnection.Compositor] = [
        .auto, .kwin, .mutter, .hyprland, .wlroots, .gamescope,
    ]
    private static let padTypes: [PunktfunkConnection.GamepadType] = [
        .auto, .xbox360, .xboxOne, .dualSense, .dualShock4, .steamDeck, .steamController2,
    ]

    static func compositorName(_ tag: Int) -> String { word(tag, compositors, \.canonicalName) }
    static func compositorTag(_ name: String) -> Int? { tag(name, compositors, \.canonicalName) }
    static func padTypeName(_ tag: Int) -> String { word(tag, padTypes, \.canonicalName) }
    static func padTypeTag(_ name: String) -> Int? { tag(name, padTypes, \.canonicalName) }

    private static func word<T: RawRepresentable>(
        _ tag: Int, _ table: [T], _ name: KeyPath<T, String>
    ) -> String where T.RawValue == UInt32 {
        table.first { Int($0.rawValue) == tag }?[keyPath: name] ?? "auto"
    }
    private static func tag<T: RawRepresentable>(
        _ word: String, _ table: [T], _ name: KeyPath<T, String>
    ) -> Int? where T.RawValue == UInt32 {
        table.first { $0[keyPath: name] == word }.map { Int($0.rawValue) }
    }
}

#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif
