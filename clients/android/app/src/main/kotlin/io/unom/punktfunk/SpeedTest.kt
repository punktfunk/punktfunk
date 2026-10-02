package io.unom.punktfunk

import android.content.Context
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

/**
 * The network speed test: measure the path to a host **over the real data plane** — connect, ask
 * the host to burst filler for two seconds, report goodput and loss, and offer to apply a
 * recommended bitrate in one tap.
 *
 * The measurement is the easy half. The half that was wrong everywhere for a long time is *where
 * the answer goes*: a measured bitrate belongs in the layer the tested host actually resolves
 * bitrate from (design/client-settings-profiles.md §5.3). Writing it to the global — the
 * long-standing behaviour — meant measuring the slow retro box downstairs quietly re-tuned the
 * desktop too. [SpeedTestTarget] is that decision, and because it depends only on the host it is
 * known *before* the result lands, so the button can say where it will write.
 */
sealed interface SpeedTestTarget {
    /** No preset in play — the global default, i.e. what has always happened. */
    data object Global : SpeedTestTarget

    /** The preset this host uses already overrides bitrate, so that override is what it reads. */
    data class Preset(val preset: StreamPreset) : SpeedTestTarget

    /**
     * The host uses a preset, but that preset inherits bitrate. Writing either layer is
     * defensible, so the user gets both buttons rather than us guessing which they meant.
     */
    data class Ask(val preset: StreamPreset) : SpeedTestTarget

    companion object {
        /**
         * Resolved exactly the way a connect resolves it (see [PresetStore.resolveFor]): the
         * one-off pick this test was started from — a pinned card carries one — else the host's
         * binding. A dangling binding resolves as no preset here too.
         */
        fun resolve(
            host: KnownHost?,
            oneOffPreset: String?,
            presets: PresetStore,
        ): SpeedTestTarget {
            val preset = presets.resolveFor(host, oneOffPreset) ?: return Global
            return if (preset.overrides.bitrateKbps != null) Preset(preset) else Ask(preset)
        }
    }
}

/** Where the speed test is: it connects, it measures, then it has an answer or a reason. */
sealed interface SpeedTestPhase {
    data object Connecting : SpeedTestPhase
    data object Measuring : SpeedTestPhase
    data class Failed(val message: String) : SpeedTestPhase

    /**
     * [throughputKbps] is what the link carries; [lossPct] is the clean round's at a rate the
     * link holds (`0.0` with no round). [recommendedKbps] is 70 % of the ceiling — headroom for
     * the FEC overhead and for the loss a real stream will meet, the same margin the desktop
     * clients apply. [findings] is what the check found, by id.
     */
    data class Done(
        val throughputKbps: Int,
        val lossPct: Double,
        val recommendedKbps: Int,
        val wall: Boolean = false,
        val clean: CleanRound? = null,
        val findings: List<Finding> = emptyList(),
    ) : SpeedTestPhase {
        val measuredMbps: Double get() = throughputKbps / 1000.0
        val recommendedMbps: Double get() = recommendedKbps / 1000.0

        /** The delivery profile the first finding that names one offers, else null. */
        val offeredProfile: Int? get() = findings.firstOrNull { it.profile != 0 }?.profile
    }
}

/** One round at a rate the link holds: the loss figure the test shows. */
data class CleanRound(val rateKbps: Int, val lossPct: Double, val jitterUs: Int)

/**
 * One finding of the network check, by id (the Rust `FindingId` as a byte): the words are
 * [findingText]'s; [profile] is the delivery profile that helps (`1` capped, `2` smooth, `0`
 * none).
 */
data class Finding(val id: Int, val severity: Int, val numbers: List<Int>, val profile: Int)

/** The network check's flat report ([NativeBridge.nativeNetworkCheck]) as a [SpeedTestPhase.Done]. */
fun parseNetworkCheck(v: DoubleArray): SpeedTestPhase.Done? {
    if (v.size < 17) return null
    val ceilingKbps = v[0].toInt()
    val clean = if (v[2] != 0.0) CleanRound(v[3].toInt(), v[4], v[5].toInt()) else null
    val n = v[16].toInt()
    if (v.size < 17 + n * 6) return null
    val findings = (0 until n).map { i ->
        val base = 17 + i * 6
        Finding(
            id = v[base].toInt(),
            severity = v[base + 1].toInt(),
            profile = v[base + 2].toInt(),
            numbers = listOf(v[base + 3].toInt(), v[base + 4].toInt(), v[base + 5].toInt()),
        )
    }
    return SpeedTestPhase.Done(
        throughputKbps = ceilingKbps,
        lossPct = clean?.lossPct ?: 0.0,
        // Integer arithmetic in this order (not `* 0.7`) so the recommendation matches the
        // desktop clients' to the kilobit.
        recommendedKbps = ceilingKbps / 10 * 7,
        wall = v[1] != 0.0,
        clean = clean,
        findings = findings,
    )
}

/**
 * A finding in words — what did not happen, then the next move — the same sentences every
 * shell shows. The offered profile is the dialog's button, not a sentence here.
 */
