// What the console asks the app's services to do (`ConsoleCmd`), drained every frame. Pairing,
// library fetches and host actions ride this bus; what the shell wants SHOWN goes back as a
// model push or a notice toast.

import CoreHaptics
import Foundation
import PunktfunkKit
import PunktfunkShared

extension ConsoleModel {
    func drainCommands() {
        guard let data = bridge.drainCmds().data(using: .utf8),
            let cmds = try? JSONSerialization.jsonObject(with: data) as? [Any]
        else { return }
        for cmd in cmds {
            // A unit variant arrives as a bare string, one with fields as `{"Name": {…}}`.
            if let name = cmd as? String {
                switch name {
                case "CancelWake": waker.cancel()
                case "Probe": Task { await store.refreshReachability(discovery: discovery) }
                case "LoadLicenses": pushLicenses()
                default: break
                }
                continue
            }
            guard let cmd = cmd as? [String: Any], let (name, body) = cmd.first else { continue }
            run(name, body as? [String: Any] ?? [:])
        }
    }

    private func run(_ name: String, _ a: [String: Any]) {
        switch name {
        case "FetchLibrary":
            fetchLibrary(
                addr: a["addr"] as? String ?? "", mgmt: port(a["mgmt"]),
                fp: a["fp_hex"] as? String ?? "", refreshOnly: false)
        case "RefreshRunning":
            fetchLibrary(
                addr: a["addr"] as? String ?? "", mgmt: port(a["mgmt"]),
                fp: a["fp_hex"] as? String ?? "", refreshOnly: true)
        case "Pair":
            pair(
                addr: a["addr"] as? String ?? "", port: port(a["port"]),
                pin: a["pin"] as? String ?? "", deviceName: a["device_name"] as? String ?? "")
        case "SendLogs":
            sendLogs(fp: a["fp_hex"] as? String ?? "", addr: a["addr"] as? String ?? "")
        case "SaveHost":
            saveHost(
                name: a["name"] as? String ?? "", addr: a["addr"] as? String ?? "",
                port: port(a["port"]))
        case "UpdateHost":
            updateHost(
                key: a["key"] as? String ?? "", name: a["name"] as? String ?? "",
                addr: a["addr"] as? String ?? "", port: port(a["port"]))
        case "ForgetHost":
            if let host = host(key: a["key"] as? String ?? "") { store.remove(host) }
        case "SavePreset":
            savePreset(
                id: a["id"] as? String ?? "", name: a["name"] as? String ?? "",
                overrides: a["overrides"] as? [String: Any] ?? [:])
        case "DeletePreset":
            presets.delete(a["id"] as? String ?? "")
        case "UnpairHost":
            if let host = host(key: a["key"] as? String ?? "") { store.forgetIdentity(host) }
        case "Wake":
            wake(key: a["key"] as? String ?? "", thenConnect: a["then_connect"] as? Bool ?? false)
        case "SetPin":
            if let host = host(key: a["key"] as? String ?? ""),
                let preset = a["preset_id"] as? String
            {
                store.setPinned(host.id, presetID: preset, pinned: a["pin"] as? Bool ?? false)
            }
        case "BindPreset":
            bindPreset(
                key: a["key"] as? String ?? "", game: a["game"] as? String,
                preset: a["preset_id"] as? String)
        case "FetchProfiles":
            fetchProfiles(
                addr: a["addr"] as? String ?? "", mgmt: port(a["mgmt"]),
                fp: a["fp_hex"] as? String ?? "")
        case "SetProfile":
            if let host = host(key: a["key"] as? String ?? "") {
                let pick = a["profile"] as? [String: Any]
                store.setProfile(
                    host.id,
                    (pick?["id"] as? String).map {
                        ProfilePick(id: $0, displayName: pick?["display_name"] as? String ?? "")
                    })
            }
        case "SetClipboard":
            if var host = host(key: a["key"] as? String ?? "") {
                host.clipboardSync = a["on"] as? Bool ?? false
                store.update(host)
            }
        case "EndGame":
            endGame(
                addr: a["addr"] as? String ?? "", mgmt: port(a["mgmt"]),
                fp: a["fp_hex"] as? String ?? "", appID: a["app_id"] as? String ?? "",
                title: a["title"] as? String ?? "")
        case "Install":
            guard let action = InstallAction(rawValue: a["action"] as? String ?? "") else { return }
            changeInstall(
                addr: a["addr"] as? String ?? "", mgmt: port(a["mgmt"]),
                fp: a["fp_hex"] as? String ?? "", appID: a["app_id"] as? String ?? "",
                title: a["title"] as? String ?? "", action: action)
        case "HostAction":
            hostAction(
                fp: a["fp_hex"] as? String ?? "", id: a["action_id"] as? String ?? "",
                label: a["label"] as? String ?? "")
        case "PadAction":
            padAction(a["action"] as? String ?? "", key: a["pad_key"] as? String ?? "")
        case "PadTest":
            padTest(a["on"] as? Bool ?? false)
        case "PromptAnswer":
            answerPrompt(id: a["id"] as? String ?? "", choice: a["choice"] as? Int)
        case "SpeedTest":
            speedTest(
                key: a["key"] as? String ?? "", addr: a["addr"] as? String ?? "",
                port: port(a["port"]), fp: a["fp_hex"] as? String ?? "")
        default:
            break
        }
    }

