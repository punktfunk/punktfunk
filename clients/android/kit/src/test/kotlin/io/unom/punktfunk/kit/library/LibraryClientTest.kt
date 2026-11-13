package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The Android mgmt client's contract with the desktop and Apple ones, and the shared wake fetch. */
class LibraryClientTest {
    @Test
    fun an_ipv6_host_is_bracketed_in_the_base_url() {
        assertEquals("https://192.168.1.9:47990", mgmtBase("192.168.1.9", 47990))
        assertEquals("https://desk.local:47990", mgmtBase("desk.local", 47990))
        assertEquals("https://[fe80::1]:47990", mgmtBase("fe80::1", 47990))
        assertEquals("https://[fe80::1]:47990", mgmtBase("[fe80::1]", 47990))
    }

    /** The pinned-host check compares against the name OkHttp hands the verifier. */
    @Test
    fun the_pinned_host_is_named_the_way_okhttp_names_it() {
        assertEquals("2001:db8::1", urlHost("2001:DB8:0:0:0:0:0:1"))
        assertEquals("fe80::1", urlHost("[fe80::1]"))
        assertEquals("desk.local", urlHost("Desk.local"))
        assertEquals("192.168.1.9", urlHost("192.168.1.9"))
    }

    @Test
    fun a_403_reads_as_not_paired_like_a_401() {
        assertTrue(LibraryClient.refused(401) is LibraryResult.Unauthorized)
        assertTrue(LibraryClient.refused(403) is LibraryResult.Unauthorized)
        assertEquals(LibraryResult.Error(say(R.string.error_mgmt_refused, 500)), LibraryClient.refused(500))
    }

