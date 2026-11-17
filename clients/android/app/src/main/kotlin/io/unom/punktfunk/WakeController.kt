package io.unom.punktfunk

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.isActive
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Wake a sleeping host and WAIT for it to come back before proceeding — the Android mirror of the
 * Apple client's `HostWaker`, driving [HostServices.wake] behind a visible "Waking…" state.
 *
 * A magic packet is fire-and-forget, and a cold box can take a minute or more to POST, boot, and
 * start answering again — far longer than a connect attempt will sit. On success this runs
 * [onOnline] (the real connect for a Wake-&-Connect, or nothing for a wake-only); on timeout it
 * parks in a retry/cancel state. One wake at a time.
 *
 * [isOnline] is a blocking probe run off the main thread: mDNS presence is a cache a sleeping
 * host keeps warm for up to 75 minutes, so it cannot answer this.
 *
 * [scope] is the composition's coroutine scope (main-dispatched), so [waking] mutations and the
 * [onOnline] callback run on the main thread; the loop itself runs on IO.
 */
class WakeController(private val scope: CoroutineScope, private val services: HostServices) {
    /** null = idle; non-null drives the "Waking…" phase of [ConnectOverlay]. */
    data class Waking(
        val hostName: String,
        /** Whether coming online chains into a connect (Wake & Connect) vs. just stopping. */
        val connectsAfter: Boolean,
        val seconds: Int = 0,
        val timedOut: Boolean = false,
    )

    var waking by mutableStateOf<Waking?>(null)
        private set

    private var loop: Job? = null

    /** Captured so "Try Again" replays the exact same wait. */
    private var replay: (() -> Unit)? = null

    /**
     * Wake the host and wait for [isOnline] to go true, then run [onOnline]. [macs]/[lastIp] target
     * the magic packet. No-ops straight to [onOnline] when there's nothing to wake with; a host
     * that is up already passes the first probe and gets no packet.
     */
    fun start(
        hostName: String,
        connectsAfter: Boolean,
        macs: List<String>,
        lastIp: String,
        isOnline: () -> Boolean,
        onOnline: () -> Unit,
    ) {
        if (macs.isEmpty()) {
            cancel()
            onOnline()
            return
        }
        replay = { run(hostName, connectsAfter, macs, lastIp, isOnline, onOnline) }
        replay?.invoke()
    }

    /** Stop waiting and dismiss the overlay (B / Cancel). */
    fun cancel() {
        loop?.cancel()
        loop = null
        replay = null
        waking = null
    }

    /** Restart the wait after a timeout (A / Try Again). */
    fun retry() {
        replay?.invoke()
    }

    private fun run(
        hostName: String,
        connectsAfter: Boolean,
        macs: List<String>,
        lastIp: String,
        isOnline: () -> Boolean,
        onOnline: () -> Unit,
    ) {
        loop?.cancel()
        waking = Waking(hostName = hostName, connectsAfter = connectsAfter)
        val claim = services.wakes.next()
        loop = scope.launch {
            val job = coroutineContext.job
            val up = withContext(Dispatchers.IO) {
                services.wake(claim, macs, lastIp, isOnline, cancelled = { !isActive }) { seconds, _, _ ->
                    // Posted, and dropped once this wait is no longer the current one.
                    scope.launch { if (loop === job) waking = waking?.copy(seconds = seconds) }
                }
            }
            loop = null
            if (up) {
                waking = null
                onOnline()
            } else {
                waking = waking?.copy(timedOut = true)
            }
        }
    }
}
