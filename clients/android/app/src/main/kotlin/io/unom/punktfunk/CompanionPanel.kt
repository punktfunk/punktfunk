package io.unom.punktfunk

import android.content.Context
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Apps
import androidx.compose.material.icons.filled.Insights
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationRail
import androidx.compose.material3.NavigationRailItem
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.unom.punktfunk.components.SectionLabel

/*
 * The companion panel (design/android-dual-screen.md §6): what the lower screen of a dual-screen
 * handheld shows while the picture has the other one. A rail on the physical left flips between
 * pages — the stats, the quick actions, a trackpad, the virtual controller — under a header that
 * says where this stream is. One composable for both shapes: the lower half of a hinge, and a
 * second display's Presentation.
 */

internal enum class CompanionPage(val label: String, val icon: ImageVector) {
    STATS("Stats", Icons.Filled.Insights),
    ACTIONS("Actions", Icons.Filled.Apps),
    TRACKPAD("Trackpad", Icons.Filled.TouchApp),
    PAD("Controller", Icons.Filled.SportsEsports),
}

/** The pages a session offers: the trackpad needs the pointer grant, the controller a pad the host takes. */
internal fun companionPages(pointer: Boolean, pad: Boolean): List<CompanionPage> =
    CompanionPage.entries.filter { (it != CompanionPage.TRACKPAD || pointer) && (it != CompanionPage.PAD || pad) }

/**
 * The page the player last picked, and which screen of each pair holds the picture, kept across
 * streams. The controller page is never kept: showing it connects a pad, and a stream must not
 * connect one on its own.
 */
internal object CompanionMemory {
    private const val PREFS = "punktfunk_companion"
    private const val PAGE = "page"

    private fun prefs(context: Context) =
        context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun page(context: Context): CompanionPage {
        val name = prefs(context).getString(PAGE, null)
        return CompanionPage.entries.firstOrNull { it.name == name } ?: CompanionPage.STATS
    }

    fun keep(context: Context, page: CompanionPage) {
        if (page != CompanionPage.PAD) prefs(context).edit().putString(PAGE, page.name).apply()
    }

    /** True when the player moved the picture onto the second screen [screen] names. */
    fun swapped(context: Context, screen: String): Boolean = prefs(context).getBoolean("swap:$screen", false)

    fun keepSwap(context: Context, screen: String, swapped: Boolean) {
        prefs(context).edit().putBoolean("swap:$screen", swapped).apply()
    }
}

/** The panel's header: the host and the title on one line, the mode on the next. */
internal data class PanelHeader(val title: String, val detail: String = "")

/**
 * The actions page in three groups. Session: what this stream can do, in the sheet's order.
 * Host: the host's actions and the shortcuts. Leave: the ways out. Statistics has a page of its
 * own, and Send text needs a keyboard that the second display's unfocusable window cannot take.
 */
private val SESSION_SLOTS = listOf(
    SlotId.Guide, SlotId.Qam, SlotId.Keyboard, SlotId.TouchMode, SlotId.PadMouse, SlotId.Pad,
    SlotId.SwapScreens, SlotId.Mic, SlotId.StreamMute,
)
private val EXIT_SLOTS = listOf(SlotId.DisconnectLinger, SlotId.EndStream)

/** From this width a page lays out in two columns: the add-on's 768 dp, a fold half; never a Thor's 472. */
private val WIDE: Dp = 560.dp

/** The content's side padding: the rail carries its own on the left. */
private val PAGE_PADDING = PaddingValues(start = 4.dp, end = 16.dp)

/**
 * The panel. [page] is one of [pages]; the controller's rail item connects the virtual pad when
 * none is up, since picking it is the ask. [trackpad] is the gesture handler its page runs, [pad]
 * the virtual controller at its page's size. Black, not the theme's surface: the panel is an OLED
 * under a game.
 */
