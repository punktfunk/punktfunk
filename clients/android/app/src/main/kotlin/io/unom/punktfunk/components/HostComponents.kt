package io.unom.punktfunk.components

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Key
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.LockOpen
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.models.HostStatus

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

/**
 * One row of a host card's overflow menu. [startsSection] draws a divider above it, which is how
 * the preset actions ("Connect with: …", "Pin as card: …") stay legible next to the host actions
 * in one flat menu — Compose has no submenus, and the Windows client made the same call.
 */
data class HostMenuItem(
    val label: String,
    val startsSection: Boolean = false,
    val onClick: () -> Unit,
)

/**
 * A host as an Apple-style card: a colored avatar carrying the host's OS mark (its initial when we
 * don't know the OS), name + address, a trust pill, and (for saved hosts) an overflow menu with
 * Wake / Edit / Forget plus whatever [menuItems] adds. Tapping the card connects.
 *
 * [presetLabel] names the settings preset this card connects with. On a host's own card that is
 * its default binding, drawn as a quiet chip — the card says what a tap will do. On a **pinned
 * card** ([presetProminent]) the host name is still the title, but the preset is the loud part,
 * because the pin exists to make that one combination a single tap.
 */
@Composable
fun HostCard(
    name: String,
    address: String,
    status: HostStatus,
    online: Boolean = false,
    /** OS-identity chain (mDNS `os` TXT / stored), drawn as the avatar's mark. "" = the initial. */
    os: String = "",
    enabled: Boolean,
    onConnect: () -> Unit,
    onForget: (() -> Unit)?,
    onEdit: (() -> Unit)? = null,
    onWake: (() -> Unit)? = null,
    presetLabel: String? = null,
    presetProminent: Boolean = false,
    accent: Color? = null,
    menuItems: List<HostMenuItem> = emptyList(),
    /**
     * Keep the preset chip's space even on a card that has no preset. `LazyVerticalGrid` sizes a
     * row to its tallest item but does NOT stretch the others, so a card that grew a chip would
     * leave its neighbour visibly short — a row of cards stepping up and down reads as broken
     * layout. The caller passes true when ANY card in that section carries a chip, so a user with
     * no presets never pays for the slot.
     */
    reservePresetSlot: Boolean = false,
    /** The profile this device plays as on the host; drawn as a small initials mark. */
    profileName: String? = null,
) {
    // D-pad / controller focus highlight: a clickable card is focusable, but the default state
    // layer is too subtle on a TV across a room — draw a clear primary-colour border when focused.
    var focused by remember { mutableStateOf(false) }
    ElevatedCard(
        onClick = onConnect,
        enabled = enabled,
        modifier = Modifier
            .fillMaxWidth()
            .padding(4.dp)
            .onFocusChanged { focused = it.isFocused }
            .then(
                if (focused) {
                    Modifier.border(2.dp, MaterialTheme.colorScheme.primary, CardDefaults.elevatedShape)
                } else {
                    Modifier
                },
            ),
    ) {
        Box(modifier = Modifier.fillMaxWidth()) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(16.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Box {
                    HostAvatar(name, online, os)
                    if (profileName != null) {
                        Box(Modifier.align(Alignment.BottomEnd)) {
                            io.unom.punktfunk.ProfileFace(profileName, 24)
                        }
                    }
                }
                Spacer(Modifier.height(10.dp))
                Text(
                    name,
                    style = MaterialTheme.typography.titleMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    textAlign = TextAlign.Center,
                )
                Text(
                    address,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    textAlign = TextAlign.Center,
                )
                if (presetLabel != null || reservePresetSlot) {
                    Spacer(Modifier.height(10.dp))
                    Box(
                        Modifier.heightIn(min = PRESET_CHIP_SLOT),
                        contentAlignment = Alignment.Center,
                    ) {
                        if (presetLabel != null) {
                            PresetChip(presetLabel, accent, prominent = presetProminent)
                        }
                    }
                }
            }

            // Trust state lives in the free top-left corner, mirroring the overflow on the right —
            // it costs no height, and it is a state you glance at rather than read. The label is
            // still there for TalkBack, and the trust DECISION is made in a dialog that spells all
            // of this out; on the card it only has to say "this one is settled" vs "this one will
            // ask something of you".
            TrustBadge(status, Modifier.align(Alignment.TopStart))

            if (onForget != null || onEdit != null || onWake != null || menuItems.isNotEmpty()) {
                var menu by remember { mutableStateOf(false) }
                Box(modifier = Modifier.align(Alignment.TopEnd)) {
                    IconButton(enabled = enabled, onClick = { menu = true }) {
                        Icon(
                            Icons.Filled.MoreVert,
                            contentDescription = "More",
                            modifier = Modifier.size(20.dp),
                            tint = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = 0.6f)
                        )
                    }
                    DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                        if (onWake != null) {
                            DropdownMenuItem(
                                text = { Text("Wake host") },
                                onClick = {
                                    menu = false
                                    onWake()
                                },
                            )
                        }
                        if (onEdit != null) {
                            DropdownMenuItem(
                                text = { Text("Edit…") },
                                onClick = {
                                    menu = false
                                    onEdit()
                                },
                            )
                        }
                        if (onForget != null) {
                            DropdownMenuItem(
                                text = { Text("Forget") },
                                onClick = {
                                    menu = false
                                    onForget()
                                },
                            )
                        }
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
        }
    }
}

