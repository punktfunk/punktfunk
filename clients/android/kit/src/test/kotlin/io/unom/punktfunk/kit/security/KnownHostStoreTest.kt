package io.unom.punktfunk.kit.security

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** Unit tests for `KnownHostStore`'s pure helpers: MAC parsing and the record codec. */
class KnownHostStoreTest {
    @Test
    fun parsesAndNormalizesSingleMac() {
        assertEquals(listOf("aa:bb:cc:dd:ee:ff"), KnownHostStore.parseMacs("AA:BB:CC:DD:EE:FF"))
    }

    @Test
    fun parsesMultipleSeparators() {
        val expected = listOf("aa:bb:cc:dd:ee:ff", "11:22:33:44:55:66")
        assertEquals(expected, KnownHostStore.parseMacs("aa:bb:cc:dd:ee:ff, 11:22:33:44:55:66"))
        assertEquals(expected, KnownHostStore.parseMacs("aa:bb:cc:dd:ee:ff 11:22:33:44:55:66"))
        assertEquals(expected, KnownHostStore.parseMacs("aa:bb:cc:dd:ee:ff\n11:22:33:44:55:66"))
    }

    @Test
    fun dropsMalformedEntries() {
        // Not six octets / bad hex / wrong width are all dropped; an empty field clears the MAC.
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs(""))
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("not-a-mac"))
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("aa:bb:cc:dd:ee"))     // 5 octets
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("gg:bb:cc:dd:ee:ff"))  // non-hex
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("aaa:bb:cc:dd:ee:ff")) // wrong width
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("aa:bb:cc:dd:ee:-1")) // signed octet
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("+a:-b:+c:-d:+e:-f")) // signed octets
        assertEquals(listOf("aa:bb:cc:dd:ee:ff"), KnownHostStore.parseMacs("junk, aa:bb:cc:dd:ee:ff"))
    }

    @Test
    fun clipboardSyncDefaultsOffAndEncodesOff() {
        // A newly trusted host must not read the clipboard until the user enables it for that
        // host — the record default, and what a fresh record writes, are both off (matching
        // the Rust and Apple clients).
        val h = KnownHost("10.0.0.5", 9777, "HTPC", "a".repeat(64), true)
        assertFalse(h.clipboardSync)
        assertFalse(JSONObject(KnownHostStore.encode(h)).getBoolean("clip"))
    }

    @Test
    fun keepAddressRoundTripsAndDefaultsOff() {
        val plain = KnownHost("10.0.0.5", 9777, "HTPC", "a".repeat(64), true)
        assertFalse(plain.keepAddress)
        assertFalse(KnownHostStore.decode(KnownHostStore.encode(plain))!!.keepAddress)
        val kept = plain.copy(keepAddress = true)
        assertTrue(JSONObject(KnownHostStore.encode(kept)).getBoolean("keep_addr"))
        assertTrue(KnownHostStore.decode(KnownHostStore.encode(kept))!!.keepAddress)
        // A record written before the field existed has no key and reads back off.
        val legacy = JSONObject(KnownHostStore.encode(kept)).apply { remove("keep_addr") }
        assertFalse(KnownHostStore.decode(legacy.toString())!!.keepAddress)
    }

    @Test
    fun encodedRecordCarriesTheOsChainAndLegacyRecordsReadBackEmpty() {
        val h = KnownHost("10.0.0.5", 9777, "HTPC", "a".repeat(64), true, os = "linux/fedora/bazzite")
        val j = JSONObject(KnownHostStore.encode(h))
        assertEquals("linux/fedora/bazzite", j.getString("os"))
        // A record written before the field existed has no "os" key; the parse contract
        // (optString) reads it back as empty — same additive rule as every late field.
        val legacy = JSONObject().put("addr", "10.0.0.5").put("port", 9777).toString()
        assertEquals("", JSONObject(legacy).optString("os", ""))
    }

    @Test
    fun perTitlePresetBindingsSurviveTheRoundTripAndDefaultEmpty() {
        val plain = KnownHost("10.0.0.5", 9777, "HTPC", "a".repeat(64), true)
        assertTrue(plain.gamePresets.isEmpty())
        val bound = plain.copy(gamePresets = mapOf("halo" to "p2"))
        val j = JSONObject(KnownHostStore.encode(bound))
        assertEquals("p2", j.getJSONObject("game_presets").getString("halo"))
        // A record written before the field existed has no key and reads back empty —
        // the same additive rule as `os` above, so no migration is owed.
        val legacy = JSONObject(KnownHostStore.encode(plain))
        assertEquals(0, legacy.getJSONObject("game_presets").length())
    }

    /** The console's sort stamps survive a round trip; a record without them reads back undated. */
    @Test
    fun addedAndLastUsedRoundTripAndDefaultNull() {
        val plain = KnownHost("10.0.0.5", 9777, "HTPC", "a".repeat(64), true)
        val back = KnownHostStore.decode(KnownHostStore.encode(plain))!!
        assertEquals(null, back.addedAt)
        assertEquals(null, back.lastUsed)
        val stamped = plain.copy(addedAt = 1_700_000_000, lastUsed = 1_700_000_100)
        assertEquals(stamped, KnownHostStore.decode(KnownHostStore.encode(stamped)))
    }

    /** A record from before the rename keeps its bindings, and encoding writes both spellings. */
    @Test
    fun aPreRenameRecordKeepsItsBindings() {
        val plain = KnownHost("10.0.0.5", 9777, "HTPC", "a".repeat(64), true)
        val old = JSONObject(KnownHostStore.encode(plain))
            .put("profile", "p1")
            .put("game_profiles", JSONObject().put("halo", "p2"))
        old.remove("preset")
        old.remove("game_presets")
        val h = KnownHostStore.decode(old.toString())!!
        assertEquals("p1", h.presetId)
        assertEquals(mapOf("halo" to "p2"), h.gamePresets)
        val j = JSONObject(KnownHostStore.encode(h))
        assertEquals("p1", j.getString("preset"))
        assertEquals("p1", j.getString("profile"))
        assertEquals("p2", j.getJSONObject("game_profiles").getString("halo"))
    }

    /** A card follows the host's renames while it carries the host's own name. */
    @Test
    fun aCardFollowsTheHostNameUnlessThePlayerNamedIt() {
        val seen = KnownHost("10.0.0.5", 9777, "bazzite", "a".repeat(64), true)
            .followingName("bazzite")
        val renamed = seen.followingName("Living Room")
        assertEquals("Living Room", renamed.name)
        assertEquals("Couch", renamed.copy(name = "Couch").followingName("Den").name)
        // A legacy record whose name already differs from the first advert keeps it.
        assertEquals("Desk", KnownHost("10.0.0.5", 9777, "Desk", "", false).followingName("box").name)
        assertEquals("Living Room", KnownHostStore.decode(KnownHostStore.encode(renamed))?.advertName)
    }
}

