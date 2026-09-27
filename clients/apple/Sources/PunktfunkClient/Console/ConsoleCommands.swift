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
            bindPreset(key: a["key"] as? String ?? "", preset: a["preset_id"] as? String)
        case "SetClipboard":
            if var host = host(key: a["key"] as? String ?? "") {
                host.clipboardSync = a["on"] as? Bool ?? false
                store.update(host)
            }
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
                conn = try PunktfunkConnection(
                    host: addr, port: port, width: 1280, height: 720, refreshHz: 60,
                    pinSHA256: pin, identity: identity)
            } catch {
                await self?.pushSpeed(key, ["Failed": "Couldn't reach \(addr) — it may be asleep."])
                return
            }
            defer { conn.close() }
            conn.startSpeedTest(targetKbps: 3_000_000, durationMs: 5_000)
            await self?.pushSpeed(key, "Measuring")
            // The host clamps the burst to five seconds; its report lands just after.
            let deadline = Date().addingTimeInterval(13)
            while Date() < deadline {
                try? await Task.sleep(nanoseconds: 200_000_000)
                guard let r = conn.probeResult() else { break }
                guard r.done else {
                    // The live figure, for the console's graph.
                    await self?.pushSpeed(key, ["Progress": ["kbps": r.throughputKbps]])
                    continue
                }
                let done: [String: Any] = [
                    "throughput_kbps": r.throughputKbps, "loss_pct": r.lossPct,
                    "recommended_kbps": r.throughputKbps / 10 * 7,
                ]
                await self?.pushSpeed(key, ["Done": done])
                return
            }
            await self?.pushSpeed(
                key, ["Failed": "The measurement never finished — the connection may have dropped."])
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
            pushArt(DemoMode.games, from: DemoMode.art)
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
            bridge.push(.libraryBegin, "{}")
        }
        let task = Task { [weak self] in
            guard let self else { return }
            var cached: CachedLibrary?
            if !refreshOnly { cached = await LibraryCache.shared?.load(hostID: host.id.uuidString) }
            if let cached {
                bridge.push(.libraryCached, ConsoleJSON.libraryGames(cached.games))
            }
            let running = await LibraryClient.running(
                address: addr, port: mgmt, certPEM: identity.certPEM, keyPEM: identity.keyPEM,
                hostFingerprint: host.pinnedSHA256)
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
                // The cached shelf stays up, marked offline; its covers come from the art cache.
                if let cached {
                    loadArt(cached.games, host: host, identity: identity, mgmt: mgmt)
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

    /// Posters, as they arrive. The shell decodes each at the size it draws.
    private func loadArt(
        _ games: [GameEntry], host: StoredHost, identity: ClientIdentity, mgmt: UInt16
    ) {
        guard let loader = try? LibraryArtLoader(
            address: host.address, port: mgmt, certPEM: identity.certPEM,
            keyPEM: identity.keyPEM, hostFingerprint: host.pinnedSHA256)
        else { return }
        pushArt(games, from: loader)
    }

    /// Each title's first poster that loads, in the order the touch grid takes them.
    private func pushArt(_ games: [GameEntry], from loader: any LibraryArtSource) {
        artTask?.cancel()
        artTask = Task { [weak self] in
            for game in games {
                if Task.isCancelled { return }
                // The capsule first, then the header: the same order the touch grid takes.
                for url in game.art.posterCandidates {
                    guard let bytes = try? await loader.data(for: url) else { continue }
                    self?.bridge.art(id: game.id, bytes: bytes)
                    break
                }
            }
        }
    }

    // MARK: - hosts

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

    private func bindPreset(key: String, preset: String?) {
        guard var host = host(key: key) else { return }
        host.presetID = preset
        store.update(host)
    }

    private func wake(key: String, thenConnect: Bool) {
        guard let host = host(key: key) else { return }
        waker.start(
            host: host, connectsAfter: thenConnect, macs: host.wakeMacs, lastIP: host.address,
            isOnline: { [weak self] in
                guard let self else { return false }
                await store.refreshReachability(discovery: discovery)
                return store.probedOnline.contains(host.id)
            },
            onOnline: { [weak self] in
                guard let self else { return }
                bridge.push(.wake, "null")
                if thenConnect { actions.connect(host, .inherit) }
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
