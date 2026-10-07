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
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.components.EmptyHostsState
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.HostMenuItem
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.models.HostStatus

/**
 * The touch home: the saved/discovered host grid with the Add-host FAB over it — everything
 * `ConnectScreen` draws when the console UI is off, and the counterpart of [buildHomeTiles] +
 * `GamepadHome` when it is on.
 *
 * Pure display: every action arrives as a callback, because they all end in state the screen owns
 * (a dial in flight, the trust prompt, the host store). What this file DOES own is the arrangement
 * — which sections exist, in what order, and which actions a given card offers — and the two rules
 * that are easy to get wrong from the outside: a pinned card is a shortcut and so withholds the
 * host's destructive actions, and every card in a section reserves the preset chip's space as soon
 * as one of them needs it.
 */
@Composable
internal fun ConnectGrid(
    savedHosts: List<KnownHost>,
    /** Every live advert — the OS mark prefers it over the stored one, and "searching…" reads it. */
    discovered: List<DiscoveredHost>,
    /** Adverts with no saved record behind them, de-duped by the caller (it needs them too). */
    discoveredUnsaved: List<DiscoveredHost>,
    /** Saved hosts answering the QUIC probe, "address:port" — the routed half of "online". */
    reachable: Set<String>,
    presets: List<StreamPreset>,
    pinsFor: (KnownHost) -> List<StreamPreset>,
    connecting: Boolean,
    /** A confirmation ("75 Mbit/s set in …"); [status] is the failure line. Never the same thing. */
    notice: String?,
    status: String?,
    lnpGranted: Boolean,
    /** Raise the local-network-permission prompt — the banner's "Allow…" and the wake guard. */
    onAskLocalNetwork: () -> Unit,
    /**
     * Dial a saved host. The second argument is `connect`'s one-off preset reference: null follows
     * the host's binding (a plain tap), a preset id forces that preset, and the empty string
     * forces the global defaults — a real, different action on a bound host, which is why it has to
     * survive as a value rather than collapsing into "unset".
     */
    onConnect: (KnownHost, String?) -> Unit,
    onConnectDiscovered: (DiscoveredHost) -> Unit,
    onForget: (KnownHost) -> Unit,
    onEdit: (KnownHost) -> Unit,
    onWake: (KnownHost) -> Unit,
    onSpeedTest: (KnownHost) -> Unit,
    /** Upload this device's recent log to the host — see the menu row's gate below. */
    onSendLogs: (KnownHost) -> Unit,
    /** Open the profile picker for this host; offered once it has a saved pick. */
    onSwitchProfile: (KnownHost) -> Unit,
    /** What each paired host last said this device may do TO it, by fingerprint
     *  (`design/host-actions.md` §7). Absent = no rows. */
    hostActions: Map<String, List<HostActions.Action>>,
    onHostAction: (KnownHost, HostActions.Action) -> Unit,
    onCopyLink: (KnownHost, StreamPreset?) -> Unit,
    onTogglePin: (KnownHost, StreamPreset) -> Unit,
    /**
     * Open this card's game library. The second argument is the shelf's pinned preset id, exactly
     * as [onConnect] takes the card's one-off: browsing IS this card's connect with a title picked
     * first, so a pinned card's shelf launches with that card's preset.
     */
    onBrowseLibrary: (KnownHost, StreamPreset?) -> Unit,
    /** `Settings.defaultHost` — which card wears the checkmark. Null when none is written. */
    defaultHost: String?,
    /** Point the start-screen setting at this host, or clear it (`false`). */
    onMakeDefault: (KnownHost, Boolean) -> Unit,
    onRescan: () -> Unit,
    onAddHost: () -> Unit,
) {
    // The preset rows a card's overflow menu grows. With no presets at all it stays empty — a
    // user who never wants this feature sees no new clutter anywhere but the settings scope chips.
    // "Connect with" is a ONE-OFF on every card: it never rebinds the host, which is why rebinding
    // lives in the Edit sheet instead.
    fun hostMenu(kh: KnownHost, pin: StreamPreset?): List<HostMenuItem> = buildList {
        // Browsing IS a connect-shaped action — this card's connect with a title picked first — so
        // a PINNED card offers it too, and its shelf launches with that card's preset. Pairing is
        // the whole gate: the fetch authenticates with the pinned identity, so an unpaired card
        // could only ever be refused.
        if (kh.paired) {
            add(HostMenuItem("Browse library…") { onBrowseLibrary(kh, pin) })
        }
        if (pin == null) {
            add(HostMenuItem("Network speed test") { onSpeedTest(kh) })
        }
        // "Send logs to host" — the same row the console's host menu carries
        // (`pf-console-ui`'s `options.rs`), on the same gate: the upload authenticates with the
        // streaming cert, so it needs a paired identity and a host that is answering. It belongs
        // HERE too and not only in the console: a device whose console never comes up is exactly
        // the one whose logs somebody needs, and the touch home was its only shell.
        if (pin == null && kh.paired && kh.isOnline(reachable)) {
            add(HostMenuItem("Send logs to host") { onSendLogs(kh) })
        }
        // The host's own actions — sleep, restart, shut it down (`design/host-actions.md` §7),
        // the other half of the Wake-on-LAN round trip. Nothing is decided here: the list is
        // empty unless the host answered AND this device's access carries the grant, so no row
        // appears that the host would refuse. A pinned card is a shortcut to one preset, not a
        // second host, so it offers none — same rule as "Send logs" above.
        if (pin == null) {
            hostActions[kh.fpHex].orEmpty().forEach { a ->
                val label = if (a.available) a.label else "${a.label} (unavailable)"
                add(HostMenuItem(label) { onHostAction(kh, a) })
            }
        }
        if (pin == null && kh.paired && kh.asProfile != null) {
            add(HostMenuItem("Switch profile\u2026") { onSwitchProfile(kh) })
        }
        add(HostMenuItem("Copy link") { onCopyLink(kh, pin) })
        // Which host the app opens on. Needs a pairing to point at — the start screen skips an
        // unpaired host, so writing one would set a pointer that never resolves. An unchecked row
        // is not "not the default": a lone paired host is the default with nothing written.
        if (pin == null && kh.paired) {
            val isDefault = defaultHost?.lowercase() == kh.id.lowercase()
            add(
                HostMenuItem(if (isDefault) "Default host ✓" else "Make default host") {
                    onMakeDefault(kh, !isDefault)
                },
            )
        }
        if (presets.isEmpty()) return@buildList
        if (pin != null) {
            add(HostMenuItem("Unpin card", startsSection = true) { onTogglePin(kh, pin) })
        }
        add(
            HostMenuItem("Connect with: Default settings", startsSection = true) {
                // The empty reference is "force the defaults", not "unset" — on a bound host that
                // is a real, different action from a plain tap.
                onConnect(kh, "")
            },
        )
        presets.forEach { p ->
            add(HostMenuItem("Connect with: ${p.name}") { onConnect(kh, p.id) })
        }
        if (pin == null) {
            presets.forEachIndexed { i, p ->
                val pinned = p.id in kh.pinnedPresetIds
                add(
                    HostMenuItem(
                        if (pinned) "Unpin card: ${p.name}" else "Pin as card: ${p.name}",
                        startsSection = i == 0,
                    ) { onTogglePin(kh, p) },
                )
            }
        }
    }

    // The saved-hosts grid: each host's own card, then one card per preset it has pinned, so a
    // pinned combination is a plain one-click connect instead of a trip through a menu.
    val savedCards = savedHosts.flatMap { kh ->
        listOf(HostCardEntry(kh, null)) + pinsFor(kh).map { HostCardEntry(kh, it) }
    }
    // Cards in one grid row must be the same height (the grid won't stretch them), so as soon as
    // ANY saved card carries a preset chip, they all reserve its space. Nobody who doesn't use
    // presets ever sees the gap.
    val anyPresetChip = savedCards.any { it.pin != null || it.host.presetId != null }

    Box(Modifier.fillMaxSize()) {
        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 160.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 16.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            item(span = { GridItemSpan(maxLineSpan) }) { GridHeader(notice, status) }

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
                    SectionLabel("Saved hosts")
                }
                items(savedCards, key = { it.key }) { entry ->
                    val kh = entry.host
                    val pin = entry.pin
                    val bound = kh.presetId?.let { id -> presets.firstOrNull { it.id == id } }
                    HostCard(
                        name = kh.name,
                        address = "${kh.address}:${kh.port}",
                        status = if (kh.paired) HostStatus.PAIRED else HostStatus.TOFU,
                        online = kh.isOnline(reachable),
                        // Live advert preferred (the store lags a discovery tick), else stored.
                        os = discovered.firstOrNull { kh.matches(it) && it.os.isNotEmpty() }?.os
                            ?: kh.os,
                        enabled = !connecting,
                        // A pinned card connects with ITS preset; the host's own card follows the
                        // binding, which is exactly what its chip says it will do.
                        onConnect = { onConnect(kh, pin?.id) },
                        // Edit / Forget / Wake live on the host's own card only: a pinned card is a
                        // shortcut, not a second host, and offering destructive host actions on it
                        // would blur exactly that.
                        onForget = if (pin != null) null else ({ onForget(kh) }),
                        onEdit = if (pin != null) null else ({ onEdit(kh) }),
                        // Explicit wake-only: offered when the host is offline and we have a MAC. The
                        // screen runs it through the WakeController so it shows the "Waking…" overlay
                        // and waits for the host to come online (matched by fingerprint, so a new DHCP
                        // address on a cold boot still counts as "up") rather than firing a single
                        // silent packet.
                        onWake = if (pin == null && kh.mac.isNotEmpty() && !kh.isOnline(reachable)) {
                            ({ onWake(kh) })
                        } else {
                            null
                        },
                        presetLabel = pin?.name ?: bound?.name,
                        presetProminent = pin != null,
                        accent = accentColor(pin?.accent ?: bound?.accent),
                        menuItems = hostMenu(kh, pin),
                        profileName = if (pin == null) kh.asProfile?.displayName else null,
                        reservePresetSlot = anyPresetChip,
                    )
                }
            }

            if (discoveredUnsaved.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Spacer(Modifier.height(12.dp))
                    SectionLabel("Discovered on the network")
                }
                // The advert's instance key: two same-named hosts can merge onto one address.
                items(discoveredUnsaved, key = { "disc-${it.key}" }) { dh ->
                    HostCard(
                        name = dh.name,
                        address = "${dh.host}:${dh.port}",
                        status = if (dh.pairingRequired) HostStatus.PAIRING else HostStatus.TOFU,
                        online = true, // in the discovered list ⇒ live on mDNS right now
                        os = dh.os,
                        enabled = !connecting,
                        onConnect = { onConnectDiscovered(dh) },
                        onForget = null,
                    )
                }
            }

            // Active-discovery hint: discovery runs whenever this screen is up, so while it's
            // scanning but nothing's turned up yet (and we're not mid-connect), show it's working
            // rather than looking idle/empty. Suppressed while local network access is denied —
            // a spinner would be a lie there (the browse can't receive anything); the banner above
            // owns that state.
            // Scan again is offered whether or not anything turned up: the case that sends people
            // here is ONE expected host missing, not an empty list, and a browse that quietly went
            // deaf (blocked when it started, or backed off to its hour-long re-query) looks
            // exactly like a network without that host on it.
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
            text = { Text("Add host") },
            expanded = !connecting,
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(20.dp),
        )
    }
}

/** The grid's masthead plus the two result banners: a confirmation, then the failure line. */
@Composable
private fun GridHeader(notice: String?, status: String?) {
    Column(horizontalAlignment = Alignment.CenterHorizontally) {
        Spacer(Modifier.height(8.dp))
        Text("Punktfunk", style = MaterialTheme.typography.headlineLarge)
        Text(
            "stream a remote desktop",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(Modifier.height(24.dp))
        notice?.let {
            Banner(it, error = false)
            Spacer(Modifier.height(16.dp))
        }
        // In-flight progress (connecting / waking) is the full-screen ConnectOverlay's job, so
        // `status` only ever carries a result/error here — a filled error container reads as a
        // real failure banner, not just red text lost in the layout.
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
                "Local network access is off",
                style = MaterialTheme.typography.titleSmall,
                color = MaterialTheme.colorScheme.onErrorContainer,
            )
            Text(
                "Android blocks Punktfunk from finding or reaching hosts until you allow it.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onErrorContainer,
                textAlign = TextAlign.Center,
            )
            TextButton(onClick = onAsk) { Text("Allow…") }
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
                "Searching the local network…",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.width(8.dp))
        }
        TextButton(onClick = onRescan) { Text("Scan again") }
    }
}
