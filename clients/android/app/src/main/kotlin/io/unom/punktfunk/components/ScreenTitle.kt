package io.unom.punktfunk.components

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R

/** The margin a screen's heading and its content share. */
val SCREEN_MARGIN = 20.dp

/** How tall [ScreenTitle] stands, for a bar that swaps it for something else. */
val SCREEN_TITLE_HEIGHT = 64.dp

/**
 * A screen's heading: one size and one place on every tab and every pushed page, with the
 * screen's acts trailing. [onBack] makes it a pushed page's: a back arrow leads.
 */
@Composable
fun ScreenTitle(
    text: String,
    modifier: Modifier = Modifier,
    onBack: (() -> Unit)? = null,
    actions: @Composable RowScope.() -> Unit = {},
) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        // 48 dp for the buttons under 16 dp: SCREEN_TITLE_HEIGHT in all.
        modifier = modifier.fillMaxWidth().padding(start = SCREEN_MARGIN, end = 4.dp, top = 16.dp).height(48.dp),
    ) {
        if (onBack != null) {
            IconButton(onClick = onBack, modifier = Modifier.padding(end = 4.dp)) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.common_back))
            }
        }
        Text(
            text,
            style = MaterialTheme.typography.headlineMedium,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f).semantics { heading() },
        )
        actions()
    }
}
