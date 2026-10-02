package io.unom.punktfunk.screenshots

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.BlendMode
import androidx.compose.foundation.Image
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalView
import kotlin.math.roundToInt
import android.graphics.Canvas
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RadialGradient
import android.graphics.Shader
import android.graphics.Typeface
import android.graphics.drawable.BitmapDrawable
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.BatteryFull
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.SignalCellular4Bar
import androidx.compose.material.icons.filled.Wifi
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.BrandDark
import io.unom.punktfunk.ConnectModal
import io.unom.punktfunk.ConnectPhase
import io.unom.punktfunk.OsdScaled
import coil.ImageLoader
import coil.test.FakeImageLoaderEngine
import io.unom.punktfunk.AddHostSheet
import io.unom.punktfunk.ControllersScreen
import io.unom.punktfunk.TouchGrid
import io.unom.punktfunk.PadInfo
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.library.Artwork
import io.unom.punktfunk.kit.library.GameEntry
import androidx.compose.ui.platform.LocalConfiguration
import io.unom.punktfunk.Settings
import io.unom.punktfunk.TouchMode
import io.unom.punktfunk.SettingsCategory
import io.unom.punktfunk.SettingsScreen
import io.unom.punktfunk.HudLine
import io.unom.punktfunk.StatsOverlay
import io.unom.punktfunk.StatsVerbosity
import io.unom.punktfunk.StreamStartBanner
import io.unom.punktfunk.PresetEditorFields
import io.unom.punktfunk.PresetStore
import io.unom.punktfunk.SettingsOverlay
import io.unom.punktfunk.SpeedTestPrompt
import io.unom.punktfunk.SpeedTestPhase
import io.unom.punktfunk.SpeedTestTarget
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.HostMenuItem
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.newPreset
import io.unom.punktfunk.models.HostStatus

// The CI screenshot scenes: the REAL app composables, fed embedded mock state, under the forced
// brand palette (Material You has no wallpaper to seed from on the JVM). The stream-video surface
// and ConnectScreen/App are intentionally absent — they require the live JNI core / a session.

/** Forces the deterministic punktfunk brand scheme (see Theme.kt) instead of dynamic colour. */
@Composable
internal fun ShotTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = BrandDark, content = content)
}

/**
 * Robolectric has no system UI, so every capture was missing the status bar and the content sat
 * where the bar belongs — on the Pixel render the app title collided with the camera punch-hole.
 * This frame draws a plausible bar (time left, radios right, the CENTRE left empty for the hole)
 * and pushes the scene below it, the same geometry real insets produce. The height mirrors a
 * Pixel's tall bar as measured off a real 1344×2992 capture (~145 px ≈ 40 dp).
 */
@Composable
internal fun ShotStatusFrame(content: @Composable () -> Unit) {
    Column(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background)) {
        Row(
            Modifier.fillMaxWidth().height(40.dp).padding(horizontal = 28.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                "21:47",
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
            )
            Row(
                horizontalArrangement = Arrangement.spacedBy(5.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(
                    Icons.Filled.Wifi, contentDescription = null,
                    tint = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
                    modifier = Modifier.size(15.dp),
                )
                Icon(
                    Icons.Filled.SignalCellular4Bar, contentDescription = null,
                    tint = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
                    modifier = Modifier.size(14.dp),
                )
                Icon(
                    Icons.Filled.BatteryFull, contentDescription = null,
                    tint = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
                    modifier = Modifier.size(16.dp),
                )
            }
        }
        Box(Modifier.weight(1f).fillMaxWidth()) { content() }
    }
}

private data class MockHost(
    val name: String,
    val address: String,
    val status: HostStatus,
    val preset: String? = null,
    val pin: String? = null,
    val accent: Color? = null,
    val online: Boolean = false,
)

// Ordered so an UNCHIPPED card sits beside a CHIPPED one in the same grid row, and a long trust
// label ("Trust on first use") beside a short one ("Paired"). Both are what used to make cards in a
// row step up and down — the grid sizes a row to its tallest item and doesn't stretch the rest — so
// this arrangement is the regression net for it.
private val SAVED = listOf(
    MockHost("Office", "192.168.1.50:9777", HostStatus.TOFU),
    MockHost(
        "Living Room PC", "192.168.1.42:9777", HostStatus.PAIRED,
        preset = "Game", pin = "Work", accent = Color(0xFFFF8A4C), online = true,
    ),
)
private val DISCOVERED = listOf(
    // Discovered ⇒ advertising right now, so both are online.
    MockHost("studio-deck", "192.168.1.61:9777", HostStatus.PAIRING, online = true),
    MockHost("HTPC", "192.168.1.70:9777", HostStatus.TOFU, online = true),
)