    private func port(_ value: Any?) -> UInt16 { UInt16(value as? Int ?? 0) }

    /// A preset the console's editor saved whole, merged onto this app's copy of it.
    private func savePreset(id: String, name: String, overrides: [String: Any]) {
        guard !id.isEmpty else { return }
        var preset = presets.preset(id: id) ?? StreamPreset(name: name, id: id)
        preset.name = name
        preset.overrides = ConsoleJSON.overlay(overrides, over: preset.overrides)
        presets.put(preset)
    }

    /// What this app bundles beside the console's own texts, for its Licences screen.
    private func pushLicenses() {
        let sections: [[String: String]] = [
            ["heading": "Swift packages", "text": Licenses.swiftPackages],
            [
                "heading": "Third-party software",
                "text": Licenses.thirdPartyIntro + "\n\n" + Licenses.thirdPartyNotices,
            ],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: sections),
            let json = String(data: data, encoding: .utf8)
        else { return }
        bridge.push(.licenses, json)
    }

    /// The console's link test: one probe burst over a second connect, each phase pushed back
    /// as `SpeedPhase`. The console raised the takeover itself and owns clearing it. 720p60, as
    /// `pf_client_core::speed` connects: nothing presents a frame, and a 4K encode for a burst
    /// would be slower for nothing. The recommendation is that module's integer rule.
    private func speedTest(key: String, addr: String, port: UInt16, fp: String) {
        guard let identity = (try? ClientIdentityStore.shared.load())?.identity else {
            pushSpeed(key, ["Failed": "This device has no client certificate yet."])
            return
        }
        let pin = host(fp: fp, addr: addr, port: port)?.pinnedSHA256
        Task.detached(priority: .userInitiated) { [weak self] in
            let conn: PunktfunkConnection
            do {
                // A diagnostic session: probes only, the host's facts asked for.
                conn = try PunktfunkConnection(
                    host: addr, port: port, width: 1280, height: 720, refreshHz: 60,
                    pinSHA256: pin, identity: identity,
                    deliveryFlags: PunktfunkConnection.deliveryFacts
                        | PunktfunkConnection.deliveryProbeOnly)
            } catch {
                await self?.pushSpeed(key, ["Failed": "Couldn't reach \(addr) — it may be asleep."])
                return
            }
            defer { conn.close() }
            await self?.pushSpeed(key, "Measuring")
            // The whole check, reported once: the ceiling, the clean round and the findings.
            guard let r = conn.networkCheck() else {
                await self?.pushSpeed(
                    key, ["Failed": "The measurement never finished — the connection may have dropped."])
                return
            }
            var done: [String: Any] = [
                "throughput_kbps": r.ceilingKbps, "wall": r.wall,
                "loss_pct": r.clean?.lossPct ?? 0,
                "recommended_kbps": r.ceilingKbps / 10 * 7,
                "findings": r.findings.map { f -> [String: Any] in
                    [
                        "id": f.id, "severity": f.severity, "numbers": f.numbers,
                        "profile": f.profile == 0 ? NSNull() : f.profile,
                    ]
                },
            ]
            if let c = r.clean {
                done["clean"] = ["rate_kbps": c.rateKbps, "loss_pct": c.lossPct, "jitter_us": c.jitterUs]
            } else {
                done["clean"] = NSNull()
            }
            await self?.pushSpeed(key, ["Done": done])
        }
    }

