package io.unom.punktfunk

import android.content.res.Resources
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import java.math.BigDecimal
import java.math.RoundingMode
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
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
     * link holds (`0.0` with no round). [recommendedKbps] is the core's headroom under the
     * ceiling, for the FEC overhead and the loss a real stream will meet. [findings] is what the
     * check found, by id.
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
    }
}

/** One round at a rate the link holds: the loss figure the test shows. */
data class CleanRound(val rateKbps: Int, val lossPct: Double, val jitterUs: Int)

/**
 * One finding of the network check, by id (the Rust `FindingId` as a byte): the words are
 * [findingText]'s.
 */
data class Finding(val id: Int, val severity: Int, val numbers: List<Int>)

/**
 * The network check's flat report ([NativeBridge.nativeNetworkCheck]) as a [SpeedTestPhase.Done];
 * null when it is short of its header, its findings or the trailing recommendation.
 */
fun parseNetworkCheck(v: DoubleArray): SpeedTestPhase.Done? {
    if (v.size < 16) return null
    val clean = if (v[2] != 0.0) CleanRound(v[3].toInt(), v[4], v[5].toInt()) else null
    val n = v[15].toInt()
    if (v.size < 16 + n * 5 + 1) return null
    val findings = (0 until n).map { i ->
        val base = 16 + i * 5
        Finding(
            id = v[base].toInt(),
            severity = v[base + 1].toInt(),
            numbers = listOf(v[base + 2].toInt(), v[base + 3].toInt(), v[base + 4].toInt()),
        )
    }
    return SpeedTestPhase.Done(
        throughputKbps = v[0].toInt(),
        lossPct = clean?.lossPct ?: 0.0,
        recommendedKbps = v[16 + n * 5].toInt(),
        wall = v[1] != 0.0,
        clean = clean,
        findings = findings,
    )
}

/**
 * A finding as a message and its figures — the same sentences every shell shows, pinned by
 * `clients/shared/finding-vectors.json`. Figures are written with a `.` in every language.
 */
fun findingMessage(id: Int, numbers: List<Int>): Say {
    val a = numbers.getOrElse(0) { 0 }
    val b = numbers.getOrElse(1) { 0 }
    val pct = { x: Int -> fixed(x / 100.0, 1) }
    return when (id) {
        1 -> if (a > 0 && b > 0) {
            say(R.string.finding_speed_mismatch_body, a, b)
        } else {
            say(R.string.finding_speed_mismatch_unknown_body)
        }
        2 -> say(R.string.finding_burst_intolerant_body, pct(a))
        3 -> if (a > 0) {
            Say.Plural(R.plurals.finding_receive_buffer_body, a, listOf(a, b))
        } else {
            say(R.string.finding_receive_buffer_cap_body, b)
        }
        4 -> say(R.string.finding_link_fault_body, pct(a))
        5 -> say(R.string.finding_queue_build_up_body, fixed(a / 1000.0, 0), fixed(b / 1000.0, 0))
        6 -> if (a > 0) {
            Say.Plural(R.plurals.finding_host_send_buffer_body, a, listOf(a))
        } else {
            say(R.string.finding_host_send_buffer_cap_body, b)
        }
        7 -> if (a > 0) say(R.string.finding_wifi_loss_body, pct(a)) else say(R.string.finding_wifi_body)
        else -> say(R.string.finding_unknown_body, id)
    }
}

/**
 * [x] to [places] decimals the way Rust's `{:.N}` and C's `%.Nf` print it: the exact binary value,
 * ties to even, `.` in every locale. `String.format` rounds the shortest decimal half-up instead,
 * so 0.15 would read 0.2 here and 0.1 in the console.
 */
internal fun fixed(x: Double, places: Int): String =
    BigDecimal(x).setScale(places, RoundingMode.HALF_EVEN).toPlainString()

/** [mbps] as a readout, `940 Mbit/s`: the unit stays as written in every language. */
internal fun mbitRate(mbps: Double): String = fixed(mbps, 0) + " Mbit/s"

