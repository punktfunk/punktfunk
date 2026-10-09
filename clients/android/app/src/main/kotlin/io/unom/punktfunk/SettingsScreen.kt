package io.unom.punktfunk

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.filled.VolumeUp
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material.icons.filled.Tv
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.VerticalDivider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.DeviceGyro
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.link.StartIn
import io.unom.punktfunk.kit.link.StartScreen
import io.unom.punktfunk.kit.security.KnownHostStore

/**
 * Stream settings, organised as an iOS-Settings / Android-system-settings style list of category
 * subpages. On a phone the category list pushes to a full-screen detail; on a tablet / large screen
 * it becomes a two-pane list-detail (the list stays on the left, the detail on the right). Edits
 * persist immediately; [onBack] returns to the connect screen.
 *
 * **Structure mirrors the desktop/Apple settings revamp** ([SettingsCategory], and the Windows
 * client's `app/settings.rs`), so every client reads the same way: General = session/app behaviour,
 * Display = everything about the picture, Input = touch/keyboard/mouse, Audio, Controllers, About.
 * Each field carries its explanation DIRECTLY under it (the `described()` idiom — see
 * [SettingDropdown]'s `caption` and [ToggleRow]'s `subtitle`) rather than as loose paragraphs
 * floating between controls; the only form-level notes are the "applies from the next session"
 * footers, one per affected category.
 *
 * **One settings UI, not two.** The scope switcher on top edits either the global defaults
 * ([onChange], the base layer every preset inherits from) or one preset's overrides — the same
 * rows either way, so a preset can never drift from the surface it overrides. In preset scope
 * only presetable settings render, every row shows the EFFECTIVE value, and a row the preset
 * overrides carries a marker and a reset. See [SettingsOverlay] for the model.
 */
@Composable
fun SettingsScreen(
    initial: Settings,
    onChange: (Settings) -> Unit,
    onBack: () -> Unit,
    /**
     * Seeds the pushed detail page. The live app always starts on the category list (null); the
     * screenshot harness passes a category to capture one, the way the GTK client's
     * `PUNKTFUNK_SHOT_SETTINGS_SCOPE` seeds its scope.
     */
    initialCategory: SettingsCategory? = null,
    /** Seeds the scope the same way, for a screenshot of a preset being edited. */
    initialPresetId: String? = null,
) {
    var globals by remember { mutableStateOf(initial) }
    val context = LocalContext.current
    val presetStore = remember { PresetStore(context) }
    val hostStore = remember { KnownHostStore(context) }
    var presets by remember { mutableStateOf(presetStore.all()) }
    // Which layer is being edited: null = the global defaults, else a preset id. Survives rotation
    // but not a trip out of Settings — coming back should land on the defaults, which is what the
    // host cards actually connect with unless they say otherwise.
    var scopeId by rememberSaveable { mutableStateOf(initialPresetId) }
    // A preset deleted from under the scope (or a store that never had it) falls back to defaults.
    val active = scopeId?.let { id -> presets.firstOrNull { it.id == id } }
    if (scopeId != null && active == null) scopeId = null

    var showLicenses by remember { mutableStateOf(false) }
    var showShortcuts by remember { mutableStateOf(false) }
    var showControllers by remember { mutableStateOf(false) }
    var showQuickActions by remember { mutableStateOf(false) }
    var editing by remember { mutableStateOf<EditIntent?>(null) }
    var deleting by remember { mutableStateOf<StreamPreset?>(null) }

    // Every row renders the EFFECTIVE value: the globals with this preset's overrides on top, so a
    // row the preset doesn't override reads as the live global — and keeps following it.
    val s = globals.effectiveFor(active)

    /**
     * The scope an edit writes to, resolved AT THE EDIT rather than closed over at composition.
     *
     * [update] and [resetField] reach the rows as `::update` / `::resetField` — callable references,
     * and two of those compare EQUAL however different the scope they captured. Compose therefore
     * sees an unchanged callback and skips [CategoryDetail] on a scope switch that doesn't move any
     * value on screen — which is the ordinary case, since a preset inherits the globals until it
     * overrides something. The row then kept calling the reference it was first handed and wrote
     * into the scope the user had just LEFT: edit a default, switch to a preset, edit the same row
     * — the globals move again and the preset records nothing; switch back and the next edit lands
     * on the preset. Reading the live state here is what makes the write follow the chips.
     */
    fun scopePreset(): StreamPreset? = scopeId?.let { id -> presets.firstOrNull { it.id == id } }

    /**
     * One control commits [next], naming its own [field]. In preset scope it pins that field when
     * the value it showed changed — to today's global value too, which is the pin; re-picking what
     * it shows writes nothing, and a field no preset carries changes nothing.
     */
    fun update(field: String, next: Settings) {
        val preset = scopePreset()
        if (preset == null) {
            globals = next
            onChange(next)
            return
        }
        // A skipped row shows what a fresh one would (that is WHY it skipped), so the effective
        // settings recomputed here are the ones it rendered.
        val shown = globals.effectiveFor(preset)
        if (SettingsFields.controlRows(field).all { it.get(shown) == it.get(next) }) return
        presetStore.save(preset.copy(overrides = preset.overrides.pin(field, next)))
        presets = presetStore.all()
    }

    fun resetField(field: String) {
        val preset = scopePreset() ?: return
        presetStore.save(preset.copy(overrides = preset.overrides.clear(field)))
        presets = presetStore.all()
    }

    // Mic uplink — turning it on requests RECORD_AUDIO; if denied, the toggle stays off.
    val micLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> update("mic_enabled", s.copy(micEnabled = granted)) }
    val onMicChange: (Boolean) -> Unit = { on ->
        when {
            !on -> update("mic_enabled", s.copy(micEnabled = false))
            ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) ==
                PackageManager.PERMISSION_GRANTED -> update("mic_enabled", s.copy(micEnabled = true))
            else -> micLauncher.launch(Manifest.permission.RECORD_AUDIO)
        }
    }

    // Deep sub-screens replace the whole settings surface (they carry their own back).
    if (showLicenses) {
        LicensesScreen(onBack = { showLicenses = false })
        return
    }
    if (showShortcuts) {
        ShortcutsScreen(onBack = { showShortcuts = false })
        return
    }
    if (showControllers) {
        ControllersScreen(gamepadSetting = s.gamepad, onBack = { showControllers = false })
        return
    }
    if (showQuickActions) {
        QuickActionsScreen(
            blob = s.overlayActions,
            onChange = { update("overlay_actions", s.copy(overlayActions = it)) },
            // Reset drops the override in preset scope (design §3.3) and clears the global
            // otherwise; an empty blob is the platform default.
            onReset = {
                if (active != null) {
                    resetField("overlay_actions")
                } else {
                    update("overlay_actions", s.copy(overlayActions = ""))
                }
            },
            onBack = { showQuickActions = false },
            overridden = active?.overrides?.overridden()?.contains("overlay_actions") == true,
        )
        return
    }

    // Selected category persists across rotation (stored by name — null = the bare list on a phone).
    var selectedName by rememberSaveable { mutableStateOf(initialCategory?.name) }
    val categories = SettingsCategory.entries.filter { active == null || it.presetable }
    val selected = selectedName
        ?.let { n -> categories.firstOrNull { it.name == n } }

    Column(Modifier.fillMaxSize()) {
        PresetScopeChips(
            presets = presets,
            selectedId = active?.id,
            onSelect = { id ->
                scopeId = id
                // About has no presetable rows at all; don't strand the pane on it.
                if (id != null && selected?.presetable == false) selectedName = null
            },
            onNew = { editing = EditIntent.New(nextAccent(presets)) },
            onEdit = { p -> editing = EditIntent.Existing(p) },
            onDuplicate = { p ->
                val copy = newPreset(uniqueName(presetStore, p.name), p.accent)
                    .copy(overrides = p.overrides)
                presetStore.save(copy)
                presets = presetStore.all()
                scopeId = copy.id
            },
            onDelete = { p -> deleting = p },
            modifier = Modifier.padding(top = 12.dp),
        )
        if (active != null) {
            Text(
                "Overrides the default settings for hosts that use “${active.name}”.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 20.dp, end = 20.dp, top = 10.dp),
            )
        }
        Spacer(Modifier.height(12.dp))
        HorizontalDivider()

        CompositionLocalProvider(
            LocalSettingsScope provides SettingsScopeState(
                presetScope = active != null,
                overridden = active?.overrides?.overridden() ?: emptySet(),
                onReset = ::resetField,
                showAdvanced = globals.showAdvanced,
                onShowAdvanced = { on ->
                    globals = globals.copy(showAdvanced = on)
                    onChange(globals)
                },
            ),
        ) {
            CategoryPanes(
                categories = categories,
                selected = selected,
                onSelect = { selectedName = it?.name },
                onBack = onBack,
            ) { cat, back ->
                // Keyed on the scope: switching chips rebuilds the page rather than recomposing
                // it in place. Correctness no longer depends on it (see [scopePreset]), but a
                // row's own `remember` is per-scope state too — "Custom…" picked while editing
                // a preset has no business still being picked over on the defaults.
                key(active?.id) {
                    CategoryDetail(
                        category = cat,
                        settings = s,
                        onChange = ::update,
                        context = context,
                        onMicChange = onMicChange,
                        onOpenControllers = { showControllers = true },
                        onOpenQuickActions = { showQuickActions = true },
                        onOpenLicenses = { showLicenses = true },
                        onOpenShortcuts = { showShortcuts = true },
                        onBack = back,
                    )
                }
            }
        }
    }

    PresetDialogs(
        editing = editing,
        deleting = deleting,
        presetStore = presetStore,
        hostStore = hostStore,
        onSaved = { id -> presets = presetStore.all(); scopeId = id; editing = null },
        onDeleted = { presets = presetStore.all(); scopeId = null; deleting = null },
        onDismiss = { editing = null; deleting = null },
    )
}

