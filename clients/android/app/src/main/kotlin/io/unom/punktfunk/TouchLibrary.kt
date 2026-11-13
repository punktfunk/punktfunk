package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.Sort
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.DesktopWindows
import androidx.compose.material.icons.filled.Download
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.Search
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextField
import androidx.compose.material3.TextFieldDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.boundsInWindow
import androidx.compose.ui.layout.layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import coil3.ImageLoader
import io.unom.punktfunk.components.PRESENCE_ONLINE
import io.unom.punktfunk.components.launcherIcon
import io.unom.punktfunk.components.resolveOsIcon
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.library.Download
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.InstallAction
import io.unom.punktfunk.kit.library.LibraryCollation
import io.unom.punktfunk.kit.library.LibraryGroupBy
import io.unom.punktfunk.kit.library.LibraryGroupKey
import io.unom.punktfunk.kit.library.LibrarySection
import io.unom.punktfunk.kit.library.LibraryShelf
import io.unom.punktfunk.kit.library.LibrarySortKey
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.library.ShelfRows
import io.unom.punktfunk.kit.library.TileBadge
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost

/** A paired host's desktop in the Desktops row: what a tap streams, and whether the host answers. */
internal data class DesktopTile(val host: KnownHost, val online: Boolean = false, val playing: String? = null)

/**
 * The touch library: a bar with search, sort and the shelf's menu, over the shelf's sections.
 * With [hidden] set it shows what the shelf's profile hides instead, as a plain grid.
 *
 * Internal (not private): the screenshot harness and the tests compose it with a mock shelf.
 */