/** The connect screen's host grid, reconstructed from the real HostCard/SectionLabel components. */
@Composable
internal fun HostsScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 160.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(16.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            item(span = { GridItemSpan(maxLineSpan) }) {
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Spacer(Modifier.height(8.dp))
                    Text("Punktfunk", style = MaterialTheme.typography.headlineLarge)
                    Text(
                        "stream a remote desktop",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(24.dp))
                }
            }
            item(span = { GridItemSpan(maxLineSpan) }) { SectionLabel("Saved hosts") }
            // A pinned card is its OWN grid cell right after its host — the same flat list the
            // connect screen builds, not a second card crammed into the host's cell.
            SAVED.forEach { h ->
                item {
                    HostCard(
                        h.name, h.address, h.status, online = h.online, enabled = true,
                        onConnect = {}, onForget = {}, onEdit = {},
                        // The bound preset is a quiet chip: the card says what a tap will do.
                        presetLabel = h.preset,
                        accent = h.accent,
                        menuItems = listOf(
                            HostMenuItem("Connect with: Default settings", startsSection = true) {},
                            HostMenuItem("Connect with: Game") {},
                        ),
                        // One card in this section has a chip, so every card reserves its space —
                        // the shot is here to catch a row that steps.
                        reservePresetSlot = true,
                    )
                }
                if (h.pin != null) {
                    item {
                        HostCard(
                            h.name, h.address, h.status, online = h.online, enabled = true,
                            onConnect = {}, onForget = null,
                            presetLabel = h.pin, presetProminent = true, accent = h.accent,
                            menuItems = listOf(HostMenuItem("Unpin card", startsSection = true) {}),
                            reservePresetSlot = true,
                        )
                    }
                }
            }
            item(span = { GridItemSpan(maxLineSpan) }) {
                Spacer(Modifier.height(12.dp))
                SectionLabel("Discovered on the network")
            }
            items(DISCOVERED) { h ->
                HostCard(
                    h.name, h.address, h.status, online = h.online,
                    enabled = true, onConnect = {}, onForget = null,
                )
            }
        }
    }
}

/** A representative non-default settings state, shared by the settings scenes. */
private val SHOT_SETTINGS = Settings(
    width = 1920,
    height = 1080,
    hz = 120,
    bitrateKbps = 50_000,
    compositor = 1,
    gamepad = 2,
    micEnabled = true,
    statsVerbosity = StatsVerbosity.DETAILED,
    touchMode = TouchMode.TRACKPAD,
)

/**
 * The real SettingsScreen at its root — the shared category map (General / Display / Input /
 * Audio / Controllers / About) every client now presents.
 */
@Composable
internal fun SettingsScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        SettingsScreen(initial = SHOT_SETTINGS, onChange = {}, onBack = {})
    }
}

/**
 * One category page, seeded through `initialCategory` — the sub-section headers, the
 * caption-under-control fields and the "applies from the next session" footer only exist inside a
 * category, so the root shot alone can't regress-catch them. Display is the richest page.
 */
@Composable
internal fun SettingsCategoryScene(category: SettingsCategory) {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        SettingsScreen(
            initial = SHOT_SETTINGS,
            onChange = {},
            onBack = {},
            initialCategory = category,
        )
    }
}

/**
 * The same settings surface in a PRESET's scope: the scope chips with "Game" selected, only
 * presetable rows, every row showing the effective value, and the overridden ones carrying their
 * marker and reset. One settings UI, two layers — this shot is what proves it stayed one.
 */
@Composable
internal fun SettingsPresetScene() {
    val store = PresetStore(LocalContext.current)
    val preset = remember {
        val p = newPreset("Game").copy(
            accent = "#FF8A4C",
            // A representative mix: a resolution and refresh the preset pins, and a codec — the
            // rest of the page keeps following the defaults, visibly unmarked.
            overrides = SettingsOverlay(width = 3840, height = 2160, hz = 120, codec = "h264"),
        )
        store.save(p)
        store.save(newPreset("Work"))
        p
    }
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        SettingsScreen(
            initial = SHOT_SETTINGS,
            onChange = {},
            onBack = {},
            initialCategory = SettingsCategory.Display,
            initialPresetId = preset.id,
        )
    }
}

