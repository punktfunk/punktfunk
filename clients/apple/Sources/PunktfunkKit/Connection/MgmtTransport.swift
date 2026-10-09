// HTTPS transport for the host's management REST API, built on Network.framework rather than
// URLSession.
//
// WHY NOT URLSession. App Transport Security governs the URL loading system, and its default
// policy exempts only "local" destinations — `.local` names, unqualified names, and RFC1918 /
// link-local IP literals. Everything else must present a certificate that passes system trust
// evaluation. A punktfunk host is self-signed by construction (there is no CA that could vouch
// for a box on someone's LAN), so the library worked at 192.168.x and died at the TLS layer on
// every other address: a Tailscale peer (100.64/10 is CGNAT, NOT RFC1918), a WireGuard peer, or a
// public IP. No ATS key can express "any address the user typed" — the exception keys are
// domain-scoped — so the only ways out were disabling ATS app-wide (which also drops the TLS
// floor and the cleartext block on third-party cover-art fetches, the one surface we did NOT want
// to open) or leaving the URL loading system for this one origin. This is that second option.
//
// Network.framework is not subject to ATS, and `sec_protocol_options_set_verify_block` lets us
// state the trust rule we actually mean: the leaf certificate must hash to the fingerprint the
// user pinned during PIN pairing. That is a stronger check than CA trust here, not a weaker one,
// and it is the same rule the QUIC stream plane has always applied via punktfunk-core — which is
// precisely why streaming kept working over Tailscale while the library did not.
//
// Connections are POOLED and kept alive: a library screen fetches one JSON payload and then a
// poster per title, and giving each its own TLS handshake was pure latency. `MgmtConnectionPool`
// keeps a small number of connections per host, hands them out one request at a time, and makes
// callers wait rather than opening an unbounded number.

import CryptoKit
import Foundation
import Network
import Security

enum MgmtTransportError: Error, Sendable {
    /// The host's certificate did not hash to the pinned fingerprint — an impostor, or a host
    /// that was reinstalled/re-keyed since pairing.
    case pinMismatch
    case connection(String)
    case timedOut
    case tooLarge
    case invalidPort(UInt16)
}

enum MgmtTransport {
    /// Largest response we will buffer. The host's art proxy serves Steam hero images that run to
    /// a few MB; anything past this is not a poster and not a library payload.
    static let maxResponseBytes = 16 * 1024 * 1024

    /// One request to `https://host:port/path`, authenticated by mTLS (`identity`) and pinned by
    /// `pin`. A `body` goes out with `contentType` and a length.
    ///
    /// Runs over a pooled keep-alive connection. A connection the host has since dropped is
    /// indistinguishable from a live one until we write to it, so a REUSED connection that fails
    /// is retried, up to once per pooled socket; a fresh connection that fails is a real error.
    static func request(
        host: String,
        port: UInt16,
        method: String,
        path: String,
        body: Data?,
        contentType: String?,
        identity: SecIdentity,
        pin: Data,
        timeout: TimeInterval = 15
    ) async throws -> HTTPResponse {
        guard let nwPort = NWEndpoint.Port(rawValue: port) else {
            throw MgmtTransportError.invalidPort(port)
        }
        let key = "\(unbracketed(host)):\(port):\(hex(pin))"
        var lastError: Error = MgmtTransportError.connection("no attempt made")

        // The pool holds up to `maxPerHost` sockets and the host can have half-closed all of
        // them while idle, so one retry is not enough: an idle library screen would otherwise
        // report an unreachable host that a manual refresh immediately reaches. A FRESH
        // connection still gets exactly one attempt — retrying that only doubles a real
        // failure's latency.
        for attempt in 0...MgmtConnectionPool.maxPerHost {
            let connection = try await MgmtConnectionPool.shared.acquire(key: key) {
                MgmtConnection(host: unbracketed(host), port: nwPort, identity: identity, pin: pin)
            }
            let wasReused = connection.hasServedRequest
            do {
                let response = try await connection.perform(
                    method: method, path: path, body: body, contentType: contentType,
                    timeout: timeout)
                await MgmtConnectionPool.shared.release(connection, key: key)
                return response
            } catch {
                await MgmtConnectionPool.shared.release(connection, key: key)
                lastError = error
                if !wasReused || attempt == MgmtConnectionPool.maxPerHost { throw error }
            }
        }
        throw lastError
    }

    static func hex(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }

    /// Saved hosts store bare addresses, but a user who pasted a bracketed IPv6 literal shouldn't
    /// get an unresolvable endpoint out of it.
    static func unbracketed(_ host: String) -> String {
        guard host.hasPrefix("["), host.hasSuffix("]"), host.count > 2 else { return host }
        return String(host.dropFirst().dropLast())
    }
}

/// A pool of keep-alive connections, at most `maxPerHost` per host. Callers past that wait for one
/// to come back rather than opening more — a library grid can ask for dozens of posters at once,
/// and answering that with dozens of TLS handshakes is what this exists to prevent.
actor MgmtConnectionPool {
    static let shared = MgmtConnectionPool()

    private var available: [String: [MgmtConnection]] = [:]
    /// Connections created and not yet closed, per host — the cap this pool enforces.
    private var live: [String: Int] = [:]
    private var waiters: [String: [CheckedContinuation<Void, Never>]] = [:]
    /// Bumped by `closeAll`: a connection checked out under an older epoch is closed on release
    /// rather than pooled, so the close reaches sockets still in flight, not just idle ones.
    private var epoch: [String: UInt64] = [:]
    /// Also the retry budget: every pooled socket may be a stale keep-alive.
    static let maxPerHost = 4
    /// Idle longer than this and a pooled socket is replaced, not trusted: a host that vanished
    /// silently costs a full request timeout per socket before the retry loop gives up.
    static let maxIdle: TimeInterval = 20

    /// Throws `CancellationError` for a cancelled caller, before it takes a connection. A grid
    /// scrolled past queues a wait per tile; those must not hold the tiles on screen back.
    func acquire(key: String, make: () -> MgmtConnection) async throws -> MgmtConnection {
        var woken = false
        while true {
            if Task.isCancelled {
                // The slot this caller was woken for goes to the next in line.
                if woken { wakeNext(key) }
                throw CancellationError()
            }
            if var idle = available[key], let connection = idle.popLast() {
                available[key] = idle
                let fresh = ProcessInfo.processInfo.systemUptime - connection.pooledAt < Self.maxIdle
                if connection.isHealthy, fresh {
                    connection.poolEpoch = epoch[key] ?? 0
                    return connection
                }
                connection.close()
                live[key] = max(0, (live[key] ?? 1) - 1)
                continue
            }
            if (live[key] ?? 0) < Self.maxPerHost {
                live[key] = (live[key] ?? 0) + 1
                let connection = make()
                connection.poolEpoch = epoch[key] ?? 0
                return connection
            }
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                waiters[key, default: []].append(continuation)
            }
            woken = true
        }
    }

    private func wakeNext(_ key: String) {
        guard var queue = waiters[key], !queue.isEmpty else { return }
        let next = queue.removeFirst()
        waiters[key] = queue
        next.resume()
    }

    /// Callers parked on `key`, for the tests.
    func waiting(key: String) -> Int { waiters[key]?.count ?? 0 }

    /// Always call this, on success AND on failure: a connection that is never returned leaks a
    /// slot, and enough leaked slots would hang every later request on the waiter queue.
    func release(_ connection: MgmtConnection, key: String) {
        let stillCurrent = connection.poolEpoch == (epoch[key] ?? 0)
        if connection.isHealthy, stillCurrent,
           (available[key]?.count ?? 0) < Self.maxPerHost {
            connection.pooledAt = ProcessInfo.processInfo.systemUptime
            available[key, default: []].append(connection)
        } else {
            connection.close()
            live[key] = max(0, (live[key] ?? 1) - 1)
        }
        wakeNext(key)
    }

    /// Drop every connection for a host — used when a library screen goes away, so we don't sit
    /// on sockets the user is done with. Idle ones close here; checked-out ones were stamped
    /// with the pre-close epoch, so `release` closes them when they come back.
    func closeAll(matching prefix: String) {
        let keys = live.keys.filter { $0.hasPrefix(prefix) }
        for key in keys {
            epoch[key, default: 0] += 1
            for connection in available[key] ?? [] {
                connection.close()
                live[key] = max(0, (live[key] ?? 0) - 1)
            }
            available[key] = []
        }
    }
}

/// One TLS connection to a host, serving requests one at a time. The pool guarantees a single
/// caller at a time, so there is no request queueing here.
///
/// Everything mutable is touched only on `queue`, which also runs the connection's callbacks, the
/// verify block and the timeout — so the state below needs no locking and two callbacks can never
/// race to resume the same continuation.
final class MgmtConnection: @unchecked Sendable {
    private let queue = DispatchQueue(label: "io.unom.punktfunk.mgmt-connection")
    private let connection: NWConnection
    private let host: String
    private let port: UInt16

