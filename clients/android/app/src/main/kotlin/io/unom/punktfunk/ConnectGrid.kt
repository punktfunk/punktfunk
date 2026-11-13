package io.unom.punktfunk

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.components.EmptyHostsState
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.HostLine
import io.unom.punktfunk.components.HostMenuItem
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.security.KnownHost

/**
 * The touch home: the saved and discovered hosts with the Add-host button over them.
 *
 * Pure display: every act arrives as a callback and ends in state the screen owns. This file owns
 * the arrangement and each card's long-press menu, which holds the daily acts only. Setup,
 * diagnostics, power and removal live on the host page ([HostDetailScreen]), opened by a card's
 * info button. A pinned card is a shortcut: it has no page and offers only its own acts.
 */
@Composable
internal fun ConnectGrid(
    savedHosts: List<KnownHost>,
    /** Every live advert: the OS mark prefers it over the stored one, and "searching…" reads it. */
    discovered: List<DiscoveredHost>,
    /** Adverts with no saved record behind them. */
    discoveredUnsaved: List<DiscoveredHost>,
    /** The ids of the saved hosts that answered the last probe. */
    reachable: Set<String>,
    presets: List<StreamPreset>,
    pinsFor: (KnownHost) -> List<StreamPreset>,
    connecting: Boolean,
    /** A confirmation ("75 Mbit/s set in …"); [status] is the failure line. */
    notice: String?,
    status: String?,
    lnpGranted: Boolean,
    onAskLocalNetwork: () -> Unit,
    /**
     * Dial a saved host. The second argument is the one-off preset: null follows the host's
     * binding, a preset id forces that preset, and "" forces the global defaults.
     */
    onConnect: (KnownHost, String?) -> Unit,
    onConnectDiscovered: (DiscoveredHost) -> Unit,
    /** Open the host page. */
    onDetails: (KnownHost) -> Unit,
    onWake: (KnownHost) -> Unit,
    /** Open the profile picker for this host; offered once it has a saved pick. */
    onSwitchProfile: (KnownHost) -> Unit,
    /** What each paired host last said this device may stream, by fingerprint: the card's line. */
    screens: Map<String, DisplayTargets.Targets>,
    /** What each host has up, by fingerprint: "Playing …". */
    nowPlaying: Map<String, String>,
    onCopyLink: (KnownHost, StreamPreset?) -> Unit,
    onTogglePin: (KnownHost, StreamPreset) -> Unit,
    /** Open this card's library; a pinned card's shelf launches with its preset. */
    onBrowseLibrary: (KnownHost, StreamPreset?) -> Unit,
    /** The host Start in opens on, explicit or derived: its card wears the star. */
    defaultHostId: String?,
    /** A connect wakes a sleeping host first: "Offline · wakes on connect". */
    autoWake: Boolean,
    onRescan: () -> Unit,
    onAddHost: () -> Unit,
) {
    // "Connect with" is a one-off on every card: it never rebinds the host. Rebinding is the
    // host page's.
    @Composable
    fun hostMenu(kh: KnownHost, pin: StreamPreset?): List<HostMenuItem> = buildList {
        // The fetch authenticates with the pinned identity, so only a paired card browses.
        if (kh.paired) {
            add(HostMenuItem(stringResource(R.string.hostcard_browse_library)) { onBrowseLibrary(kh, pin) })
        }
        if (pin != null) {
            add(HostMenuItem(stringResource(R.string.hostcard_copy_link)) { onCopyLink(kh, pin) })
            add(HostMenuItem(stringResource(R.string.hostcard_unpin_card)) { onTogglePin(kh, pin) })
            return@buildList
        }
        if (kh.mac.isNotEmpty() && !kh.isOnline(reachable)) {
            add(HostMenuItem(stringResource(R.string.hostcard_wake)) { onWake(kh) })
        }
        if (kh.paired && kh.asProfile != null) {
            add(HostMenuItem(stringResource(R.string.hostcard_switch_profile)) { onSwitchProfile(kh) })
        }
        add(HostMenuItem(stringResource(R.string.hostcard_copy_link)) { onCopyLink(kh, null) })
        add(HostMenuItem(stringResource(R.string.hostcard_details)) { onDetails(kh) })
        if (presets.isEmpty()) return@buildList
        add(
            HostMenuItem(stringResource(R.string.android_hostcard_connect_with_defaults), startsSection = true) {
                onConnect(kh, "")
            },
        )
        presets.forEach { p ->
            add(
                HostMenuItem(stringResource(R.string.android_hostcard_connect_with_preset, p.name)) {
                    onConnect(kh, p.id)
                },
            )
        }
    }

    // Each host's own card, then one card per preset it has pinned.
    val savedCards = savedHosts.flatMap { kh ->
        listOf(HostCardEntry(kh, null)) + pinsFor(kh).map { HostCardEntry(kh, it) }
    }
    val res = LocalContext.current.resources

    Box(Modifier.fillMaxSize()) {
        LazyVerticalGrid(
            // One column on a phone, two or more on a tablet.
            columns = GridCells.Adaptive(minSize = 340.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 16.dp),
            horizontalArrangement = Arrangement.spacedBy(12.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            if (notice != null || status != null) {
                item(span = { GridItemSpan(maxLineSpan) }) { NoticeBanners(notice, status) }
            }

            if (!lnpGranted) {
                item(span = { GridItemSpan(maxLineSpan) }) { LocalNetworkBanner(onAskLocalNetwork) }
            }

            if (savedHosts.isEmpty() && discoveredUnsaved.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    EmptyHostsState()
                }
            }

            if (savedHosts.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    SectionLabel(stringResource(R.string.hosts_section_saved))
                }
                items(savedCards, key = { it.key }) { entry ->
                    val kh = entry.host
                    val pin = entry.pin
                    val shown = pin ?: kh.presetId?.let { id -> presets.firstOrNull { it.id == id } }
                    HostCard(
                        name = kh.name,
                        status = HostLine.of(
                            pinned = kh.fpHex.isNotEmpty(),
                            online = kh.isOnline(reachable),
                            connecting = false,
                            playing = nowPlaying[kh.fpHex],
                            wakes = autoWake && kh.mac.isNotEmpty(),
                        ),
                        // Live advert preferred (the store lags a discovery tick), else stored.
                        os = discovered.firstOrNull { kh.matches(it) && it.os.isNotEmpty() }?.os
                            ?: kh.os,
                        enabled = !connecting,
                        // A pinned card connects with its preset; the host's own follows the binding.
                        onConnect = { onConnect(kh, pin?.id) },
                        onDetails = if (pin == null) ({ onDetails(kh) }) else null,
                        filled = kh.fpHex.isNotEmpty(),
                        presetLabel = shown?.name,
                        presetProminent = pin != null,
                        accent = accentColor(shown?.accent),
                        menuItems = hostMenu(kh, pin),
                        isDefault = pin == null && defaultHostId == kh.id,
                        profileName = if (pin == null) kh.asProfile?.displayName else null,
                        profilePicture = kh.asProfile?.avatar,
                        profileArt = if (kh.asProfile?.avatar != null) rememberProfileArt(kh) else null,
                        // Named only when it is not the only screen.
                        screenLine = screens[kh.fpHex]?.takeIf { kh.paired }?.cardLineUnder(shown, res),
                    )
                }
            }

            if (discoveredUnsaved.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Spacer(Modifier.height(12.dp))
                    SectionLabel(stringResource(R.string.hosts_section_discovered))
                }
                // The advert's instance key: two same-named hosts can merge onto one address.
                items(discoveredUnsaved, key = { "disc-${it.key}" }) { dh ->
                    HostCard(
                        name = dh.name,
                        status = if (dh.pairingRequired) HostLine.PairingRequired else HostLine.Found,
                        os = dh.os,
                        enabled = !connecting,
                        onConnect = { onConnectDiscovered(dh) },
                        filled = false,
                        // No name of its own yet, so the address tells two apart.
                        address = "${dh.host}:${dh.port}",
                    )
                }
            }

            // Scan again is offered whether or not anything turned up: one expected host missing
            // looks the same as a browse that went deaf. Without the local network grant the
            // banner above owns the state.
            if (lnpGranted && !connecting) {
                item(span = { GridItemSpan(maxLineSpan) }) { SearchRow(discovered.isEmpty(), onRescan) }
            }

            item(span = { GridItemSpan(maxLineSpan) }) {
                Spacer(Modifier.height(96.dp))
            }
        }

        ExtendedFloatingActionButton(
            onClick = onAddHost,
            icon = { Icon(Icons.Filled.Add, contentDescription = null) },
            text = { Text(stringResource(R.string.hosts_add)) },
            expanded = !connecting,
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(20.dp),
        )
    }
}

