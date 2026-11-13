import Foundation

/// Anything that answers poster bytes for a cover-art URL. The production implementation is
/// [`LibraryArtLoader`]; the screenshot harness substitutes a canned source so store frames carry
/// artwork without a host on the network.
public protocol LibraryArtSource: Sendable {
    func data(for url: URL) async throws -> Data
    /// Release pooled connections when the owning screen goes away. Sources without connections
    /// have nothing to do.
    func close() async
}

/// One fetch per key, shared by everyone who asks while it flies, and cancelled once the last
/// of them is. A tile scrolled past gives up its fetch; a tile still on screen keeps it.
final class ArtFlights: @unchecked Sendable {
    private struct Flight {
        let task: Task<Data, Error>
        var waiters: Int
    }

    private let lock = NSLock()
    private var flights: [String: Flight] = [:]

    func value(
        for key: String, fetch: @escaping @Sendable () async throws -> Data
    ) async throws -> Data {
        let task: Task<Data, Error> = lock.withLock {
            if var flying = flights[key] {
                flying.waiters += 1
                flights[key] = flying
                return flying.task
            }
            let task = Task.detached(operation: fetch)
            flights[key] = Flight(task: task, waiters: 1)
            return task
        }
        defer { lock.withLock { if flights[key]?.task == task { flights[key] = nil } } }
        return try await withTaskCancellationHandler {
            try await task.value
        } onCancel: {
            lock.withLock {
                guard var flying = flights[key], flying.task == task else { return }
                flying.waiters -= 1
                if flying.waiters > 0 {
                    flights[key] = flying
                } else {
                    flights[key] = nil
                    task.cancel()
                }
            }
        }
    }
}

/// Loads cover art for the library UI, routing each URL to the transport that suits its origin.
///
/// A `GameEntry`'s art candidates mix two very different things: the host's own art proxy
/// (`/api/v1/library/art/...`, a host-relative path asked of this loader's host) and public
/// store CDN URLs carried verbatim on custom/GOG/Heroic entries. Host URLs go over [`MgmtTransport`] with
/// the paired identity and the pinned fingerprint — outside the URL loading system, so App
/// Transport Security can stay ON app-wide. Every other origin keeps ordinary `URLSession` with
/// full system trust evaluation and no client certificate, which is exactly what it should get.
///
/// Posters are cached on disk (`ArtCache`), so a second visit to a library costs no network at
/// all — and the connections behind a first visit are pooled and kept alive rather than paying a
/// TLS handshake per tile.
///
/// Built once per library screen and reused across a whole grid's worth of posters.
public final class LibraryArtLoader: LibraryArtSource, @unchecked Sendable {
    private let target: MgmtTarget
    /// Third-party origins only, with the system's normal certificate validation and no URLCache
    /// (`ArtCache` owns persistence). Process-wide and never invalidated: a fetch that outlives
    /// `close()` would otherwise create a task on a dead session, which raises.
    private static let cdn: URLSession = {
        let config = URLSessionConfiguration.default
        config.urlCache = nil
        return URLSession(configuration: config)
    }()
    /// nil when the caches directory is unavailable — then we simply always fetch.
    private var cache: ArtCache? { ArtCache.shared }
    /// One fetch per cache key at a time — the same entry shown in two sections must not fetch
    /// its art twice on a cold cache. Failures are deliberately not remembered.
    private let flights = ArtFlights()

    public init(_ target: MgmtTarget) {
        self.target = target
    }

    /// Image bytes for one art URL, cached on disk after the first fetch. A miss propagates the
    /// error so the poster can move on to its next candidate.
    public func data(for url: URL) async throws -> Data {
        // Inline art never leaves the manifest — decode it here rather than probe a cache that
        // could never hold it.
        if url.scheme?.lowercased() == "data" { return try Self.inlineBytes(url) }
        let key = Self.cacheKey(
            for: url, hostAddress: target.address, hostPort: target.port, pin: target.pin)
        if let cache, let cached = await cache.data(forKey: key) { return cached }
        let fetched = try await flights.value(for: key) { try await self.fetch(url) }
        if let cache { await cache.store(fetched, forKey: key) }
        return fetched
    }

    /// The poster the disk cache or the URL itself already holds. Never the network: a
    /// powered-off host costs a full request timeout per poster it is asked for.
    public func cached(for url: URL) async -> Data? {
        if url.scheme?.lowercased() == "data" { return try? Self.inlineBytes(url) }
        let key = Self.cacheKey(
            for: url, hostAddress: target.address, hostPort: target.port, pin: target.pin)
        return await cache?.data(forKey: key)
    }