    private enum Phase { case idle, connecting, ready, dead }
    private var phase: Phase = .idle
    private var pending: CheckedContinuation<HTTPResponse, Error>?
    private var pendingRequest: Data?
    /// Bytes read past the end of the last response. Non-empty only if a host pipelines ahead of
    /// us, which none do — but dropping them would silently corrupt the next read.
    private var buffer = Data()
    private var operation = 0
    private var servedRequest = false

    /// The pool's checkout stamp — written and read only by `MgmtConnectionPool`, which is why
    /// it needs no queue hop.
    var poolEpoch: UInt64 = 0
    /// When the pool last took it back (system uptime). Pool-owned, like `poolEpoch`.
    var pooledAt: TimeInterval = 0
    /// False once the connection has failed; the pool discards these instead of handing them out.
    private(set) var isHealthy = true
    /// Has this connection completed at least one request? Drives the retry-once rule in
    /// `MgmtTransport.request` — only a connection the host may have dropped since is worth retrying.
    var hasServedRequest: Bool { servedRequest }

    /// A nil `identity` presents no client certificate. Only the pool's tests pass one: they
    /// never start the connection, and building an identity writes to the Keychain.
    init(host: String, port: NWEndpoint.Port, identity: SecIdentity?, pin: Data) {
        self.host = host
        self.port = port.rawValue
        let options = NWProtocolTLS.Options()
        let sec = options.securityProtocolOptions
        sec_protocol_options_set_min_tls_protocol_version(sec, .TLSv12)
        // Our half of the mTLS handshake: the same paired identity the host authorizes the
        // read-only library routes by (mgmt/auth.rs `cert_may_access`).
        if let identity, let secIdentity = sec_identity_create(identity) {
            sec_protocol_options_set_local_identity(sec, secIdentity)
        }
        let rejected = RejectionFlag()
        // Replaces system trust evaluation wholesale, which is the point: the host is self-signed
        // and carries no SAN, so there is nothing for the system policy to succeed at. Pinning the
        // leaf's SHA-256 is the real check.
        sec_protocol_options_set_verify_block(sec, { _, trust, complete in
            let secTrust = sec_trust_copy_ref(trust).takeRetainedValue()
            guard let chain = SecTrustCopyCertificateChain(secTrust) as? [SecCertificate],
                  let leaf = chain.first
            else {
                rejected.value = true
                complete(false)
                return
            }
            let fingerprint = Data(SHA256.hash(data: SecCertificateCopyData(leaf) as Data))
            let matches = fingerprint == pin
            if !matches { rejected.value = true }
            complete(matches)
        }, queue)
        self.connection = NWConnection(
            to: .hostPort(host: NWEndpoint.Host(host), port: port),
            using: NWParameters(tls: options, tcp: NWProtocolTCP.Options()))
        self.rejection = rejected
        self.connection.stateUpdateHandler = { [weak self] state in
            self?.handle(state)
        }
    }

    /// Set from the verify block, read when mapping the resulting handshake failure. Its own
    /// object because the block is built before `self` exists.
    private let rejection: RejectionFlag
    private final class RejectionFlag: @unchecked Sendable { var value = false }

    func perform(
        method: String = "GET", path: String, body: Data? = nil, contentType: String? = nil,
        timeout: TimeInterval
    ) async throws -> HTTPResponse {
        try await withCheckedThrowingContinuation { continuation in
            queue.async { [self] in
                guard self.phase != .dead else {
                    continuation.resume(throwing: MgmtTransportError.connection("connection closed"))
                    return
                }
                self.operation += 1
                let op = self.operation
                self.pending = continuation
                self.pendingRequest = Self.requestBytes(
                    host: self.host, port: self.port,
                    method: method, path: path, body: body, contentType: contentType)
                self.buffer.removeAll(keepingCapacity: true)
                self.queue.asyncAfter(deadline: .now() + timeout) { [weak self] in
                    guard let self, self.operation == op else { return }
                    // Retire the socket, never pool it: the request we gave up on is still in
                    // flight with a receive armed, so a reused connection would parse that late
                    // response as the NEXT request's — a poster answering a /library call.
                    self.phase = .dead
                    self.isHealthy = false
                    self.connection.cancel()
                    self.finish(.failure(MgmtTransportError.timedOut))
                }
                switch self.phase {
                case .idle:
                    self.phase = .connecting
                    self.connection.start(queue: self.queue)
                case .ready:
                    self.send()
                case .connecting, .dead:
                    break // `.ready` (or a failure) will pick the pending request up
                }
            }
        }
    }

