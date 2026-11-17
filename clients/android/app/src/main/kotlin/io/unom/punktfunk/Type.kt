package io.unom.punktfunk

import androidx.compose.material3.Typography
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight

// Geist — the punktfunk brand typeface (the same family the website and the Apple client ship).
// Bundled as static OTF weights in res/font and applied to every Material 3 text style below, so the
// Android UI carries the brand type identically to the other clients. Geist Sans only — Geist Mono
// is intentionally not shipped (the licenses screen's technical block uses the platform monospace).
//
// Licensed under the SIL Open Font License 1.1 (assets/GEIST-OFL.txt, on the licenses screen).
val Geist = FontFamily(
    Font(R.font.geist_regular, FontWeight.Normal),
    Font(R.font.geist_medium, FontWeight.Medium),
    Font(R.font.geist_semibold, FontWeight.SemiBold),
    Font(R.font.geist_bold, FontWeight.Bold),
)

/**
 * The default Material 3 type scale re-based on [Geist]. Material 3's [Typography] has no
 * `defaultFontFamily` shortcut (that was Material 2), so each of the 15 roles is re-emitted with the
 * Geist family while keeping Material's sizes, line heights, letter spacing and per-role weights.
 */
val PunktfunkTypography: Typography = Typography().run {
    Typography(
        displayLarge = displayLarge.copy(fontFamily = Geist),
        displayMedium = displayMedium.copy(fontFamily = Geist),
        displaySmall = displaySmall.copy(fontFamily = Geist),
        headlineLarge = headlineLarge.copy(fontFamily = Geist),
        headlineMedium = headlineMedium.copy(fontFamily = Geist),
        headlineSmall = headlineSmall.copy(fontFamily = Geist),
        titleLarge = titleLarge.copy(fontFamily = Geist),
        titleMedium = titleMedium.copy(fontFamily = Geist),
        titleSmall = titleSmall.copy(fontFamily = Geist),
        bodyLarge = bodyLarge.copy(fontFamily = Geist),
        bodyMedium = bodyMedium.copy(fontFamily = Geist),
        bodySmall = bodySmall.copy(fontFamily = Geist),
        labelLarge = labelLarge.copy(fontFamily = Geist),
        labelMedium = labelMedium.copy(fontFamily = Geist),
        labelSmall = labelSmall.copy(fontFamily = Geist),
    )
}
