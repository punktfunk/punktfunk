package io.unom.punktfunk.screenshots

import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import io.unom.punktfunk.ConnectGrid
import io.unom.punktfunk.DesktopTile
import io.unom.punktfunk.HostActions
import io.unom.punktfunk.HostDetailScreen
import io.unom.punktfunk.HostPageActs
import io.unom.punktfunk.LibState
import io.unom.punktfunk.StreamPreset
import io.unom.punktfunk.TitleDetailSheet
import io.unom.punktfunk.TouchLibrary
import io.unom.punktfunk.components.HostLine
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.library.GameStats
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost

// The touch shell's screens over mock data: the real composables, since each takes what it draws
// and hands back what was pressed.

private const val FP_LIVING = "a4b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9e4f5"
private const val FP_OFFICE = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0"

private val GAME = StreamPreset("p-game", "Game", accent = "#FF8A4C")
private val WORK = StreamPreset("p-work", "Work", accent = "#4CC2FF")

private val LIVING = KnownHost(
    "192.168.1.42", 9777, "Living Room PC", FP_LIVING, paired = true,
    mac = listOf("aa:bb:cc:dd:ee:ff"), os = "linux/fedora/bazzite", id = "living",
    presetId = GAME.id, pinnedPresetIds = listOf(WORK.id),
)
private val OFFICE = KnownHost(
    "192.168.1.50", 9777, "Office", FP_OFFICE, paired = true,
    mac = listOf("11:22:33:44:55:66"), os = "windows", id = "office",
)
private val LAPTOP = KnownHost("192.168.1.77", 9777, "Laptop", "", paired = false, os = "apple", id = "laptop")

private val DISCOVERED = listOf(
    DiscoveredHost("studio", "studio-deck", "192.168.1.61", 9777, pairingRequired = true, os = "linux/steamos"),
    DiscoveredHost("htpc", "HTPC", "192.168.1.70", 9777, os = "linux/ubuntu"),
)

/** The Hosts tab: a host with a game up and a bound preset, its pinned card, a host a connect wakes. */
@Composable
internal fun HostsScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        ConnectGrid(
            savedHosts = listOf(LIVING, OFFICE, LAPTOP),
            discovered = DISCOVERED,
            discoveredUnsaved = DISCOVERED,
            reachable = setOf(LIVING.id, LAPTOP.id),
            presets = listOf(GAME, WORK),
            pinsFor = { kh -> listOf(WORK).filter { it.id in kh.pinnedPresetIds } },
            connecting = false,
            notice = null,
            status = null,
            lnpGranted = true,
            onAskLocalNetwork = {},
            onConnect = { _, _ -> },
            onConnectDiscovered = {},
            onDetails = {},
            onWake = {},
            onSwitchProfile = {},
            screens = emptyMap(),
            nowPlaying = mapOf(FP_LIVING to "Starfall Vale"),
            onCopyLink = { _, _ -> },
            onTogglePin = { _, _ -> },
            onBrowseLibrary = { _, _ -> },
            defaultHostId = LIVING.id,
            autoWake = true,
            onRescan = {},
            onAddHost = {},
        )
    }
}

/** The host page of a paired host that answers, with presets and the power acts it grants. */
@Composable
internal fun HostPageScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        HostDetailScreen(
            host = LIVING,
            os = LIVING.os,
            status = HostLine.Playing("Starfall Vale"),
            notice = null,
            failure = null,
            isDefault = true,
            presets = listOf(GAME, WORK),
            power = listOf(
                HostActions.Action("power.sleep", "Sleep", danger = false, available = true, unavailableReason = ""),
                HostActions.Action("power.reboot", "Restart", danger = true, available = true, unavailableReason = ""),
            ),
            screens = null,
            busy = false,
            acts = HostPageActs(
                back = {}, connect = {}, browse = {}, switchProfile = null, bind = {}, togglePin = {},
                pickScreen = {}, edit = {}, setClipboard = {}, speedTest = {}, copyLink = {}, pair = {},
                unpair = {}, setDefault = {}, wake = null, power = {}, sendLogs = {}, remove = {},
            ),
        )
    }
}

/** The mock shelf with what the host records about playing it: three played, one of them up. */
private fun playedGames() = shotGames().map { game ->
    val hour = 3_600_000L
    val now = System.currentTimeMillis()
    when (game.id) {
        "steam:starfall" -> game.copy(stats = GameStats(now - 20 * 60_000, 14 * hour, hour, 12))
        "custom:aurora" -> game.copy(stats = GameStats(now - 3 * hour, 2 * hour, hour, 4))
        "gog:ember" -> game.copy(stats = GameStats(now - 50 * hour, 31 * hour, 2 * hour, 20))
        else -> game
    }
}

/** The Library tab: every section with something to show. */
@Composable
internal fun TouchLibraryScene() {
    val context = LocalContext.current
    val loader = remember { shotLibraryLoader(context) }
    val games = remember { playedGames() }
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        TouchLibrary(
            state = LibState.Ready(
                games, loader, ClientIdentity("", ""),
                running = mapOf("steam:starfall" to RunningGame("steam:starfall", "Starfall Vale", "running")),
            ),
            launching = false,
            onReload = {},
            onLaunch = { _, _ -> },
            onLink = { _, _ -> },
            favorites = setOf("heroic:neon", "steam:tide"),
            desktops = listOf(DesktopTile(LIVING, online = true, playing = "Starfall Vale"), DesktopTile(OFFICE)),
        )
    }
}

/** A played favorite's details. */
@Composable
internal fun TitleDetailScene() {
    val context = LocalContext.current
    val loader = remember { shotLibraryLoader(context) }
    val game = remember {
        playedGames().first { it.id == "steam:starfall" }
            .copy(developer = "Northlight", releaseYear = 2024, genres = listOf("Adventure", "Exploration"))
    }
    TouchLibraryScene()
    TitleDetailSheet(
        game = game, loader = loader, running = true, favorite = true,
        onPlay = {}, onFavorite = {}, onLink = {}, onEndGame = null, onDismiss = {},
    )
}