/**
 * The speed test's result, in its most interesting shape: a host bound to a preset that INHERITS
 * bitrate, so both layers are defensible and both buttons are offered. The note under the numbers
 * is what stops "Apply" from being a write in an unknown direction.
 */
@Composable
internal fun SpeedTestScene() {
    SpeedTestPrompt(
        hostName = "Living Room PC",
        target = SpeedTestTarget.Ask(newPreset("Game")),
        phase = SpeedTestPhase.Done(throughputKbps = 412_000, lossPct = 0.3, recommendedKbps = 288_400),
        onApply = {},
        onDismiss = {},
    )
}

/**
 * Creating a preset. Small, but it is the first thing a user meets when they reach for this
 * feature — and dialogs only get a shot each because a layout slip inside one is invisible from
 * every other scene (this one shipped with the field and its caption touching).
 */
@Composable
internal fun NewPresetScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        Column(Modifier.padding(24.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Text("New preset", style = MaterialTheme.typography.headlineSmall)
            // The dialog's own body, not a rebuild of it — the layout under test is the real one.
            PresetEditorFields(
                name = "Travel",
                accent = "#60A5FA",
                duplicate = false,
                creating = true,
                onNameChange = {},
                onAccentChange = {},
            )
            Text("Duplicate name", style = MaterialTheme.typography.headlineSmall)
            PresetEditorFields(
                name = "Game",
                accent = "#FF8A4C",
                duplicate = true,
                creating = false,
                onNameChange = {},
                onAccentChange = {},
            )
        }
    }
}

/** The real TOFU AlertDialog (mirrors ConnectScreen's PendingTrust.Kind.TRUST_NEW), shown over the host grid. */
@Composable
internal fun TrustDialog() {
    AlertDialog(
        onDismissRequest = {},
        title = { Text("Trust this host?") },
        text = {
            Column {
                Text("First connection to 192.168.1.61:9777.")
                Text("Fingerprint 9f8e7d6c5b4a3928…")
                Text(
                    "This host allows trust-on-first-use, but that can't tell an impostor " +
                        "from the real host. Pairing with a PIN is stronger — it proves both sides.",
                )
            }
        },
        confirmButton = { TextButton({}) { Text("Trust (TOFU)") } },
        dismissButton = { TextButton({}) { Text("Pair with PIN…") } },
    )
}

/** The PIN-pairing AlertDialog (mirrors ConnectScreen's PendingTrust.Kind.PAIR). The live screen
 *  uses OutlinedTextFields, but a TextField inside a Dialog window never reaches idle under
 *  Robolectric (its focus/cursor machinery animates forever) — so the PIN is shown as a static
 *  display here, which also reads better in a marketing shot. */