fun findingText(id: Int, numbers: List<Int>): String {
    val a = numbers.getOrElse(0) { 0 }
    val b = numbers.getOrElse(1) { 0 }
    val pct = { x: Int -> x / 100.0 }
    return when (id) {
        1 -> if (a > 0 && b > 0) {
            "The host's port is faster than this device's ($a vs $b Mbit/s), so bursts overflow " +
                "the switch between them."
        } else {
            "The host's port is faster than this device's, so bursts overflow the switch between them."
        }
        2 -> "This device drops the start of every burst (%.1f %% lost) — the adapter's power saving " +
            "is the usual cause.".let { it.format(pct(a)) }
        3 -> if (a > 0) {
            "This device's own receive buffer dropped $a packets; the system caps it at $b KB."
        } else {
            "The system caps this device's receive buffer at $b KB."
        }
        4 -> "Loss at a rate no link refuses (%.1f %%): check the cable, the port or the adapter driver."
            .format(pct(a))
        5 -> "Something on the path buffers instead of dropping (%.0f ms spread); keep the bitrate under %.0f Mbit/s."
            .format(a / 1000.0, b / 1000.0)
        6 -> if (a > 0) {
            "The host's send buffer refused $a packets; raise its limit."
        } else {
            "The host's send buffer is capped at $b KB; raise its limit."
        }
        7 -> if (a > 0) "This device is on Wi-Fi; bursts lose %.1f %%.".format(pct(a)) else "This device is on Wi-Fi."
        else -> "Finding $id."
    }
}

/** What an offered profile is called on a button. */
fun profileName(profile: Int): String = when (profile) {
    1 -> "capped"
    2 -> "smooth"
    else -> "none"
}

/**
 * Connect to [host]:[port] as a diagnostic session, run the network check, and report. Suspends
 * on IO — call from a coroutine; [onPhase] is invoked as it progresses so the dialog can narrate.
 *
 * The connect is deliberately minimal: 1280×720@60, no launch, host-default bitrate, and probes
 * only — the host builds no display or encoder for it. The check itself is the core's
 * (`client::health::health_check`): the ceiling the bring-up ramp proved, a clean round at half of
 * it, two shaped legs, both ends' facts, and the findings.
 */
suspend fun runSpeedTest(
    context: Context,
    identity: ClientIdentity,
    host: String,
    port: Int,
    pinHex: String,
    // Kept for the console's graph; the check reports once, when it is done.
    @Suppress("UNUSED_PARAMETER") onProgress: (Int) -> Unit = {},
    onPhase: (SpeedTestPhase) -> Unit,
) {
    onPhase(SpeedTestPhase.Connecting)
    val probeSettings = Settings(
        width = 1280,
        height = 720,
        hz = 60,
        bitrateKbps = 0, // the host's default: this measures the link, not an encoder setting
        hdrEnabled = false,
        audioChannels = 2,
    )
    val handle = connectToHost(
        context, probeSettings, identity, host, port, pinHex,
        launch = null, dialer = "speed-test", timeoutMs = SPEED_TEST_CONNECT_TIMEOUT_MS,
        deliveryFlags = DELIVERY_FACTS or DELIVERY_PROBE_ONLY,
    )
    if (handle == 0L) {
        onPhase(
            SpeedTestPhase.Failed(
                ConnectErrors.connectMessage(NativeBridge.nativeTakeLastError(), requestAccess = false),
            ),
        )
        return
    }
    try {
        onPhase(SpeedTestPhase.Measuring)
        val report = withContext(Dispatchers.IO) { NativeBridge.nativeNetworkCheck(handle) }
        val done = report?.let(::parseNetworkCheck)
        onPhase(done ?: SpeedTestPhase.Failed("The host wouldn't run the measurement."))
    } finally {
        withContext(Dispatchers.IO) { NativeBridge.nativeClose(handle) }
    }
}

/** `EXT_DELIVERY_FACTS`: ask the host for its own network facts. */
const val DELIVERY_FACTS = 1

/** `EXT_DELIVERY_PROBE_ONLY`: a diagnostic session that builds no pipeline. */
const val DELIVERY_PROBE_ONLY = 2

/**
 * Write a measured bitrate into the layer [target] names. [toPreset] picks the side of a
 * [SpeedTestTarget.Ask]; it is ignored for the other targets, which have only one answer. Returns
 * a human phrase naming where it went, for the confirmation.
 */
fun applySpeedTestResult(
    kbps: Int,
    target: SpeedTestTarget,
    toPreset: Boolean,
    presets: PresetStore,
    settings: Settings,
    onGlobalChange: (Settings) -> Unit,
): String {
    val preset = when (target) {
        is SpeedTestTarget.Preset -> target.preset
        is SpeedTestTarget.Ask -> target.preset.takeIf { toPreset }
        SpeedTestTarget.Global -> null
    }
    return if (preset == null) {
        onGlobalChange(settings.copy(bitrateKbps = kbps))
        "the default bitrate"
    } else {
        // Only the bitrate moves — a speed test has nothing to say about the rest of the preset.
        // Re-read rather than trusting the copy this dialog was opened with, so a rename or another
        // edit in between isn't clobbered.
        val live = presets.byId(preset.id) ?: preset
        presets.save(live.copy(overrides = live.overrides.copy(bitrateKbps = kbps)))
        "“${live.name}”"
    }
}

private const val SPEED_TEST_CONNECT_TIMEOUT_MS = 15_000