@Composable
internal fun CompanionPanel(
    pages: List<CompanionPage>,
    page: CompanionPage,
    onPage: (CompanionPage) -> Unit,
    header: PanelHeader,
    stats: List<HudLine>,
    tier: StatsVerbosity,
    onTier: (StatsVerbosity) -> Unit,
    cfg: OverlayConfig,
    actions: RingActions,
    haptics: ConsoleHaptics,
    trackpad: suspend PointerInputScope.() -> Unit,
    pad: @Composable (IntSize) -> Unit,
    modifier: Modifier = Modifier,
) {
    Row(modifier.fillMaxSize().background(Color.Black)) {
        NavigationRail(containerColor = Color.Transparent) {
            Spacer(Modifier.height(8.dp))
            for (p in pages) {
                NavigationRailItem(
                    selected = p == page,
                    onClick = {
                        haptics.tick()
                        if (p == CompanionPage.PAD && !actions.padShown()) actions.togglePad()
                        onPage(p)
                    },
                    icon = { Icon(p.icon, contentDescription = null) },
                    label = { Text(p.label) },
                )
            }
        }
        Column(Modifier.weight(1f).fillMaxHeight()) {
            Header(header)
            Box(Modifier.fillMaxWidth().weight(1f)) {
                when (page) {
                    CompanionPage.STATS -> StatsPage(stats, tier, onTier)
                    CompanionPage.ACTIONS -> ActionsPage(cfg, actions, haptics)
                    CompanionPage.TRACKPAD -> TrackpadPage(trackpad)
                    CompanionPage.PAD -> PadPage(actions, pad)
                }
            }
        }
    }
}

