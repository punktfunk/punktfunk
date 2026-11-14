package io.unom.punktfunk.components

import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Star
import androidx.compose.material.icons.outlined.Info
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.ProfileArt
import io.unom.punktfunk.ProfileFace
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.firstLetter

/** Left-aligned section header above each block of the connect screen. */
@Composable
fun SectionLabel(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.fillMaxWidth().padding(bottom = 8.dp),
    )
}

/** One row of a host card's long-press menu. [startsSection] draws a divider above it. */
data class HostMenuItem(
    val label: String,
    val startsSection: Boolean = false,
    val onClick: () -> Unit,
)

/**
 * A host's state as one sentence: its card's second line and its page's subtitle. [Found] and
 * [PairingRequired] are a discovered host's, which has no record yet.
 */
sealed interface HostLine {
    data object Connecting : HostLine
    data class Playing(val game: String) : HostLine
    data object Online : HostLine
    data object NotPaired : HostLine

    /** Offline, and a connect sends a Wake-on-LAN packet before it dials. */
    data object OfflineWakes : HostLine
    data object Offline : HostLine
    data object Found : HostLine
    data object PairingRequired : HostLine

    /** The host answers: a green dot. */
    val up: Boolean get() = this !is Offline && this !is OfflineWakes && this !is Connecting

    @Composable
    fun text(): String = when (this) {
        Connecting -> stringResource(R.string.hostcard_status_connecting)
        is Playing -> stringResource(R.string.hostcard_status_playing, game)
        Online -> stringResource(R.string.hostcard_status_online)
        NotPaired -> stringResource(R.string.hostcard_status_not_paired)
        OfflineWakes -> stringResource(R.string.hostcard_status_offline_wakes)
        Offline -> stringResource(R.string.hostcard_status_offline)
        Found -> stringResource(R.string.hostcard_status_found)
        PairingRequired -> stringResource(R.string.hostcard_status_pairing_required)
    }

    companion object {
        /**
         * A saved host's sentence. [pinned]: the record holds the host's fingerprint. [wakes]:
         * auto-wake is on and a MAC is known.
         */
        fun of(pinned: Boolean, online: Boolean, connecting: Boolean, playing: String?, wakes: Boolean): HostLine =
            when {
                connecting -> Connecting
                online && !pinned -> NotPaired
                online && playing != null -> Playing(playing)
                online -> Online
                wakes -> OfflineWakes
                else -> Offline
            }
    }
}

