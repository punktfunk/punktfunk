package io.unom.punktfunk.kit.discovery

import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The reachability sweep behind the online pips. A host is probed at its saved address, then the
 * live advert's and the addresses it left, so a host that moved from Wi-Fi to Ethernet still
 * reads as live.
 */
class PresenceTest {
    private val fp = "ab".repeat(32)
    private val desk = KnownHost("192.168.1.9", 9777, "Desk", fp, paired = true, id = "desk")
    private fun advert(host: String) =
        DiscoveredHost(key = "id", name = "Desk", host = host, port = 9777, fingerprint = fp)

    @Test
    fun the_saved_address_is_asked_first_and_the_live_one_still_asked() {
        assertEquals(
            listOf(HostAddr("192.168.1.9", 9777), HostAddr("192.168.1.20", 9777)),
            Presence.candidates(desk, advert("192.168.1.20")),
        )
        assertEquals(listOf(HostAddr("192.168.1.9", 9777)), Presence.candidates(desk, advert("192.168.1.9")))
        assertEquals(listOf(HostAddr("192.168.1.9", 9777)), Presence.candidates(desk, null))
    }

    /** A stale live address no longer routes; the saved one answers. The host is up, there. */
    @Test
    fun a_host_that_answers_at_its_saved_address_is_up_when_the_advert_is_stale() {
        val up = Presence.sweep(listOf(desk), liveFor = { advert("10.0.0.5") }) { addr, _ -> fp.takeIf { addr == "192.168.1.9" } }
        assertEquals(HostAddr("192.168.1.9", 9777), up["desk"])
    }

    /** A Tailscale address answers from the LAN too, so the LAN advert must not replace it. */
    @Test
    fun a_saved_address_that_answers_beats_the_advert() {
        val routed = desk.copy(address = "100.64.0.7")
        val up = Presence.sweep(listOf(routed), liveFor = { advert("192.168.1.9") }) { _, _ -> fp }
        assertEquals(HostAddr("100.64.0.7", 9777), up["desk"])
    }

    /** A cold boot on a new lease: the saved address is dead, the advert answers there. */
    @Test
    fun a_host_on_a_new_lease_is_reported_at_the_address_that_answered() {
        val up = Presence.sweep(listOf(desk), liveFor = { advert("192.168.1.20") }) { addr, _ -> fp.takeIf { addr == "192.168.1.20" } }
        assertEquals(HostAddr("192.168.1.20", 9777), up["desk"])
        assertNull(Presence.sweep(listOf(desk), liveFor = { null }) { _, _ -> null }["desk"])
    }

    /** VPN off at home, then mobile data: the LAN is gone, and an address it left answers. */
    @Test
    fun a_host_is_found_again_at_an_address_it_left() {
        val moved = desk.copy(prevAddresses = listOf("100.64.0.7"))
        assertEquals(
            listOf(HostAddr("192.168.1.9", 9777), HostAddr("192.168.1.20", 9777), HostAddr("100.64.0.7", 9777)),
            Presence.candidates(moved, advert("192.168.1.20")),
        )
        val up = Presence.sweep(listOf(moved), liveFor = { null }) { addr, _ -> fp.takeIf { addr == "100.64.0.7" } }
        assertEquals(HostAddr("100.64.0.7", 9777), up["desk"])
        // Both answer while the saved one is silent: the advert comes first.
        val home = Presence.sweep(listOf(moved), liveFor = { advert("192.168.1.20") }) { addr, _ ->
            fp.takeIf { addr != "192.168.1.9" }
        }
        assertEquals(HostAddr("192.168.1.20", 9777), home["desk"])
    }

    /** An unpinned record is named by its address; an answer anywhere else is a stranger's. */
    @Test
    fun a_host_saved_by_address_is_asked_only_there() {
        val typed = KnownHost("192.168.1.9", 9777, "Desk", "", paired = false, id = "typed", prevAddresses = listOf("10.0.0.5"))
        assertEquals(listOf(HostAddr("192.168.1.9", 9777)), Presence.candidates(typed, advert("192.168.1.20")))
    }

    /** A kept address is the only one asked: no advert, no address the host left. */
    @Test
    fun a_kept_address_is_asked_alone() {
        val kept = desk.copy(keepAddress = true, prevAddresses = listOf("100.64.0.7"))
        assertEquals(listOf(HostAddr("192.168.1.9", 9777)), Presence.candidates(kept, advert("192.168.1.20")))
        val asked = mutableListOf<String>()
        val up = Presence.sweep(listOf(kept), liveFor = { advert("192.168.1.20") }) { addr, _ ->
            asked += addr
            fp.takeIf { addr != "192.168.1.9" }
        }
        assertNull(up["desk"])
        assertEquals(listOf("192.168.1.9"), asked)
        asked.clear()
        assertFalse(Presence.probeSelf(kept, advert("192.168.1.20")) { addr, _ -> asked += addr; null })
        assertEquals(listOf("192.168.1.9"), asked)
    }

    /**
     * A stranger holding the saved address completes the handshake, so the sweep must ask WHO
     * answered: counting it lights the pip and, since wake reads `!online`, keeps the wake
     * packet from the host that is actually asleep.
     */
    @Test
    fun a_sweep_ignores_an_address_a_stranger_answers() {
        val stranger = "cd".repeat(32)
        assertNull(Presence.sweep(listOf(desk), liveFor = { null }) { _, _ -> stranger }["desk"])
    }

    @Test
    fun one_missed_probe_keeps_a_host_online_and_two_take_it_down() {
        val t = PresenceTracker()
        assertEquals(setOf("desk"), t.apply(setOf("desk"), setOf("desk")))
        assertEquals(setOf("desk"), t.apply(setOf("desk"), emptySet()))
        assertEquals(emptySet<String>(), t.apply(setOf("desk"), emptySet()))
        // Back on the first answer; a host never seen up gets no grace.
        assertEquals(setOf("desk"), t.apply(setOf("desk", "sofa"), setOf("desk")))
        assertEquals(emptySet<String>(), t.apply(setOf("sofa"), emptySet()))
    }

    /**
     * An address is not an identity: a stranger who inherits a sleeping host's lease completes
     * the same handshake. Counting that as the host lights the pip and, since wake is gated on
     * `!online`, silences Wake-on-LAN for the machine that needs it.
     */
    @Test
    fun a_probe_answered_by_someone_else_is_not_this_host() {
        val ours = "ab".repeat(32)
        val theirs = "cd".repeat(32)
        val pinned = KnownHost("192.168.1.9", 9777, "Desk", ours, true)
        assertFalse(Presence.isSelf(pinned, null))
        assertFalse(Presence.isSelf(pinned, theirs))
        assertTrue(Presence.isSelf(pinned, ours))
        assertTrue(Presence.isSelf(pinned, ours.uppercase()))
        // Saved by address, never paired: no pin to compare, so any answer is the one it names.
        val unpinned = KnownHost("192.168.1.9", 9777, "192.168.1.9", "", false)
        assertTrue(Presence.isSelf(unpinned, theirs))
        assertFalse(Presence.isSelf(unpinned, null))
    }

    /** A warm advert is not a woken host: only a probe answer is, at the advert's new lease. */
    @Test
    fun a_wake_probe_follows_the_advert_and_never_trusts_it_alone() {
        val asked = mutableListOf<String>()
        assertFalse(Presence.probeSelf(desk, advert("192.168.1.20")) { addr, _ -> asked += addr; null })
        assertEquals(listOf("192.168.1.20"), asked)
        assertTrue(Presence.probeSelf(desk, advert("192.168.1.20")) { addr, _ -> fp.takeIf { addr == "192.168.1.20" } })
        assertTrue(Presence.probeSelf(desk, null) { addr, _ -> fp.takeIf { addr == "192.168.1.9" } })
    }
}