@Composable
internal fun PairDialog() {
    AlertDialog(
        onDismissRequest = {},
        title = { Text("Pair with PIN") },
        text = {
            Column {
                Text("Enter the 4-digit PIN shown on the host.")
                Spacer(Modifier.height(16.dp))
                Surface(
                    color = MaterialTheme.colorScheme.surfaceVariant,
                    shape = MaterialTheme.shapes.medium,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(
                        "4  8  2  7",
                        style = MaterialTheme.typography.headlineMedium,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.fillMaxWidth().padding(vertical = 16.dp),
                    )
                }
                Spacer(Modifier.height(12.dp))
                Text(
                    "This device: Pixel 9 Pro",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = { TextButton({}) { Text("Pair") } },
        dismissButton = { TextButton({}) { Text("Cancel") } },
    )
}

/**
 * The live stats HUD (the real StatsOverlay) at the given [verbosity] tier, over a real captured
 * frame when `PUNKTFUNK_SHOT_HERO` names a PNG, else a synthetic gradient. The mode line is this
 * canvas's own pixel size and refresh rate, as a stream sized to the display would report.
 * [loss] false zeroes the lost, skipped and FEC counters, so a store shot has no counter line.
 */
@Composable
internal fun StreamScene(verbosity: StatsVerbosity = StatsVerbosity.DETAILED, loss: Boolean = true) {
    val config = LocalConfiguration.current
    val density = LocalDensity.current.density
    val w = (config.screenWidthDp * density).roundToInt()
    val h = (config.screenHeightDp * density).roundToInt()
    val hz = LocalView.current.display?.refreshRate?.roundToInt()?.takeIf { it > 0 } ?: 60
    // 921.4 Mb/s at 5120×1440@240: the bitrate scales with pixel rate.
    val mbps = w.toDouble() * h * hz * (921.4 / (5120.0 * 1440 * 240))
    val fps = hz * 238.0 / 240.0
    val (lost, skipped, fec) = if (loss) Triple(2.0, 1.0, 5.0) else Triple(0.0, 0.0, 0.0)
    val hero = remember {
        System.getenv("PUNKTFUNK_SHOT_HERO")?.takeIf { it.isNotEmpty() }
            ?.let {
                BitmapFactory.decodeFile(
                    it,
                    BitmapFactory.Options().apply { inPreferredConfig = Bitmap.Config.ARGB_8888 },
                )
            }?.asImageBitmap()
    }
    Box(
        Modifier
            .fillMaxSize()
            .background(
                Brush.linearGradient(listOf(Color(0xFF2A1E5C), Color(0xFF0E1B3D), Color(0xFF06122B))),
            ),
    ) {
        hero?.let { Image(it, null, Modifier.fillMaxSize(), contentScale = ContentScale.Crop) }
        // The Standard view (the default) as `punktfunk_core::hud` formats this window: a 10-bit
        // HDR HEVC feed on the ranked low-latency decoder, light loss (2 of 240 lost, 1 skipped)
        // and a converged audio ring. OsdScaled as in StreamScreen, so the tv- shots carry the
        // TV overlay scale.
        OsdScaled { StatsOverlay(shotLines(verbosity, w, h, hz, fps, mbps, loss), Modifier.align(Alignment.TopStart).padding(12.dp)) }
    }
}

/** [StreamScene]'s overlay lines, the Standard vocabulary at [verbosity]. */
private fun shotLines(
    verbosity: StatsVerbosity,
    w: Int,
    h: Int,
    hz: Int,
    fps: Double,
    mbps: Double,
    loss: Boolean,
): List<HudLine> {
    fun f(format: String, vararg v: Any) = String.format(java.util.Locale.ROOT, format, *v)
    val lostPct = if (loss) " · lost 0.8%" else ""
    val compact = HudLine(0, f("%.0f fps · %.1f Mb/s · decode 0.4 ms", fps, mbps) + lostPct)
    val normal = listOf(
        HudLine(0, "$w×$h@$hz · HEVC 10-bit · c2.qti.hevc.decoder · low-latency · HDR"),
        HudLine(1, f("received %.0f fps · decoded %.0f · presented %.0f · %.1f Mb/s", fps, fps, fps - 1, mbps)),
        HudLine(1, "host 0.6 ms · decode 0.4 ms · display 0.2 ms (avg)"),
        HudLine(if (loss) 3 else 1, if (loss) "lost 0.8% · skipped 0.4% · rtt 1.2 ms" else "lost 0.0% · skipped 0.0% · rtt 1.2 ms"),
    )
    val detailed = normal + listOf(
        HudLine(1, "host min/max 0.4/1.1 ms"),
        HudLine(2, "audio buffer 28 ms · a/v +4 ms"),
    )
    return when (verbosity) {
        StatsVerbosity.OFF -> emptyList()
        StatsVerbosity.COMPACT -> listOf(compact)
        StatsVerbosity.NORMAL -> normal
        StatsVerbosity.DETAILED -> detailed
    }
}

/**
 * The default-UI connect flow (the real [ConnectModal]) in each phase — instant "Connecting…"
 * feedback, the "Waking…" wait, and the wake-timed-out prompt. These render as a Material dialog over
 * the host grid, so the test composes [HostsScene] behind them and captures the whole screen.
 */
@Composable
internal fun ConnectingScene() =
    ConnectModal(ConnectPhase.Connecting("Living Room PC"), onCancel = {}, onRetry = {})

@Composable
internal fun WakingScene() =
    ConnectModal(
        ConnectPhase.Waking("Living Room PC", seconds = 12, connectsAfter = true),
        onCancel = {}, onRetry = {},
    )

@Composable
internal fun WakeTimedOutScene() =
    ConnectModal(ConnectPhase.WakeTimedOut("Living Room PC"), onCancel = {}, onRetry = {})

/**
 * The real console settings screen — the section tab strip, the glass rows, the focused row's
 * unfolded detail, and the living (calmed) backdrop behind them. The touch [SettingsScene] can't
 * stand in for it: this is a different screen with different navigation, and the strip is the part
 * a layout regression would eat first.
 */
/**
 * The start-of-stream banner over the same synthetic "streamed frame" — the real
 * [StreamStartBanner] at full opacity, since the caller owns the 6 s timer and a shot must not race
 * it. Two variants because the WORDS are the point: the banner names pad chords or touch gestures
 * depending on what the session actually has, and a screenshot is the only place the two can be
 * compared side by side.
 */
@Composable
internal fun StreamBannerScene(pad: Boolean) {
    Box(
        Modifier
            .fillMaxSize()
            .background(
                Brush.linearGradient(
                    listOf(Color(0xFF2A1E5C), Color(0xFF0E1B3D), Color(0xFF06122B)),
                ),
            ),
    ) {
        StreamStartBanner(
            text = if (pad) {
                "Select + A quick actions · Hold Select + Start + L1 + R1 to leave · Select + X stats"
            } else {
                "Back or a two-finger twist opens quick actions · three-finger tap for stats"
            },
            alpha = 1f,
            modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp),
        )
    }
}

/**
 * The companion panel on a dual-screen handheld's lower screen: the real [CompanionPanel] on
 * [page], with the stream scene's Normal lines and a session where every action is available.
 */
@Composable
internal fun CompanionScene(page: io.unom.punktfunk.CompanionPage) {
    io.unom.punktfunk.CompanionPanel(
        pages = io.unom.punktfunk.CompanionPage.entries,
        page = page,
        onPage = {},
        header = io.unom.punktfunk.PanelHeader("Living Room PC · Starfall Vale", "1920×1080 · 120 Hz"),
        stats = shotLines(StatsVerbosity.NORMAL, 1920, 1080, 120, 119.0, 92.1, loss = false),
        tier = StatsVerbosity.NORMAL,
        onTier = {},
        cfg = io.unom.punktfunk.OverlayConfig.platformDefault(),
        actions = io.unom.punktfunk.fakeRingActions(),
        haptics = remember { io.unom.punktfunk.ConsoleHaptics(null) },
        trackpad = {},
        pad = {},
    )
}

/**
 * The controllers screen with [shotPads] injected — Robolectric enumerates no input devices, and
 * the connected-pad card is the point of the shot. Wrapped in a background [Surface]: the
 * activity provides the dark ground in the app, and without one here the content color falls
 * back to black-on-white while the cards stay dark.
 */
@Composable
internal fun ControllersScene() =
    Surface(color = MaterialTheme.colorScheme.background) {
        ControllersScreen(gamepadSetting = 0, onBack = {}, padsOverride = shotPads())
    }

/**
 * The "Add a host" bottom sheet over the host grid — the store's onboarding frame. State is
 * hoisted in production (ConnectScreen), so the scene passes a filled-in form directly; the
 * mode label mirrors what a paired 120 Hz phone shows on the connect button.
 */
@Composable
internal fun AddHostScene() {
    HostsScene()
    AddHostSheet(
        hostName = "Living Room PC", onHostNameChange = {},
        host = "192.168.1.42", onHostChange = {},
        port = "9777", onPortChange = {},
        connecting = false, modeLabel = "2992×1344@120",
        onDismiss = {}, onConnect = { _, _, _ -> },
    )
}


// The Compose console's scenes (home carousel, settings, coverflow, connect takeover) are gone
// with the screens themselves: the console is the Skia shell now
// (design/android-skia-console-port.md), which renders over native GL and cannot compose under
// Roborazzi. Its store shots come from the desktop screenshot dump (the same pixels) or from a
// device capture. The scenes that remain are the touch UI and the two Compose platform screens
// the console still opens (Controllers, Licences).

/** The two pads the store listing names: DualSense (adaptive triggers, LEDs, rumble) and Xbox. */
internal fun shotPads() = listOf(
    PadInfo(
        name = "DualSense Wireless Controller",
        detail = "054C:0CE6 · gamepad · joystick",
        forwarded = true, controllerNumber = 1,
        resolvedPref = Gamepad.PREF_DUALSENSE, canRumble = true,
    ),
    PadInfo(
        name = "Xbox Wireless Controller",
        detail = "045E:0B13 · gamepad · joystick",
        forwarded = true, controllerNumber = 2,
        resolvedPref = Gamepad.PREF_XBOXONE, canRumble = true,
    ),
)

/**
 * Publish the palette locals `App` would normally provide. A scene that calls a console screen
 * directly gets the DEFAULT dark ink without this, and a pale-palette shot would then silently
 * prove nothing at all.
 */
/**
 * The TOUCH library — the poster grid a finger reaches through a host card's "Browse library…",
 * with the same mock shelf the coverflow scene uses. Same construction as [LibraryScene]: the real
 * [TouchGrid] under a rebuilt header, because the screen around it takes its state off the network.
 */
@Composable
internal fun TouchLibraryScene() {
    val context = LocalContext.current
    val loader = remember { shotLibraryLoader(context) }
    val games = remember { shotGames() }
    Surface(color = MaterialTheme.colorScheme.background) {
        Column(Modifier.fillMaxSize()) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth().padding(start = 4.dp, end = 4.dp, top = 8.dp),
            ) {
                IconButton(onClick = {}) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                }
                Text(
                    "Living Room PC — Library",
                    style = MaterialTheme.typography.titleLarge,
                    modifier = Modifier.weight(1f),
                )
                IconButton(onClick = {}) {
                    Icon(Icons.Filled.Refresh, contentDescription = "Reload")
                }
            }
            TouchGrid(games, loader, onLaunch = {}, onCopyLink = {}, modifier = Modifier.weight(1f))
        }
    }
}

