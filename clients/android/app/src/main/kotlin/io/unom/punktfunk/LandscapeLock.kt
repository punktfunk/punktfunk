package io.unom.punktfunk

import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext

/** Under this smallest width a window is a phone's. Android's own large-screen line. */
internal const val PHONE_SW_DP = 600

/**
 * Holds a phone's window in landscape while [hold]: the stream's picture and the console are
 * landscape layouts. Tablets, unfolded foldables and desktop windows stay free. The size class is
 * read live, so folding or unfolding re-decides. Holders stack on [MainActivity.holdLandscape].
 */
@Composable
internal fun HoldLandscape(holder: String, hold: Boolean) {
    val activity = LocalContext.current as? MainActivity ?: return
    val on = hold && LocalConfiguration.current.smallestScreenWidthDp < PHONE_SW_DP
    DisposableEffect(activity, on) {
        activity.holdLandscape(holder, on)
        onDispose { activity.holdLandscape(holder, false) }
    }
}