/**
 * The category list beside (≥ 640 dp) or in front of the detail page. Two-pane: a side list with
 * a cross-fading detail, never an empty one. Compact: the list pushes to a full-screen detail
 * and back, like the iOS / Android system settings — a horizontal slide tracking the drill-in.
 */
@Composable
private fun CategoryPanes(
    categories: List<SettingsCategory>,
    selected: SettingsCategory?,
    onSelect: (SettingsCategory?) -> Unit,
    onBack: () -> Unit,
    detail: @Composable (SettingsCategory, (() -> Unit)?) -> Unit,
) {
    BoxWithConstraints(Modifier.fillMaxSize()) {
        val twoPane = maxWidth >= 640.dp
        LaunchedEffect(twoPane, categories) {
            if (twoPane && selected == null) onSelect(categories.first())
        }
        if (twoPane) {
            BackHandler(onBack = onBack)
            Row(Modifier.fillMaxSize()) {
                CategoryList(
                    categories = categories,
                    selected = selected,
                    twoPane = true,
                    onSelect = onSelect,
                    modifier = Modifier.width(300.dp).fillMaxHeight(),
                )
                VerticalDivider()
                Box(Modifier.weight(1f).fillMaxHeight()) {
                    AnimatedContent(
                        targetState = selected ?: categories.first(),
                        transitionSpec = { fadeIn(tween(200)) togetherWith fadeOut(tween(200)) },
                        label = "SettingsPane",
                    ) { cat -> detail(cat, null) }
                }
            }
        } else {
            BackHandler { if (selected != null) onSelect(null) else onBack() }
            AnimatedContent(
                targetState = selected,
                transitionSpec = {
                    if (targetState != null) {
                        slideInHorizontally { it } + fadeIn() togetherWith
                            slideOutHorizontally { -it } + fadeOut()
                    } else {
                        slideInHorizontally { -it } + fadeIn() togetherWith
                            slideOutHorizontally { it } + fadeOut()
                    }
                },
                label = "SettingsPush",
            ) { sel ->
                if (sel == null) {
                    CategoryList(
                        categories = categories,
                        selected = null,
                        twoPane = false,
                        onSelect = onSelect,
                        modifier = Modifier.fillMaxSize(),
                    )
                } else {
                    detail(sel) { onSelect(null) }
                }
            }
        }
    }
}

/** The preset editor and the delete confirmation, whichever intent is pending. */
@Composable
private fun PresetDialogs(
    editing: EditIntent?,
    deleting: StreamPreset?,
    presetStore: PresetStore,
    hostStore: KnownHostStore,
    onSaved: (id: String) -> Unit,
    onDeleted: () -> Unit,
    onDismiss: () -> Unit,
) {
    editing?.let { intent ->
        val existing = (intent as? EditIntent.Existing)?.preset
        PresetEditorDialog(
            title = if (existing == null) "New preset" else "Edit preset",
            confirmLabel = if (existing == null) "Create" else "Save",
            initialName = existing?.name.orEmpty(),
            initialAccent = existing?.accent ?: (intent as? EditIntent.New)?.accent,
            creating = existing == null,
            taken = { presetStore.nameTaken(it, except = existing?.id) },
            onConfirm = { name, accent ->
                val saved = existing?.copy(name = name, accent = accent)
                    ?: newPreset(name, accent)
                presetStore.save(saved)
                onSaved(saved.id)
            },
            onDismiss = onDismiss,
        )
    }
    deleting?.let { preset ->
        val hosts = hostStore.all()
        DeletePresetDialog(
            preset = preset,
            boundHosts = hosts.count { it.presetId == preset.id },
            pinnedCards = hosts.count { preset.id in it.pinnedPresetIds },
            onConfirm = {
                presetStore.delete(preset.id)
                // Bindings and pins are left dangling on purpose: they resolve to "no preset" and
                // to "no card", which is exactly right, and rewriting every host record here would
                // be a second write pass over data the user didn't ask us to touch.
                onDeleted()
            },
            onDismiss = onDismiss,
        )
    }
}

/** What the preset editor is for — a fresh preset (with the colour creation picked out for it),
 * or one that already exists. */
private sealed interface EditIntent {
    data class New(val accent: String) : EditIntent
    data class Existing(val preset: StreamPreset) : EditIntent
}

/** "Work" → "Work copy" → "Work copy 2" — the first name Duplicate can actually save. */
private fun uniqueName(store: PresetStore, base: String): String {
    val first = "$base copy"
    if (!store.nameTaken(first)) return first
    var n = 2
    while (store.nameTaken("$first $n")) n++
    return "$first $n"
}

// ---- Scope plumbing ----------------------------------------------------------------------------

/**
 * What the rows need to know about the layer being edited. Carried as a composition local rather
 * than threaded through every category and row: the rows are the same rows in both scopes, and the
 * scope only decides whether tier-G rows render at all and whether a row wears an override marker.
 * [showAdvanced] is the device-wide tier switch, written to the globals from either scope.
 */