@Composable
internal fun TouchLibrary(
    state: LibState,
    launching: Boolean,
    onReload: () -> Unit,
    onLaunch: (ClientIdentity, GameEntry) -> Unit,
    onLink: (GameEntry, LinkAction) -> Unit,
    /** Ask to end a title this device launched; the caller confirms. */
    onEndGame: (GameEntry) -> Unit = {},
    /** Install, pause, resume or remove a title's download; the caller confirms a removal. */
    onFiles: (GameEntry, InstallAction) -> Unit = { _, _ -> },
    /** The title this shelf last launched: where the grid opens. Null on a first visit. */
    resumeAt: String? = null,
    /** The shelf's profile while this device may hide its titles; null offers no hiding. */
    hidesFor: String? = null,
    /** What that profile hides, in the shelf's place; null shows the shelf. */
    hidden: LibState? = null,
    onHidden: (open: Boolean) -> Unit = {},
    /** Hide a shelf title (`hidden`) or show a hidden one; `next` takes its focus once it leaves. */
    onSetHidden: (game: GameEntry, hidden: Boolean, next: String?) -> Unit = { _, _, _ -> },
    /** The title to take focus now; [onRefocused] says it did. */
    refocus: String? = null,
    onRefocused: () -> Unit = {},
    snackbar: SnackbarHostState = remember { SnackbarHostState() },
    layout: ShelfLayout = ShelfLayout(),
    onLayout: (ShelfLayout) -> Unit = {},
    favorites: Set<String> = emptySet(),
    onFavorite: (GameEntry) -> Unit = {},
    desktops: List<DesktopTile> = emptyList(),
    onDesktop: (KnownHost) -> Unit = {},
    /** Drawn over the sections, inside the scroll: the shelf chips. */
    header: (@Composable () -> Unit)? = null,
) {
    val showsHidden = hidesFor != null && hidden != null
    var search by rememberSaveable { mutableStateOf("") }
    var searching by rememberSaveable { mutableStateOf(false) }
    var customize by remember { mutableStateOf(false) }
    var detail by remember { mutableStateOf<GameEntry?>(null) }
    val closeSearch = {
        searching = false
        search = ""
    }
    BackHandler(enabled = searching && !showsHidden, onBack = closeSearch)
    // The shelf's scroll outlives the hidden view, never a fresh load.
    val saved = rememberSaveableStateHolder()
    if (state is LibState.Loading) SideEffect { saved.removeState("shelf") }
    val ready = state as? LibState.Ready
    Box(Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize()) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth().padding(start = 4.dp, end = 4.dp, top = 8.dp).height(56.dp),
            ) {
                when {
                    showsHidden -> {
                        IconButton(onClick = { onHidden(false) }) {
                            Icon(
                                Icons.AutoMirrored.Filled.ArrowBack,
                                contentDescription = stringResource(R.string.common_back),
                            )
                        }
                        BarTitle(stringResource(R.string.lib_hidden_for, hidesFor!!), Modifier.weight(1f))
                        IconButton(onClick = onReload, enabled = hidden !is LibState.Loading) {
                            Icon(Icons.Filled.Refresh, contentDescription = stringResource(R.string.lib_reload))
                        }
                    }
                    searching -> SearchField(search, onChange = { search = it }, onClose = closeSearch)
                    else -> {
                        BarTitle(stringResource(R.string.lib_title), Modifier.weight(1f).padding(start = 12.dp))
                        IconButton(onClick = { searching = true }) {
                            Icon(Icons.Filled.Search, contentDescription = stringResource(R.string.lib_search_titles))
                        }
                        SortMenu(layout, onLayout)
                        ShelfMenu(
                            hidesFor = hidesFor,
                            onHidden = { onHidden(true) },
                            onCustomize = { customize = true },
                            onReload = onReload.takeIf { state !is LibState.Loading },
                        )
                    }
                }
            }
            if (showsHidden) {
                HiddenShelf(hidden!!, hidesFor!!, onSetHidden, refocus, onRefocused, Modifier.weight(1f))
            } else {
                saved.SaveableStateProvider("shelf") {
                    TouchGrid(
                        games = ready?.games.orEmpty(),
                        loader = ready?.loader,
                        onLaunch = { game -> ready?.let { onLaunch(it.identity, game) } },
                        onLink = onLink,
                        onEndGame = onEndGame,
                        running = ready?.running.orEmpty(),
                        downloads = ready?.downloads.orEmpty(),
                        grants = ready?.grants,
                        onFiles = onFiles,
                        resumeAt = resumeAt,
                        hideLabel = hidesFor?.let { stringResource(R.string.lib_hide_for, it) },
                        onHide = { game, next -> onSetHidden(game, true, next) },
                        refocus = refocus,
                        onRefocused = onRefocused,
                        layout = layout,
                        search = search,
                        favorites = favorites,
                        onFavorite = onFavorite,
                        onDetails = { detail = it },
                        desktops = desktops,
                        onDesktop = onDesktop,
                        header = {
                            Column {
                                header?.invoke()
                                // The titles are remembered, not observed. Never an error: a
                                // cached library is a working one.
                                ready?.stale?.note?.let { StaleNote(stringResource(it)) }
                            }
                        },
                        // In the sections' place while there are none. The desktops stay above it.
                        footer = when (state) {
                            is LibState.Loading -> ({ ShelfLoading() })
                            is LibState.Message -> ({ ShelfLine(state.text.asString()) })
                            is LibState.Ready -> null
                        },
                        modifier = Modifier.weight(1f),
                    )
                }
            }
        }
        SnackbarHost(snackbar, Modifier.align(Alignment.BottomCenter))
        if (launching) {
            Box(
                Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.55f)),
                contentAlignment = Alignment.Center,
            ) {
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(14.dp),
                ) {
                    CircularProgressIndicator(color = Color.White)
                    Text(
                        stringResource(R.string.lib_launching),
                        color = Color.White,
                        style = MaterialTheme.typography.bodyLarge,
                    )
                }
            }
        }
    }
    if (customize) {
        LibrarySectionsSheet(layout, onLayout) { customize = false }
    }
    // The title as the shelf has it now: a favorite or a download may have moved since it opened.
    detail?.let { opened -> ready?.games?.firstOrNull { it.id == opened.id } }?.let { game ->
        val up = ready!!.running[game.id]
        TitleDetailSheet(
            game = game,
            loader = ready.loader,
            running = up != null,
            favorite = game.id in favorites,
            onPlay = {
                detail = null
                onLaunch(ready.identity, game)
            },
            onFavorite = { onFavorite(game) },
            onLink = { onLink(game, it) },
            onEndGame = if (up?.endable == true) {
                {
                    detail = null
                    onEndGame(game)
                }
            } else {
                null
            },
            onDismiss = { detail = null },
        )
    }
}

@Composable
private fun BarTitle(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        style = MaterialTheme.typography.headlineSmall,
        fontWeight = FontWeight.SemiBold,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = modifier,
    )
}