    private func pushSpeed(_ key: String, _ phase: Any) {
        bridge.push(.speed, ConsoleJSON.string(["key": key, "phase": phase]))
    }

    /// The Players card's rumble test: one firm pulse on the pad it names. The grants are
    /// Android's and never reach here.
    private func padAction(_ action: String, key: String) {
        guard action == "rumble",
            let pad = GamepadManager.shared.controllers.first(where: { $0.id == key }),
            let engine = pad.controller.haptics?.createEngine(withLocality: .default)
        else { return }
        do {
            try engine.start()
            let pulse = CHHapticEvent(
                eventType: .hapticContinuous,
                parameters: [CHHapticEventParameter(parameterID: .hapticIntensity, value: 1)],
                relativeTime: 0, duration: 0.35)
            try engine.makePlayer(with: CHHapticPattern(events: [pulse], parameters: []))
                .start(atTime: CHHapticTimeImmediate)
            // The closure holds the engine until the pulse has played.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { engine.stop() }
        } catch {
            notice("Couldn't run the rumble test — \(error.localizedDescription)")
        }
    }

    // MARK: - library

    /// The shelf's catalog, its cached copy first so the grid is never empty while the fetch
    /// runs, then what the host answers. `refreshOnly` asks about running titles alone.
    func fetchLibrary(addr: String, mgmt: UInt16, fp: String, refreshOnly: Bool) {
        guard let host = host(fp: fp, addr: addr, port: 0) else { return }
        if !refreshOnly { fetchSerial += 1 }
        let serial = fetchSerial
        // The demo host serves no management API; its shelf is built in. The shot harness has
        // no host to ask, so every host shows that shelf there.
        #if DEBUG
        let builtIn = DemoMode.isDemo(host) || ScreenshotMode.isActive
        #else
        let builtIn = DemoMode.isDemo(host)
        #endif
        if builtIn {
            bridge.push(.libraryRunning, ConsoleJSON.runningGames([]))
            if refreshOnly { return }
            bridge.push(.libraryBegin, "{}")
            bridge.push(.libraryGames, ConsoleJSON.libraryGames(DemoMode.games))
            bridge.push(.libraryPhase, "\"Ready\"")
            let art = DemoMode.art
            pushArt(DemoMode.games) { try? await art.data(for: $0) }
            return
        }
        guard let identity = (try? ClientIdentityStore.shared.load())?.identity else {
            bridge.push(
                .libraryPhase,
                ConsoleJSON.libraryError(
                    title: "No identity", body: "This device has no client certificate yet.",
                    canRetry: false))
            return
        }
        // A running-titles refresh must not cut a list fetch short; only a new fetch does.
        if !refreshOnly {
            fetching?.cancel()
            artTask?.cancel()
            artShown = []
            bridge.push(.libraryBegin, "{}")
        }
        let task = Task { [weak self] in
            guard let self else { return }
            var cached: CachedLibrary?
            if !refreshOnly { cached = await LibraryCache.shared?.load(hostID: host.id.uuidString) }
            if let cached, serial == self.fetchSerial {
                bridge.push(.libraryCached, ConsoleJSON.libraryGames(cached.games))
                // The cached covers go up with the cached shelf, not after the host's answer.
                loadArt(cached.games, host: host, identity: identity, mgmt: mgmt, cachedOnly: true)
            }
            let status = await LibraryClient.status(
                address: addr, port: mgmt, certPEM: identity.certPEM, keyPEM: identity.keyPEM,
                hostFingerprint: host.pinnedSHA256)
            let running = status.games
            // A newer fetch owns the shelf by the time a slow host answers: not its titles.
            guard serial == self.fetchSerial else { return }
            bridge.push(.libraryDownloads, ConsoleJSON.downloads(status.downloads, grants: status.grants))
            bridge.push(.libraryRunning, ConsoleJSON.runningGames(running))
            if refreshOnly { return }
            do {
                let games = try await LibraryClient.fetch(
                    address: addr, port: mgmt, certPEM: identity.certPEM, keyPEM: identity.keyPEM,
                    hostFingerprint: host.pinnedSHA256
                ).launchersFirst
                bridge.push(.libraryGames, ConsoleJSON.libraryGames(games))
                bridge.push(.libraryPhase, games.isEmpty ? "\"Empty\"" : "\"Ready\"")
                await LibraryCache.shared?.store(games, hostID: host.id.uuidString)
                loadArt(games, host: host, identity: identity, mgmt: mgmt)
            } catch {
                // A newer fetch owns the shelf now.
                if Task.isCancelled { return }
                // The cached shelf stays up, marked offline, with the covers the cache held.
                if cached != nil {
                    bridge.push(.libraryStale, "2")
                    return
                }
                bridge.push(
                    .libraryPhase,
                    ConsoleJSON.libraryError(
                        title: "Couldn't read the library", body: "\(error)", canRetry: true))
            }
        }
        if !refreshOnly { fetching = task }
    }