private class SettingsScopeState(
    val presetScope: Boolean,
    val overridden: Set<String>,
    val onReset: (String) -> Unit,
    val showAdvanced: Boolean = false,
    val onShowAdvanced: (Boolean) -> Unit = {},
)

private val LocalSettingsScope = compositionLocalOf {
    SettingsScopeState(presetScope = false, overridden = emptySet(), onReset = {})
}

/**
 * Wraps rows that are facts about THIS DEVICE or this app rather than about a stream — the console
 * UI toggle, the library browser, auto-wake, the controller diagnostics, rumble mirroring, SC2
 * capture (design §3, tiers G and H). They never belong to a preset, so in preset scope they
 * simply don't render.
 */
@Composable
private fun DeviceScopeOnly(content: @Composable () -> Unit) {
    if (!LocalSettingsScope.current.presetScope) content()
}

/**
 * A category's advanced rows: their own group while Show advanced is on or the preset overrides one
 * of [fields]; otherwise one row naming how many hold a changed value, which shows them. Nothing
 * when none changed.
 */
@Composable
private fun AdvancedGroup(
    changed: Int,
    fields: Set<String> = emptySet(),
    content: @Composable ColumnScope.() -> Unit,
) {
    val scope = LocalSettingsScope.current
    if (scope.showAdvanced || fields.any { it in scope.overridden }) {
        SettingsGroup("Advanced", content = content)
    } else if (changed > 0 && !scope.presetScope) {
        SettingsGroup {
            ClickableRow(
                title = if (changed == 1) "1 advanced setting changed" else "$changed advanced settings changed",
                subtitle = "Show advanced settings",
                onClick = { scope.onShowAdvanced(true) },
            )
        }
    }
}

/** How many of [flags] are set — an advanced group's changed count. */
private fun changedCount(vararg flags: Boolean) = flags.count { it }

/**
 * The accent marker and reset a row wears when the selected preset overrides it. Nothing renders
 * in the defaults scope, or on a row the preset inherits — an inherited row shows the live global
 * value in the ordinary quiet style, which is the whole point of inherit-by-default.
 */
@Composable
private fun OverrideBadge(field: String?) {
    val scope = LocalSettingsScope.current
    if (!scope.presetScope || field == null || field !in scope.overridden) return
    // One compact line. A `TextButton` here carried its own 48dp touch target and dwarfed both the
    // control it annotates and the caption under it; the reset keeps a generous padded hit area
    // instead, which is the right trade for a secondary action inside a dense settings list.
    Row(
        verticalAlignment = Alignment.CenterVertically,
        // No vertical padding of its own: this row is the FIRST thing in a card whose column
        // already pads 16dp, and anything added here reads as double the gap every other row has.
        // The bottom padding is the badge's tie to the control it annotates — deliberately tighter
        // than the gap down to the caption, so the marker groups upward with its own field.
        modifier = Modifier.fillMaxWidth().padding(bottom = 6.dp),
    ) {
        AccentDot(MaterialTheme.colorScheme.primary, size = 6)
        Text(
            "Overridden",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.padding(start = 6.dp).weight(1f),
        )
        Text(
            "Reset",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier
                .clip(RoundedCornerShape(6.dp))
                .clickable { scope.onReset(field) }
                .padding(horizontal = 10.dp, vertical = 2.dp),
        )
    }
}

// ---- Categories --------------------------------------------------------------------------------

/**
 * The top-level settings groups — each opens its own subpage (list on phone, split on tablet).
 * The map and its order are the cross-client one (Apple's `SettingsCategory`, the Windows
 * NavigationView, the GTK pages): General, Display, Input, Audio, Controllers, About.
 *
 * [presetable] is false for a category with no presetable rows at all — it isn't offered in
 * preset scope, rather than opening onto an empty page.
 */
enum class SettingsCategory(
    val title: String,
    val icon: ImageVector,
    internal val presetable: Boolean = true,
) {
    General("General", Icons.Filled.Tune),
    Display("Display", Icons.Filled.Tv),
    Input("Input", Icons.Filled.TouchApp),
    Audio("Audio", Icons.AutoMirrored.Filled.VolumeUp),
    Controllers("Controllers", Icons.Filled.SportsEsports),
    About("About", Icons.Filled.Info, presetable = false),
}