/** The bar's search field; it takes focus as it opens, and its cross clears and closes it. */
@Composable
private fun SearchField(text: String, onChange: (String) -> Unit, onClose: () -> Unit) {
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { runCatching { focus.requestFocus() } }
    TextField(
        value = text,
        onValueChange = onChange,
        singleLine = true,
        placeholder = { Text(stringResource(R.string.lib_search_titles)) },
        leadingIcon = { Icon(Icons.Filled.Search, contentDescription = null) },
        trailingIcon = {
            IconButton(onClick = onClose) {
                Icon(Icons.Filled.Close, contentDescription = stringResource(R.string.common_close))
            }
        },
        keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
        shape = CircleShape,
        colors = TextFieldDefaults.colors(
            focusedIndicatorColor = Color.Transparent,
            unfocusedIndicatorColor = Color.Transparent,
        ),
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp).focusRequester(focus),
    )
}

/** Sort and group, each a list of choices under its heading. */
@Composable
private fun SortMenu(layout: ShelfLayout, onLayout: (ShelfLayout) -> Unit) {
    var open by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }) {
            Icon(Icons.AutoMirrored.Filled.Sort, contentDescription = stringResource(R.string.lib_sort_group))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            MenuHeading(stringResource(R.string.lib_sort))
            LibrarySortKey.entries.forEach { key ->
                ChoiceItem(stringResource(key.label), layout.sort == key) {
                    open = false
                    onLayout(layout.copy(sort = key))
                }
            }
            HorizontalDivider()
            MenuHeading(stringResource(R.string.lib_group))
            (listOf<LibraryGroupBy?>(null) + LibraryGroupBy.entries).forEach { by ->
                val label = when (by) {
                    null -> R.string.lib_group_none
                    LibraryGroupBy.PLATFORM -> R.string.lib_group_platform
                    LibraryGroupBy.STORE -> R.string.lib_group_store
                }
                ChoiceItem(stringResource(label), layout.groupBy == by) {
                    open = false
                    onLayout(layout.copy(groupBy = by))
                }
            }
        }
    }
}

private val LibrarySortKey.label: Int
    get() = when (this) {
        LibrarySortKey.HOST -> R.string.lib_sort_host
        LibrarySortKey.TITLE -> R.string.lib_sort_title
        LibrarySortKey.PLATFORM -> R.string.lib_sort_platform
        LibrarySortKey.STORE -> R.string.lib_sort_store
        LibrarySortKey.RECENT -> R.string.lib_sort_recent
        LibrarySortKey.PLAYTIME -> R.string.lib_sort_playtime
    }

@Composable
private fun MenuHeading(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 12.dp, end = 12.dp, top = 8.dp, bottom = 2.dp),
    )
}

@Composable
private fun ChoiceItem(label: String, selected: Boolean, onClick: () -> Unit) {
    DropdownMenuItem(
        text = { Text(label) },
        leadingIcon = { RadioButton(selected = selected, onClick = null) },
        onClick = onClick,
    )
}

/** The shelf's own acts: its sections, what its profile hides, and a reload. */
@Composable
private fun ShelfMenu(hidesFor: String?, onHidden: () -> Unit, onCustomize: () -> Unit, onReload: (() -> Unit)?) {
    var open by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }) {
            Icon(Icons.Filled.MoreVert, contentDescription = stringResource(R.string.lib_title_more))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(R.string.lib_customize)) },
                onClick = {
                    open = false
                    onCustomize()
                },
            )
            // There only where the profile may hide.
            if (hidesFor != null) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.lib_hidden_for, hidesFor)) },
                    onClick = {
                        open = false
                        onHidden()
                    },
                )
            }
            DropdownMenuItem(
                text = { Text(stringResource(R.string.lib_reload)) },
                enabled = onReload != null,
                onClick = {
                    open = false
                    onReload?.invoke()
                },
            )
        }
    }
}

@Composable
private fun StaleNote(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.semantics { liveRegion = LiveRegionMode.Polite }.padding(horizontal = 4.dp, vertical = 2.dp),
    )
}

@Composable
private fun ShelfLoading() {
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(14.dp),
        modifier = Modifier.fillMaxWidth().padding(vertical = 48.dp),
    ) {
        CircularProgressIndicator()
        Text(stringResource(R.string.lib_loading), style = MaterialTheme.typography.bodyLarge)
    }
}

