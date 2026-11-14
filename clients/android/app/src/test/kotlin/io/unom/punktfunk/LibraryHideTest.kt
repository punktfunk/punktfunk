package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performSemanticsAction
import androidx.test.platform.app.InstrumentationRegistry
import coil3.ImageLoader
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.library.Artwork
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.security.ClientIdentity
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Hide for me on the touch shelf, driven with a remote: no entry where the host sends no mark, and
 * a title that leaves hands focus to its neighbour.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36], qualifiers = "w960dp-h540dp-television-xhdpi")
class LibraryHideTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private fun game(id: String, title: String) = GameEntry(id, "steam", title, Artwork(null, null, null))

    private val arkham = game("steam:1", "Arkham")
    private val braid = game("steam:2", "Braid")
    private val celeste = game("steam:3", "Celeste")

    private fun text(id: Int, vararg args: Any) = compose.activity.getString(id, *args)

    private fun openMenu(title: String) =
        compose.onNodeWithText(title).performSemanticsAction(SemanticsActions.OnLongClick)

    /** A host that never sends `person`: the shelf has no toggle and a title's menu no Hide row. */
    @Test
    fun a_shelf_without_the_mark_offers_no_hide() {
        compose.setContent {
            val loader = ImageLoader(compose.activity)
            val ready = LibState.Ready(listOf(braid), loader, ClientIdentity("", ""))
            TouchLibrary(ready, false, {}, { _, _ -> }, { _, _ -> })
        }
        openMenu("Braid")
        compose.onNodeWithText(text(R.string.hostcard_copy_link)).assertExists()
        compose.onNodeWithText("Hide for", substring = true).assertDoesNotExist()
        // The shelf's own menu offers no way to what a profile hides either.
        compose.onNodeWithText(text(R.string.hostcard_copy_link)).performClick()
        compose.onNodeWithContentDescription(text(R.string.lib_title_more)).performClick()
        compose.onNodeWithText(text(R.string.lib_customize)).assertExists()
        compose.onNodeWithText("Hidden for", substring = true).assertDoesNotExist()
    }

    /** Add to favorites lands the title in the Favorites row; the title menu then takes it back. */
    @Test
    fun a_favorite_gets_its_row() {
        var favorites by mutableStateOf(emptySet<String>())
        compose.setContent {
            TouchGrid(
                listOf(arkham, braid), ImageLoader(compose.activity), onLaunch = {}, onLink = { _, _ -> },
                favorites = favorites,
                onFavorite = { g -> favorites = if (g.id in favorites) favorites - g.id else favorites + g.id },
            )
        }
        compose.onNodeWithText(text(R.string.lib_section_favorites)).assertDoesNotExist()
        openMenu("Braid")
        compose.onNodeWithText(text(R.string.lib_favorite_add)).performClick()
        compose.waitForIdle()
        compose.onNodeWithText(text(R.string.lib_section_favorites)).assertExists()
        assertEquals(setOf("steam:2"), favorites)
    }

    @Test
    fun a_hidden_title_hands_focus_to_its_neighbour() {
        InstrumentationRegistry.getInstrumentation().setInTouchMode(false)
        var games by mutableStateOf(listOf(arkham, braid, celeste))
        var refocus by mutableStateOf<String?>(null)
        val handed = ArrayList<String?>()
        compose.setContent {
            TouchGrid(
                games, ImageLoader(compose.activity), onLaunch = {}, onLink = { _, _ -> },
                hideLabel = text(R.string.lib_hide_for, "Kid"),
                onHide = { g, next ->
                    handed += next
                    games = games - g
                    refocus = next
                },
                refocus = refocus,
                onRefocused = { refocus = null },
            )
        }
        openMenu("Braid")
        compose.onNodeWithText(text(R.string.lib_hide_for, "Kid")).performClick()
        compose.waitForIdle()
        compose.onNodeWithText("Celeste").assertIsFocused()
        // The last title hands focus back to the one before it.
        openMenu("Celeste")
        compose.onNodeWithText(text(R.string.lib_hide_for, "Kid")).performClick()
        compose.waitForIdle()
        compose.onNodeWithText("Arkham").assertIsFocused()
        assertEquals(listOf("steam:3", "steam:1"), handed)
    }

    @Test
    fun the_hidden_view_says_when_nothing_is_hidden() {
        compose.setContent {
            val loader = ImageLoader(compose.activity)
            val ready = LibState.Ready(listOf(braid), loader, ClientIdentity("", ""))
            TouchLibrary(
                ready, false, {}, { _, _ -> }, { _, _ -> },
                hidesFor = "Kid", hidden = LibState.Ready(emptyList(), loader, ClientIdentity("", "")),
            )
        }
        compose.onNodeWithText(text(R.string.lib_hidden_for, "Kid")).assertExists()
        compose.onNodeWithText(text(R.string.lib_hidden_none, "Kid")).assertExists()
        compose.onNodeWithText("Braid").assertDoesNotExist()
    }
}