    @Test
    fun both_shells_file_a_catalog_under_the_record_id_else_the_pin() {
        val desk = KnownHost("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true, id = "desk")
        assertEquals("desk", LibraryCache.keyFor(desk, desk.fpHex))
        assertEquals("ab".repeat(32), LibraryCache.keyFor(null, "ab".repeat(32)))
    }

    /** Replays `clients/shared/library-profile-vectors.json`, the rule Rust and Swift replay too. */
    @Test
    fun a_library_fetch_follows_the_shared_profile_vectors() {
        val file = java.io.File("../../shared/library-profile-vectors.json")
        assertTrue("vectors reachable at ${file.absolutePath}", file.isFile)
        val cases = org.json.JSONObject(file.readText()).getJSONArray("cases")
        assertTrue(cases.length() > 0)
        for (i in 0 until cases.length()) {
            val c = cases.getJSONObject(i)
            val name = c.getString("name")
            val host = c.getJSONObject("host")
            val expect = c.getJSONObject("expect")
            val asked = ArrayList<String>()
            val pick = if (c.isNull("pick")) null else c.getString("pick")
            val shown = LibraryClient.catalog(pick, "https://h:47990") { path ->
                asked += path
                host.optJSONObject(path)?.let { it.getInt("status") to it.get("body").toString() } ?: (404 to "")
            }
            val want = expect.getJSONArray("asked").let { a -> (0 until a.length()).map { a.getString(it) } }
            assertEquals("$name: asked", want, asked)
            val served = if (expect.isNull("as")) null else expect.getString("as")
            assertEquals("$name: as", served, (shown as? LibraryResult.Ok)?.profile)
            val ids = expect.optJSONArray("ids")?.let { a -> (0 until a.length()).map { a.getString(it) } }
            assertEquals("$name: ids", ids, (shown as? LibraryResult.Ok)?.games?.map { it.id })
        }
    }

    private fun page(ids: List<String>, next: String?): Pair<Int, String> {
        val items = ids.joinToString(",") { """{"id":"$it","store":"custom","title":"$it","art":{"portrait":"/api/v1/library/art/$it/portrait"}}""" }
        val cursor = next?.let { ""","next_cursor":"$it"""" } ?: ""
        return 200 to """{"items":[$items],"total":4,"platforms":[]$cursor}"""
    }

    @Test
    fun a_walk_follows_the_cursor_to_the_last_page() {
        val asked = ArrayList<String?>()
        val walked = LibraryClient.walkPages("https://h:47990") { cursor ->
            asked += cursor
            when (cursor) {
                null -> page(listOf("a", "b"), "c1")
                "c1" -> page(listOf("c"), "c2")
                else -> page(listOf("d"), null)
            }
        } as LibraryClient.Walk.Done
        assertEquals(listOf("a", "b", "c", "d"), walked.games.map { it.id })
        assertEquals(listOf(null, "c1", "c2"), asked)
        assertEquals("https://h:47990/api/v1/library/art/a/portrait", walked.games[0].art.portrait)
    }

    @Test
    fun a_game_carries_the_hosts_overrides_and_an_older_host_none() {
        val body = """{"items":[
            {"id":"steam:1","store":"steam","title":"A","art":{},
             "overrides":{"lines":1080,"hz":60,"hdr":false,"gamepad":"dualsense","beyond_screen":true,"future":1}},
            {"id":"steam:2","store":"steam","title":"B","art":{},"overrides":{"name":"4K","lines":"x"}},
            {"id":"steam:3","store":"steam","title":"C","art":{}}
        ]}"""
        val games = (LibraryClient.walkPages("https://h:47990") { 200 to body } as LibraryClient.Walk.Done).games
        assertEquals(
            GameOverrides(lines = 1080, hz = 60, hdr = false, gamepad = "dualsense", beyondScreen = true),
            games[0].overrides,
        )
        assertNull("a name alone sets nothing", games[1].overrides)
        assertNull(games[2].overrides)
    }

    @Test
    fun a_walk_ends_on_a_cursor_that_does_not_move() {
        var calls = 0
        val walked = LibraryClient.walkPages("https://h:47990") {
            calls++
            page(listOf("a"), "stuck")
        } as LibraryClient.Walk.Done
        assertEquals(2, calls)
        assertEquals(2, walked.games.size)
    }

    @Test
    fun a_refused_page_stops_the_walk_with_its_status() {
        val walked = LibraryClient.walkPages("https://h:47990") { cursor ->
            if (cursor == null) page(listOf("a"), "c1") else 403 to ""
        }
        assertEquals(LibraryClient.Walk.Refused(403), walked)
    }

    @Test
    fun a_cursor_is_encoded_into_the_page_path() {
        assertEquals("/api/v1/library/page?limit=200", LibraryClient.pagePath(null))
        assertEquals("/api/v1/library/page?limit=200&cursor=a%2Bb%3D", LibraryClient.pagePath("a+b="))
    }

    private val unreachable = LibraryResult.Error(say(R.string.android_lib_error_unreachable))

    private class Run {
        var packets = 0
        var fetches = 0
        var wakingCalls = 0
        var slept = 0L
    }

    private fun fetch(
        waking: Boolean,
        answers: (Int) -> LibraryResult,
        run: Run,
        isCancelled: () -> Boolean = { false },
    ) = LibraryClient.acrossWake(
        waking = waking,
        fetch = { answers(run.fetches++) },
        wake = { run.packets++ },
        isCancelled = isCancelled,
        onWaking = { run.wakingCalls++ },
        sleep = { run.slept += it },
    )

    /** A stream-port reply is two bytes of status, then a body that may be binary. */
    @Test
    fun a_stream_reply_is_its_status_then_its_body() {
        val (code, body) = tunnelReply(byteArrayOf(0x01, 0x94.toByte(), 0x00, 0xff.toByte()))!!
        assertEquals(404, code)
        assertEquals(listOf<Byte>(0x00, 0xff.toByte()), body.toList())
        assertEquals(200, tunnelReply(byteArrayOf(0x00, 0xc8.toByte()))?.first)
        assertNull(tunnelReply(byteArrayOf(0x01)))
    }

    /**
     * No packet without auto-wake on and a MAC to send to, and nothing may say "waking". A packet
     * here would load the native library, which a JVM test does not have.
     */
    @Test
    fun a_packet_needs_auto_wake_and_a_mac() {
        var waking = 0
        for ((macs, autoWake) in listOf(listOf("aa:bb:cc:dd:ee:ff") to false, emptyList<String>() to true)) {
            val res = LibraryClient.fetchAcrossWake(
                "192.168.1.9", 47990, "", "", "",
                macs = macs, autoWake = autoWake, profile = null, onWaking = { waking++ },
            )
            assertTrue(res is LibraryResult.Unauthorized)
        }
        assertEquals(0, waking)
        val run = Run()
        fetch(waking = false, answers = { unreachable }, run = run)
        assertEquals(0, run.packets)
        assertEquals(1, run.fetches)
    }

    @Test
    fun a_waking_fetch_retries_across_the_boot_window_and_resends() {
        val run = Run()
        assertEquals(unreachable, fetch(waking = true, answers = { unreachable }, run = run))
        assertEquals(LibraryClient.WAKE_ATTEMPTS, run.fetches)
        assertEquals(1, run.wakingCalls)
        // The first packet, then one after every second miss (none after the last).
        assertEquals(6, run.packets)
        assertEquals((LibraryClient.WAKE_ATTEMPTS - 1) * LibraryClient.WAKE_RETRY_MS, run.slept)
    }

    /** Waiting does not pair a device: a refusal ends the loop at once. */
    @Test
    fun a_settled_answer_ends_the_retries() {
        val run = Run()
        val refusal = LibraryClient.refused(403)
        assertEquals(refusal, fetch(waking = true, answers = { if (it < 2) unreachable else refusal }, run = run))
        assertEquals(3, run.fetches)
        val ok = Run()
        assertTrue(fetch(waking = true, answers = { LibraryResult.Ok(emptyList()) }, run = ok) is LibraryResult.Ok)
        assertEquals(1, ok.fetches)
    }

    @Test
    fun a_cancelled_fetch_stops_asking() {
        val run = Run()
        fetch(waking = true, answers = { unreachable }, run = run, isCancelled = { run.fetches >= 2 })
        assertEquals(2, run.fetches)
    }
}