/** The line in a shelf's place: why there is none, or that nothing matches. */
@Composable
private fun ShelfLine(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodyLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        textAlign = TextAlign.Center,
        modifier = Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 48.dp),
    )
}

/** What the shelf's profile hides: dimmed tiles whose menu only shows them again. */
@Composable
private fun HiddenShelf(
    shown: LibState,
    hidesFor: String,
    onSetHidden: (GameEntry, Boolean, String?) -> Unit,
    refocus: String?,
    onRefocused: () -> Unit,
    modifier: Modifier,
) {
    when {
        shown is LibState.Loading -> Box(modifier.fillMaxWidth(), contentAlignment = Alignment.Center) { ShelfLoading() }
        shown is LibState.Message -> Box(modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
            ShelfLine(shown.text.asString())
        }
        shown is LibState.Ready && shown.games.isEmpty() ->
            Box(modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
                ShelfLine(stringResource(R.string.lib_hidden_none, hidesFor))
            }
        shown is LibState.Ready -> TouchGrid(
            games = shown.games,
            loader = shown.loader,
            onLaunch = {},
            onLink = { _, _ -> },
            hideLabel = stringResource(R.string.lib_show_for, hidesFor),
            onHide = { game, next -> onSetHidden(game, false, next) },
            hidden = true,
            refocus = refocus,
            onRefocused = onRefocused,
            modifier = modifier,
        )
    }
}

/** One item of the shelf's scroll: a full-width band, or one poster of the games grid. */
private sealed interface Cell {
    val key: String

    data class Band(override val key: String, val content: @Composable () -> Unit) : Cell
    data class Tile(val game: GameEntry, val caption: String?) : Cell {
        override val key get() = "game-${game.id}"
    }
}

/** What a tile needs beyond its title: the shelf's art, state and acts, the same for every tile. */
private class TileContext(
    val loader: ImageLoader,
    val running: Map<String, RunningGame>,
    val downloads: Map<String, Download>,
    val grants: Int?,
    val favorites: Set<String>,
    val hidden: Boolean,
    val hideLabel: String?,
    val onLaunch: (GameEntry) -> Unit,
    val onLink: (GameEntry, LinkAction) -> Unit,
    val onEndGame: (GameEntry) -> Unit,
    val onFiles: (GameEntry, InstallAction) -> Unit,
    val onFavorite: ((GameEntry) -> Unit)?,
    val onDetails: ((GameEntry) -> Unit)?,
    val onHide: (GameEntry) -> Unit,
)

/**
 * The shelf: its sections in [layout]'s order, each hidden while it has nothing to show. Desktops,
 * Recently played, Favorites and Launchers are rows; Games is the grid, sorted and grouped. A
 * [hidden] shelf is the grid alone. One scroll holds them all, so the page moves as one.
 *
 * Internal (not private): the screenshot harness and the tests compose it with a mock shelf.
 */