/**
 * The store migration, run against a REAL pre-migration prefs blob — records exactly as the
 * `"address:port"`-keyed store wrote them, IPv6 and all. It runs once against live user data on
 * every upgraded install, so the property under test is blunt: **every host survives**, with its
 * trust intact and the retiring global clipboard setting carried onto it.
 */
class KnownHostMigrationTest {
    /** Verbatim shape of the old writer: no `id`, no `clip`, MACs as a comma-joined string. */
    private fun legacy(addr: String, port: Int, name: String, fp: String, paired: Boolean, mac: String) =
        JSONObject()
            .put("addr", addr)
            .put("port", port)
            .put("name", name)
            .put("fp", fp)
            .put("paired", paired)
            .put("mac", mac)
            .toString()

    private val preMigration: Map<String, Any?> = mapOf(
        "192.168.1.42:9777" to legacy("192.168.1.42", 9777, "Living Room PC", "a".repeat(64), true, "aa:bb:cc:dd:ee:ff"),
        "192.168.1.50:9777" to legacy("192.168.1.50", 9777, "Office", "b".repeat(64), false, ""),
        // An IPv6 host: the old key contains colons of its own, which is exactly why the record
        // always carried its address in the VALUE rather than parsing it back out of the key.
        "fd00::1:9777" to legacy("fd00::1", 9777, "Basement", "c".repeat(64), true, ""),
    )

