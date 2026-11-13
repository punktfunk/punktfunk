package io.unom.punktfunk

import androidx.compose.runtime.Composable
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import io.unom.punktfunk.kit.Say

/** This line in the screen's language. Reads the configuration, so a language change recomposes. */
@Composable
internal fun Say.asString(): String {
    LocalConfiguration.current
    return words(LocalContext.current.resources)
}