@Composable
internal fun TouchGrid(
    games: List<GameEntry>,
    /** Null only where there are no titles to draw. */
    loader: ImageLoader?,
    onLaunch: (GameEntry) -> Unit,
    onLink: (GameEntry, LinkAction) -> Unit,
    onEndGame: (GameEntry) -> Unit = {},
    /** Which titles the host has up, by library id: a Resume badge. */
    running: Map<String, RunningGame> = emptyMap(),
    /** `/status` downloads by library id, and this device's grants: a tile's badge and menu row. */
    downloads: Map<String, Download> = emptyMap(),
    grants: Int? = null,
    onFiles: (GameEntry, InstallAction) -> Unit = { _, _ -> },
    /** The title this shelf last launched: where the grid opens. Null on a first visit. */
    resumeAt: String? = null,
    /** The menu row that hides a title, or in a [hidden] grid shows it; null offers neither. */
    hideLabel: String? = null,
    /** That row's act, handed the title and the one that takes its focus once it leaves. */
    onHide: (game: GameEntry, next: String?) -> Unit = { _, _ -> },
    /** The titles are ones a profile hides: dimmed, and a tap opens the menu. */
    hidden: Boolean = false,
    /** The title to take focus now; [onRefocused] says it did. */
    refocus: String? = null,
    onRefocused: () -> Unit = {},
    layout: ShelfLayout = ShelfLayout(),
    search: String = "",
    favorites: Set<String> = emptySet(),
    /** Null offers no favorites. */
    onFavorite: ((GameEntry) -> Unit)? = null,
    /** Opens a title's details; null offers none. */
    onDetails: ((GameEntry) -> Unit)? = null,
    desktops: List<DesktopTile> = emptyList(),
    onDesktop: (KnownHost) -> Unit = {},
    header: (@Composable () -> Unit)? = null,
    footer: (@Composable () -> Unit)? = null,
    modifier: Modifier = Modifier,
) {
    val res = LocalContext.current.resources
    val up = running.keys
    val rows = remember(games, layout, up, favorites, search, hidden) {
        val label: (GameEntry) -> String = { it.storeLabel.words(res) }
        if (hidden) {
            val all = LibraryShelf.grid(games, LibrarySortKey.HOST, null, emptySet(), label)
            ShelfRows(emptyList(), emptyList(), emptyList(), all)
        } else {
            LibraryShelf.rows(games, layout.sort, layout.groupBy, up, favorites, search, label)
        }
    }
    // A title that leaves hands focus to the next one in the grid, else the one before.
    val flat = rows.games.flatMap { it.games }
    fun next(game: GameEntry) = flat.indexOf(game).let { i -> (flat.getOrNull(i + 1) ?: flat.getOrNull(i - 1))?.id }
    val tiles = loader?.let {
        TileContext(
            it, running, downloads, grants, favorites, hidden, hideLabel, onLaunch, onLink, onEndGame, onFiles,
            onFavorite, onDetails, onHide = { game -> onHide(game, next(game)) },
        )
    }

    val cells = buildList {
        // Always first, even empty: the grid holds its first item in place, so a row that
        // appears under this one shows at the top instead of above the view.
        add(Cell.Band("header") { header?.invoke() })
        fun strip(section: LibrarySection, label: Int, games: List<GameEntry>, caption: (GameEntry) -> String?) {
            if (games.isEmpty() || tiles == null) return
            add(Cell.Band("heading-${section.id}") { SectionHeading(stringResource(label)) })
            add(
                Cell.Band("strip-${section.id}") {
                    Strip {
                        items(games, key = { it.id }) { game ->
                            TouchPoster(game, tiles, caption(game), modifier = Modifier.width(STRIP_TILE))
                        }
                    }
                },
            )
        }
        val shown = if (hidden) listOf(LibrarySection.GAMES) else layout.sections.filter { it.second }.map { it.first }
        for (section in shown) {
            when (section) {
                LibrarySection.DESKTOPS -> if (desktops.isNotEmpty()) {
                    add(Cell.Band("heading-desktops") { SectionHeading(stringResource(R.string.lib_section_desktops)) })
                    add(
                        Cell.Band("strip-desktops") {
                            Strip { items(desktops, key = { it.host.id }) { DesktopCard(it) { onDesktop(it.host) } } }
                        },
                    )
                }
                LibrarySection.RECENT -> strip(section, R.string.lib_section_recent, rows.recent) {
                    PlayStatsText.lastPlayed(res, it.stats)
                }
                LibrarySection.FAVORITES -> strip(section, R.string.lib_section_favorites, rows.favorites) { null }
                LibrarySection.LAUNCHERS -> strip(section, R.string.lib_section_launchers, rows.launchers) { null }
                // The console's band of platform and store tiles; here Group does that.
                LibrarySection.COLLECTIONS -> {}
                LibrarySection.GAMES -> rows.games.forEach { group ->
                    // A hidden shelf of one group is a plain grid.
                    if (!hidden || rows.games.size > 1) {
                        val heading = group.key.label(res)
                        add(Cell.Band("heading-${group.key}") { SectionHeading(heading) })
                    }
                    group.games.forEach { add(Cell.Tile(it, sortCaption(layout.sort, it, res))) }
                }
            }
        }
        if (search.isNotEmpty() && rows.run { recent.isEmpty() && favorites.isEmpty() && launchers.isEmpty() && games.isEmpty() }) {
            add(Cell.Band("no-match") { ShelfLine(stringResource(R.string.lib_no_match_body, search)) })
        }
        footer?.let { add(Cell.Band("footer", it)) }
    }

    val grid = rememberLazyGridState()
    // Put the player back at the title they launched: leaving a stream composes this screen
    // anew. One shot, so a later re-order never moves the grid under a finger. Keyed on the
    // cells: the title may arrive with the live catalog.
    var restored by rememberSaveable { mutableStateOf(false) }
    LaunchedEffect(cells.size, resumeAt) {
        if (restored || resumeAt == null) return@LaunchedEffect
        val index = cells.indexOfFirst { it is Cell.Tile && it.game.id == resumeAt }
        if (index < 0) return@LaunchedEffect
        restored = true
        grid.scrollToItem(index)
    }
    LazyVerticalGrid(
        columns = GridCells.Adaptive(minSize = 130.dp),
        state = grid,
        modifier = modifier.fillMaxWidth(),
        // The header item's spacing makes up the top margin.
        contentPadding = PaddingValues(start = SHELF_MARGIN, end = SHELF_MARGIN, top = 4.dp, bottom = SHELF_MARGIN),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        items(
            count = cells.size,
            key = { cells[it].key },
            span = { if (cells[it] is Cell.Tile) GridItemSpan(1) else GridItemSpan(maxLineSpan) },
            contentType = { cells[it]::class },
        ) { i ->
            when (val cell = cells[i]) {
                is Cell.Band -> cell.content()
                is Cell.Tile -> if (tiles != null) {
                    TouchPoster(
                        cell.game, tiles, cell.caption,
                        takeFocus = refocus == cell.game.id, onFocused = onRefocused,
                    )
                }
            }
        }
    }
}

