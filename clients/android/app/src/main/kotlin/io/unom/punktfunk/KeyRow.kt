package io.unom.punktfunk

import android.os.Build
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.imeAnimationTarget
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardHide
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Shortcuts
import io.unom.punktfunk.kit.SlotId

/**
 * The key row over the summoned soft keyboard: [buttons], the `row` of the quick actions the
 * player set up, each a key or a quick action. A modifier latches in [latch]; the row leaving
 * lets every one go. A long press holds a key down so the host repeats it ([RowButton]). An
 * action goes to [fire], and [cfg] and [actions] draw its face as the ring would. [onPress] runs
 * before each button, so the IME settles its composition first. The row's surface takes every
 * touch on it, so none reaches the stream's gesture layer below.
 *
 * It docks on the keyboard's top edge, drawn only once the keyboard has come to rest. Below API 30
 * androidx (core 1.19.1, `WindowInsetsCompat.Impl20`) reads the keyboard from the content and
 * visible insets, which leave it out under the stream's `SOFT_INPUT_ADJUST_NOTHING`
 * ([StreamWindow]), so there the row takes the screen's top.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
internal fun BoxScope.KeyRow(
    buttons: List<RowSlot>,
    cfg: OverlayConfig,
    actions: RingActions,
    latch: KeyLatch,
    haptics: ConsoleHaptics,
    onPress: () -> Unit,
    fire: (Shortcuts.Action, Shortcuts.Phase) -> Unit,
) {
    val density = LocalDensity.current
    val docked = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
    val target = if (docked) WindowInsets.imeAnimationTarget.getBottom(density) else 0
    val ime = if (docked) WindowInsets.ime.getBottom(density) else 0
    if (buttons.isEmpty() || (docked && target == 0)) return
    DisposableEffect(latch) { onDispose { latch.release() } }
    // Mid-slide the keyboard's edge is not where it will stop: hidden, the latch kept.
    if (ime != target) return
    val sides = if (docked) WindowInsetsSides.Horizontal else WindowInsetsSides.Horizontal + WindowInsetsSides.Top
    val send by rememberUpdatedState(fire)
    Surface(
        color = MaterialTheme.colorScheme.surface,
        modifier = Modifier
            .align(if (docked) Alignment.BottomStart else Alignment.TopStart)
            .offset { IntOffset(0, -ime) }
            .fillMaxWidth(),
    ) {
        Row(
            Modifier
                .windowInsetsPadding(WindowInsets.safeDrawing.only(sides))
                .height(44.dp)
                .horizontalScroll(rememberScrollState())
                .padding(horizontal = 4.dp, vertical = 4.dp),
        ) {
            buttons.forEachIndexed { i, slot ->
                val finger = remember(i, slot, latch) { RowButton(latch, slot) { action, phase -> send(action, phase) } }
                val press = { act: () -> Unit ->
                    haptics.tick()
                    onPress()
                    act()
                }
                RowCap(
                    slot, cfg, actions, latch,
                    Modifier.fillMaxHeight().widthIn(min = 52.dp, max = 160.dp).padding(horizontal = 2.dp)
                        .semantics { onClick { press(finger::tap); true } }
                        // A drag that starts on a button scrolls the row, so a button fires at the
                        // tap's end. The row leaving mid-hold cancels the press, which lifts it too.
                        .pointerInput(finger) {
                            detectTapGestures(
                                onPress = {
                                    try {
                                        tryAwaitRelease()
                                    } finally {
                                        finger.end()
                                    }
                                },
                                onLongPress = { press(finger::longPress) },
                                onTap = { press(finger::tap) },
                            )
                        },
                )
            }
        }
    }
}

/**
 * One row button's face: a key's legend, lit while it is latched; a key combination's name or
 * chord; any other action's icon as the ring draws it, dimmed while the action cannot run. The
 * row and its editor both draw it.
 */
@Composable
internal fun RowCap(slot: RowSlot, cfg: OverlayConfig, actions: RingActions, latch: KeyLatch?, modifier: Modifier) {
    when (slot) {
        is RowSlot.Key -> rowKey(slot.name)?.let { KeyCap(it, armed = latch != null && it.vk in latch.latched, modifier) }
        is RowSlot.Action -> {
            val s = spec(slot.slot, cfg, actions, textContext())
            // On the row the Keyboard action hides the keyboard the row sits on.
            val hides = slot.slot == SlotId.Keyboard
            Cap(armed = false, modifier.alpha(if (s.enabled) 1f else 0.45f), state = s.state) {
                val icon = if (hides) Icons.Filled.KeyboardHide else s.icon
                val label = if (hides) stringResource(R.string.controls_hide_keyboard) else s.label
                if (icon != null) {
                    Icon(icon, contentDescription = label, modifier = Modifier.size(20.dp))
                } else {
                    Text(
                        s.label,
                        style = MaterialTheme.typography.labelLarge,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.padding(horizontal = 8.dp),
                    )
                }
            }
        }
    }
}
