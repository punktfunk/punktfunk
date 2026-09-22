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

    /// The home carousel: saved hosts by name, each followed by its pinned preset cards, then
    /// the discovered-but-unsaved. Mirrors the desktop service's `rows()`.
    public static func hostRows(
        saved: [StoredHost], discovered: [DiscoveredHost], online: Set<StoredHost.ID>,
        presets: [StreamPreset], actions hostActions: [String: [HostAction]] = [:],
        running: [String: String] = [:]
    ) -> String {
        var out: [[String: Any]] = []
        for host in saved.sorted(by: { $0.name.lowercased() < $1.name.lowercased() }) {
            let advert = discovered.first { d in
                (!fingerprint(host).isEmpty
                    && d.fingerprintHex?.caseInsensitiveCompare(fingerprint(host)) == .orderedSame)
                    || (d.host == host.address && d.port == host.port)
            }
            let base = row(host, advert: advert, online: online.contains(host.id),
                           presets: presets, hostActions: hostActions, running: running)
            out.append(base)
            // A pinned card shares the primary tile's live state; its key rides the preset id
            // behind a NUL, which no fingerprint or `addr:port` can hold.
            for id in host.pinnedPresetIDs ?? [] {
                guard let preset = presets.first(where: { $0.id == id }) else { continue }
                var card = base
                card["key"] = "\(base["key"] as? String ?? "")\u{0}\(preset.id)"
                card["pin"] = chip(preset)
                card["bound_preset"] = NSNull()
                out.append(card)
            }
        }
        let unsaved = discovered.filter { d in
            !saved.contains { host in
                (!fingerprint(host).isEmpty
                    && d.fingerprintHex?.caseInsensitiveCompare(fingerprint(host)) == .orderedSame)
                    || (host.address == d.host && host.port == d.port)
            }
        }
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
            // This client binds no preset per title yet, so the bind screen's marks are empty.
            "game_presets": [String: String](),
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
    public static func libraryGames(_ games: [GameEntry]) -> String {
        string(
            games.map { g in
                [
                    "id": g.id, "title": g.title, "store": g.store,
                    "launcher": g.role == "launcher",
                    "icon": g.icon.flatMap { validIconToken($0) ? $0 : nil } ?? "",
                    "platform": g.platform ?? NSNull(), "developer": g.developer ?? NSNull(),
                    "year": g.releaseYear ?? NSNull(), "genres": g.genres ?? [],
                    "running": false,
                ]
            })
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
                return ["app_id": id, "state": g.state, "awaiting_window": g.awaitingWindow ?? false]
            })
    }
}
