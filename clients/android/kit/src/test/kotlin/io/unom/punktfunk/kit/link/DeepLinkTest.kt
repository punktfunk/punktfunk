package io.unom.punktfunk.kit.link

import io.unom.punktfunk.kit.security.KnownHost
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * **The cross-language contract.** `clients/shared/deeplink-vectors.json` is consumed verbatim by
 * the Rust, Swift and Kotlin suites, so the three parsers cannot drift into three different
 * security postures — a URL that Rust refuses as a control-character smuggle must not quietly
 * parse here. Any new case belongs in that file, not in this one.
 */
class DeepLinkVectorTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs a unit test with the module directory as its working directory, so the shared
        // file is two levels up (clients/android/kit → clients/shared). Resolved rather than
        // copied: a copy would be a fourth contract, free to go stale.
        val file = File("../../shared/deeplink-vectors.json")
        assertTrue(
            "the shared vector file must be reachable at ${file.absolutePath}",
            file.isFile,
        )
        JSONObject(file.readText())
    }

    @Test
    fun everySharedVectorAgrees() {
        val cases = vectors.getJSONArray("cases")
        assertTrue("the vector file is the contract; keep it rich", cases.length() > 20)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val name = case.getString("name")
            val result = DeepLinks.parse(case.getString("url"))
            if (case.has("error")) {
                val refusal = result as? DeepLinkResult.Refused
                    ?: throw AssertionError("$name: expected ${case.getString("error")}, parsed ok")
                assertEquals(name, case.getString("error"), refusal.error.code)
                assertTrue("$name: a refusal must be explainable", refusal.message().isNotEmpty())
                continue
            }
            val link = (result as? DeepLinkResult.Parsed)?.link
                ?: throw AssertionError("$name: refused, expected a parse — $result")
            val want = case.getJSONObject("expect")
            assertEquals(name, want.getString("route"), link.route.word)
            assertEquals(name, want.getString("host_ref"), link.hostRef)
            assertEquals("$name fp", want.optStringOrNull("fp"), link.fp)
            assertEquals("$name launch", want.optStringOrNull("launch"), link.launch)
            assertEquals("$name preset", want.optStringOrNull("preset"), link.preset)
            assertEquals("$name name", want.optStringOrNull("name"), link.name)
            assertEquals("$name as", want.optStringOrNull("as"), link.asProfile)
            assertEquals("$name host_addr", want.optStringOrNull("host_addr"), link.host?.first)
            assertEquals(
                "$name host_port",
                if (want.has("host_port")) want.getInt("host_port") else null,
                link.host?.second,
            )
            if (case.has("emit")) assertEquals("$name emit", case.getString("emit"), link.toUrl())
        }
    }

    private fun JSONObject.optStringOrNull(key: String): String? =
        if (has(key)) getString(key) else null
}

/**
 * Resolution and emission — the half the vector file can't cover, because it depends on what is in
 * THIS device's host store. The rules are the one-click contract in resolution form: an id beats a
 * name beats an address, an ambiguous name refuses rather than guesses, and a link whose record is
 * gone still lands on the confirmation sheet via `host=`+`fp=` instead of dying. Only the id — the
 * one reference nothing can guess — dials on its own; a name or an address resolves to the same
 * host behind a confirmation.
 */
class DeepLinkResolutionTest {
    private val fp = "a".repeat(64)
    private val desk = host("Desk", "192.168.1.50", "11111111-2222-4333-8444-555555555555", fp)
    private val hosts = listOf(
        desk,
        host("Couch", "192.168.1.60", "66666666-7777-4888-8999-aaaaaaaaaaaa", ""),
        host("Couch", "192.168.1.61", "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff", ""),
    )

    private fun resolve(url: String) =
        DeepLinks.resolveHost((DeepLinks.parse(url) as DeepLinkResult.Parsed).link, hosts)

    @Test
    fun idBeatsNameBeatsAddress() {
        assertEquals(desk, (resolve("punktfunk://connect/${desk.id}") as HostResolution.Record).host)
        assertEquals(desk, (resolve("punktfunk://connect/desk") as HostResolution.Record).host)
        assertEquals(desk, (resolve("punktfunk://connect/192.168.1.50") as HostResolution.Record).host)
        assertEquals(
            desk,
            (resolve("punktfunk://connect/192.168.1.50:9777") as HostResolution.Record).host,
        )
        // Two hosts answer to "Couch" — refuse with a notice, never pick one.
        assertEquals(HostResolution.Ambiguous, resolve("punktfunk://connect/couch"))
    }

    /**
     * The record id is a UUID nothing can guess; a display name ("Gaming PC") and a LAN address are
     * guesses any web page can make. So the id — and only the id — is the silent one-click dial;
     * everything else that finds a saved host stops at [HostResolution.Confirm].
     */
    @Test
    fun onlyTheRecordIdDialsWithoutAsking() {
        assertEquals(HostResolution.Known(desk), resolve("punktfunk://connect/${desk.id}"))
        assertEquals(HostResolution.Confirm(desk), resolve("punktfunk://connect/desk"))
        assertEquals(HostResolution.Confirm(desk), resolve("punktfunk://connect/DESK"))
        assertEquals(HostResolution.Confirm(desk), resolve("punktfunk://connect/192.168.1.50"))
        assertEquals(HostResolution.Confirm(desk), resolve("punktfunk://connect/192.168.1.50:9777"))
        // …including the `host=` recovery path, exactly as its own doc always claimed.
        assertEquals(
            HostResolution.Confirm(desk),
            resolve("punktfunk://connect/00000000-0000-4000-8000-000000000000?host=192.168.1.50"),
        )
        // A launch id doesn't buy a name any authority it didn't have.
        assertEquals(
            HostResolution.Confirm(desk),
            resolve("punktfunk://connect/desk?launch=steam:570"),
        )
    }

    @Test
    fun aStaleIdRecoversThroughTheHostParameter() {
        val stale = "00000000-0000-4000-8000-000000000000"
        assertEquals(
            desk,
            (resolve("punktfunk://connect/$stale?host=192.168.1.50") as HostResolution.Record).host,
        )
        // …but a stale id is NOT a hostname: dialing "00000000-…" would be a confusing dead end
        // rather than the recovery the grammar specifies.
        assertEquals(HostResolution.Unresolvable, resolve("punktfunk://connect/$stale"))
        // Neither is a display name that can't be an address.
        assertEquals(HostResolution.Unresolvable, resolve("punktfunk://connect/Basement%20PC"))
    }

    /** Both OS installs of a dual-boot box at one address: the link's `fp` picks its own record. */
    @Test
    fun aLinksPinPicksItsOwnOsAtASharedAddress() {
        val linuxFp = "b".repeat(64)
        val third = "c".repeat(64)
        val linux = host("Desk (Linux)", "192.168.1.50", "66666666-7777-4888-8999-bbbbbbbbbbbb", linuxFp)
        val both = listOf(desk, linux)
        fun r(url: String) = DeepLinks.resolveHost((DeepLinks.parse(url) as DeepLinkResult.Parsed).link, both)
        assertEquals(HostResolution.Confirm(desk), r("punktfunk://connect/192.168.1.50?fp=$fp"))
        assertEquals(HostResolution.Confirm(linux), r("punktfunk://connect/192.168.1.50?fp=$linuxFp"))
        // A pin nobody holds is a new host — the sheet, never the neighbour.
        assertEquals(
            HostResolution.Unknown("192.168.1.50", DeepLinks.DEFAULT_PORT, null, third),
            r("punktfunk://connect/192.168.1.50?fp=$third"),
        )
    }

    @Test
    fun anUnknownHostBecomesTheConfirmationSheetsInput() {
        val r = resolve("punktfunk://connect/10.0.0.9:7000?name=Studio&fp=$fp")
        assertEquals(HostResolution.Unknown("10.0.0.9", 7000, "Studio", fp), r)
        // An mDNS/DNS name we've never saved is offered the same way — the sheet, never a connect.
        assertEquals(
            HostResolution.Unknown("nas.local", DeepLinks.DEFAULT_PORT, null, null),
            resolve("punktfunk://connect/nas.local"),
        )
    }

    @Test
    fun aPinThatContradictsTheStoredOneIsTheLinkLying() {
        fun link(url: String) = (DeepLinks.parse(url) as DeepLinkResult.Parsed).link
        assertTrue(link("punktfunk://connect/desk?fp=${"b".repeat(64)}").pinConflict(desk))
        assertFalse(link("punktfunk://connect/desk?fp=$fp").pinConflict(desk))
        // No pin stored (an address-only record) → nothing to contradict; the trust flow runs.
        assertFalse(link("punktfunk://connect/desk?fp=${"b".repeat(64)}").pinConflict(hosts[1]))
    }

    @Test
    fun selfEmittedLinksRoundTripAndSurviveAWipedStore() {
        val h = desk.copy(port = 7777)
        val link = DeepLinks.forHost(h, launch = "steam:570", preset = "aaaaaaaaaaaa")
        val url = link.toUrl()
        assertEquals(
            "punktfunk://connect/${h.id}?fp=$fp&host=192.168.1.50:7777" +
                "&launch=steam:570&preset=aaaaaaaaaaaa&profile=aaaaaaaaaaaa",
            url,
        )
        assertEquals(link, (DeepLinks.parse(url) as DeepLinkResult.Parsed).link)

        // Names with spaces and non-ASCII survive the round trip.
        val labelled = DeepLink(hostRef = "Wohnzimmer PC", name = "Büro · Mac")
        assertTrue(labelled.toUrl().startsWith("punktfunk://connect/Wohnzimmer%20PC?"))
        assertEquals(labelled, (DeepLinks.parse(labelled.toUrl()) as DeepLinkResult.Parsed).link)

        // An emitted IPv6 host parameter comes back bracketed, so it parses again.
        val v6 = DeepLink(hostRef = "x", host = "::1" to 1234)
        assertEquals(v6.host, (DeepLinks.parse(v6.toUrl()) as DeepLinkResult.Parsed).link.host)
    }

    @Test
    fun aHostWithNoPinEmitsNoFingerprint() {
        assertNull(DeepLinks.forHost(hosts[1]).fp)
    }

    private fun host(name: String, addr: String, id: String, fp: String) =
        KnownHost(addr, DeepLinks.DEFAULT_PORT, name, fp, paired = true, id = id)
}