/** A believable shelf: four titles with art plus the Steam launcher entry (brand-mark tile). */
private fun shotGames() = listOf(
    GameEntry("custom:aurora", "custom", "Aurora Drift", Artwork("shot://art/aurora", null, null)),
    GameEntry("steam:starfall", "steam", "Starfall Vale", Artwork("shot://art/starfall", null, null)),
    GameEntry("heroic:neon", "heroic", "Neon Circuit", Artwork("shot://art/neon", null, null)),
    GameEntry("gog:ember", "gog", "Ember Peaks", Artwork("shot://art/ember", null, null)),
    GameEntry("steam:launcher", "steam", "Steam", Artwork(null, null, null), role = "launcher", icon = "steam"),
)

private fun shotLibraryLoader(context: Context): ImageLoader {
    val engine = FakeImageLoaderEngine.Builder()
        .intercept("shot://art/aurora", poster(context, "AURORA DRIFT", ::drawAurora))
        .intercept("shot://art/starfall", poster(context, "STARFALL VALE", ::drawStarfall))
        .intercept("shot://art/neon", poster(context, "NEON CIRCUIT", ::drawNeon))
        .intercept("shot://art/ember", poster(context, "EMBER PEAKS", ::drawEmber))
        .default(ColorDrawable(0xFF221E44.toInt()))
        .build()
    return ImageLoader.Builder(context).components { add(engine) }.build()
}