/** The grid's margin, which a row scrolls across to the screen's edge. */
private val SHELF_MARGIN = 16.dp

/** A row's poster width: the grid's column minimum, so a row's posters match the grid's. */
private val STRIP_TILE = 130.dp

/** What a tile says under its title for the sort: when it was played, or for how long. */
private fun sortCaption(sort: LibrarySortKey, game: GameEntry, res: android.content.res.Resources): String? =
    when (sort) {
        // Blank, not null, for a title with nothing recorded: its tile keeps the line.
        LibrarySortKey.RECENT -> PlayStatsText.lastPlayed(res, game.stats).orEmpty()
        LibrarySortKey.PLAYTIME -> PlayStatsText.playTime(res, game.stats).orEmpty()
        else -> null
    }

/** A group's heading: the ungrouped bucket is Games, the nameless one Other. */
private fun LibraryGroupKey.label(res: android.content.res.Resources): String = when (this) {
    LibraryGroupKey.Launchers -> res.getString(R.string.lib_section_launchers)
    is LibraryGroupKey.Store -> name
    is LibraryGroupKey.Platform -> when (name) {
        LibraryCollation.ALL -> res.getString(R.string.lib_section_games)
        LibraryCollation.OTHER_PLATFORM -> res.getString(R.string.lib_group_other)
        else -> name
    }
}

@Composable
private fun SectionHeading(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        letterSpacing = 0.sp,
        modifier = Modifier.padding(top = 4.dp),
    )
}

/** A horizontal row that scrolls across the grid's margins to the screen's edges. */
@Composable
internal fun Strip(content: androidx.compose.foundation.lazy.LazyListScope.() -> Unit) {
    LazyRow(
        contentPadding = PaddingValues(horizontal = SHELF_MARGIN),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        modifier = Modifier.bleed(SHELF_MARGIN),
        content = content,
    )
}

/** Lay out [by] wider on each side than the parent offers: across its padding. */
private fun Modifier.bleed(by: Dp) = layout { measurable, constraints ->
    val extra = by.roundToPx() * 2
    val placeable = measurable.measure(
        constraints.copy(minWidth = constraints.maxWidth + extra, maxWidth = constraints.maxWidth + extra),
    )
    layout(constraints.maxWidth, placeable.height) { placeable.place(-by.roundToPx(), 0) }
}