/**
 * A host in two lines: its name, then [status] with the preset it connects with. A tap connects, a
 * long press opens [menuItems], and the info button opens the host page ([onDetails]). A pinned
 * card ([presetProminent]) is a shortcut and has no page; a discovered one shows its [address].
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun HostCard(
    name: String,
    status: HostLine,
    /** OS-identity chain (mDNS `os` TXT / stored), drawn as the tile's mark. "" = the initial. */
    os: String = "",
    enabled: Boolean,
    onConnect: () -> Unit,
    onDetails: (() -> Unit)? = null,
    /** The record holds the host's identity: a filled tile. A discovered host's is outlined. */
    filled: Boolean = true,
    address: String? = null,
    presetLabel: String? = null,
    presetProminent: Boolean = false,
    accent: Color? = null,
    menuItems: List<HostMenuItem> = emptyList(),
    /** The host Start in opens on: a star on the tile. */
    isDefault: Boolean = false,
    /** The profile this device plays as on the host; drawn as a small face. */
    profileName: String? = null,
    profilePicture: String? = null,
    profileArt: ProfileArt? = null,
    /** The screen a connect gets when it is not the only screen: "Second screen", "Streams LG TV". */
    screenLine: String? = null,
) {
    // The default state layer is too faint for a D-pad across a room.
    var focused by remember { mutableStateOf(false) }
    var menu by remember { mutableStateOf(false) }
    val shape = RoundedCornerShape(16.dp)
    val moreLabel = stringResource(R.string.hostcard_more_a11y)
    Box {
        Surface(
            shape = shape,
            color = MaterialTheme.colorScheme.surfaceContainerHigh,
            border = BorderStroke(
                if (focused) 2.dp else 1.dp,
                if (focused) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.outlineVariant,
            ),
            modifier = Modifier
                .fillMaxWidth()
                .clip(shape)
                .onFocusChanged { focused = it.isFocused }
                .combinedClickable(
                    enabled = enabled,
                    onLongClickLabel = moreLabel.takeIf { menuItems.isNotEmpty() },
                    onLongClick = { if (menuItems.isNotEmpty()) menu = true },
                    onClick = onConnect,
                ),
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.padding(start = 14.dp, top = 14.dp, bottom = 14.dp, end = 4.dp),
            ) {
                Box {
                    HostTile(
                        name, os, filled = filled, connecting = status == HostLine.Connecting,
                        modifier = Modifier.alpha(if (status.up || status == HostLine.Connecting) 1f else 0.55f),
                    )
                    if (isDefault) DefaultHostBadge(Modifier.align(Alignment.BottomEnd).offset(4.dp, 4.dp))
                    if (profileName != null) {
                        Box(Modifier.align(Alignment.TopEnd).offset(5.dp, (-5).dp)) {
                            ProfileFace(profileName, 20, picture = profilePicture, art = profileArt)
                        }
                    }
                }
                Spacer(Modifier.width(14.dp))
                Column(Modifier.weight(1f)) {
                    Text(
                        name,
                        style = MaterialTheme.typography.titleMedium,
                        fontWeight = FontWeight.SemiBold,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                    if (address != null) {
                        Text(
                            address,
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    // One height with or without a chip, so every card in a row stands the same.
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        modifier = Modifier.height(STATUS_ROW),
                    ) {
                        HostStatusLine(
                            status,
                            // A plain "Online" says nothing beside the preset a tap uses.
                            dotOnly = presetLabel != null && status == HostLine.Online,
                            modifier = Modifier.weight(1f, fill = false),
                        )
                        if (presetLabel != null) PresetChip(presetLabel, accent, prominent = presetProminent)
                        if (screenLine != null) {
                            Text(
                                screenLine,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                modifier = Modifier.weight(1f, fill = false),
                            )
                        }
                    }
                }
                if (onDetails != null) {
                    IconButton(onClick = onDetails) {
                        Icon(
                            Icons.Outlined.Info,
                            contentDescription = stringResource(R.string.hostcard_details_a11y, name),
                            tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                } else {
                    Spacer(Modifier.width(10.dp))
                }
            }
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            menuItems.forEach { item ->
                if (item.startsSection) HorizontalDivider()
                DropdownMenuItem(
                    text = { Text(item.label) },
                    onClick = {
                        menu = false
                        item.onClick()
                    },
                )
            }
        }
    }
}

/** Line two's height: the chip's, so a card with a preset is no taller than one without. */
private val STATUS_ROW = 26.dp

/**
 * [status] behind its presence dot: green while the host answers, a hollow ring when it doesn't,
 * none while dialing. A game up reads in green. [dotOnly] drops the words, kept for TalkBack.
 */
@Composable
fun HostStatusLine(status: HostLine, modifier: Modifier = Modifier, dotOnly: Boolean = false) {
    val words = status.text()
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = modifier.clearAndSetSemantics { contentDescription = words },
    ) {
        if (status != HostLine.Connecting) {
            Box(
                Modifier
                    .size(8.dp)
                    .clip(CircleShape)
                    .then(
                        if (status.up) {
                            Modifier.background(PRESENCE_ONLINE)
                        } else {
                            // A shape as well as a colour: it survives greyscale.
                            Modifier.border(1.5.dp, MaterialTheme.colorScheme.onSurfaceVariant, CircleShape)
                        },
                    ),
            )
        }
        if (!dotOnly) {
            Text(
                words,
                style = MaterialTheme.typography.bodyMedium,
                color = if (status is HostLine.Playing) PRESENCE_ONLINE else MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(start = if (status == HostLine.Connecting) 0.dp else 6.dp),
            )
        }
    }
}

/**
 * The preset a card connects with. Quiet on a bound host's own card, where it says what a tap
 * does; filled on a pinned card, where the preset is why the card exists.
 */
@Composable
private fun PresetChip(label: String, accent: Color?, prominent: Boolean) {
    val tint = accent ?: MaterialTheme.colorScheme.primary
    Row(
        modifier = Modifier
            .clip(RoundedCornerShape(50))
            .background(tint.copy(alpha = if (prominent) 0.24f else 0.12f))
            .padding(horizontal = 9.dp, vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(7.dp).clip(CircleShape).background(tint))
        Spacer(Modifier.width(6.dp))
        Text(
            label,
            style = MaterialTheme.typography.labelMedium,
            fontWeight = if (prominent) FontWeight.Bold else FontWeight.Medium,
            color = tint,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

/** Fixed, not the theme's primary: Material You may seed a green that would then mean nothing. */
internal val PRESENCE_ONLINE = Color(0xFF4ADE80)

/**
 * The host's squared tile: its OS mark when [os] resolves to one we ship, else its initial.
 * [filled] is a saved identity; an outline is a host not saved or not paired yet. [connecting]
 * spins in the mark's place.
 */
@Composable
fun HostTile(
    name: String,
    os: String,
    modifier: Modifier = Modifier,
    filled: Boolean = true,
    connecting: Boolean = false,
    size: Dp = 48.dp,
) {
    val scheme = MaterialTheme.colorScheme
    val shape = RoundedCornerShape(size * 0.27f)
    val ink = if (filled) scheme.onPrimaryContainer else scheme.primary
    Box(
        modifier = modifier
            .size(size)
            .clip(shape)
            .background(if (filled) scheme.primaryContainer else scheme.primary.copy(alpha = 0.14f))
            .then(if (filled) Modifier else Modifier.border(1.dp, scheme.primary.copy(alpha = 0.45f), shape)),
        contentAlignment = Alignment.Center,
    ) {
        val osIcon = resolveOsIcon(os)
        when {
            connecting -> CircularProgressIndicator(Modifier.size(size * 0.45f), color = ink, strokeWidth = 2.dp)
            osIcon != null -> Icon(osIcon, contentDescription = os, modifier = Modifier.size(size * 0.5f), tint = ink)
            else -> Text(
                name.trim().takeIf { it.isNotEmpty() }?.let(::firstLetter) ?: "?",
                style = if (size >= 64.dp) MaterialTheme.typography.headlineSmall else MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.Bold,
                color = ink,
            )
        }
    }
}

/** The default host's mark: a star on the tile's corner, ringed in the card's colour. */
@Composable
private fun DefaultHostBadge(modifier: Modifier = Modifier) {
    val label = stringResource(R.string.hostcard_default_host)
    Box(
        modifier = modifier
            .size(18.dp)
            .clip(CircleShape)
            .background(MaterialTheme.colorScheme.surfaceContainerHigh)
            .padding(1.5.dp)
            .clip(CircleShape)
            .background(MaterialTheme.colorScheme.primary)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            Icons.Filled.Star,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.onPrimary,
            modifier = Modifier.size(10.dp),
        )
    }
}

/** Shown when there are no saved or discovered hosts. */
@Composable
fun EmptyHostsState() {
    Column(
        modifier = Modifier.fillMaxWidth().padding(vertical = 56.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text(stringResource(R.string.hosts_empty_title), style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(8.dp))
        Text(
            stringResource(R.string.hosts_empty_body),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}