@Composable
private fun Header(h: PanelHeader) {
    Column(Modifier.fillMaxWidth().padding(PAGE_PADDING).padding(top = 12.dp, bottom = 8.dp)) {
        Text(
            h.title, style = MaterialTheme.typography.titleMedium, color = MaterialTheme.colorScheme.onSurface,
            maxLines = 1, overflow = TextOverflow.Ellipsis,
        )
        if (h.detail.isNotEmpty()) {
            Text(
                h.detail, style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

/** The HUD's lines at arm's length: the tier to pick on top, larger type below, two columns when wide. Never Off here. */
@Composable
private fun StatsPage(lines: List<HudLine>, tier: StatsVerbosity, onTier: (StatsVerbosity) -> Unit) {
    BoxWithConstraints(Modifier.fillMaxSize()) {
        val columns = if (maxWidth >= WIDE && lines.size > 3) 2 else 1
        Column(Modifier.fillMaxSize().padding(PAGE_PADDING)) {
            val tiers = listOf(StatsVerbosity.COMPACT, StatsVerbosity.NORMAL, StatsVerbosity.DETAILED)
            SingleChoiceSegmentedButtonRow {
                tiers.forEachIndexed { i, t ->
                    SegmentedButton(
                        selected = t == tier,
                        onClick = { onTier(t) },
                        shape = SegmentedButtonDefaults.itemShape(index = i, count = tiers.size),
                    ) { Text(t.label) }
                }
            }
            Spacer(Modifier.height(12.dp))
            if (lines.isEmpty()) {
                Text(
                    "The numbers arrive within a second.",
                    style = MaterialTheme.typography.bodyLarge, color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            } else {
                Row(
                    Modifier.verticalScroll(rememberScrollState()),
                    horizontalArrangement = Arrangement.spacedBy(24.dp),
                ) {
                    for (column in lines.chunked((lines.size + columns - 1) / columns)) {
                        Column(Modifier.weight(1f)) {
                            for (line in column) {
                                Text(
                                    line.text, color = roleColor(line.role), fontFamily = FontFamily.Monospace,
                                    fontSize = 14.sp, lineHeight = 20.sp, letterSpacing = 0.sp,
                                )
                            }
                            Spacer(Modifier.height(16.dp))
                        }
                    }
                }
            }
        }
    }
}

/** The ring's catalogue as tiles under group headers, fired through the ring's own rules: two presses to leave. */
@Composable
private fun ActionsPage(cfg: OverlayConfig, actions: RingActions, haptics: ConsoleHaptics) {
    val state = remember { RingState() }
    ExpireRingHint(state)
    // End game only while this device's launch is on the stream: a dimmed tile says nothing.
    val endGame = listOfNotNull(SlotId.EndGame.takeIf { actions.streamedGame() != null })
    val host = actions.hostActions().map { SlotId.Host(it.id) } + cfg.shortcuts.map { SlotId.Shortcut(it.id) }
    val groups = listOf("Session" to SESSION_SLOTS, "Host" to host, "Leave" to endGame + EXIT_SLOTS)
        .filter { it.second.isNotEmpty() }
    Box(Modifier.fillMaxSize()) {
        LazyVerticalGrid(
            GridCells.Adaptive(104.dp),
            contentPadding = PaddingValues(start = 4.dp, end = 16.dp, bottom = 56.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            for ((name, slots) in groups) {
                item(key = "group:$name", span = { GridItemSpan(maxLineSpan) }) {
                    Box(Modifier.padding(top = 8.dp)) { SectionLabel(name) }
                }
                items(slots, key = { it.id }) { slot ->
                    val s = spec(slot, cfg, actions)
                    ActionTile(s, armed = state.armed == s.id) { fireSlot(s, slot, state, cfg, actions, haptics) {} }
                }
            }
        }
        state.hint?.let {
            Text(
                it,
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = 12.dp)
                    .background(MaterialTheme.colorScheme.inverseSurface, MaterialTheme.shapes.small)
                    .padding(horizontal = 14.dp, vertical = 8.dp),
                color = MaterialTheme.colorScheme.inverseOnSurface,
                style = MaterialTheme.typography.bodyLarge,
            )
        }
    }
}

@Composable
private fun ActionTile(spec: SlotSpec, armed: Boolean, onTap: () -> Unit) {
    val scheme = MaterialTheme.colorScheme
    val (fill, tint) = when {
        armed -> scheme.errorContainer to scheme.onErrorContainer
        !spec.enabled -> scheme.surfaceVariant.copy(alpha = 0.45f) to scheme.onSurface.copy(alpha = 0.38f)
        else -> scheme.surfaceVariant to scheme.onSurfaceVariant
    }
    Surface(
        onClick = onTap,
        shape = MaterialTheme.shapes.large,
        color = fill,
        contentColor = tint,
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 88.dp)
            .semantics {
                stateDescription = when {
                    armed -> "armed — press again"
                    !spec.enabled -> spec.reason
                    else -> spec.state
                }
            },
    ) {
        Column(
            Modifier.padding(10.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.Center,
        ) {
            when {
                spec.chip != null -> ChordKeycap(spec.chip, tint, 48.dp)
                spec.icon != null -> Icon(spec.icon, contentDescription = null, tint = tint, modifier = Modifier.size(28.dp))
            }
            Spacer(Modifier.height(6.dp))
            Text(
                spec.label, color = tint, style = MaterialTheme.typography.labelLarge, textAlign = TextAlign.Center,
                maxLines = 3, overflow = TextOverflow.Ellipsis,
            )
            if (spec.state.isNotEmpty()) {
                Text(spec.state, color = tint.copy(alpha = 0.7f), style = MaterialTheme.typography.labelMedium)
            }
        }
    }
}

/** The whole page is a touchpad for the host pointer, whatever touch mode the picture uses. */
@Composable
private fun TrackpadPage(input: suspend PointerInputScope.() -> Unit) {
    Surface(
        shape = MaterialTheme.shapes.large,
        color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.6f),
        modifier = Modifier
            .fillMaxSize()
            .padding(PAGE_PADDING)
            .padding(bottom = 16.dp)
            .pointerInput(Unit, input),
    ) {
        Box(contentAlignment = Alignment.Center) {
            Text(
                "Tap to click · two fingers scroll · two-finger tap right-clicks",
                style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant,
                textAlign = TextAlign.Center, modifier = Modifier.padding(24.dp),
            )
        }
    }
}

/** The virtual controller at this page's size, or the way to bring it back once it was put away. */
@Composable
private fun PadPage(actions: RingActions, pad: @Composable (IntSize) -> Unit) {
    var size by remember { mutableStateOf(IntSize.Zero) }
    Box(Modifier.fillMaxSize().onSizeChanged { size = it }, contentAlignment = Alignment.Center) {
        if (actions.padShown()) {
            pad(size)
        } else {
            FilledTonalButton(onClick = { actions.togglePad() }) {
                Icon(Icons.Filled.SportsEsports, contentDescription = null)
                Spacer(Modifier.width(8.dp))
                Text("Show the controller")
            }
        }
    }
}