/** A host's desktop: its mark and name, a presence dot, and what a tap does. */
@Composable
private fun DesktopCard(tile: DesktopTile, onClick: () -> Unit) {
    val shape = RoundedCornerShape(14.dp)
    Surface(
        shape = shape,
        color = MaterialTheme.colorScheme.surfaceContainerHigh,
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
        modifier = Modifier.size(width = 190.dp, height = 108.dp).clip(shape).clickable(onClick = onClick),
    ) {
        Column(Modifier.padding(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    resolveOsIcon(tile.host.os) ?: Icons.Filled.DesktopWindows,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.size(22.dp),
                )
                Spacer(Modifier.weight(1f))
                Box(
                    Modifier.size(8.dp).clip(CircleShape).background(
                        if (tile.online) PRESENCE_ONLINE else MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.4f),
                    ),
                )
            }
            Spacer(Modifier.weight(1f))
            Text(
                tile.host.name,
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.SemiBold,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Text(
                tile.playing?.let { stringResource(R.string.lib_resume_title, it) } ?: stringResource(R.string.lib_desktop),
                style = MaterialTheme.typography.bodySmall,
                color = if (tile.playing != null) PRESENCE_ONLINE else MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

/**
 * One tile: 2:3 poster, store badge, title, and [caption] when the sort or the row has one. A tap
 * launches; a long press opens the title's acts. A hidden tile is dimmed, and its menu only shows
 * it again.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TouchPoster(
    game: GameEntry,
    tiles: TileContext,
    caption: String?,
    modifier: Modifier = Modifier,
    /** Take focus now: the title beside this one just left. */
    takeFocus: Boolean = false,
    onFocused: () -> Unit = {},
) {
    var menu by remember { mutableStateOf(false) }
    val focus = remember { FocusRequester() }
    LaunchedEffect(takeFocus) {
        if (!takeFocus) return@LaunchedEffect
        runCatching { focus.requestFocus() }
        onFocused()
    }
    val context = LocalContext.current
    val up = tiles.running[game.id]
    val download = tiles.downloads[game.id]
    val files = InstallAction.forTitle(game.install, download, tiles.grants)
    val badge = TileBadge.forTitle(game.install, download)
    val store = game.storeLabel.asString()
    val storeDescription = stringResource(
        if (game.isLauncher) R.string.lib_store_opens_a11y else R.string.lib_store_from_a11y,
        store,
    )
    val shape = MaterialTheme.shapes.medium
    val hidden = tiles.hidden
    val hide = tiles.hideLabel
    // This copy's own frame: a title also sits in a row, and the launch flies out of the one pressed.
    var frame by remember { mutableStateOf(Rect.Zero) }
    val launch = {
        TileFrames.record(game.id, frame)
        tiles.onLaunch(game)
    }
    Box(modifier) {
        Column(
            Modifier
                .focusRequester(focus)
                .combinedClickable(
                    onClickLabel = stringResource(
                        if (hidden) R.string.lib_title_more_a11y else R.string.lib_title_launch_a11y,
                        game.title,
                    ),
                    onLongClickLabel = stringResource(R.string.lib_title_more_a11y, game.title),
                    onClick = { if (hidden) menu = true else launch() },
                    onLongClick = { menu = true },
                )
                .alpha(if (hidden) 0.45f else 1f),
        ) {
            Box(
                Modifier
                    .fillMaxWidth()
                    .aspectRatio(2f / 3f)
                    .onGloballyPositioned { frame = it.boundsInWindow() }
                    .clip(shape)
                    .background(MaterialTheme.colorScheme.surfaceVariant),
                contentAlignment = Alignment.Center,
            ) {
                TouchPosterArt(game, tiles.loader)
                // Opposite corner from the store chip, so the two never meet on a narrow tile.
                if (up != null) {
                    Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.TopEnd) { RunningBadge() }
                } else if (badge != null) {
                    Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.TopEnd) {
                        PosterBadge(badge)
                    }
                }
                game.overrides?.summaryWords(context.resources)?.let { words ->
                    Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.BottomStart) {
                        PosterBadge(TileBadge("host", say(R.string.lib_host_override, words)))
                    }
                }
                Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.TopStart) {
                    Text(
                        store,
                        style = MaterialTheme.typography.labelSmall,
                        // A launcher's badge is brand-filled; a game's sits on a dark wash over
                        // its own art, where the theme's ink would be a coin toss.
                        color = if (game.isLauncher) MaterialTheme.colorScheme.onPrimary else Color.White,
                        modifier = Modifier
                            .semantics { contentDescription = storeDescription }
                            .clip(PillShape)
                            .background(
                                if (game.isLauncher) MaterialTheme.colorScheme.primary else Color.Black.copy(alpha = 0.5f),
                            )
                            .padding(horizontal = 8.dp, vertical = 3.dp),
                    )
                }
            }
            Text(
                game.title,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                // Two lines held for every title, so a row's tiles stand the same height.
                minLines = 2,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(top = 6.dp),
            )
            if (caption != null) {
                Text(
                    caption,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.7f),
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            @Composable
            fun row(label: String, onClick: () -> Unit) = DropdownMenuItem(
                text = { Text(label) },
                onClick = {
                    menu = false
                    onClick()
                },
            )
            if (hidden) {
                hide?.let { row(it) { tiles.onHide(game) } }
                return@DropdownMenu
            }
            // Play leads: a long press has no hint that a tap plays.
            row(stringResource(if (up != null) R.string.lib_resume else R.string.common_play), launch)
            tiles.onFavorite?.let { toggle ->
                val marked = game.id in tiles.favorites
                row(stringResource(if (marked) R.string.lib_favorite_remove else R.string.lib_favorite_add)) {
                    toggle(game)
                }
            }
            tiles.onDetails?.let { open -> row(stringResource(R.string.lib_title_details)) { open(game) } }
            row(stringResource(R.string.hostcard_copy_link)) { tiles.onLink(game, LinkAction.Copy) }
            if (files != null) row(files.label(game.install).asString()) { tiles.onFiles(game, files) }
            if (up?.endable == true) row(stringResource(R.string.lib_end_game)) { tiles.onEndGame(game) }
            hide?.let { row(it) { tiles.onHide(game) } }
        }
    }
}

