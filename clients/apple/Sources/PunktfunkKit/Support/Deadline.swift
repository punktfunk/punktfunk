// A deadline that holds over work which ignores cancellation.

import Foundation

public enum Deadline {
    /// `work`'s answer, or `fallback` once `seconds` have passed. A task group waits for every
    /// child before it returns, so it cannot cap work that ignores its cancel; this returns at
    /// the deadline and drops the late answer.
    public static func first<T: Sendable>(
        within seconds: TimeInterval, fallback: T, _ work: @escaping @Sendable () async -> T
    ) async -> T {
        await withCheckedContinuation { continuation in
            let once = Once(continuation)
            Task { once.resume(await work()) }
            Task {
                try? await Task.sleep(nanoseconds: UInt64(max(0, seconds) * 1_000_000_000))
                once.resume(fallback)
            }
        }
    }

    /// Resumes its continuation with the first value it is handed.
    private final class Once<T: Sendable>: @unchecked Sendable {
        private let lock = NSLock()
        private var continuation: CheckedContinuation<T, Never>?

        init(_ continuation: CheckedContinuation<T, Never>) { self.continuation = continuation }

        func resume(_ value: T) {
            lock.lock()
            let taken = continuation
            continuation = nil
            lock.unlock()
            taken?.resume(returning: value)
        }
    }
}