/** The two result banners over the grid and the host page: a confirmation, then the failure line. */
@Composable
internal fun NoticeBanners(notice: String?, status: String?) {
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        notice?.let {
            Banner(it, error = false)
            Spacer(Modifier.height(16.dp))
        }
        // In-flight progress is the connect overlay's, so `status` only ever carries a failure.
        status?.let {
            Banner(it, error = true)
            Spacer(Modifier.height(16.dp))
        }
    }
}

@Composable
private fun Banner(text: String, error: Boolean) {
    val scheme = MaterialTheme.colorScheme
    Surface(
        color = if (error) scheme.errorContainer else scheme.secondaryContainer,
        shape = MaterialTheme.shapes.medium,
        modifier = Modifier.fillMaxWidth(),
    ) {
        Text(
            text,
            style = MaterialTheme.typography.bodyMedium,
            color = if (error) scheme.onErrorContainer else scheme.onSecondaryContainer,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
        )
    }
}

/**
 * Local network access denied: discovery can't ever find anything and every connect would time
 * out — say so at the top, with the fix one tap away, instead of letting the screen look broken.
 */
@Composable
private fun LocalNetworkBanner(onAsk: () -> Unit) {
    Surface(
        color = MaterialTheme.colorScheme.errorContainer,
        shape = MaterialTheme.shapes.medium,
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Text(
                stringResource(R.string.android_lnp_off_title),
                style = MaterialTheme.typography.titleSmall,
                color = MaterialTheme.colorScheme.onErrorContainer,
            )
            Text(
                stringResource(R.string.android_lnp_off_body),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onErrorContainer,
                textAlign = TextAlign.Center,
            )
            TextButton(onClick = onAsk) { Text(stringResource(R.string.android_lnp_allow)) }
        }
    }
    Spacer(Modifier.height(12.dp))
}

/**
 * Discovery runs whenever the grid is up: while nothing has turned up, show it working rather
 * than idle. Scan again is offered either way — the case that sends people here is ONE expected
 * host missing, and a browse that quietly went deaf looks exactly like a network without it.
 */
@Composable
private fun SearchRow(searching: Boolean, onRescan: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(vertical = 12.dp),
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        if (searching) {
            CircularProgressIndicator(modifier = Modifier.size(16.dp), strokeWidth = 2.dp)
            Spacer(Modifier.width(8.dp))
            Text(
                stringResource(R.string.hosts_searching),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.width(8.dp))
        }
        TextButton(onClick = onRescan) { Text(stringResource(R.string.hosts_rescan)) }
    }
}