// The four shelf posters, drawn procedurally at capture time — the same designs the Apple
// harness draws with CoreGraphics (`ShotPosterArt.swift`), so both listings show the same shelf.
// All geometry below is in a 600×900, y-UP space (matching the CG source); `posterY()` flips it.

private const val POSTER_W = 600
private const val POSTER_H = 900

private fun posterY(v: Float) = POSTER_H - v

/** Deterministic LCG (same constants and seeds as the Swift twin) so every capture is identical. */
private class ShotRand(var state: ULong) {
    fun next(): Float {
        state = state * 6364136223846793005UL + 1442695040888963407UL
        return (state shr 33).toFloat() / (1L shl 31).toFloat()
    }
    fun range(lo: Float, hi: Float) = lo + next() * (hi - lo)
}

private fun poster(context: Context, title: String, draw: (Canvas) -> Unit): Drawable {
    val bmp = Bitmap.createBitmap(POSTER_W, POSTER_H, Bitmap.Config.ARGB_8888)
    val canvas = Canvas(bmp)
    draw(canvas)
    posterTitle(canvas, title)
    return BitmapDrawable(context.resources, bmp)
}

/** Vertical gradient over the full canvas; stops bottom-to-top as (location, color). */
private fun sky(canvas: Canvas, stops: List<Pair<Float, Int>>) {
    canvas.drawRect(
        0f, 0f, POSTER_W.toFloat(), POSTER_H.toFloat(),
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            shader = LinearGradient(
                0f, POSTER_H.toFloat(), 0f, 0f,
                stops.map { it.second }.toIntArray(),
                stops.map { it.first }.toFloatArray(),
                Shader.TileMode.CLAMP,
            )
        },
    )
}

