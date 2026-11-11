// A per-key answer cache with a time-to-live, for the stores that ask each paired host the same
// question on the presence beat: its power actions, its screens, what it plays. The store asks,
// fetches and lands the outcome; the cache decides when to ask again. Time is an argument, so
// `swift test` runs every row (`TTLCacheTests`).

import Foundation

public struct TTLCache<Value> {
    /// One fetch's claim on a key, taken by `ask`.
    public struct Ticket: Equatable, Sendable {
        public let key: String
    }

    public let ttl: TimeInterval
    private var values: [String: Value] = [:]
    private var askedAt: [String: Date] = [:]

    public init(ttl: TimeInterval) {
        self.ttl = ttl
    }

    /// The last answer for `key`; nil until one lands.
    public subscript(key: String) -> Value? { values[key] }

    /// A ticket to fetch `key` with, or nil while the last ask is younger than the TTL. Stamped
    /// before the fetch, so a host that hangs is not asked again every beat.
    public mutating func ask(_ key: String, now: Date) -> Ticket? {
        if let at = askedAt[key], now.timeIntervalSince(at) < ttl { return nil }
        askedAt[key] = now
        return Ticket(key: key)
    }

    /// A fetch's outcome: an answer replaces the value, nil keeps it.
    public mutating func land(_ ticket: Ticket, _ answer: Value?) {
        if let answer { values[ticket.key] = answer }
    }

    /// An answer the caller already holds, fresh as of `now`.
    public mutating func store(_ value: Value, for key: String, now: Date) {
        values[key] = value
        askedAt[key] = now
    }

    /// The next ask fetches. `keepValue` leaves the last answer until a new one lands.
    public mutating func invalidate(_ key: String, keepValue: Bool = false) {
        askedAt[key] = nil
        if !keepValue { values[key] = nil }
    }
}
