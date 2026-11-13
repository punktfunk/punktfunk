package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.HostText
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.say
import java.io.File
import java.nio.file.Files
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Replays every section of `clients/shared/hide-for-me-vectors.json`, as Rust and Swift do. */
class HideTest {
    private val vectors: JSONObject by lazy {
        val file = File("../../shared/hide-for-me-vectors.json")
        assertTrue("vectors reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun cases(section: String): List<JSONObject> =
        vectors.getJSONArray(section).let { a -> (0 until a.length()).map { a.getJSONObject(it) } }

    private fun strings(a: JSONArray?): List<String>? = a?.let { (0 until it.length()).map(it::getString) }

    private fun opt(o: JSONObject, k: String) = if (o.isNull(k)) null else o.getString(k)

    /** A reply body as the wire carries it: `null` is an empty body. */
    private fun body(reply: JSONObject) = if (reply.isNull("body")) "" else reply.get("body").toString()

    @Test
    fun the_offer_follows_the_shared_vectors() {
        val cases = cases("offer")
        assertTrue(cases.isNotEmpty())
        for (c in cases) {
            val listed = c.optJSONArray("listed")?.let { a ->
                (0 until a.length()).map { ListedProfile.parse(a.getJSONObject(it)) }
            }
            val row = Hide.offer(listed, opt(c, "pick"))
            assertEquals(c.getString("name"), opt(c, "offered"), row?.displayName)
        }
    }

    @Test
    fun a_hide_follows_the_shared_vectors() {
        val cases = cases("set_hidden")
        assertTrue(cases.isNotEmpty())
        for (c in cases) {
            val name = c.getString("name")
            val (path, sent) = Hide.request(c.getString("profile"), c.getString("id"), c.getBoolean("hidden"))
            assertEquals("$name: path", c.getString("path"), path)
            assertEquals("$name: body", c.getJSONObject("body").toString(), JSONObject(sent).toString())
            val reply = c.getJSONObject("reply")
            val outcome = HideOutcome.fromReply(reply.getInt("status"), HostText.apiError(body(reply)))
            val (kind, said) = when (outcome) {
                HideOutcome.Done -> "done" to null
                // In a JVM test a host message has no resources: its English is what shows.
                is HideOutcome.Refused -> "refused" to when (val s = outcome.said) {
                    is Say.Host -> s.english
                    is Say.Text -> s.value
                    else -> s.toString()
                }
                is HideOutcome.Failed -> "failed" to null
            }
            assertEquals("$name: outcome", c.getString("outcome"), kind)
            assertEquals("$name: said", opt(c, "said"), said)
        }
    }

    @Test
    fun the_hidden_list_follows_the_shared_vectors() {
        val cases = cases("hidden_list")
        assertTrue(cases.isNotEmpty())
        for (c in cases) {
            val name = c.getString("name")
            val host = c.getJSONObject("host")
            val asked = ArrayList<String>()
            val shown = Hide.hiddenList(c.getString("profile"), "https://h:47990") { path ->
                asked += path
                host.optJSONObject(path)?.let { it.getInt("status") to body(it) } ?: (404 to "")
            }
            assertEquals("$name: asked", strings(c.getJSONArray("asked")), asked)
            assertEquals("$name: ids", strings(c.optJSONArray("ids")), (shown as? LibraryResult.Ok)?.games?.map { it.id })
        }
    }

    /** Each outcome reads as the shared message, and the host's sentence stands as it came. */
    @Test
    fun each_outcome_reads_as_one_line() {
        val refused = Say.Text("This device can't change this profile.")
        assertEquals(refused, HideOutcome.Refused(refused).notice(true, "Dota 2", "Kid"))
        assertEquals(say(R.string.lib_hidden_done, "Dota 2", "Kid"), HideOutcome.Done.notice(true, "Dota 2", "Kid"))
        assertEquals(say(R.string.lib_shown_done, "Dota 2", "Kid"), HideOutcome.Done.notice(false, "Dota 2", "Kid"))
        val why = say(R.string.error_mgmt_refused, 502)
        assertEquals(HideOutcome.Failed(why), HideOutcome.fromReply(502, null))
        assertEquals(say(R.string.lib_show_failed, "Dota 2", why), HideOutcome.Failed(why).notice(false, "Dota 2", "Kid"))
    }

    /** A host that never sends `person` has no action: every row reads false. */
    @Test
    fun a_row_without_person_offers_nothing() {
        val rows = ListedProfile.parseList("""[{"id":"kid","display_name":"Kid","unlock":"none"}]""")
        assertEquals(false, rows!!.single().person)
        assertNull(Hide.offer(rows, "kid"))
    }

    /** A hide edits the cached catalog in place, keeping its age; a show puts the title back. */
    @Test
    fun the_cached_catalog_follows_a_hide_and_a_show() {
        val cache = LibraryCache(Files.createTempDirectory("pf-lib").toFile())
        val dota = GameEntry("steam:570", "steam", "Dota 2", Artwork(null, null, null))
        val portal = GameEntry("steam:400", "steam", "Portal", Artwork(null, null, null))
        cache.store("host", "kid", listOf(dota, portal), fetchedAt = 42)
        cache.noteHidden("host", "kid", dota.id, back = null)
        assertEquals(CachedLibrary(listOf(portal), 42), cache.load("host", "kid"))
        cache.noteHidden("host", "kid", dota.id, back = dota)
        assertEquals(listOf(portal, dota), cache.load("host", "kid")?.games)
        cache.noteHidden("host", null, dota.id, back = null)
        assertNull("no cache, nothing written", cache.load("host", null))
        cache.noteHidden("host", "kid", portal.id, back = null)
        cache.noteHidden("host", "kid", dota.id, back = null)
        assertNull("the last title gone, no cache", cache.load("host", "kid"))
    }
}