/**
 * The preset a card connects with. Quiet on a bound host's own card (it is a note about what a tap
 * does); filled and tinted on a pinned card, where the preset IS the reason the card exists — the
 * accent field the schema reserves earns its keep here.
 */
@Composable
private fun PresetChip(label: String, accent: Color?, prominent: Boolean) {
    val tint = accent ?: MaterialTheme.colorScheme.primary
    Row(
        modifier = Modifier
            .clip(RoundedCornerShape(50))
            .background(tint.copy(alpha = if (prominent) 0.24f else 0.12f))
            .padding(horizontal = 10.dp, vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(7.dp).clip(CircleShape).background(tint))
        Spacer(Modifier.width(6.dp))
        Text(
            label,
            style = if (prominent) {
                MaterialTheme.typography.labelLarge
            } else {
                MaterialTheme.typography.labelMedium
            },
            color = tint,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

/**
 * Reserved height for the preset chip — the one part of a card that varies. `LazyVerticalGrid`
 * sizes a row to its tallest item and does NOT stretch the others, so a card that grew a chip its
 * neighbour lacks would leave the row stepping up and down.
 *
 * `heightIn(min =)`, not a fixed height: at a large accessibility font scale the chip must be
 * allowed to grow rather than clip, and the reservation is sized with room to spare because the
 * equal-height guarantee only holds while every card fits INSIDE it.
 */
private val PRESET_CHIP_SLOT = 26.dp

/** Live presence, on any dynamic scheme: green reads as "up" to everyone, and Material You's
 * primary might be any hue at all — including a green that would then mean nothing. */
private val PRESENCE_ONLINE = Color(0xFF4ADE80)

/**
 * The host's avatar (Apple-contact style) with its presence as a dot on the corner — the idiom
 * every contact list already uses, and one fewer labelled badge on a small card. It carries the
 * host's OS mark when [os] resolves to one we ship, and the host's initial otherwise.
 *
 * [online] is true when the host advertises on mDNS OR answers the reachability probe, so a
 * routed/VPN host that never advertises still reads as up. Online is a FILLED green dot, offline a
 * hollow grey ring: the difference is a shape as well as a colour, so it survives both a
 * colour-blind reader and a screenshot in greyscale. TalkBack gets the word either way.
 */
@Composable
fun HostAvatar(name: String, online: Boolean = false, os: String = "") {
    val letter = name.trim().firstOrNull()?.uppercaseChar()?.toString() ?: "?"
    val cardColor = CardDefaults.elevatedCardColors().containerColor
    val osIcon = resolveOsIcon(os)
    Box {
        Box(
            modifier = Modifier
                .size(44.dp)
                .clip(CircleShape)
                .background(MaterialTheme.colorScheme.primaryContainer),
            contentAlignment = Alignment.Center,
        ) {
            // The OS mark IS the avatar when we know the OS — it identifies the machine better than
            // the initial ever did, and it's the same circle, so a card whose host advertises no OS
            // (or one we ship no mark for) keeps the letter and the row still reads as one set.
            if (osIcon != null) {
                Icon(
                    osIcon,
                    contentDescription = os,
                    modifier = Modifier.size(24.dp),
                    tint = MaterialTheme.colorScheme.onPrimaryContainer,
                )
            } else {
                Text(
                    letter,
                    style = MaterialTheme.typography.titleMedium,
                    color = MaterialTheme.colorScheme.onPrimaryContainer,
                )
            }
        }
        Box(
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .size(13.dp)
                .clip(CircleShape)
                // A ring in the card's own colour is what makes the dot read as sitting ON the
                // avatar rather than beside it.
                .background(cardColor)
                .padding(2.dp)
                .clip(CircleShape)
                .then(
                    if (online) {
                        Modifier.background(PRESENCE_ONLINE)
                    } else {
                        Modifier
                            .background(cardColor)
                            .border(1.5.dp, MaterialTheme.colorScheme.onSurfaceVariant, CircleShape)
                    },
                )
                .semantics { contentDescription = if (online) "Online" else "Offline" },
        )
    }
}

/**
 * The host's trust state as a corner glyph: locked (paired — nothing more to do), a key (this host
 * will ask for a PIN), or an open lock (trust-on-first-use, the weakest of the three). The full
 * label rides along as the content description, and the dialogs that actually make the decision
 * spell it out in sentences.
 */
@Composable
private fun TrustBadge(status: HostStatus, modifier: Modifier = Modifier) {
    val (icon, tint) = when (status) {
        HostStatus.PAIRED -> Icons.Filled.Lock to MaterialTheme.colorScheme.primary
        HostStatus.PAIRING -> Icons.Filled.Key to MaterialTheme.colorScheme.tertiary
        HostStatus.TOFU -> Icons.Filled.LockOpen to MaterialTheme.colorScheme.onSurfaceVariant
    }
    Icon(
        icon,
        contentDescription = status.label,
        tint = tint.copy(alpha = 0.85f),
        modifier = modifier.padding(14.dp).size(18.dp),
    )
}

/** Shown when there are no saved or discovered hosts. */
@Composable
fun EmptyHostsState() {
    Column(
        modifier = Modifier.fillMaxWidth().padding(vertical = 56.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("No hosts yet", style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(8.dp))
        Text(
            "Hosts on your network show up here automatically.\nTap “Add host” to enter one by address.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}