private fun glowDot(canvas: Canvas, x: Float, y: Float, radius: Float, color: Int) {
    canvas.drawCircle(
        x, posterY(y), radius,
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            shader = RadialGradient(
                x, posterY(y), radius, color, color and 0x00FFFFFF, Shader.TileMode.CLAMP,
            )
        },
    )
}

private fun shotAlpha(color: Int, a: Float) = (color and 0x00FFFFFF) or ((a * 255).toInt() shl 24)

/** Three strokes, wide-and-faint to thin-and-bright, in screen blend — the cheap neon glow. */
private fun glowStroke(canvas: Canvas, path: Path, width: Float, color: Int) {
    for ((mult, a) in listOf(2.6f to 0.12f, 1.3f to 0.28f, 0.55f to 0.85f)) {
        canvas.drawPath(
            path,
            Paint(Paint.ANTI_ALIAS_FLAG).apply {
                style = Paint.Style.STROKE
                strokeCap = Paint.Cap.ROUND
                strokeJoin = Paint.Join.ROUND
                strokeWidth = width * mult
                this.color = shotAlpha(color, a)
                blendMode = BlendMode.SCREEN
            },
        )
    }
}

private fun posterTitle(canvas: Canvas, title: String) {
    sky(canvas, listOf(0f to shotAlpha(0x000000, 0.55f), 0.22f to shotAlpha(0x000000, 0f)))
    canvas.drawText(
        title, POSTER_W / 2f, posterY(72f),
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = shotAlpha(0xFFFFFF, 0.94f)
            textSize = 46f
            letterSpacing = 5f / 46f
            typeface = Typeface.create("sans-serif-condensed", Typeface.BOLD)
            textAlign = Paint.Align.CENTER
            setShadowLayer(8f, 0f, 2f, shotAlpha(0x000000, 0.6f))
        },
    )
}

private fun drawAurora(canvas: Canvas) {
    sky(canvas, listOf(0f to 0xFF221E5C.toInt(), 0.45f to 0xFF141040.toInt(), 1f to 0xFF0B0830.toInt()))
    val rng = ShotRand(11UL)
    repeat(48) {
        val x = rng.range(0f, 600f)
        val y = rng.range(300f, 890f)
        val r = rng.range(1.4f, 3.2f)
        glowDot(canvas, x, y, r, shotAlpha(0xFFFFFF, rng.range(0.25f, 0.8f)))
    }
    data class Ribbon(
        val base: Float, val amp: Float, val freq: Float,
        val phase: Float, val w: Float, val c: Int,
    )
    for (r in listOf(
        Ribbon(700f, 55f, 1.15f, 0.4f, 30f, 0xFF6656F2.toInt()),
        Ribbon(615f, 70f, 1.4f, 2.2f, 24f, 0xFF8F7BFF.toInt()),
        Ribbon(530f, 45f, 0.95f, 4.1f, 18f, 0xFF35D0C5.toInt()),
    )) {
        val path = Path()
        for (i in 0..60) {
            val t = i / 60f
            val x = t * 600f
            val y = r.base + r.amp * kotlin.math.sin(t * Math.PI.toFloat() * r.freq + r.phase) + 40f * t
            if (i == 0) path.moveTo(x, posterY(y)) else path.lineTo(x, posterY(y))
        }
        glowStroke(canvas, path, r.w, r.c)
    }
    // A low ridge grounds the scene — without it the poster's bottom half is bare sky.
    for ((fill, baseline, rough) in listOf(
        Triple(0xFF191345.toInt(), 212f, 30f),
        Triple(0xFF0E0A2E.toInt(), 148f, 38f),
    )) {
        val path = Path()
        path.moveTo(0f, posterY(0f))
        path.lineTo(0f, posterY(baseline + rng.range(-rough, rough)))
        for (i in 1..9) {
            val x = i / 9f * 600f
            path.lineTo(x, posterY(baseline + rng.range(-rough, rough)))
        }
        path.lineTo(600f, posterY(0f))
        path.close()
        canvas.drawPath(path, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = fill })
    }
}

