package io.unom.punktfunk

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Favorite
import androidx.compose.material.icons.filled.FavoriteBorder
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.Link
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Schedule
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import coil3.ImageLoader
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.LibrarySection
import io.unom.punktfunk.kit.library.LibrarySections

/**
 * A title's details: cover, what it is, what the host recorded about playing it, and its acts.
 * Opened from the title menu's Details…; a tap on the poster still just plays.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun TitleDetailSheet(
    game: GameEntry,
    loader: ImageLoader,
    /** The host has the title up: Play reads Resume. */
    running: Boolean,
    favorite: Boolean,
    onPlay: () -> Unit,
    onFavorite: () -> Unit,
    onLink: (LinkAction) -> Unit,
    /** Set while the host runs a launch of this device's; the caller confirms. */
    onEndGame: (() -> Unit)?,
    onDismiss: () -> Unit,
) {
    val context = LocalContext.current
    val res = context.resources
    val canPin = remember { canPinShortcuts(context) }
    // Store · platform, the credits, the genres, then what the host sets: a line only with something.
    val facts = listOf(
        listOfNotNull(game.storeLabel.asString(), game.platform).filter { it.isNotEmpty() }.joinToString(" · "),
        listOfNotNull(game.developer, game.releaseYear?.toString()).filter { it.isNotEmpty() }.joinToString(" · "),
        game.genres.joinToString(", "),
        game.overrides?.summaryWords(res)?.let { stringResource(R.string.lib_host_override, it) }.orEmpty(),
    ).filter { it.isNotEmpty() }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            verticalArrangement = Arrangement.spacedBy(16.dp),
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .padding(bottom = 24.dp),
        ) {
            Row(horizontalArrangement = Arrangement.spacedBy(16.dp)) {
                Box(
                    Modifier
                        .width(112.dp)
                        .aspectRatio(2f / 3f)
                        .clip(MaterialTheme.shapes.medium)
                        .background(MaterialTheme.colorScheme.surfaceVariant),
                    contentAlignment = Alignment.Center,
                ) { TouchPosterArt(game, loader) }
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text(game.title, style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.SemiBold)
                    facts.forEach {
                        Text(
                            it,
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }
            PlayStatsText.line(res, game.stats)?.let { line ->
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        Icons.Filled.Schedule,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.size(16.dp),
                    )
                    Text(
                        line,
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(start = 8.dp),
                    )
                }
            }
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(onClick = onPlay, modifier = Modifier.weight(1f)) {
                    Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                    Text(
                        stringResource(if (running) R.string.lib_resume else R.string.common_play),
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.padding(start = 8.dp),
                    )
                }
                FilledTonalIconButton(onClick = onFavorite) {
                    Icon(
                        if (favorite) Icons.Filled.Favorite else Icons.Filled.FavoriteBorder,
                        contentDescription = stringResource(
                            if (favorite) R.string.lib_favorite_remove else R.string.lib_favorite_add,
                        ),
                    )
                }
                FilledTonalIconButton(onClick = { onLink(LinkAction.Copy) }) {
                    Icon(Icons.Filled.Link, contentDescription = stringResource(R.string.hostcard_copy_link))
                }
            }
            // The title's link, handed to the home screen or a frontend: boots straight into it.
            Column {
                if (canPin) {
                    TextButton(onClick = { onLink(LinkAction.HomeScreen) }) {
                        Text(stringResource(R.string.link_home_screen))
                    }
                }
                TextButton(onClick = { onLink(LinkAction.LauncherFile) }) {
                    Text(stringResource(R.string.link_launcher_file))
                }
                onEndGame?.let {
                    TextButton(onClick = it) {
                        Text(stringResource(R.string.lib_end_game), color = MaterialTheme.colorScheme.error)
                    }
                }
            }
        }
    }
}

/**
 * The library's sections, each with a switch, in the order the tab draws them. A switched-off
 * section stays listed; one with nothing to show hides on the tab by itself.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun LibrarySectionsSheet(layout: ShelfLayout, onLayout: (ShelfLayout) -> Unit, onDismiss: () -> Unit) {
    // The console's Collections band keeps its place in the stored list, and is not offered here.
    val offered = layout.sections.indices.filter { layout.sections[it].first != LibrarySection.COLLECTIONS }
    fun swap(a: Int, b: Int) {
        val next = layout.sections.toMutableList()
        next[a] = layout.sections[b]
        next[b] = layout.sections[a]
        onLayout(layout.copy(sections = next))
    }
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(Modifier.fillMaxWidth().padding(horizontal = 24.dp).padding(bottom = 24.dp)) {
            Text(stringResource(R.string.player_library_sections), style = MaterialTheme.typography.titleLarge)
            offered.forEachIndexed { at, index ->
                val (section, on) = layout.sections[index]
                Row(verticalAlignment = Alignment.CenterVertically, modifier = Modifier.padding(top = 8.dp)) {
                    Text(
                        stringResource(section.label),
                        style = MaterialTheme.typography.bodyLarge,
                        modifier = Modifier.weight(1f),
                    )
                    IconButton(onClick = { swap(index, offered[at - 1]) }, enabled = at > 0) {
                        Icon(
                            Icons.Filled.KeyboardArrowUp,
                            contentDescription = stringResource(R.string.lib_sections_move_up),
                        )
                    }
                    IconButton(onClick = { swap(index, offered[at + 1]) }, enabled = at < offered.lastIndex) {
                        Icon(
                            Icons.Filled.KeyboardArrowDown,
                            contentDescription = stringResource(R.string.lib_sections_move_down),
                        )
                    }
                    Switch(
                        checked = on,
                        onCheckedChange = { checked ->
                            onLayout(
                                layout.copy(
                                    sections = layout.sections.map { if (it.first == section) section to checked else it },
                                ),
                            )
                        },
                    )
                }
            }
            TextButton(
                onClick = { onLayout(layout.copy(sections = LibrarySections.parse(null))) },
                enabled = layout.sections != LibrarySections.parse(null),
                modifier = Modifier.padding(top = 8.dp),
            ) { Text(stringResource(R.string.lib_sections_restore)) }
        }
    }
}

private val LibrarySection.label: Int
    get() = when (this) {
        LibrarySection.DESKTOPS -> R.string.lib_section_desktops
        LibrarySection.RECENT -> R.string.lib_section_recent
        LibrarySection.FAVORITES -> R.string.lib_section_favorites
        LibrarySection.LAUNCHERS -> R.string.lib_section_launchers
        LibrarySection.COLLECTIONS -> R.string.lib_section_collections
        LibrarySection.GAMES -> R.string.lib_section_games
    }
