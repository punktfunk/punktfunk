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
        j["compositor"] = compositorName(defaults.object(forKey: DefaultsKey.compositor) as? Int ?? 0)
        j["gamepad"] = padTypeName(defaults.object(forKey: DefaultsKey.gamepadType) as? Int ?? 0)
        // Derived, never stored: the overlay is off when its tier is.
        j["show_stats"] = (defaults.string(forKey: DefaultsKey.statsVerbosity) ?? "normal") != "off"
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

    /// Defaults match the app's own `@AppStorage` declarations: a key nobody has written yet
    /// must read as what the touch UI shows, or the first console frame would offer to change
    /// a setting the player never set.
    private static let fields: [Field] = [
        .int("width", DefaultsKey.streamWidth, 1920),
        .int("height", DefaultsKey.streamHeight, 1080),
        .int("refresh_hz", DefaultsKey.streamHz, 60),
        .bool("match_window", DefaultsKey.matchWindow, false),
        .int("bitrate_kbps", DefaultsKey.bitrateKbps, 0),
        .double("render_scale", DefaultsKey.renderScale, 1.0),
        .string("video_fit", DefaultsKey.videoFit, "fit"),
        .string("codec", DefaultsKey.codec, "auto"),
        .bool("hdr_enabled", DefaultsKey.hdrEnabled, true),
        .bool("enable_444", DefaultsKey.enable444, false),
        .bool("ten_bit_sdr", DefaultsKey.tenBitSdr, false),
        .int("audio_channels", DefaultsKey.audioChannels, 2),
        .string("audio_format", DefaultsKey.audioFormat, "opus"),
        .bool("mic_enabled", DefaultsKey.micEnabled, true),
        .bool("echo_cancel", DefaultsKey.echoCancel, true),
        .bool("keep_host_audio", DefaultsKey.keepHostAudio, false),
        .string("speaker_device", DefaultsKey.speakerUID, ""),
        .string("mic_device", DefaultsKey.micUID, ""),
        .string("touch_mode", DefaultsKey.touchMode, "trackpad"),
        .string("mouse_mode", DefaultsKey.mouseMode, "capture"),
        .bool("invert_scroll", DefaultsKey.invertScroll, false),
        .string("overlay_actions", DefaultsKey.overlayActions, ""),
        .bool("inhibit_shortcuts", DefaultsKey.inhibitShortcuts, true),
        .bool("gamepad_forwarding", DefaultsKey.gamepadForwarding, true),
        .string("system_buttons", DefaultsKey.systemButtons, "auto"),
        .string("guide_gesture", DefaultsKey.guideGesture, "auto"),
        .string("stats_verbosity", DefaultsKey.statsVerbosity, "normal"),
        .bool("advanced_stats", DefaultsKey.advancedStats, false),
        .bool("fullscreen_on_stream", DefaultsKey.fullscreenWhileStreaming, true),
        .string("present_priority", DefaultsKey.presentPriority, "latency"),
        .int("smooth_buffer", DefaultsKey.smoothBuffer, 0),
        .bool("vsync", DefaultsKey.vsync, false),
        .bool("allow_vrr", DefaultsKey.allowVRR, true),
        .string("ui_palette", DefaultsKey.uiPalette, "violet"),
        .string("library_sort", DefaultsKey.librarySort, ""),
        .string("library_sections", DefaultsKey.librarySections, ""),
        // Unset stays unset: the console's own default is the Games tab's grid.
        .string("library_view", DefaultsKey.libraryView, ""),
        .string("start_in", DefaultsKey.startIn, StartIn.hosts.stored),
        .bool("auto_wake", DefaultsKey.autoWake, true),
        // The console's own off switch lands on the touch, TV or Mac UI.
        .bool("gamepad_ui_enabled", DefaultsKey.gamepadUIEnabled, true),
        .bool("background_keep_alive", DefaultsKey.backgroundKeepAlive, false),
        .int("background_timeout_minutes", DefaultsKey.backgroundTimeoutMinutes, 10),
        .string("hud_placement", DefaultsKey.hudPlacement, "topTrailing"),
        .string("host_sort", DefaultsKey.hostSort, "added"),
        .string("host_grouping", DefaultsKey.hostGrouping, "none"),
        .string("gamepad_ui_mode", DefaultsKey.gamepadUIMode, GamepadUIEnvironment.modeWhenConnected),
        // `Settings::extra` (flattened, so plain top-level keys). The `android.` prefix is
        // where these were first written; the console reads the same names here.
        .bool("android.rumble_on_phone", DefaultsKey.rumbleOnDevice, false),
        .bool("android.gyro_on_phone", DefaultsKey.gyroFromDevice, false),
        .bool("android.sc2_capture", DefaultsKey.sc2Capture, false),
    ]

    // MARK: - the two that differ

    /// The compositor's wire value (`PunktfunkConnection.Compositor`) against the console's name.
    private static let compositors: [(Int, String)] = [
        (0, "auto"), (1, "kwin"), (3, "mutter"), (5, "hyprland"), (2, "wlroots"), (4, "gamescope"),
    ]
    private static let padTypes: [(Int, String)] = [
        (0, "auto"), (1, "xbox360"), (3, "xboxone"), (2, "dualsense"), (4, "dualshock4"),
        (6, "steamdeck"),
    ]

    static func compositorName(_ tag: Int) -> String {
        compositors.first { $0.0 == tag }?.1 ?? "auto"
    }
    static func compositorTag(_ name: String) -> Int? {
        compositors.first { $0.1 == name }?.0
    }
    static func padTypeName(_ tag: Int) -> String {
        padTypes.first { $0.0 == tag }?.1 ?? "auto"
    }
    static func padTypeTag(_ name: String) -> Int? {
        padTypes.first { $0.1 == name }?.0
    }
}

#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif
