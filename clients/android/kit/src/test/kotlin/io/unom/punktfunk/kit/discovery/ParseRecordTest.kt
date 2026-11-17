package io.unom.punktfunk.kit.discovery

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the native-record parser (`key␟name␟addr␟port␟fp␟pair␟mac␟os␟mgmt`), the Kotlin
 * half of the discovery JNI seam. No Android types. Run: `./gradlew :kit:testDebugUnitTest`.
 */
class ParseRecordTest {
    private val s = '\u001F' // field separator (must match the Rust side, discovery.rs FIELD_SEP)

    private fun rec(vararg f: String) = f.joinToString(s.toString())

    @Test
    fun parsesFullRecord() {
        val fp = "a".repeat(64)
        val h = parseHostRecord(rec("host-123", "home-worker-2", "192.168.1.70", "9777", fp, "required"))!!
        assertEquals("host-123", h.key)
        assertEquals("home-worker-2", h.name)
        assertEquals("192.168.1.70", h.host)
        assertEquals(9777, h.port)
        assertEquals(fp, h.fingerprint)
        assertTrue(h.pairingRequired)
    }

    @Test
    fun optionalPairingAndEmptyFingerprint() {
        val h = parseHostRecord(rec("id", "name", "10.0.0.5", "9777", "", "optional"))!!
        assertNull(h.fingerprint)
        assertEquals(false, h.pairingRequired)
    }

    @Test
    fun sevenFieldRecordHasNoOs() {
        // A record without the 8th field: `os` defaults empty, everything else parses.
        val h = parseHostRecord(rec("k", "n", "10.0.0.5", "9777", "", "optional", "aa:bb:cc:dd:ee:ff"))!!
        assertEquals(listOf("aa:bb:cc:dd:ee:ff"), h.mac)
        assertEquals("", h.os)
    }

    @Test
    fun eighthFieldCarriesTheOsChain() {
        val h = parseHostRecord(
            rec("k", "n", "10.0.0.5", "9777", "", "optional", "", "linux/fedora/bazzite"),
        )!!
        assertEquals("linux/fedora/bazzite", h.os)
        // A record without the 9th field has no mgmt port, so the caller falls back to the
        // default. Absent must read as "unknown", never as port 0.
        assertNull(h.mgmtPort)
    }

    @Test
    fun ninthFieldCarriesTheMgmtPort() {
        // 47991, not the default port — a host that MOVED its mgmt port is the whole reason this
        // field is on the wire, and a test pinned to the default would pass against a hardcode.
        val h = parseHostRecord(
            rec("k", "n", "10.0.0.5", "9777", "", "optional", "", "linux/arch", "47991"),
        )!!
        assertEquals(47991, h.mgmtPort)
    }

    @Test
    fun mgmtPortOutOfRangeOrUnparsableReadsAsUnknown() {
        // Unauthenticated advert data: 0 (the "not advertised" sentinel the Rust side emits),
        // a non-number, and an out-of-range value must all mean "assume the default" rather than
        // produce a port the client would then fail to connect to.
        val base = arrayOf("k", "n", "10.0.0.5", "9777", "", "optional", "", "linux/arch")
        assertNull(parseHostRecord(rec(*base, "0"))!!.mgmtPort)
        assertNull(parseHostRecord(rec(*base, "not-a-port"))!!.mgmtPort)
        assertNull(parseHostRecord(rec(*base, "70000"))!!.mgmtPort)
        assertNull(parseHostRecord(rec(*base, ""))!!.mgmtPort)
    }

    @Test
    fun osChainIsSanitizedAsUntrustedInput() {
        // mDNS is unauthenticated: junk is dropped, case folds, token/count caps apply.
        val h = parseHostRecord(rec("k", "n", "10.0.0.5", "9777", "", "optional", "", "Linux/Fe do!ra"))!!
        assertEquals("linux/fedora", h.os)
        assertEquals("", sanitizeOsChain("///!!!"))
        assertEquals("a/b/c/d/e", sanitizeOsChain("a/b/c/d/e/f/g"))
    }

    @Test
    fun iconWalkIsMostSpecificFirstWithAliases() {
        assertEquals(listOf("bazzite", "fedora", "linux"), osIconTokens("linux/fedora/bazzite"))
        assertEquals(listOf("steam", "arch", "linux"), osIconTokens("linux/arch/steamos"))
        assertEquals(listOf("apple"), osIconTokens("macos"))
        assertTrue(osIconTokens("").isEmpty())
    }

    @Test
    fun emptyKeyFallsBackToAddrPort() {
        // Host advertised no `id` TXT → the native side leaves the key blank; we synthesize addr:port.
        val h = parseHostRecord(rec("", "name", "10.0.0.5", "9777", "", "required"))!!
        assertEquals("10.0.0.5:9777", h.key)
    }

    @Test
    fun emptyNameFallsBackToAddr() {
        val h = parseHostRecord(rec("k", "", "10.0.0.5", "9777", "", "optional"))!!
        assertEquals("10.0.0.5", h.name)
    }

    @Test
    fun rejectsTooFewFields() {
        assertNull(parseHostRecord("only${'\u001F'}three${'\u001F'}fields"))
        assertNull(parseHostRecord(""))
    }

    @Test
    fun rejectsBadPortOrAddress() {
        assertNull(parseHostRecord(rec("k", "n", "10.0.0.5", "notaport", "", "required")))
        assertNull(parseHostRecord(rec("k", "n", "10.0.0.5", "0", "", "required")))
        assertNull(parseHostRecord(rec("k", "n", "10.0.0.5", "70000", "", "required")))
        assertNull(parseHostRecord(rec("k", "n", "", "9777", "", "required")))
    }
}