private fun drawStarfall(canvas: Canvas) {
    sky(
        canvas,
        listOf(
            0f to 0xFF2A0C24.toInt(), 0.35f to 0xFF7A2B58.toInt(),
            0.8f to 0xFFE86FA8.toInt(), 1f to 0xFFF7A8C8.toInt(),
        ),
    )
    val rng = ShotRand(23UL)
    repeat(6) {
        val hx = rng.range(60f, 560f)
        val hy = rng.range(420f, 840f)
        val len = rng.range(90f, 170f)
        val dx = kotlin.math.cos(2.15f)
        val dy = kotlin.math.sin(2.15f)
        val path = Path()
        path.moveTo(hx, posterY(hy))
        path.lineTo(hx + dx * len, posterY(hy + dy * len))
        glowStroke(canvas, path, 4f, 0xFFFFE3EF.toInt())
        glowDot(canvas, hx, hy, 11f, shotAlpha(0xFFFFFF, 0.9f))
    }
    for ((fill, baseline, rough) in listOf(
        Triple(0xFF3A1430.toInt(), 300f, 26f),
        Triple(0xFF1D0818.toInt(), 216f, 34f),
    )) {
        val path = Path()
        path.moveTo(0f, posterY(0f))
        path.lineTo(0f, posterY(baseline))
        for (i in 1..8) {
            val x = i / 8f * 600f
            path.lineTo(x, posterY(baseline + rng.range(-rough, rough)))
        }
        path.lineTo(600f, posterY(0f))
        path.close()
        canvas.drawPath(path, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = fill })
    }
}

private fun drawNeon(canvas: Canvas) {
    sky(canvas, listOf(0f to 0xFF0A2A33.toInt(), 1f to 0xFF04161C.toInt()))
    val rng = ShotRand(7UL)
    val ring = Path().apply {
        addOval(300f - 105f, posterY(560f) - 105f, 300f + 105f, posterY(560f) + 105f, Path.Direction.CW)
    }
    glowStroke(canvas, ring, 10f, 0xFF35D0C5.toInt())
    val gateX = listOf(-105f, 105f, 0f, 0f)
    val gateY = listOf(0f, 0f, -105f, 105f)
    for (i in 0 until 9) {
        var px: Float
        var py: Float
        if (i < 4) {
            px = 300f + gateX[i]
            py = 560f + gateY[i]
        } else {
            px = 40f * kotlin.math.round(rng.range(1f, 14f))
            py = 40f * kotlin.math.round(rng.range(1f, 21f))
        }
        val path = Path()
        path.moveTo(px, posterY(py))
        var horizontal = rng.next() > 0.5f
        repeat(rng.range(3f, 6f).toInt()) {
            val step = 40f * kotlin.math.round(rng.range(1f, 4f)) * (if (rng.next() > 0.5f) 1f else -1f)
            if (horizontal) px = (px + step).coerceIn(20f, 580f) else py = (py + step).coerceIn(20f, 880f)
            path.lineTo(px, posterY(py))
            horizontal = !horizontal
        }
        val color = if (rng.next() > 0.6f) 0xFF7FE8DE.toInt() else 0xFF35D0C5.toInt()
        glowStroke(canvas, path, 5f, color)
        glowDot(canvas, px, py, 12f, shotAlpha(color, 0.9f))
    }
}

private fun drawEmber(canvas: Canvas) {
    sky(
        canvas,
        listOf(
            0f to 0xFF200A04.toInt(), 0.3f to 0xFF7A2E12.toInt(),
            0.42f to 0xFFEF8F4B.toInt(), 1f to 0xFF2A0E06.toInt(),
        ),
    )
    glowDot(canvas, 300f, 385f, 160f, shotAlpha(0xFFC37A, 0.85f))
    val rng = ShotRand(41UL)
    for ((fill, baseline, rough) in listOf(
        Triple(0xFF5A2410.toInt(), 340f, 42f),
        Triple(0xFF401708.toInt(), 255f, 56f),
        Triple(0xFF200A04.toInt(), 165f, 48f),
    )) {
        val path = Path()
        path.moveTo(0f, posterY(0f))
        path.lineTo(0f, posterY(baseline + rng.range(-rough, rough)))
        for (i in 1..10) {
            val x = i / 10f * 600f
            path.lineTo(x, posterY(baseline + rng.range(-rough, rough)))
        }
        path.lineTo(600f, posterY(0f))
        path.close()
        canvas.drawPath(path, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = fill })
    }
    repeat(20) {
        val x = rng.range(30f, 570f)
        val y = rng.range(180f, 620f)
        val r = rng.range(2.5f, 6f)
        glowDot(canvas, x, y, r, shotAlpha(0xFFB067, rng.range(0.35f, 0.9f)))
    }
}
