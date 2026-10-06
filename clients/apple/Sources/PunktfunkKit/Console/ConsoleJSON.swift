// What this client hands the console, in the console's OWN model shapes (`HostRow`,
// `WakeStatus`, `PairPhase`, `LibraryGame`, `KnownHosts`), so no Swift mirror type can drift
// from the Rust structs that deserialize them. The Android client writes the same JSON
// (`ConsoleJson.kt`) — a player who moves between a phone and an Apple TV finds one carousel.

import Foundation
import PunktfunkShared

public enum ConsoleJSON {
    public static func string(_ value: Any) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed])
        else { return "null" }
        return String(data: data, encoding: .utf8) ?? "null"
    }

    // MARK: - host rows

    /// The pinned certificate's fingerprint, lowercase hex; empty until the host is paired.
    private static func fingerprint(_ host: StoredHost) -> String {
        host.pinnedSHA256.map { $0.map { String(format: "%02x", $0) }.joined() } ?? ""
    }

    /// `HostRow.key` — the pinned fingerprint when there is one, else `addr:port`.
    public static func rowKey(_ fingerprint: String, _ address: String, _ port: UInt16) -> String {
        fingerprint.isEmpty ? "\(address):\(port)" : fingerprint
    }

    private static func chip(_ preset: StreamPreset) -> [String: Any] {
        [
            "id": preset.id, "name": preset.name, "accent": preset.accent ?? NSNull(),
            // Read by the speed test alone: a preset that PINS bitrate is the layer its host
            // streams at, so the console must not offer to write the global instead.
            "bitrate_kbps": preset.overrides.bitrateKbps ?? NSNull(),
        ]
    }

    private static func actions(_ list: [HostAction]) -> [[String: Any]] {
        list.map {
            [
                "id": $0.id, "label": $0.label, "danger": $0.danger, "available": $0.available,
                "unavailable_reason": $0.unavailableReason ?? "",
            ]
        }
    }

    /// The home carousel: saved hosts in store order, which is the order they were added, each
    /// followed by its pinned preset cards, then the discovered-but-unsaved by name. The console
    /// applies the player's sort on top. `clients/shared/host-row-vectors.json` holds this, the
    /// desktop service's `rows()` and the Android producer to the same rows.
    public static func hostRows(
        saved: [StoredHost], discovered: [DiscoveredHost], online: Set<StoredHost.ID>,
        presets: [StreamPreset], actions hostActions: [String: [HostAction]] = [:],
        running: [String: String] = [:]
    ) -> String {
        var out: [[String: Any]] = []
        let same = { (d: DiscoveredHost, host: StoredHost) in
            d.matches(pin: fingerprint(host), address: host.address, port: host.port)
        }
        for host in saved {
            let advert = discovered.first { same($0, host) }
            let base = row(host, advert: advert, online: online.contains(host.id),
                           presets: presets, hostActions: hostActions, running: running)
            out.append(base)
            // A pinned card shares the primary tile's live state; its key rides the preset id
            // behind a NUL, which no fingerprint or `addr:port` can hold.
            var seen = Set<String>()
            for id in host.pinnedPresetIDs ?? [] where seen.insert(id).inserted {
                guard let preset = presets.first(where: { $0.id == id }) else { continue }
                var card = base
                card["key"] = "\(base["key"] as? String ?? "")\u{0}\(preset.id)"
                card["pin"] = chip(preset)
                card["bound_preset"] = NSNull()
                out.append(card)
            }
        }
        let unsaved = discovered.filter { d in !saved.contains { same(d, $0) } }
        for d in unsaved.sorted(by: { $0.name.lowercased() < $1.name.lowercased() }) {
            let fp = d.fingerprintHex ?? ""
            out.append([
                "key": rowKey(fp, d.host, d.port), "name": d.name.isEmpty ? d.host : d.name,
                "addr": d.host, "port": Int(d.port), "fp_hex": fp, "paired": false,
                "saved": false, "online": true, "mgmt_port": Int(d.mgmtPort ?? punktfunkDefaultMgmtPort),
                "can_wake": false, "clipboard_sync": false, "last_used": NSNull(),
                "os": d.osChain, "pin": NSNull(), "bound_preset": NSNull(),
                // Unsaved: no identity to ask what it is running.
                "running": "",
            ])
        }
        return string(out)
    }

    private static func row(
        _ host: StoredHost, advert: DiscoveredHost?, online: Bool, presets: [StreamPreset],
        hostActions: [String: [HostAction]], running: [String: String]
    ) -> [String: Any] {
        let fp = fingerprint(host)
        let bound = host.presetID.flatMap { id in presets.first { $0.id == id } }
        return [
            "key": rowKey(fp, host.address, host.port),
            "id": host.id.uuidString,
            "name": host.displayName,
            "addr": host.address,
            "port": Int(host.port),
            "fp_hex": fp,
            "paired": host.pinnedSHA256 != nil,
            "saved": true,
            // Presence is the probe alone. An advert only says where to look: a suspending host
            // sends no mDNS goodbye, so its record lingers long enough to keep the pip green.
            "online": online,
            "mgmt_port": Int(advert?.mgmtPort ?? host.effectiveMgmtPort),
            "can_wake": !online && !host.wakeMacs.isEmpty,
            "clipboard_sync": host.clipboardSync ?? false,
            "last_used": host.lastConnected.map { Int($0.timeIntervalSince1970) } ?? NSNull(),
            "os": advert.map { $0.osChain.isEmpty ? (host.osChain ?? "") : $0.osChain }
                ?? (host.osChain ?? ""),
            "actions": actions(hostActions[fp] ?? []),
            "pin": NSNull(),
            "bound_preset": bound.map(chip) ?? NSNull(),
            "running": running[fp] ?? "",
            "game_presets": host.gamePresets ?? [:],
            "profile": host.pickedProfile.map { ["id": $0.id, "display_name": $0.displayName] }
                ?? NSNull(),
        ]
    }

    /// One row as a console entry (`{"library": <HostRow>}`) — the shelf to open.
    public static func entry(_ host: StoredHost, pin: StreamPreset?, presets: [StreamPreset]) -> String {
        var row = row(
            host, advert: nil, online: true, presets: presets, hostActions: [:], running: [:])
        if let pin {
            row["key"] = "\(row["key"] as? String ?? "")\u{0}\(pin.id)"
            row["pin"] = chip(pin)
            row["bound_preset"] = NSNull()
        }
        return string(["library": row])
    }

    /// `{"pair": HostRow}`: the console's Pair screen for this host, over Home.
    public static func pairEntry(_ host: StoredHost, presets: [StreamPreset]) -> String {
        string([
            "pair": row(
                host, advert: nil, online: true, presets: presets, hostActions: [:], running: [:]),
        ])
    }

    /// A question the console asks in place of a system alert; `choices` lead with the default.
    public static func prompt(id: String, title: String, message: String, choices: [String]) -> String {
        string(["id": id, "title": title, "message": message, "choices": choices])
    }

    /// `KnownHosts` — what the console needs to build a `punktfunk://` link.
    public static func knownHosts(_ saved: [StoredHost]) -> String {
        string([
            "hosts": saved.map { host in
                [
                    "name": host.name, "addr": host.address, "port": Int(host.port),
                    "fp_hex": fingerprint(host), "paired": host.pinnedSHA256 != nil,
                    "id": host.id.uuidString, "mac": host.wakeMacs, "os": host.osChain ?? "",
                    "mgmt_port": host.mgmtPort.map(Int.init) ?? NSNull(),
                    "preset_id": host.presetID ?? NSNull(),
                    "pinned_presets": host.pinnedPresetIDs ?? [],
                ]
            }
        ])
    }

    /// One connected controller as the Players tab shows it.
    public struct Pad {
        public var name: String
        /// Stable across a refresh: the rumble test names the pad by it.
        public var key: String
        /// The virtual-pad type's wire byte, which picks the card's glyph family.
        public var pref: UInt32
        public var detail: String
        public var forwarded: Bool
        public var rumble: Bool
        /// 0...1, nil when the pad reports none.
        public var battery: Float?
        public var charging: Bool

        public init(_ c: GamepadManager.DiscoveredController, forwarded: Bool) {
            self.init(
                name: c.name, key: c.id, pref: c.kind.rawValue, detail: c.productCategory,
                forwarded: forwarded, rumble: c.hasHaptics, battery: c.batteryLevel,
                charging: c.isCharging)
        }

        public init(
            name: String, key: String, pref: UInt32, detail: String, forwarded: Bool,
            rumble: Bool, battery: Float?, charging: Bool
        ) {
            (self.name, self.key, self.pref, self.detail) = (name, key, pref, detail)
            (self.forwarded, self.rumble, self.battery, self.charging) =
                (forwarded, rumble, battery, charging)
        }
    }

    /// The pads push: the legend's pad and its glyph family, then every pad, then the inputs
    /// that are not pads (`kind` is `keyboard`, `mouse` or `remote`).
    public static func pads(
        _ pads: [Pad], active: Pad?, others: [(name: String, kind: String)] = []
    ) -> String {
        var doc: [String: Any] = [
            "pads": pads.map { pad -> [String: Any] in
                [
                    "name": pad.name, "key": pad.key, "pref": pad.pref, "detail": pad.detail,
                    "forwarded": pad.forwarded, "rumble": pad.rumble,
                    "battery": pad.battery.map {
                        ["percent": Int(($0 * 100).rounded()), "charging": pad.charging]
                    } as Any? ?? NSNull(),
                ]
            }
        ]
        doc["others"] = others.map { ["name": $0.name, "kind": $0.kind] }
        if let active {
            doc["label"] = active.name
            doc["pref"] = active.pref
        }
        return string(doc)
    }

    /// The catalog with each preset's overrides, so a settings row can say when a host's bound
    /// preset outranks the global it shows.
    public static func presets(_ presets: [StreamPreset]) -> String {
        string(
            presets.map { preset in
                ["id": preset.id, "name": preset.name, "overrides": overrides(preset.overrides)]
            })
    }

    /// A preset's overrides in the console's own overlay spelling. Only the keys the console
    /// reads: an override it cannot parse leaves that preset unmarked, not the catalog empty.
    private static func overrides(_ o: SettingsOverlay) -> [String: Any] {
        var j: [String: Any] = [:]
        j["width"] = o.width
        j["height"] = o.height
        j["match_window"] = o.matchWindow
        j["compositor"] = o.compositor.map(ConsoleSettings.compositorName)
        j["gamepad"] = o.gamepadType.map(ConsoleSettings.padTypeName)
        j["refresh_hz"] = o.refreshHz
        j["bitrate_kbps"] = o.bitrateKbps
        j["render_scale"] = o.renderScale
        j["video_fit"] = o.videoFit
        j["codec"] = o.codec
        j["hdr_enabled"] = o.hdrEnabled
        j["enable_444"] = o.enable444
        j["ten_bit_sdr"] = o.tenBitSdr
        j["audio_channels"] = o.audioChannels
        j["audio_format"] = o.audioFormat
        j["mic_enabled"] = o.micEnabled
        j["echo_cancel"] = o.echoCancel
        j["keep_host_audio"] = o.keepHostAudio
        j["touch_mode"] = o.touchMode
        j["mouse_mode"] = o.mouseMode
        j["invert_scroll"] = o.invertScroll
        j["overlay_actions"] = o.overlayActions
        j["inhibit_shortcuts"] = o.inhibitShortcuts
        j["gamepad_forwarding"] = o.gamepadForwarding
        j["system_buttons"] = o.systemButtons
        j["guide_gesture"] = o.guideGesture
        j["stats_verbosity"] = o.statsVerbosity
        j["fullscreen_on_stream"] = o.fullscreenWhileStreaming
        j["present_priority"] = o.presentPriority
        j["smooth_buffer"] = o.smoothBuffer
        j["vsync"] = o.vsync
        j["allow_vrr"] = o.allowVRR
        return j.compactMapValues { $0 }
    }

    /// The keys `overrides(_:)` sends: exactly the ones a console save sets or clears.
    private static let consoleKeys: Set<String> = [
        "width", "height", "match_window", "compositor", "gamepad", "refresh_hz", "bitrate_kbps",
        "render_scale", "video_fit", "codec", "hdr_enabled", "enable_444", "ten_bit_sdr",
        "audio_channels", "audio_format", "mic_enabled", "echo_cancel", "keep_host_audio",
        "touch_mode", "mouse_mode", "invert_scroll", "overlay_actions", "inhibit_shortcuts",
        "gamepad_forwarding", "system_buttons", "guide_gesture", "stats_verbosity",
        "fullscreen_on_stream", "present_priority", "smooth_buffer", "vsync", "allow_vrr",
    ]

    /// The console's saved overlay over `base`: every key the console edits comes from it,
    /// set or cleared, and every override only this app knows stays.
    public static func overlay(_ console: [String: Any], over base: SettingsOverlay) -> SettingsOverlay {
        guard let data = try? JSONEncoder().encode(base),
            var j = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return base }
        for key in consoleKeys { j[key] = nil }
        for (key, value) in console where consoleKeys.contains(key) { j[key] = value }
        // The console names these two; this app's overlay stores the wire number.
        j["compositor"] = (console["compositor"] as? String).flatMap(ConsoleSettings.compositorTag)
        j["gamepad"] = (console["gamepad"] as? String).flatMap(ConsoleSettings.padTypeTag)
        guard let merged = try? JSONSerialization.data(withJSONObject: j.compactMapValues { $0 }),
            let overlay = try? JSONDecoder().decode(SettingsOverlay.self, from: merged)
        else { return base }
        return overlay
    }

    // MARK: - wake and pair

    public static func wake(
        key: String, name: String, seconds: Int, timedOut: Bool, online: Bool, thenConnect: Bool
    ) -> String {
        string([
            "key": key, "name": name, "seconds": seconds, "timed_out": timedOut,
            "online": online, "then_connect": thenConnect,
        ])
    }

    public static let pairIdle = "\"Idle\""
    public static let pairBusy = "\"Busy\""
    public static func pairFailed(_ why: String) -> String { string(["Failed": why]) }
    public static func pairPaired(key: String) -> String { string(["Paired": ["key": key]]) }

    // MARK: - library

    /// `[LibraryGame]` from this client's catalog — the desktop service's `to_model` mapping.
    /// Without `stats` the Recent and Most played sorts fall back to host order.
    public static func libraryGames(_ games: [GameEntry]) -> String {
        string(
            games.map { g in
                [
                    "id": g.id, "title": g.title, "store": g.store,
                    "launcher": g.role == "launcher",
                    "icon": g.icon.flatMap { validIconToken($0) ? $0 : nil } ?? "",
                    "platform": g.platform ?? NSNull(), "developer": g.developer ?? NSNull(),
                    "year": g.releaseYear ?? NSNull(), "genres": g.genres ?? [],
                    "stats": g.stats.map { s -> [String: Any] in
                        [
                            "last_played_unix_ms": s.lastPlayedUnixMs, "play_time_ms": s.playTimeMs,
                            "last_run_ms": s.lastRunMs, "launch_count": s.launchCount,
                        ]
                    } ?? NSNull(),
                    "running": false,
                    "install": g.install.map { i -> [String: Any] in
                        var o: [String: Any] = ["state": i.state]
                        if let size = i.sizeBytes { o["size_bytes"] = size }
                        if let free = i.freeBytes { o["free_bytes"] = free }
                        return o
                    } ?? NSNull(),
                ]
            })
    }

    /// `/status` `downloads[]` and `grants`, as the console's `DownloadsPush`.
    public static func downloads(_ downloads: [HostDownload], grants: UInt32?) -> String {
        struct Push: Encodable {
            var downloads: [HostDownload]
            var grants: UInt32?
        }
        let json = try? JSONEncoder().encode(Push(downloads: downloads, grants: grants))
        return json.flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
    }

    /// `GameEntry::icon_token`'s re-validation: lowercase-first, at most 32 of `[a-z0-9-]`.
    private static func validIconToken(_ t: String) -> Bool {
        !t.isEmpty && t.count <= 32 && t.first!.isLowercase && t.first!.isLetter
            && t.allSatisfy { $0.isNumber || $0 == "-" || ($0.isLowercase && $0.isLetter) }
    }

    public static func libraryError(title: String, body: String, canRetry: Bool) -> String {
        string(["Error": ["title": title, "body": body, "can_retry": canRetry]])
    }

    /// `/status` games; an entry without an id has no tile.
    public static func runningGames(_ games: [RunningGame]) -> String {
        string(
            games.compactMap { g -> [String: Any]? in
                guard let id = g.appID else { return nil }
                return [
                    "app_id": id, "state": g.state, "awaiting_window": g.awaitingWindow ?? false,
                    "endable": g.endable ?? false,
                ]
            })
    }
}