/**
 * A mark on a tile: its files (`42 %` downloading, paused, failed, the size to download) or what
 * the host sets for its stream (`host`). One line; a narrow tile cuts it short.
 */
@Composable
private fun PosterBadge(badge: TileBadge) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .clip(PillShape)
            .background(Color.Black.copy(alpha = 0.72f))
            .padding(horizontal = 6.dp, vertical = 3.dp),
    ) {
        Icon(
            when (badge.icon) {
                "pause" -> Icons.Filled.Pause
                "alert" -> Icons.Filled.Warning
                "host" -> Icons.Filled.Tune
                else -> Icons.Filled.Download
            },
            contentDescription = null,
            tint = Color.White,
            modifier = Modifier.size(12.dp),
        )
        Text(
            badge.text.asString(),
            style = MaterialTheme.typography.labelSmall,
            fontWeight = FontWeight.SemiBold,
            color = Color.White,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(start = 4.dp),
        )
    }
}

/** A title's cover, else its launcher's brand mark, else its name. */
@Composable
internal fun TouchPosterArt(game: GameEntry, loader: ImageLoader) {
    PosterArt(game, loader) {
        // A launcher ships no poster, so its brand mark is the poster: its name says "opens
        // Steam", where a title would read as a cover that failed to load.
        val mark = launcherIcon(game.iconToken)
        if (mark != null) {
            Icon(
                imageVector = mark,
                contentDescription = game.title,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.fillMaxSize(0.45f),
            )
        } else {
            Text(
                if (game.isLauncher) game.storeLabel.asString() else game.title,
                style = MaterialTheme.typography.titleSmall,
                fontWeight = FontWeight.SemiBold,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(12.dp),
            )
        }
    }
}

/**
 * "Already up on the host": a tap resumes. A glyph, since a tile goes down to 130 dp and carries
 * the store chip in the opposite corner. A fixed green, not the theme's primary: the host reports
 * this state, and the launcher chip owns the brand fill one corner away.
 */
@Composable
private fun RunningBadge() {
    val description = stringResource(R.string.lib_badge_running_a11y)
    Box(
        modifier = Modifier
            .semantics { contentDescription = description }
            .clip(PillShape)
            .background(RESUME_GREEN)
            .padding(horizontal = 6.dp, vertical = 3.dp),
    ) {
        Icon(Icons.Filled.PlayArrow, contentDescription = null, tint = RESUME_INK, modifier = Modifier.size(12.dp))
    }
}

/** The host presence green the console palette uses for "this machine is up". */
private val RESUME_GREEN = Color(0xFF33D64A)

/** Near-black on that green: a fixed pair, so neither theme nor palette washes it out. */
private val RESUME_INK = Color(0xFF0A1A0D)
