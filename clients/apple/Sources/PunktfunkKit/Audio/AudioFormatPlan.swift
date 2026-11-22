// The output format decisions, as pure functions of the negotiated format and what the device
// reports. `SessionAudio` gathers the numbers, asks here, and builds the graph or the log line.

enum AudioFormatPlan {
    /// How the main mixer meets the output node.
    enum MixerOutput: Equatable {
        /// Leave the connection the engine makes on first use of `mainMixerNode`.
        case engineDefault
        /// The output's own width, with its layout, at `rate`.
        case width(channels: Int, rate: Double)
        /// Stereo at `rate`.
        case stereo(rate: Double)
    }

    /// The engine's default connection is stereo whatever the route carries, so a 5.1 or 7.1
    /// route folds inside the engine. A `captureRate` above 0 (voice processing) sets the render
    /// rate: the processor fails to initialize (-10875) unless render and capture share one, and
    /// its render side otherwise opens at 44.1 kHz. `hasLayout` is whether a layout for the
    /// output's width exists; without one the width cannot be declared.
    static func mixerOutput(
        outputChannels: Int, outputRate: Double, hasLayout: Bool, captureRate: Double
    ) -> MixerOutput {
        let rate = captureRate > 0 ? captureRate : outputRate
        if outputChannels > 2, rate > 0, hasLayout {
            return .width(channels: outputChannels, rate: rate)
        }
        return captureRate > 0 ? .stereo(rate: captureRate) : .engineDefault
    }

    /// The channel count to ask the AVAudioSession for again once it is active, or nil. Apple
    /// validates the first ask against the active route's maximum, so it can come back as 2:
    /// an 8-channel wire on a 5.1 AVR asks for 6.
    static func channelReask(wire: Int, maximum: Int, current: Int) -> Int? {
        let reachable = min(wire, maximum)
        return reachable > 2 && current < reachable ? reachable : nil
    }

    /// What the log says about the output an engine opened, against the negotiated format.
    enum Note: Equatable {
        /// The mixer runs narrower than the session on an output wide enough for it.
        case mixerFolds(mixer: Int)
        /// The device runs the negotiated rate and width.
        case negotiated
        /// The engine resamples to the device's rate.
        case resampling(device: Int)
        /// The route is narrower than the session.
        case folded(output: Int)
        /// The route is wider than the session; the tail channels are silent.
        case silentTail(output: Int)
    }

    /// Empty when the device reads 0 Hz: the node has no device yet, so there is nothing to say.
    static func notes(
        deviceRate: Int, outputChannels: Int, mixerChannels: Int, wireRate: Int, wireChannels: Int
    ) -> [Note] {
        guard deviceRate > 0 else { return [] }
        var notes: [Note] = []
        if mixerChannels < wireChannels, outputChannels >= wireChannels {
            notes.append(.mixerFolds(mixer: mixerChannels))
        }
        if deviceRate == wireRate, outputChannels == wireChannels {
            return notes + [.negotiated]
        }
        if deviceRate != wireRate { notes.append(.resampling(device: deviceRate)) }
        if outputChannels < wireChannels {
            notes.append(.folded(output: outputChannels))
        } else if outputChannels > wireChannels {
            notes.append(.silentTail(output: outputChannels))
        }
        return notes
    }
}