/** The category list — the settings root. Highlights the [selected] row when it drives a detail pane. */
@Composable
private fun CategoryList(
    categories: List<SettingsCategory>,
    selected: SettingsCategory?,
    twoPane: Boolean,
    onSelect: (SettingsCategory) -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 12.dp, vertical = 20.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Text(
            "Settings",
            style = MaterialTheme.typography.headlineMedium,
            modifier = Modifier.padding(start = 8.dp, bottom = 12.dp),
        )
        categories.forEach { cat ->
            val highlighted = twoPane && selected == cat
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(14.dp))
                    .background(if (highlighted) MaterialTheme.colorScheme.secondaryContainer else Color.Transparent)
                    .clickable { onSelect(cat) }
                    .padding(horizontal = 14.dp, vertical = 15.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(
                    cat.icon,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.padding(end = 16.dp),
                )
                Text(cat.title, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
                if (!twoPane) {
                    Icon(
                        Icons.AutoMirrored.Filled.KeyboardArrowRight,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

/** One category's controls. [onBack] non-null (phone push) shows a back arrow; null (tablet pane) hides it. */
@Composable
private fun CategoryDetail(
    category: SettingsCategory,
    settings: Settings,
    onChange: (String, Settings) -> Unit,
    context: android.content.Context,
    onMicChange: (Boolean) -> Unit,
    onOpenControllers: () -> Unit,
    onOpenQuickActions: () -> Unit,
    onOpenLicenses: () -> Unit,
    onOpenShortcuts: () -> Unit,
    onBack: (() -> Unit)?,
) {
    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            if (onBack != null) {
                IconButton(onClick = onBack, modifier = Modifier.padding(end = 4.dp)) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                }
            }
            Text(category.title, style = MaterialTheme.typography.headlineMedium)
        }
        when (category) {
            SettingsCategory.General -> GeneralSettings(settings, onChange)
            SettingsCategory.Display -> DisplaySettings(settings, onChange, context)
            SettingsCategory.Input -> InputSettings(settings, onChange, onOpenQuickActions)
            SettingsCategory.Audio -> AudioSettings(settings, onChange, onMicChange)
            SettingsCategory.Controllers -> ControllerSettings(settings, onChange, onOpenControllers)
            SettingsCategory.About -> AboutSettings(context, onOpenShortcuts, onOpenLicenses)
        }
    }
}

@Composable
private fun GeneralSettings(s: Settings, update: (String, Settings) -> Unit) {
    val context = LocalContext.current
    // Turning the keep-alive on asks for notifications, because the ongoing notification is how
    // the session announces itself and the only way to end it from outside the app. A refusal
    // keeps the setting: the keep-alive still works, it just has nothing on screen.
    val noteLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) {}
    DeviceScopeOnly {
        SettingsGroup("Session") {
            ToggleRow(
                title = "Auto-wake on connect",
                subtitle = "Wake a sleeping saved host before connecting. Turn off if hosts " +
                    "behind a VPN look offline when they aren't.",
                checked = s.autoWakeEnabled,
                onCheckedChange = { on -> update("auto_wake", s.copy(autoWakeEnabled = on)) },
            )
            // Hidden on a TV, where the notification the End button lives on has nowhere to
            // appear: a session left running would be one nobody outside the app can stop.
            if (!isTvDevice(context)) {
                ToggleRow(
                    title = "Keep streaming in the background",
                    subtitle = "Leaving the app holds the session instead of ending it — audio " +
                        "keeps playing, and the picture comes back where you left it. An " +
                        "ongoing notification shows the session and can end it.",
                    checked = s.backgroundKeepAlive,
                    onCheckedChange = { on ->
                        if (on && Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                            noteLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
                        }
                        update("background_keep_alive", s.copy(backgroundKeepAlive = on))
                    },
                )
                // Only decides anything while the switch above is on, so it is hidden rather than
                // dimmed when it isn't — the same rule the console-UI group follows below.
                if (s.backgroundKeepAlive) {
                    SettingDropdown(
                        label = "Give up after",
                        options = BACKGROUND_TIMEOUT_OPTIONS,
                        selected = s.backgroundTimeoutMinutes,
                        caption = "A host cannot tell someone who walked away from someone who " +
                            "is watching, so a session nobody comes back to disconnects itself. " +
                            "Returning later reconnects.",
                    ) { v -> update("background_timeout_minutes", s.copy(backgroundTimeoutMinutes = v)) }
                }
            }
        }
    }
    SettingsGroup("Statistics") {
        SettingDropdown(
            label = "Statistics overlay",
            options = STATS_VERBOSITY_OPTIONS,
            selected = s.statsVerbosity,
            field = "stats_verbosity",
            caption = "Compact is one line; Detailed adds the decoder and latency breakdown. " +
                "A 3-finger tap, or Select + X on a pad, cycles the tiers in-stream.",
        ) { v -> update("stats_verbosity", s.copy(statsVerbosity = v)) }
        DeviceScopeOnly {
            ClickableRow(title = "What each number means", subtitle = "docs.punktfunk.unom.io/docs/stats") {
                runCatching {
                    context.startActivity(
                        android.content.Intent(
                            android.content.Intent.ACTION_VIEW,
                            android.net.Uri.parse("https://docs.punktfunk.unom.io/docs/stats"),
                        ),
                    )
                }
            }
        }
    }
    DeviceScopeOnly {
        SettingsGroup("Library") {
            SettingDropdown(
                label = "Start in",
                options = START_IN_OPTIONS,
                selected = StartIn.parse(s.startIn).stored,
                caption = startInCaption(s, LocalContext.current),
            ) { v -> update("start_in", s.copy(startIn = v)) }
        }
        // The footer is null on every device where the console works, so it costs nothing there —
        // and on the ones where it doesn't, it is the only place the app admits that this switch
        // is being overruled. See `SkiaConsole.unavailable`.
        SettingsGroup("Interface", footer = io.unom.punktfunk.console.SkiaConsole.unavailable()) {
            ToggleRow(
                title = "Controller-optimized UI",
                subtitle = "Swap the touch home for the console home — the host carousel and " +
                    "gamepad chrome. A TV always uses it.",
                checked = s.gamepadUiEnabled,
                onCheckedChange = { on -> update("gamepad_ui_enabled", s.copy(gamepadUiEnabled = on)) },
            )
            // Only decides anything while the switch above is on, so it is HIDDEN rather than
            // dimmed when it isn't — a picker whose every option changes nothing is worse than
            // no picker, and this group is short enough that nothing jumps far.
            if (s.gamepadUiEnabled) {
                SettingDropdown(
                    label = "Show it",
                    options = GAMEPAD_UI_MODE_OPTIONS,
                    selected = s.gamepadUiMode,
                    caption = "With a controller: the touch home comes back when the last one " +
                        "disconnects. Always keeps the console home either way — for a device " +
                        "that lives docked to a TV.",
                ) { v -> update("gamepad_ui_mode", s.copy(gamepadUiMode = v)) }
            }
        }
    }
    // Device-wide, and in both scopes: it changes what these pages list, not a stream.
    val scope = LocalSettingsScope.current
    SettingsGroup {
        ToggleRow(
            title = "Show advanced",
            subtitle = "Adds the settings most players never need to change.",
            checked = scope.showAdvanced,
            onCheckedChange = scope.onShowAdvanced,
        )
    }
    // The overlay's vocabulary, corner and size and the exit hint: this device's screen (tier G).
    DeviceScopeOnly {
        val d = Settings()
        val corner = s.hudPlacement.ifEmpty { "topLeading" }
        AdvancedGroup(
            changed = changedCount(
                s.advancedStats != d.advancedStats,
                corner != "topLeading",
                s.statsScalePct != d.statsScalePct,
                s.exitHint != d.exitHint,
            ),
        ) {
            ToggleRow(
                title = "Advanced statistics",
                subtitle = "Off shows the figures Moonlight's overlay also shows. On shows " +
                    "capture to glass as p50/p95 and every stage between.",
                checked = s.advancedStats,
                onCheckedChange = { on -> update("advanced_stats", s.copy(advancedStats = on)) },
            )
            SettingDropdown(
                label = "Statistics position",
                options = HUD_PLACEMENT_OPTIONS,
                selected = corner,
                caption = "The corner the statistics overlay sits in.",
            ) { v -> update("hud_placement", s.copy(hudPlacement = v)) }
            SettingDropdown(
                label = "Statistics size",
                options = STATS_SCALE_OPTIONS,
                selected = s.statsScalePct.coerceIn(75, 200),
                caption = "The overlay's size, on top of the system display size.",
            ) { v -> update("stats_scale_pct", s.copy(statsScalePct = v)) }
            ToggleRow(
                title = "Exit hint",
                subtitle = "Shows how to leave for a few seconds when a stream starts.",
                checked = s.exitHint,
                onCheckedChange = { on -> update("exit_hint", s.copy(exitHint = on)) },
            )
        }
    }
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun DisplaySettings(s: Settings, update: (String, Settings) -> Unit, context: android.content.Context) {
    val (nw, nh, nhz) = nativeDisplayMode(context, secondScreen = s.secondScreen)
    // The safe-area row carries its resolved size the same way the native row does. On a display with
    // no cutout this equals the native mode — the row stays, honestly showing that it changes nothing
    // here, rather than silently vanishing on some devices and not others.
    val (sw, sh, _) = safeDisplayMode(context, secondScreen = s.secondScreen)
    // "Custom…" picked while the stored size is still a preset — keeps the size fields visible
    // until an edit actually makes it custom (or a preset is re-picked). Custom itself is detected
    // from the stored size, never flagged (see [isCustomResolution]), so nothing new persists.
    var customPicked by remember { mutableStateOf(false) }
    val (panel, panelSafe) = panelScreens(context)
    val families = remember(panel, panelSafe) { Resolutions.families(panel, panelSafe) }
    val showCustom = customPicked || s.isCustomResolution(families)
    var customBitratePicked by remember { mutableStateOf(false) }
    SettingsGroup("Resolution") {
        // The family the dropdown lists. A chip writes that family's size nearest the current
        // height, so the dropdown always holds a row of the family it shows.
        val family = s.resolutionFamily(families)
        Text(
            "Aspect ratio",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        // Wraps rather than scrolls, so no entry hides past a phone's edge.
        FlowRow(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            families.forEachIndexed { i, a ->
                FilterChip(
                    selected = i == family,
                    onClick = {
                        customPicked = false
                        val (w, h) = Resolutions.nearestIn(a, s.height)
                        update(SettingsOverlay.FIELD_RESOLUTION, s.copy(width = w, height = h))
                    },
                    label = { Text(a.label) },
                )
            }
        }
        SettingDropdown(
            label = "Resolution",
            options = resolutionOptions(families[family]).map { (w, h, lbl) ->
                (w to h) to when (w) {
                    0 -> "$lbl ($nw × $nh)"
                    SAFE_AREA_MODE -> "$lbl ($sw × $sh)"
                    else -> lbl
                }
            } +
                // The (-1, -1) sentinel can't collide with a real size; once a custom size is
                // stored its label carries the live value, like the native row carries ($nw × $nh).
                ((-1 to -1) to if (s.isCustomResolution(families)) "Custom (${s.width} × ${s.height})" else "Custom…"),
            selected = if (showCustom) -1 to -1 else s.width to s.height,
            field = SettingsOverlay.FIELD_RESOLUTION,
            caption = "The host makes a display exactly this size — no scaling. Native follows " +
                "this device's panel.",
        ) { (w, h) ->
            // ONLY -1 is "Custom…". The other negative value is the safe-area sentinel, which is a
            // stored mode like any preset — a blanket `w < 0` here would open the custom fields for it
            // and overwrite it with a concrete size.
            if (w == -1) {
                // Seed from the current *effective* size so the fields start from something
                // sensible (the resolved native mode, not the 0 × 0 placeholder).
                customPicked = true
                update(
                    SettingsOverlay.FIELD_RESOLUTION,
                    s.copy(width = if (s.width > 0) s.width else nw, height = if (s.height > 0) s.height else nh),
                )
            } else {
                customPicked = false
                update(SettingsOverlay.FIELD_RESOLUTION, s.copy(width = w, height = h))
            }
        }
        if (showCustom) {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                ResolutionField(label = "Width", value = s.width, modifier = Modifier.weight(1f)) { w ->
                    val (cw, ch) = Resolutions.custom(w, s.height, s.codec)
                    update(SettingsOverlay.FIELD_RESOLUTION, s.copy(width = cw, height = ch))
                }
                ResolutionField(label = "Height", value = s.height, modifier = Modifier.weight(1f)) { h ->
                    val (cw, ch) = Resolutions.custom(s.width, h, s.codec)
                    update(SettingsOverlay.FIELD_RESOLUTION, s.copy(width = cw, height = ch))
                }
            }
        }
        SettingDropdown(
            label = "Refresh rate",
            options = REFRESH_OPTIONS.map { (hz, lbl) -> hz to (if (hz == 0) "$lbl ($nhz Hz)" else lbl) },
            selected = s.hz,
            field = "refresh_hz",
            caption = "Native follows this device's refresh rate.",
        ) { hz -> update("refresh_hz", s.copy(hz = hz)) }
    }

    // The GPU probe, not a MediaCodec one — PyroWave decodes as Vulkan compute.
    val pyrowaveCapable = remember { VideoDecoders.pyrowaveCapable() }
    // HDR is only meaningful on a panel that can present HDR10; on an SDR display the toggle is
    // disabled (and HDR is never advertised) so the host doesn't send PQ the panel mis-tone-maps.
    val hdrCapable = remember { displaySupportsHdr(context) }
    val hdrOn = s.hdrEnabled && hdrCapable
    SettingsGroup("Picture", footer = "Display changes apply from the next session.") {
        SettingDropdown(
            label = "Picture fit",
            options = VIDEO_FIT_OPTIONS,
            selected = io.unom.punktfunk.kit.VideoFit.fromName(s.videoFit).wire,
            field = "video_fit",
            caption = "When the stream's shape differs from this screen. Fit shows the whole " +
                "picture with black bars, Crop to fill cuts the edges off, Stretch to fill " +
                "distorts it.",
        ) { fit -> update("video_fit", s.copy(videoFit = fit)) }

        // PyroWave takes its rate from the host, so the row reads Automatic and locks; the stored
        // rate is kept for the other codecs.
        val pyrowaveOn = s.codec == "pyrowave" && pyrowaveCapable
        // Custom is read from the stored rate, like the resolution above; the flag only keeps the
        // field open between picking "Custom…" and typing a number.
        val showCustomBitrate = !pyrowaveOn && (customBitratePicked || s.isCustomBitrate())
        SettingDropdown(
            label = "Bitrate",
            options = BITRATE_OPTIONS + (CUSTOM_BITRATE to
                if (s.isCustomBitrate()) "Custom (${bitrateLabel(s.bitrateKbps)})" else "Custom…"),
            selected = when {
                pyrowaveOn -> 0
                showCustomBitrate -> CUSTOM_BITRATE
                else -> s.bitrateKbps
            },
            field = "bitrate_kbps",
            caption = if (pyrowaveOn) {
                "PyroWave sets its own rate from the stream mode."
            } else {
                "Automatic lets the host decide."
            },
            enabled = !pyrowaveOn,
        ) { kbps ->
            customBitratePicked = kbps == CUSTOM_BITRATE
            if (kbps != CUSTOM_BITRATE) update("bitrate_kbps", s.copy(bitrateKbps = kbps))
        }
        if (showCustomBitrate) {
            BitrateField(s.bitrateKbps) { kbps -> update("bitrate_kbps", s.copy(bitrateKbps = kbps)) }
        }

        ToggleRow(
            title = "10-bit HDR",
            subtitle = if (hdrCapable) {
                "HDR10, when the host has HDR content to send."
            } else {
                "This display can't present HDR10 — streams stay SDR"
            },
            checked = hdrOn,
            enabled = hdrCapable,
            field = "hdr_enabled",
            onCheckedChange = { on -> update("hdr_enabled", s.copy(hdrEnabled = on)) },
        )
        // The timeline presenter's intent — the Apple client's "Prioritize" pair, same stored
        // values, so a preset written on one platform means the same thing here.
        SettingDropdown(
            label = "Prioritize",
            options = PRESENT_PRIORITY_OPTIONS,
            selected = if (s.presentPriority == "smooth") "smooth" else "latency",
            field = "present_priority",
            caption = "Lowest latency shows each frame the moment it can reach the panel; " +
                "Smoothness buffers a little to absorb network jitter.",
        ) { v -> update("present_priority", s.copy(presentPriority = v)) }
    }

    SettingsGroup("Second screen") {
        ToggleRow(
            title = "Second screen",
            subtitle = "A smaller second screen — a dual-screen handheld's lower panel, a foldable " +
                "half open — shows the companion panel, or the picture with Screens. Off leaves it " +
                "to the system: for a phone on a TV.",
            checked = s.secondScreen,
            field = "android.second_screen",
            onCheckedChange = { on -> update("android.second_screen", s.copy(secondScreen = on)) },
        )
    }

    val d = Settings()
    val smoothing = s.presentPriority == "smooth"
    AdvancedGroup(
        changed = changedCount(
            smoothing && s.smoothBuffer != d.smoothBuffer,
            s.renderScale != d.renderScale,
            s.codec != d.codec,
            s.tenBitSdr != d.tenBitSdr,
            s.lowLatencyMode != d.lowLatencyMode,
            s.compositor != d.compositor,
        ),
        fields = setOf(
            "smooth_buffer", "render_scale", "codec", "ten_bit_sdr", "low_latency_mode", "compositor",
        ),
    ) {
        if (smoothing) {
            SettingDropdown(
                label = "Smoothness buffer",
                options = smoothBufferOptions(if (s.hz > 0) s.hz else nhz),
                selected = if (s.smoothBuffer in 1..3) s.smoothBuffer else 0,
                field = "smooth_buffer",
                caption = "Each buffered frame absorbs one refresh of jitter and adds one of " +
                    "display latency — the cost shown is at the session's refresh rate.",
            ) { v -> update("smooth_buffer", s.copy(smoothBuffer = v)) }
        }
        SettingDropdown(
            label = "Render scale",
            options = RENDER_SCALE_OPTIONS,
            // Snap the stored value (a Float round-tripped to Double) to the nearest preset so the
            // exact Double keys match.
            selected = RenderScale.PRESETS.minByOrNull { kotlin.math.abs(it - s.renderScale) } ?: 1.0,
            field = "render_scale",
            caption = "Above native is sharper but costs bandwidth and decode; below is " +
                "lighter on the host.",
        ) { scale -> update("render_scale", s.copy(renderScale = scale)) }

        // Only codecs this device can actually decode are offered — a preference the client never
        // advertises would be a dead setting (see [codecOptionsFor]).
        val av1Capable = remember { VideoDecoders.pickDecoder("video/av01") != null }
        // Mirror the Automatic AV1 rule in HostConnect (hardware AV1 AND no partial-frame
        // support) so the picker says what "Automatic" actually does on THIS device.
        val autoPrefersAv1 = remember {
            VideoDecoders.decodableCodecBits() and 4 != 0 && !VideoDecoders.partialFrameCapable()
        }
        SettingDropdown(
            label = "Video codec",
            options = codecOptionsFor(s.codec, av1Capable, pyrowaveCapable),
            selected = s.codec,
            field = "codec",
            caption = if (autoPrefersAv1) {
                "A preference — the host falls back if it can't encode this one. " +
                    "Automatic prefers AV1 on this device."
            } else {
                "A preference — the host falls back if it can't encode this one."
            },
        ) { c -> update("codec", s.copy(codec = c)) }

        // Asks nothing of the panel, so no capability gate: an 8-bit display shows a dithered
        // Main10 stream, and the gain is gradients that do not band. Inert while HDR is on
        // above — that already carries 10 bits — so the row dims rather than disappearing.
        ToggleRow(
            title = "10-bit SDR",
            subtitle = if (hdrOn) {
                "HDR already streams in 10-bit"
            } else {
                "Smoother gradients on any display, for a little more bandwidth."
            },
            checked = s.tenBitSdr && !hdrOn,
            enabled = !hdrOn,
            field = "ten_bit_sdr",
            onCheckedChange = { on -> update("ten_bit_sdr", s.copy(tenBitSdr = on)) },
        )
        // Android has no decoder or GPU picker (MediaCodec resolves both); this one knob is worth
        // varying per host, since a marginal link is where the plain decode path helps.
        ToggleRow(
            title = "Low-latency mode",
            subtitle = "The fast decode pipeline. Turn it off if the stream stutters or " +
                "glitches on this device.",
            checked = s.lowLatencyMode,
            field = "low_latency_mode",
            onCheckedChange = { on -> update("low_latency_mode", s.copy(lowLatencyMode = on)) },
        )
        SettingDropdown(
            label = "Host compositor",
            options = COMPOSITOR_OPTIONS,
            selected = s.compositor,
            field = "compositor",
            caption = "Linux hosts only; falls back to auto-detection when unavailable.",
        ) { c -> update("compositor", s.copy(compositor = c)) }
    }
}

@Composable
private fun InputSettings(s: Settings, update: (String, Settings) -> Unit, onOpenQuickActions: () -> Unit) {
    SettingsGroup("Touch & pointer") {
        SettingDropdown(
            label = "Touch input",
            options = TOUCH_MODE_OPTIONS,
            selected = s.touchMode,
            field = "touch_mode",
            caption = "Trackpad moves the cursor by relative swipes; Direct pointer jumps it " +
                "to your finger; Passthrough sends real multi-touch; Off ignores touches on the picture.",
        ) { mode -> update("touch_mode", s.copy(touchMode = mode)) }
        Column {
            OverrideBadge("overlay_actions")
            ClickableRow(
                title = "Quick actions",
                subtitle = "Back, a two-finger twist, Ctrl+Alt+Shift+O or Select + A on a pad " +
                    "opens it mid-stream. " +
                    "Which actions the in-stream dial offers and the shortcuts it can send; " +
                    "a preset that changes it owns the whole dial",
                onClick = onOpenQuickActions,
            )
        }
        // A device setting: a preset carries no value for it.
        DeviceScopeOnly {
            ToggleRow(
                title = "Back opens quick actions",
                subtitle = "Off, Back does nothing mid-stream. It still opens them when no twist, " +
                    "keyboard or pad can",
                checked = s.backOpensRing,
                onCheckedChange = { on -> update("android.back_opens_ring", s.copy(backOpensRing = on)) },
            )
        }
    }
    SettingsGroup("Keyboard & mouse") {
        SettingDropdown(
            label = "Mouse input",
            options = MOUSE_MODE_OPTIONS,
            selected = s.mouseMode,
            field = "mouse_mode",
            caption = "Capture locks the pointer to the stream for mouse-look; Desktop leaves " +
                "it free. Ctrl+Alt+Shift+Q flips it live.",
        ) { mode -> update("mouse_mode", s.copy(mouseMode = mode)) }
        ToggleRow(
            title = "Invert scroll direction",
            subtitle = "Reverses wheel and two-finger scrolling",
            checked = s.invertScroll,
            field = "invert_scroll",
            onCheckedChange = { on -> update("invert_scroll", s.copy(invertScroll = on)) },
        )
        // Alt+Tab, the Meta chords and the Language key never reach an app; the key service
        // filters them ahead of Android. It is enabled under Accessibility, which we can only
        // open, and only after [KeyCaptureDisclosure].
        val context = LocalContext.current
        var disclose by remember { mutableStateOf(false) }
        ClickableRow(
            title = if (KeyCaptureService.running) "Keyboard shortcuts · on" else "Keyboard shortcuts",
            subtitle = if (KeyCaptureService.running) {
                "Every shortcut reaches the host, Alt+Tab and the Windows key included"
            } else {
                "Android keeps Alt+Tab and the Windows key for itself. Turn Punktfunk on under " +
                    "Accessibility to send them; until then Alt+` stands in for Alt+Tab"
            },
            onClick = { if (KeyCaptureService.running) openAccessibilitySettings(context) else disclose = true },
        )
        if (disclose) KeyCaptureDisclosure(onDismiss = { disclose = false })
        // "Shared clipboard" is NOT here: it is a trust decision about one host, so it lives on the
        // host record and is edited from that host's Edit sheet.
    }
}

@Composable
private fun AudioSettings(s: Settings, update: (String, Settings) -> Unit, onMicChange: (Boolean) -> Unit) {
    SettingsGroup(footer = "Applies from the next session.") {
        SettingDropdown(
            label = "Audio channels",
            options = AUDIO_CHANNEL_OPTIONS,
            selected = s.audioChannels,
            field = "audio_channels",
            caption = "Requested from the host; it downmixes if it has fewer.",
        ) { ch -> update("audio_channels", s.copy(audioChannels = ch)) }
        ToggleRow(
            title = "Stream microphone",
            subtitle = "Feeds this device's microphone to the host",
            checked = s.micEnabled,
            field = "mic_enabled",
            onCheckedChange = onMicChange,
        )
    }
    val d = Settings()
    AdvancedGroup(
        changed = changedCount(
            s.audioFormat != d.audioFormat,
            s.keepHostAudio != d.keepHostAudio,
            s.echoCancel != d.echoCancel,
        ),
        fields = setOf("audio_format", "keep_host_audio", "echo_cancel"),
    ) {
        // Offered at every channel count: surround negotiates a shorter frame, and only the top of
        // this list fits nothing. The HUD's `audio lossless …` line reports what a session got.
        SettingDropdown(
            label = "Audio quality",
            options = AUDIO_FORMAT_OPTIONS,
            selected = s.audioFormat,
            field = "audio_format",
            caption = "Lossless sends uncompressed audio on top of the video — 2.3 Mbps at " +
                "48 kHz, 4.6 at 96, 8.5 at 176.4 — and the top rates are often declined, " +
                "surround especially. The host has its own switch and both must be on; " +
                "otherwise the session stays on Opus, which is already effectively " +
                "transparent. The overlay shows what a session actually got.",
        ) { f -> update("audio_format", s.copy(audioFormat = f)) }
        ToggleRow(
            title = "Keep host audio playing",
            subtitle = "The host's speakers or headphones keep playing while you stream",
            checked = s.keepHostAudio,
            field = "keep_host_audio",
            onCheckedChange = { on -> update("keep_host_audio", s.copy(keepHostAudio = on)) },
        )
        ToggleRow(
            title = "Echo cancellation",
            subtitle = "Filters the stream's own audio out of the mic pickup",
            checked = s.echoCancel,
            enabled = s.micEnabled,
            field = "echo_cancel",
            onCheckedChange = { on -> update("echo_cancel", s.copy(echoCancel = on)) },
        )
    }
}

@Composable
private fun ControllerSettings(s: Settings, update: (String, Settings) -> Unit, onOpenControllers: () -> Unit) {
    SettingsGroup(footer = "Applies from the next session.") {
        SettingDropdown(
            label = "Controller type",
            options = GAMEPAD_OPTIONS,
            selected = s.gamepad,
            field = "gamepad",
            enabled = s.gamepadForwarding,
            caption = "The virtual pad the host creates. Automatic matches your controller; " +
                "every connected one is forwarded as its own player. An X-Box type has no " +
                "gyroscope, so pick a DualSense-class one if you want motion.",
        ) { g -> update("gamepad", s.copy(gamepad = g)) }
        DeviceScopeOnly {
            ToggleRow(
                title = "Controller rumble",
                subtitle = "Off, controllers don't vibrate from the stream or in the menus, whatever the game sends",
                checked = s.padRumble,
                enabled = s.gamepadForwarding,
                onCheckedChange = { on -> update("pad_rumble", s.copy(padRumble = on)) },
            )
            ClickableRow(
                title = "Connected controllers",
                subtitle = "What the app detects, with a live input test",
                onClick = onOpenControllers,
            )
            // Both rows below say "this phone" and mean this device's own body, so the form factor
            // gates them before the hardware does: a Shield reports the attached pad's vibrator and
            // gyroscope as its own (#449). `deviceBodyVibrator` stays ungated for its other callers.
            val context = LocalContext.current
            val tv = remember { isTvDevice(context) }
            val hasBodyVibrator = remember { deviceBodyVibrator(context) != null }
            if (!tv && hasBodyVibrator) {
                ToggleRow(
                    title = "Rumble on this phone",
                    subtitle = "Also play controller 1's rumble on this phone's motor",
                    checked = s.rumbleOnPhone,
                    onCheckedChange = { on -> update("android.rumble_on_phone", s.copy(rumbleOnPhone = on)) },
                )
            }
            // The rumble mirror's sibling, data flowing the other way: needs a gyroscope to
            // mirror FROM, and a body to tilt.
            val hasGyroscope = remember { DeviceGyro.available(context) }
            if (!tv && hasGyroscope) {
                ToggleRow(
                    title = "Gyro from this phone",
                    subtitle = "When the controller has no gyro, send this phone's motion " +
                        "sensors as controller 1's",
                    checked = s.gyroOnPhone,
                    onCheckedChange = { on -> update("android.gyro_on_phone", s.copy(gyroOnPhone = on)) },
                )
            }
        }
    }
    val d = Settings()
    AdvancedGroup(
        changed = changedCount(
            s.gamepadForwarding != d.gamepadForwarding,
            s.systemButtons != d.systemButtons,
            s.guideGesture != d.guideGesture,
            s.dsCapture != d.dsCapture,
            s.padHaptics != d.padHaptics,
            s.padSpeaker != d.padSpeaker,
        ),
        fields = setOf("gamepad_forwarding", "system_buttons", "guide_gesture"),
    ) {
        // The master switch, above everything it governs. Presetable, so it shows in both
        // scopes: a "Work" preset can decline to forward what "Game" forwards.
        ToggleRow(
            title = "Forward controllers",
            subtitle = "Send this device's controllers to the host. Turn it off when your " +
                "controller already reaches the host another way — USB passthrough such as " +
                "VirtualHere, or a pad plugged into the host — so games don't see two of them",
            checked = s.gamepadForwarding,
            field = "gamepad_forwarding",
            onCheckedChange = { on -> update("gamepad_forwarding", s.copy(gamepadForwarding = on)) },
        )
        SettingDropdown(
            label = "Guide button",
            options = SYSTEM_BUTTON_OPTIONS,
            selected = s.systemButtons,
            field = "system_buttons",
            enabled = s.gamepadForwarding,
            caption = "Where the guide (Xbox/PS) and share presses go while streaming. " +
                "Automatic sends them to the host whenever this device delivers them.",
        ) { v -> update("system_buttons", s.copy(systemButtons = v)) }
        SettingDropdown(
            label = "Hold Select for guide",
            options = GUIDE_GESTURE_OPTIONS,
            selected = s.guideGesture,
            field = "guide_gesture",
            enabled = s.gamepadForwarding,
            caption = "Hold Select alone to press the host's guide button — keep holding for a " +
                "Gaming-Mode host's quick-access menu. A Select tap still goes through, " +
                "slightly delayed. For devices that intercept the real guide button.",
        ) { v -> update("guide_gesture", s.copy(guideGesture = v)) }
        DeviceScopeOnly {
            // Feedback lands on the CONTROLLER's own motors and LEDs, so no vibrator gate.
            ToggleRow(
                title = "DualSense over USB",
                subtitle = "Drive a USB-connected Sony pad directly — rumble on any phone, " +
                    "plus adaptive triggers, lightbar and gyro",
                checked = s.dsCapture,
                enabled = s.gamepadForwarding,
                onCheckedChange = { on -> update("android.ds_capture", s.copy(dsCapture = on)) },
            )
            // Both only ever apply to a captured pad, so they follow that row and gate on it.
            ToggleRow(
                title = "Controller haptics",
                subtitle = "Play the host's fine-grained DualSense haptics on the pad itself — " +
                    "the pad keeps ordinary rumble for games that don't send them",
                checked = s.padHaptics,
                enabled = s.gamepadForwarding && s.dsCapture,
                onCheckedChange = { on -> update("pad_haptics", s.copy(padHaptics = on)) },
            )
            // Off by default (see Settings.padSpeaker), and a silent pad speaker looks exactly like
            // broken hardware, so the subtitle says the default out loud.
            ToggleRow(
                title = "Controller speaker",
                subtitle = "Play audio the game sends to the controller's own speaker — " +
                    "off by default, so the pad's speaker stays silent until you turn this on",
                checked = s.padSpeaker,
                enabled = s.gamepadForwarding && s.dsCapture,
                onCheckedChange = { on -> update("pad_speaker", s.copy(padSpeaker = on)) },
            )
        }
    }
}

@Composable
private fun AboutSettings(
    context: android.content.Context,
    onOpenShortcuts: () -> Unit,
    onOpenLicenses: () -> Unit,
) {
    // The app's own version, read from the installed package (the WinUI/Apple About convention:
    // identity first, then the legal rows). Empty on a harness with no real package info.
    val version = remember {
        runCatching {
            @Suppress("DEPRECATION")
            context.packageManager.getPackageInfo(context.packageName, 0).versionName
        }.getOrNull().orEmpty()
    }
    SettingsGroup {
        Column {
            Text("Punktfunk", style = MaterialTheme.typography.titleLarge)
            if (version.isNotEmpty()) {
                Text(
                    "Version $version",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        ClickableRow(
            title = "Stream controls",
            subtitle = "Keys, gestures and controller chords",
            onClick = onOpenShortcuts,
        )
        ClickableRow(
            title = "Open-source licenses",
            subtitle = "Third-party notices and credits",
            onClick = onOpenLicenses,
        )
    }
}

// ---- Row / group primitives --------------------------------------------------------------------

/**
 * A group of settings rendered inside an outlined card, with an optional sub-section [header]
 * above it and an optional form-level [footer] beneath it. The header is what turns a long
 * category into the scannable sub-sections the desktop clients have ("Resolution", "Quality",
 * "Host output"); the footer carries the one "applies from the next session" note per category —
 * per-field guidance lives on the fields themselves.
 */
@Composable
internal fun SettingsGroup(
    header: String? = null,
    footer: String? = null,
    content: @Composable ColumnScope.() -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        if (header != null) {
            Text(
                header.uppercase(),
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.primary,
                letterSpacing = 1.2.sp,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
        OutlinedCard(modifier = Modifier.fillMaxWidth()) {
            Column(
                modifier = Modifier.padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(16.dp),
                content = content,
            )
        }
        if (footer != null) {
            Text(
                footer,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
    }
}

/**
 * A title + subtitle on the left, a Switch on the right. [enabled] greys out the whole row;
 * [field] is the overlay field this row writes, which is what its override marker keys on.
 */
@Composable
private fun ToggleRow(
    title: String,
    subtitle: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    enabled: Boolean = true,
    field: String? = null,
) {
    // Dim the labels when disabled so the row reads as inactive (the Switch dims itself).
    val labelAlpha = if (enabled) 1f else 0.38f
    Column {
        OverrideBadge(field)
        Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    title,
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = labelAlpha),
                )
                Text(
                    subtitle,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = labelAlpha),
                )
            }
            Switch(checked = checked, onCheckedChange = onCheckedChange, enabled = enabled)
        }
    }
}

/** A title + subtitle on the left; the whole row is clickable (opens a sub-screen). */
@Composable
private fun ClickableRow(title: String, subtitle: String, onClick: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onClick),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(
                subtitle,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Icon(
            Icons.AutoMirrored.Filled.KeyboardArrowRight,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.size(20.dp),
        )
    }
}

/**
 * A labelled read-only dropdown over [options] (value → label); calls [onSelect] on a pick.
 * [caption] is the field's own explanation, rendered directly under the control — the `described()`
 * idiom the other clients use, so a dropdown's guidance belongs to it instead of floating as a
 * loose paragraph between rows. [field] is the overlay field this row writes, which is what its
 * override marker keys on.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun <T> SettingDropdown(
    label: String,
    options: List<Pair<T, String>>,
    selected: T,
    field: String? = null,
    caption: String? = null,
    enabled: Boolean = true,
    onSelect: (T) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = options.firstOrNull { it.first == selected }?.second
        ?: options.firstOrNull()?.second.orEmpty()
    Column {
        OverrideBadge(field)
        ExposedDropdownMenuBox(
            expanded = expanded && enabled,
            onExpandedChange = { if (enabled) expanded = it },
        ) {
            OutlinedTextField(
                value = selectedLabel,
                onValueChange = {},
                readOnly = true,
                enabled = enabled,
                label = { Text(label) },
                trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded) },
                modifier = Modifier
                    .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable)
                    .fillMaxWidth(),
            )
            ExposedDropdownMenu(
                expanded = expanded && enabled,
                onDismissRequest = { expanded = false },
            ) {
                options.forEach { (value, lbl) ->
                    DropdownMenuItem(
                        text = { Text(lbl) },
                        onClick = {
                            onSelect(value)
                            expanded = false
                        },
                    )
                }
            }
        }
        if (caption != null) {
            Text(
                caption,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 10.dp),
            )
        }
    }
}

/** One side of a custom resolution. Digits only; every non-zero keystroke commits the raw number,
 * which the caller passes through [Resolutions.custom]. The field keeps the raw text so
 * intermediate states ("15" on the way to "1512") aren't rewritten mid-typing; it snaps to the
 * committed value when focus leaves. */
@Composable
private fun ResolutionField(
    label: String,
    value: Int,
    modifier: Modifier = Modifier,
    onCommit: (Int) -> Unit,
) {
    var text by remember { mutableStateOf(if (value > 0) value.toString() else "") }
    OutlinedTextField(
        value = text,
        onValueChange = { raw ->
            text = raw.filter { it.isDigit() }.take(4)
            val v = text.toIntOrNull() ?: 0
            if (v > 0) onCommit(v)
        },
        label = { Text(label) },
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        modifier = modifier.onFocusChanged { if (!it.isFocused) text = if (value > 0) value.toString() else "" },
    )
}

/** A fixed bitrate in whole Mbps. Digits only; each keystroke commits, capped at
 * [CUSTOM_BITRATE_MAX_MBPS]. Empty or `0` commits nothing — Automatic is the menu's first entry. */
@Composable
private fun BitrateField(kbps: Int, onCommit: (Int) -> Unit) {
    val shown = if (kbps > 0) ((kbps + 500) / 1000).toString() else ""
    var text by remember { mutableStateOf(shown) }
    OutlinedTextField(
        value = text,
        onValueChange = { raw ->
            text = raw.filter { it.isDigit() }.take(4)
            val mbps = (text.toIntOrNull() ?: 0).coerceAtMost(CUSTOM_BITRATE_MAX_MBPS)
            if (mbps > 0) onCommit(mbps * 1000)
        },
        label = { Text("Bitrate (Mbps)") },
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        modifier = Modifier.fillMaxWidth().onFocusChanged { if (!it.isFocused) text = shown },
    )
}

/**
 * Names the host the Start in row resolves to, and says when it resolves to nothing — which is
 * what every value does until one host is paired. The pointer is written from a host's own card
 * menu, not from this screen, so the caption is where the two meet.
 */
private fun startInCaption(s: Settings, context: android.content.Context): String {
    val hosts = KnownHostStore(context).all()
    val host = StartScreen.defaultHost(s.defaultHost, hosts).host
        ?: return "Opens on the host list: there is no default host yet. Pair one, or pick one " +
            "from a host's menu when several are paired."
    return "Library opens ${host.name}'s games; Stream also connects to its desktop. " +
        "Back leaves either one on the host list."
}