    private fun migrated(globalClipboard: Boolean): List<JSONObject> =
        KnownHostStore.migrate(preMigration, globalClipboard).writes.values.map { JSONObject(it) }

    @Test
    fun everyHostSurvivesWithItsTrustIntact() {
        val result = KnownHostStore.migrate(preMigration, globalClipboardSync = true)
        assertEquals(3, result.writes.size)
        // Every old key is dropped — none of them is a valid new key (they aren't ids).
        assertEquals(preMigration.keys, result.removals)

        val byName = result.writes.values.map { JSONObject(it) }.associateBy { it.getString("name") }
        assertEquals(setOf("Living Room PC", "Office", "Basement"), byName.keys)

        val living = byName.getValue("Living Room PC")
        assertEquals("192.168.1.42", living.getString("addr"))
        assertEquals(9777, living.getInt("port"))
        assertEquals("a".repeat(64), living.getString("fp"))
        assertTrue(living.getBoolean("paired"))
        assertEquals("aa:bb:cc:dd:ee:ff", living.getString("mac"))

        // The IPv6 address round-trips out of the value, untouched by the key rewrite.
        assertEquals("fd00::1", byName.getValue("Basement").getString("addr"))
        assertFalse(byName.getValue("Office").getBoolean("paired"))
    }

    @Test
    fun eachRecordIsRekeyedOntoItsOwnMintedId() {
        val result = KnownHostStore.migrate(preMigration, globalClipboardSync = true)
        // The key IS the record's id, and the ids are distinct — two hosts must never collide onto
        // one record (which would silently lose one of them).
        result.writes.forEach { (key, json) -> assertEquals(key, JSONObject(json).getString("id")) }
        assertEquals(3, result.writes.keys.size)
        // The minted shape is the cross-platform one: a lowercase UUID.
        result.writes.keys.forEach { id ->
            assertEquals(36, id.length)
            assertEquals(id.lowercase(), id)
            assertEquals(listOf(8, 4, 4, 4, 12), id.split("-").map { it.length })
        }
        assertNotEquals(newRecordId(), newRecordId())
    }

    /**
     * The behaviour-preserving half: clipboard sync was one global that every host followed, so
     * after the migration every host must still be following the value that global held — on or
     * off. This is the assertion that would catch a migration silently defaulting everyone to on.
     */
    @Test
    fun theRetiringGlobalClipboardSettingLandsOnEveryHost() {
        migrated(globalClipboard = true).forEach { assertTrue(it.getBoolean("clip")) }
        migrated(globalClipboard = false).forEach { assertFalse(it.getBoolean("clip")) }
    }

    @Test
    fun migrationIsIdempotentOverItsOwnOutput() {
        val once = KnownHostStore.migrate(preMigration, globalClipboardSync = false)
        val twice = KnownHostStore.migrate(once.writes, globalClipboardSync = true)
        // Already-keyed records keep their ids and their per-host value: a second pass (a downgrade
        // then re-upgrade, a schema flag lost) must not re-mint ids that bindings point at, and must
        // not resurrect the global over a per-host answer the user has since changed.
        assertEquals(once.writes, twice.writes)
        assertTrue(twice.removals.isEmpty())
    }

    @Test
    fun entriesThatArentHostRecordsAreLeftAlone() {
        val junk = mapOf(
            "some_unrelated_flag" to true,
            "half_a_record" to JSONObject().put("name", "no address").toString(),
            "not_even_json" to "{{{",
        )
        val result = KnownHostStore.migrate(junk, globalClipboardSync = true)
        // They were already invisible to `all()`; deleting what we don't understand isn't this
        // pass's job, and writing them as hosts would invent records out of nothing.
        assertTrue(result.writes.isEmpty())
        assertTrue(result.removals.isEmpty())
    }
}