    func close() {
        queue.async {
            self.phase = .dead
            self.isHealthy = false
            self.connection.cancel()
        }
    }

    // MARK: - Queue-confined internals

    private func handle(_ state: NWConnection.State) {
        switch state {
        case .ready:
            phase = .ready
            if pendingRequest != nil { send() }
        case .failed(let error):
            phase = .dead
            isHealthy = false
            finish(.failure(mapped(error)))
        case .waiting(let error):
            // Refused, or no route: NWConnection would wait for a better path while the request
            // sat out its whole timeout. Fail it now; the callers own their retries.
            phase = .dead
            isHealthy = false
            connection.cancel()
            finish(.failure(mapped(error)))
        case .cancelled:
            phase = .dead
            isHealthy = false
            finish(.failure(MgmtTransportError.connection("cancelled")))
        default:
            break
        }
    }

    private func send() {
        guard let request = pendingRequest else { return }
        pendingRequest = nil
        connection.send(content: request, completion: .contentProcessed { [weak self] error in
            guard let self else { return }
            if let error {
                self.isHealthy = false
                self.finish(.failure(self.mapped(error)))
                return
            }
            self.receive()
        })
    }

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
            [weak self] chunk, _, isComplete, error in
            guard let self else { return }
            if let chunk, !chunk.isEmpty { self.buffer.append(chunk) }
            if self.buffer.count > MgmtTransport.maxResponseBytes {
                self.isHealthy = false
                self.finish(.failure(MgmtTransportError.tooLarge))
                return
            }
            if let error {
                self.isHealthy = false
                self.finish(.failure(self.mapped(error)))
                return
            }
            do {
                if let length = try HTTPResponseParser.messageLength(in: self.buffer) {
                    let message = self.buffer.prefix(length)
                    self.buffer = Data(self.buffer.dropFirst(length))
                    let response = try HTTPResponseParser.parse(message)
                    // A response the peer means to be last leaves nothing reusable behind. Nor
                    // does a stream with bytes left over: we never pipeline, so anything trailing
                    // means we are out of sync, and reusing the connection would misread the next
                    // response rather than fail cleanly.
                    if response.wantsClose || !self.buffer.isEmpty { self.isHealthy = false }
                    self.servedRequest = true
                    self.finish(.success(response))
                    return
                }
                if isComplete {
                    // No framing header: the body ran to EOF, so what we have is the whole thing
                    // and the connection is spent.
                    self.isHealthy = false
                    let response = try HTTPResponseParser.parse(self.buffer)
                    self.servedRequest = true
                    self.finish(.success(response))
                    return
                }
            } catch {
                self.isHealthy = false
                self.finish(.failure(error))
                return
            }
            self.receive()
        }
    }

    private func finish(_ result: Result<HTTPResponse, Error>) {
        guard let continuation = pending else { return }
        pending = nil
        operation += 1 // invalidate this operation's timeout
        continuation.resume(with: result)
    }

    /// A rejected pin surfaces as a generic handshake failure; the flag is how we recover what
    /// actually happened, so the UI can say "re-pair" instead of "offline".
    private func mapped(_ error: NWError) -> MgmtTransportError {
        rejection.value ? .pinMismatch : .connection(String(describing: error))
    }

    /// The wire bytes of one request. Pure (and `static`) so the framing is unit-testable.
    static func requestBytes(
        host: String, port: UInt16,
        method: String, path: String, body: Data?, contentType: String?
    ) -> Data {
        // An IPv6 literal is bracketed in the Host header (RFC 9110 §7.2); a name or IPv4 is not.
        let authority = host.contains(":") ? "[\(host)]:\(port)" : "\(host):\(port)"
        var head = "\(method) \(path) HTTP/1.1\r\nHost: \(authority)\r\n"
            + "User-Agent: punktfunk-apple\r\nAccept: */*\r\n"
        if let body {
            // Always framed by length — a request body has no EOF to end it on a kept-alive
            // connection, and the host's axum would otherwise wait for one.
            head += "Content-Type: \(contentType ?? "application/octet-stream")\r\n"
            head += "Content-Length: \(body.count)\r\n"
        }
        head += "\r\n"
        var request = Data(head.utf8)
        if let body { request.append(body) }
        return request
    }
}