    /// Posters, as they arrive. The shell decodes each at the size it draws. `cachedOnly`
    /// asks the disk cache alone, for a shelf whose host has not answered.
    private func loadArt(
        _ games: [GameEntry], host: StoredHost, identity: ClientIdentity, mgmt: UInt16,
        cachedOnly: Bool = false
    ) {
        guard let loader = try? LibraryArtLoader(
            address: host.address, port: mgmt, certPEM: identity.certPEM,
            keyPEM: identity.keyPEM, hostFingerprint: host.pinnedSHA256)
        else { return }
        pushArt(games) { url in
            if cachedOnly { return await loader.cached(for: url) }
            return try? await loader.data(for: url)
        }
    }

    /// Each title's first poster that loads, in the order the touch grid takes them. A title
    /// this fetch already has a poster for is skipped.
    private func pushArt(_ games: [GameEntry], poster: @escaping @Sendable (URL) async -> Data?) {
        artTask?.cancel()
        artTask = Task { [weak self] in
            for game in games {
                guard let self, !Task.isCancelled else { return }
                if artShown.contains(game.id) { continue }
                // The capsule first, then the header: the same order the touch grid takes.
                for url in game.art.posterCandidates {
                    guard let bytes = await poster(url) else { continue }
                    // A newer fetch owns the shelf and its `artShown` by now.
                    if Task.isCancelled { return }
                    bridge.art(id: game.id, bytes: bytes)
                    artShown.insert(game.id)
                    break
                }
            }
        }
    }

    // MARK: - hosts

    /// The host's profile list, handed back to the shell's connect or Switch-profile screen.
    private func fetchProfiles(addr: String, mgmt: UInt16, fp: String) {
        guard let host = host(fp: fp, addr: addr, port: 0) else {
            pushProfiles(fp, ["Failed": "That host is no longer saved."])
            return
        }
        Task { [weak self] in
            let answer = await ProfileFetch.list(host, mgmt: mgmt > 0 ? mgmt : nil)
            switch answer {
            case .listed(let rows?):
                let data = try? JSONEncoder().encode(rows)
                let json = data.flatMap { try? JSONSerialization.jsonObject(with: $0) } ?? []
                self?.pushProfiles(fp, ["Listed": json])
            case .listed(nil):
                self?.pushProfiles(fp, "NoProfiles")
            case .failed:
                self?.pushProfiles(fp, ["Failed": "Couldn't load the profiles."])
            }
        }
    }

    private func pushProfiles(_ fp: String, _ answer: Any) {
        bridge.push(.profiles, ConsoleJSON.string(["fp_hex": fp, "answer": answer]))
    }

    private func saveHost(name: String, addr: String, port: UInt16) {
        var host = StoredHost(name: name, address: addr)
        host.port = port
        store.add(host)
    }

    private func updateHost(key: String, name: String, addr: String, port: UInt16) {
        guard var host = host(key: key) else { return }
        host.name = name
        host.address = addr
        host.port = port
        store.update(host)
    }

    /// The host's default binding, or with `game` that one title's. A nil `preset` clears either.
    private func bindPreset(key: String, game: String?, preset: String?) {
        guard var host = host(key: key) else { return }
        if let game {
            var bound = host.gamePresets ?? [:]
            bound[game] = preset
            host.gamePresets = bound.isEmpty ? nil : bound
        } else {
            host.presetID = preset
        }
        store.update(host)
    }

