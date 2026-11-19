package io.unom.punktfunk.kit.discovery

import io.unom.punktfunk.kit.security.KnownHost
import java.util.concurrent.Callable
import java.util.concurrent.Executors

/** Where a saved host answered its last probe. */
data class HostAddr(val address: String, val port: Int)

/**
 * The reachability sweep behind every "online" pip on Android — the touch home and the console
 * share it, so both answer the same way. Presence is the QUIC probe alone: an mDNS advert is a
 * cache entry a suspending host sends no goodbye for.
 *
 * A host lives at more than one address — its LAN lease at home, a Tailscale address anywhere.
 * Each is asked at its saved address first; only when that is silent, at the live advert's and
 * the addresses it left ([KnownHost.prevAddresses]). The sweep reports WHICH answered, so the
 * caller re-points the record there: a Tailscale address survives coming home, the LAN one takes
 * over when the VPN is off, and Tailscale takes back over on mobile data. A record that keeps
 * its address ([KnownHost.keepAddress]) is asked there alone.
 */
object Presence {
    /** The probe's budget per address. A LAN host answers in milliseconds. */
    const val PROBE_MS = 3_000

    private val pool = Executors.newCachedThreadPool { r ->
        Thread(r, "pf-presence").apply { isDaemon = true }
    }

    /**
     * The addresses to ask, in order: the saved one, the live advert's, then the ones it left.
     * A host saved by address alone is named by it, and one that keeps its address is pinned to
     * it, so each is asked there only.
     */
    fun candidates(saved: KnownHost, live: DiscoveredHost?): List<HostAddr> {
        val stored = HostAddr(saved.address, saved.port)
        if (saved.fpHex.isEmpty() || saved.keepAddress) return listOf(stored)
        val advertised = listOfNotNull(live?.let { HostAddr(it.host, it.port) })
        return (listOf(stored) + advertised + saved.prevAddresses.map { HostAddr(it, saved.port) }).distinct()
    }

    /**
     * Is [answered] — the fingerprint that replied to a probe, or `null` when nothing did —
     * [saved] itself?
     *
     * An address is not an identity. Whoever inherits a sleeping host's DHCP lease answers at
     * it, and counting that as the host lights the pip and, since wake is gated on `!online`,
     * silences Wake-on-LAN for exactly the machine that needs it; both OS installs of a
     * dual-boot box share one lease the same way. A record saved by address alone carries no
     * pin and has nothing to compare, so any answer is the host it names.
     */
    fun isSelf(pinHex: String, answered: String?): Boolean =
        answered != null && (pinHex.isEmpty() || pinHex.equals(answered, ignoreCase = true))

    /** [isSelf] for a saved record, which carries its own pin. */
    fun isSelf(saved: KnownHost, answered: String?): Boolean = isSelf(saved.fpHex, answered)

    /**
     * Is [saved] up? Asked at [live]'s address when there is an advert (a cold boot can move the
     * lease), else at the saved one; a record that keeps its address ignores the advert. The
     * advert alone never counts. Blocking.
     */
    fun probeSelf(saved: KnownHost, live: DiscoveredHost?, probe: (String, Int) -> String?): Boolean {
        val at = live.takeUnless { saved.keepAddress }
        return isSelf(saved, probe(at?.host ?: saved.address, at?.port ?: saved.port))
    }

    /**
     * Probe every host in [saved] and return the address each one answered AT AND AS ITSELF,
     * keyed by record id. Hosts run in parallel, and a host's fallbacks are asked together once
     * its saved address is silent, so a sweep costs at most two probe budgets, not one per
     * address. Blocking — call off the main thread.
     */
    fun sweep(
        saved: List<KnownHost>,
        liveFor: (KnownHost) -> DiscoveredHost?,
        probe: (String, Int) -> String?,
    ): Map<String, HostAddr> {
        if (saved.isEmpty()) return emptyMap()
        val tasks = saved.map { kh ->
            Callable {
                val all = candidates(kh, liveFor(kh))
                val answers = { a: HostAddr -> isSelf(kh, probe(a.address, a.port)) }
                val at = all.first().takeIf(answers)
                    ?: pool.invokeAll(all.drop(1).map { a -> Callable { a.takeIf(answers) } })
                        .firstNotNullOfOrNull { runCatching { it.get() }.getOrNull() }
                at?.let { kh.id to it }
            }
        }
        return pool.invokeAll(tasks).mapNotNull { runCatching { it.get() }.getOrNull() }.toMap()
    }
}

/**
 * The online set with one sweep of grace: a host joins on its first answer and leaves after two
 * consecutive misses. One missed probe is not proof of anything — the host may still be tearing
 * down the session this device just left — and a pip that flickers grey between two green sweeps
 * hides the Wake row and reads as a broken network.
 */
class PresenceTracker {
    private val misses = mutableMapOf<String, Int>()

    /** Ids in the online set after this sweep. [probed] is every id asked; [answered] who replied. */
    var online: Set<String> = emptySet()
        private set

    fun apply(probed: Set<String>, answered: Set<String>): Set<String> {
        val next = mutableSetOf<String>()
        for (id in probed) {
            if (id in answered) {
                misses.remove(id)
                next += id
                continue
            }
            val n = (misses[id] ?: 0) + 1
            misses[id] = n
            if (id in online && n < 2) next += id
        }
        misses.keys.retainAll(probed)
        online = next
        return next
    }
}
