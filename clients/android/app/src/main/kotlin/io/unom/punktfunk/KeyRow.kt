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
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp

/**
 * The key row over the summoned soft keyboard: [KEY_ROW], the keys a phone's keyboard lacks. A
 * modifier latches in [latch]; the row leaving lets every one go. [onPress] runs before each key,
 * so the IME settles its composition first. The row's surface takes every touch on it, so none
 * reaches the stream's gesture layer below.
 *
 * It docks on the keyboard's top edge, drawn only once the keyboard has come to rest. Below API 30
 * androidx (core 1.19.1, `WindowInsetsCompat.Impl20`) reads the keyboard from the content and
 * visible insets, which leave it out under the stream's `SOFT_INPUT_ADJUST_NOTHING`
 * ([StreamWindow]), so there the row takes the screen's top.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
internal fun BoxScope.KeyRow(latch: KeyLatch, haptics: ConsoleHaptics, onPress: () -> Unit) {
    val density = LocalDensity.current
    val docked = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
    val target = if (docked) WindowInsets.imeAnimationTarget.getBottom(density) else 0
    val ime = if (docked) WindowInsets.ime.getBottom(density) else 0
    if (docked && target == 0) return
    DisposableEffect(latch) { onDispose { latch.release() } }
    // Mid-slide the keyboard's edge is not where it will stop: hidden, the latch kept.
    if (ime != target) return
    val sides = if (docked) WindowInsetsSides.Horizontal else WindowInsetsSides.Horizontal + WindowInsetsSides.Top
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
            for (key in KEY_ROW) {
                val tap = {
                    haptics.tick()
                    onPress()
                    latch.press(key)
                    latch.lift(key)
                }
                KeyCap(
                    key,
                    armed = key.vk in latch.latched,
                    modifier = Modifier.fillMaxHeight().widthIn(min = 52.dp).padding(horizontal = 2.dp)
                        .semantics { onClick { tap(); true } }
                        // A tap, not a press: a drag that starts on a key scrolls the row.
                        .pointerInput(key, latch) { detectTapGestures { tap() } },
                )
            }
        }
    }
}
