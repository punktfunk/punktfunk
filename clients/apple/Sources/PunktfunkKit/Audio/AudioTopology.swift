// The engines a session runs, as one value, and the decisions that change it. The voice
// processor cancels echo only with both directions on one engine, so a session holds that
// engine or a separate pair, never both. `SessionAudio` keeps the value under its lock.

import AVFoundation

/// A capture engine and the uplink it feeds, published and stopped together.
struct MicCapture {
    let engine: AVAudioEngine
    let uplink: MicUplink
}

enum AudioTopology {
    /// Separate engines. Either may be absent: one that failed to start is not held.
    case separate(playback: AVAudioEngine?, capture: MicCapture?)
    /// One voice-processed engine carrying both directions.
    case combined(MicCapture)

    static let none = AudioTopology.separate(playback: nil, capture: nil)

    /// A started engine on its way into the topology.
    enum Part {
        case playback(AVAudioEngine)
        case capture(MicCapture)
        case combined(MicCapture)
    }

    /// A topology holding `part` alone.
    init(_ part: Part) {
        self = .none
        _ = publish(part)
    }

    /// The engine rendering to the speaker.
    var render: AVAudioEngine? {
        switch self {
        case .separate(let playback, _): return playback
        case .combined(let mic): return mic.engine
        }
    }

    /// The separate capture engine. nil when the mic rides the combined engine.
    var capture: AVAudioEngine? {
        if case .separate(_, let mic) = self { return mic?.engine }
        return nil
    }

    /// The voice-processed engine, when that is what runs.
    var combined: AVAudioEngine? {
        if case .combined(let mic) = self { return mic.engine }
        return nil
    }

    /// Every engine, capture first: teardown stops them in this order.
    var engines: [AVAudioEngine] {
        switch self {
        case .separate(let playback, let mic): return [mic?.engine, playback].compactMap { $0 }
        case .combined(let mic): return [mic.engine]
        }
    }

    /// Every uplink. Each stops after its engine (see `MicUplink.stop`).
    var uplinks: [MicUplink] {
        switch self {
        case .separate(_, let mic): return mic.map { [$0.uplink] } ?? []
        case .combined(let mic): return [mic.uplink]
        }
    }

    /// Hold `part` and return what it displaces, for the caller to stop. A part never lands
    /// beside the combined engine or beside another of its own kind.
    mutating func publish(_ part: Part) -> AudioTopology {
        let old = self
        switch (part, old) {
        case (.playback(let engine), .separate(let playback, let mic)):
            self = .separate(playback: engine, capture: mic)
            return .separate(playback: playback, capture: nil)
        case (.capture(let new), .separate(let playback, let mic)):
            self = .separate(playback: playback, capture: new)
            return .separate(playback: nil, capture: mic)
        case (.playback(let engine), .combined):
            self = .separate(playback: engine, capture: nil)
            return old
        case (.capture(let new), .combined):
            self = .separate(playback: nil, capture: new)
            return old
        case (.combined(let new), _):
            self = .combined(new)
            return old
        }
    }

    /// Take the separate playback engine out, ahead of the swap to the combined engine.
    mutating func takePlayback() -> AVAudioEngine? {
        guard case .separate(let playback, let mic) = self else { return nil }
        self = .separate(playback: nil, capture: mic)
        return playback
    }

    /// What a mic permission grant does.
    enum GrantStep: Equatable {
        /// The mic is already live: a rebuild during the prompt asks again, so a grant can arrive
        /// twice, and a rebuild after it may have started the mic already.
        case ignore
        case addCapture
        case swapToCombined
    }

    func onGrant(combined wantsCombined: Bool) -> GrantStep {
        if capture != nil || combined != nil { return .ignore }
        return wantsCombined ? .swapToCombined : .addCapture
    }
}

/// Which engines a session start builds.
enum AudioStart: Equatable {
    /// Playback alone: the mic is off.
    case playback
    /// Playback alone: the mic permission was refused.
    case denied
    /// Separate playback and capture engines.
    case split
    /// One voice-processed engine.
    case combined
    /// Playback now, and the mic once the permission prompt is answered. Playback never waits
    /// out the prompt.
    case askThen(combined: Bool)

    enum Permission { case authorized, notDetermined, denied }

    /// A mic that is off is never asked for: no prompt, no capture.
    static func choose(micEnabled: Bool, combined: Bool, permission: Permission) -> AudioStart {
        guard micEnabled else { return .playback }
        switch permission {
        case .authorized: return combined ? .combined : .split
        case .notDetermined: return .askThen(combined: combined)
        case .denied: return .denied
        }
    }
}