    private func wake(key: String, thenConnect: Bool) {
        guard let host = host(key: key) else { return }
        waker.start(
            host: host, connectsAfter: thenConnect, macs: host.wakeMacs, lastIP: host.address,
            isOnline: { [weak self] in
                guard let self, await store.isReachable(host, discovery: discovery)
                else { return false }
                // The wake card reads this set.
                store.probedOnline.insert(host.id)
                return true
            },
            onOnline: { [weak self] in
                guard let self else { return }
                bridge.push(.wake, "null")
                if thenConnect { actions.connect(host, .inherit, host.pickedProfile?.id) }
            })
    }

    /// The wake card the shell draws, from the waker's own state.
    func pushWake() {
        guard let waking = waker.waking, let host = store.hosts.first(where: { $0.id == waking.hostID })
        else {
            bridge.push(.wake, "null")
            return
        }
        let fp = host.pinnedSHA256?.map { String(format: "%02x", $0) }.joined() ?? ""
        bridge.push(
            .wake,
            ConsoleJSON.wake(
                key: ConsoleJSON.rowKey(fp, host.address, host.port), name: waking.hostName,
                seconds: waking.seconds, timedOut: waking.timedOut,
                online: store.probedOnline.contains(host.id),
                thenConnect: waking.connectsAfter))
    }

    // MARK: - pairing, logs and host actions

    private func pair(addr: String, port: UInt16, pin: String, deviceName: String) {
        bridge.push(.pair, ConsoleJSON.pairBusy)
        ceremony.run(host: addr, port: port, pin: pin, clientName: deviceName) { [weak self] cert in
            guard let self else { return }
            guard var host = host(fp: "", addr: addr, port: port) else {
                bridge.push(.pair, ConsoleJSON.pairFailed("That host is no longer saved."))
                return
            }
            host.pinnedSHA256 = cert
            store.update(host)
            actions.paired(host, cert)
            let fp = cert.map { String(format: "%02x", $0) }.joined()
            bridge.push(.pair, ConsoleJSON.pairPaired(key: fp))
            pushHosts()
        }
    }

    private func sendLogs(fp: String, addr: String) {
        guard let host = host(fp: fp, addr: addr, port: 0) else { return }
        Task { [weak self] in
            let sent = await SendLogs.toHost(host)
            self?.notice(sent.message)
        }
    }

    /// End a title this device launched, say how it went, then re-read what the host runs so
    /// the poster's badge follows.
    private func endGame(addr: String, mgmt: UInt16, fp: String, appID: String, title: String) {
        guard let host = host(fp: fp, addr: addr, port: 0), let pin = host.pinnedSHA256,
              let identity = (try? ClientIdentityStore.shared.load())?.identity else { return }
        Task { [weak self] in
            let outcome = await LibraryClient.endGame(
                appID: appID, address: addr, port: mgmt,
                certPEM: identity.certPEM, keyPEM: identity.keyPEM, hostFingerprint: pin)
            self?.notice(outcome.notice(title: title))
            self?.fetchLibrary(addr: addr, mgmt: mgmt, fp: fp, refreshOnly: true)
        }
    }

    /// Start, resume, pause or remove a title's download, say how it went, then re-read the host:
    /// the whole catalog after a removal (the tile turns to "not installed"), else `/status`.
    private func changeInstall(
        addr: String, mgmt: UInt16, fp: String, appID: String, title: String, action: InstallAction
    ) {
        guard let host = host(fp: fp, addr: addr, port: 0), let pin = host.pinnedSHA256,
              let identity = (try? ClientIdentityStore.shared.load())?.identity else { return }
        Task { [weak self] in
            let outcome = await LibraryClient.changeInstall(
                appID: appID, action: action, address: addr, port: mgmt,
                certPEM: identity.certPEM, keyPEM: identity.keyPEM, hostFingerprint: pin)
            self?.notice(outcome.notice(action, title: title))
            let removed = outcome == .done && action == .remove
            self?.fetchLibrary(addr: addr, mgmt: mgmt, fp: fp, refreshOnly: !removed)
        }
    }

    private func hostAction(fp: String, id: String, label: String) {
        guard let host = host(fp: fp, addr: "", port: 0),
            let action = power.actions(for: host).first(where: { $0.id == id })
        else { return }
        Task { [weak self] in
            guard let outcome = await self?.power.invoke(action, on: host) else { return }
            self?.notice(outcome.ok ? "\(label) sent." : outcome.message)
        }
    }
}