    /// The bytes of a base64 `data:` URL — art a plugin inlined rather than linked. Base64 is the
    /// only form the kit emits; anything else, or a body over the transport ceiling, is refused.
    static func inlineBytes(_ url: URL) throws -> Data {
        let s = url.absoluteString
        guard let comma = s.firstIndex(of: ","), s[..<comma].lowercased().hasSuffix(";base64"),
              let data = Data(base64Encoded: String(s[s.index(after: comma)...])),
              !data.isEmpty, data.count <= MgmtTransport.maxResponseBytes
        else { throw LibraryError.badArtURL }
        return data
    }

    /// Release this host's pooled connections — call when the library screen goes away, so we
    /// don't sit on open TLS sockets the user is finished with.
    public func close() async {
        await MgmtConnectionPool.shared.closeAll(
            matching: "\(MgmtTransport.unbracketed(target.address)):\(target.port):")
    }

    /// The cache entry's identity. A HOST-origin URL is `pin | path` — the machine, not the
    /// address it happened to have at fetch time: a re-addressed host must keep its cache, and a
    /// different machine answering the same address must not inherit it. Anything else is keyed
    /// by its URL, which already names its origin.
    static func cacheKey(for url: URL, hostAddress: String, hostPort: UInt16, pin: Data) -> String {
        guard isHostOrigin(url, address: hostAddress, port: hostPort) else {
            return url.absoluteString
        }
        return "\(MgmtTransport.hex(pin))|\(requestPath(url))"
    }

    /// The request path exactly as it goes on the wire — the ENCODED components. `url.path` and
    /// `url.query` hand back percent-DECODED text: an id that needed encoding breaks the request
    /// line, and a decoded CRLF splits the request outright.
    static func requestPath(_ url: URL) -> String {
        let parts = URLComponents(url: url, resolvingAgainstBaseURL: true)
        var path = parts?.percentEncodedPath ?? ""
        if path.isEmpty { path = "/" }
        if let query = parts?.percentEncodedQuery { path += "?\(query)" }
        return path
    }

    private func fetch(_ url: URL) async throws -> Data {
        guard Self.isHostOrigin(url, address: target.address, port: target.port) else {
            // A library entry names its own art URL, so this is host-supplied. Web schemes only
            // (`data:` never gets here) — a `file:` URL would make the client read its own
            // container and cache it as a poster — and the pinned path's ceiling, since nothing
            // else bounds a CDN body.
            guard let scheme = url.scheme?.lowercased(), scheme == "https" || scheme == "http"
            else { throw LibraryError.badArtURL }
            let (bytes, response) = try await Self.cdn.bytes(from: url)
            guard let http = response as? HTTPURLResponse else { throw LibraryError.badArtURL }
            guard (200..<300).contains(http.statusCode) else {
                throw LibraryError.http(http.statusCode)
            }
            // Bound the body WHILE it streams: the ceiling is decorative if every byte is in
            // memory already when it's checked. A declared length past it is refused unread.
            let ceiling = MgmtTransport.maxResponseBytes
            guard http.expectedContentLength <= Int64(ceiling) else {
                throw MgmtTransportError.tooLarge
            }
            var data = Data()
            var block: [UInt8] = []
            block.reserveCapacity(65_536)
            for try await byte in bytes {
                block.append(byte)
                guard block.count == 65_536 else { continue }
                data.append(contentsOf: block)
                block.removeAll(keepingCapacity: true)
                if data.count > ceiling { throw MgmtTransportError.tooLarge }
            }
            data.append(contentsOf: block)
            if data.count > ceiling { throw MgmtTransportError.tooLarge }
            return data
        }
        let response = try await LibraryClient.send(Self.requestPath(url), to: target)
        guard response.status == 200 else { throw LibraryError.http(response.status) }
        return response.body
    }

    /// Does this URL point at the host's own art proxy? A host-relative path does, wherever the
    /// host is now. An absolute URL is compared on host + port rather than a string prefix, so a
    /// differently-spelled but equivalent URL still takes the pinned path.
    static func isHostOrigin(_ url: URL, address: String, port: UInt16) -> Bool {
        guard let host = url.host else {
            return url.scheme == nil && url.relativeString.hasPrefix("/")
        }
        let bare = MgmtTransport.unbracketed(address)
        let scheme = url.scheme?.lowercased()
        return host.caseInsensitiveCompare(bare) == .orderedSame
            && (url.port ?? (scheme == "http" ? 80 : 443)) == Int(port)
    }
}
