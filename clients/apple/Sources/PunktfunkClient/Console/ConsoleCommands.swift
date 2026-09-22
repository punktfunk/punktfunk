// What the console asks the app's services to do (`ConsoleCmd`), drained every frame. Pairing,
// library fetches and host actions ride this bus; what the shell wants SHOWN goes back as a
// model push or a notice toast.

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
        case "OpenPlatformScreen":
            platformScreen = a["id"] as? String
        // Owed: the link speed test (a second connect through `startSpeedTest`) and the pad
        // grants, neither of which this client has a service for yet.
        case "SpeedTest", "PadAction":
            notice("That isn't here yet on this device.")
        default:
            break
        }
    }

    private func port(_ value: Any?) -> UInt16 { UInt16(value as? Int ?? 0) }

    // MARK: - library

    /// The shelf's catalog, its cached copy first so the grid is never empty while the fetch
    /// runs, then what the host answers. `refreshOnly` asks about running titles alone.
    func fetchLibrary(addr: String, mgmt: UInt16, fp: String, refreshOnly: Bool) {
        guard let host = host(fp: fp, addr: addr, port: 0) else { return }
        guard let identity = (try? ClientIdentityStore.shared.load())?.identity else {
            bridge.push(
                .libraryPhase,
                ConsoleJSON.libraryError(
                    title: "No identity", body: "This device has no client certificate yet.",
                    canRetry: false))
            return
        }
        fetching?.cancel()
        if !refreshOnly { bridge.push(.libraryBegin, "{}") }
        fetching = Task { [weak self] in
            guard let self else { return }
            if !refreshOnly, let cached = await LibraryCache.shared?.load(hostID: host.id.uuidString) {
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
                bridge.push(
                    .libraryPhase,
                    ConsoleJSON.libraryError(
                        title: "Couldn't read the library", body: "\(error)", canRetry: true))
            }
        }
    }

    /// Posters, as they arrive. The shell decodes each at the size it draws.
    private func loadArt(
        _ games: [GameEntry], host: StoredHost, identity: ClientIdentity, mgmt: UInt16
    ) {
        guard let loader = try? LibraryArtLoader(
            address: host.address, port: mgmt, certPEM: identity.certPEM,
            keyPEM: identity.keyPEM, hostFingerprint: host.pinnedSHA256)
        else { return }
        Task { [weak self] in
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