/**
 * Connect to [host]:[port] through [services] as a diagnostic session, run the network check, and
 * report. Suspends on IO — call from a coroutine; [onPhase] is invoked as it progresses so the
 * dialog can narrate. The session closes even when the caller is cancelled mid-check.
 *
 * The connect is deliberately minimal: 1280×720@60, no launch, host-default bitrate, and probes
 * only — the host builds no display or encoder for it. The check itself is the core's
 * (`client::health::health_check`): the ceiling the bring-up ramp proved, a clean round at half of
 * it, two shaped legs, both ends' facts, and the findings.
 */
suspend fun runSpeedTest(
    services: HostServices,
    identity: ClientIdentity,
    host: String,
    port: Int,
    pinHex: String,
    // Words a refusal for [onPhase].
    res: Resources,
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
    val spec = DialSpec(
        probeSettings, identity, host, port, pinHex,
        launch = null, dialer = "speed-test", timeoutMs = SPEED_TEST_CONNECT_TIMEOUT_MS,
        deliveryFlags = DELIVERY_PROBE_ONLY,
    )
    val handle = when (val opened = services.open(spec)) {
        is HostServices.Open -> opened.handle
        is HostServices.Failed -> {
            onPhase(
                SpeedTestPhase.Failed(
                    res.getString(ConnectErrors.connectMessage(opened.token, requestAccess = false)),
                ),
            )
            return
        }
        // No ticket: nothing cancels it.
        HostServices.Cancelled -> return
    }
    try {
        onPhase(SpeedTestPhase.Measuring)
        val report = withContext(Dispatchers.IO) { NativeBridge.nativeNetworkCheck(handle) }
        val done = report?.let(::parseNetworkCheck)
        onPhase(done ?: SpeedTestPhase.Failed(res.getString(R.string.speedtest_no_result_body)))
    } finally {
        withContext(Dispatchers.IO + NonCancellable) { NativeBridge.nativeClose(handle) }
    }
}

/** `EXT_DELIVERY_PROBE_ONLY`: a diagnostic session that builds no pipeline. */
const val DELIVERY_PROBE_ONLY = 2

/** The layer a speed test wrote its bitrate to. */
sealed interface SpeedTestWrote {
    data object Default : SpeedTestWrote
    data class Preset(val name: String) : SpeedTestWrote
}

/** The confirmation after a write: [rate] is a readout (see [mbitRate]) and [wrote] says where it went. */
fun speedTestApplied(wrote: SpeedTestWrote, rate: String): Say = when (wrote) {
    SpeedTestWrote.Default -> say(R.string.speedtest_applied_default, rate)
    is SpeedTestWrote.Preset -> say(R.string.speedtest_applied_preset, rate, wrote.name)
}

/**
 * Write a measured bitrate into the layer [target] names. [toPreset] picks the side of a
 * [SpeedTestTarget.Ask]; it is ignored for the other targets, which have only one answer. Returns
 * where it went, for the confirmation.
 */
fun applySpeedTestResult(
    kbps: Int,
    target: SpeedTestTarget,
    toPreset: Boolean,
    presets: PresetStore,
    settings: Settings,
    onGlobalChange: (Settings) -> Unit,
): SpeedTestWrote {
    val preset = when (target) {
        is SpeedTestTarget.Preset -> target.preset
        is SpeedTestTarget.Ask -> target.preset.takeIf { toPreset }
        SpeedTestTarget.Global -> null
    }
    return if (preset == null) {
        onGlobalChange(settings.copy(bitrateKbps = kbps))
        SpeedTestWrote.Default
    } else {
        // Only the bitrate moves — a speed test has nothing to say about the rest of the preset.
        // Re-read rather than trusting the copy this dialog was opened with, so a rename or another
        // edit in between isn't clobbered.
        val live = presets.byId(preset.id) ?: preset
        presets.save(live.copy(overrides = live.overrides.copy(bitrateKbps = kbps)))
        SpeedTestWrote.Preset(live.name)
    }
}

private const val SPEED_TEST_CONNECT_TIMEOUT_MS = 15_000
