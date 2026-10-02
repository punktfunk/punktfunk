package io.unom.punktfunk.kit.security

import android.content.Context
import java.util.UUID
import org.json.JSONArray
import org.json.JSONObject

/**
 * A host the user has trusted (pinned). [fpHex] is the pinned host-cert SHA-256 (64-hex); [paired]
 * is true when trust was established via the SPAKE2 PIN ceremony (vs trust-on-first-use).
 *
 * [id] is the record's **stable identity** — minted once, never changed, and the key this record is
 * stored under. Everything that needs to point AT a host (a settings-preset binding, a pinned
 * card, a `punktfunk://` link) points at the id, so renaming a host or moving it to a new address
 * doesn't strand those references. Mirrors the Apple client's `StoredHost.id` and the Rust
 * `KnownHost.id`; the shape is a lowercase UUID v4, one grammar on every platform.
 */
data class KnownHost(
    val address: String,
    val port: Int,
    val name: String,
    val fpHex: String,
    val paired: Boolean,
    /**
     * Wake-on-LAN MAC(s) (`aa:bb:cc:dd:ee:ff`) learned from the host's mDNS `mac` TXT while it was
     * online, so the client can wake it once it sleeps. Empty until first learned.
     */
    val mac: List<String> = emptyList(),
    /**
     * The host's OS-identity chain (`windows` | `linux/<family>/<id>`, ...) learned from its mDNS
     * `os` TXT while online, so the card's OS icon survives the host going to sleep. Empty until
     * first learned (or forever, against an older host).
     */
    val os: String = "",
    /**
     * The host's management-API port (mDNS `mgmt` TXT), where the game library is served — NOT
     * [port], which is the native QUIC plane. Learned while online and kept for the same reason as
     * [mac] and [os], except this one is load-bearing: a host that moved its mgmt port off 47990
     * (the supported way to share a machine with a Sunshine fork, whose web UI owns that port)
     * served its library only while mDNS was reachable, because the advert was the sole place the
     * real port ever existed. `null` until learned — resolve with [effectiveMgmtPort].
     * Mirrors the Apple client's `StoredHost.mgmtPort` and the Rust `KnownHost.mgmt_port`.
     */
    val mgmtPort: Int? = null,
    /** Stable record identity — see the class doc. Minted here for a genuinely new record. */
    val id: String = newRecordId(),
    /**
     * Sync text copied on this device to this host and back while streaming. **A property of the
     * host, not of the stream** (design/client-settings-profiles.md §3, tier H): it is a trust
     * decision about that machine, so it is never in a settings preset and never global — the
     * work box and the couch box get their own answers. Only effective when the host advertises
     * the clipboard capability; the protocol is opt-in per session either way.
     *
     * Off until the user enables it for THIS host: a newly paired or TOFU-trusted machine must
     * not read the clipboard by default, and every absent-value fallback matches (Rust and Apple
     * already defaulted off; security-review 2026-08-31 M-8).
     */
    val clipboardSync: Boolean = false,
    /**
     * The settings preset a plain tap on this host connects with — `null` (or an id whose preset
     * was deleted) means the global defaults, i.e. today's behaviour. A dangling id is never an
     * error and never blocks a connect.
     */
    val presetId: String? = null,
    /**
     * The delivery profile to ask this host for (`1` capped, `2` smooth), set from a network
     * check's finding. Per host: a Wi-Fi TV and a wired desk differ. `0` asks nothing.
     */
    val delivery: Int = 0,
    /**
     * Presets pinned as their own cards for this host (design §5.2a). Presentation only: order is
     * card order, and this is NOT the default binding ([presetId] is). Duplicates and presets
     * that no longer exist are dropped when the cards are rendered.
     */
    val pinnedPresetIds: List<String> = emptyList(),
    /**
     * Library title id → preset id: what launching that title streams with. Beats [presetId],
     * which is what a title with no entry here inherits, and is itself beaten by a one-off pick.
     * Mirrors the Rust `KnownHost.game_presets`. A dangling id falls through to [presetId], the
     * way a dangling pin simply disappears — never an error, never a blocked launch.
     */
    val gamePresets: Map<String, String> = emptyMap(),
    /**
     * Addresses this host was moved away from automatically, newest first, at most
     * [PREV_ADDRESSES_MAX]. A host lives at more than one — its LAN lease at home, a Tailscale
     * address anywhere — so when [address] goes silent the presence sweep asks these too.
     * Mirrors the Rust `KnownHost.prev_addrs` and the Apple client's `StoredHost.previousAddresses`.
     */
    val prevAddresses: List<String> = emptyList(),
    /** When this record was saved (Unix seconds); the console's "Date added" order. `null` on a
     *  record saved before the field existed, which sorts ahead of every dated one. */
    val addedAt: Long? = null,
    /** Unix seconds of the last session that connected; `null` until one has. Mirrors the Rust
     *  `KnownHost.last_used` and the Apple client's `StoredHost.lastConnected`. */
    val lastUsed: Long? = null,
) {
    /** This record re-pointed at [to]:[toPort], remembering the address it leaves. */
    fun movedTo(to: String, toPort: Int): KnownHost = copy(
        address = to,
        port = toPort,
        prevAddresses = (listOf(address) + prevAddresses).filter { it != to }.distinct()
            .take(PREV_ADDRESSES_MAX),
    )

    companion object {
        /** How many left-behind addresses a host keeps. */
        const val PREV_ADDRESSES_MAX = 3
    }

    /**
     * Where this host's management API actually is: the port learned from its advert, else 47990.
     * The twin of the Apple client's `StoredHost.effectiveMgmtPort` and the Rust
     * `KnownHost::effective_mgmt_port`. Resolve through this — the constant is the FALLBACK, not
     * the answer.
     */
    val effectiveMgmtPort: Int
        get() = mgmtPort ?: io.unom.punktfunk.kit.library.DEFAULT_MGMT_PORT
}

/**
 * Persists trusted hosts — the pinned-fingerprint store *and* the saved-hosts list — keyed by
 * [KnownHost.id]. Plain `SharedPreferences` in app-private storage: pinned fingerprints are public
 * host identities, not secrets; the property we need is integrity, which app sandboxing provides.
 *
 * Records used to be keyed by `"address:port"`, which meant editing a host's address had to
 * re-key its record (delete + write) or leave a ghost behind, and meant nothing could hold a
 * durable reference to a host. Keying by the minted stable id retires both. [migrate] moves an
 * existing store over in one pass — see its doc for what else rides along.
 */
class KnownHostStore(context: Context) {
    private val prefs =
        context.applicationContext.getSharedPreferences(PREFS_HOSTS, Context.MODE_PRIVATE)

    init {
        migrateIfNeeded(context)
    }

    /**
     * The trusted record for [address]:[port], or `null` if this host has never been trusted.
     * A pinned record beats an unpinned placeholder saved at the same address. An address can
     * carry more than one identity — both OS installs of a dual-boot box answer at one lease —
     * so a caller holding a fingerprint or a card asks [resolve] instead.
     */
    fun get(address: String, port: Int): KnownHost? {
        val at = all().filter { it.address == address && it.port == port }
        return at.firstOrNull { it.fpHex.isNotEmpty() } ?: at.firstOrNull()
    }

    /**
     * The unpinned placeholder saved at [address]:[port], or `null`. A record pinned there is an
     * identity the address does not name on its own: it may be the other OS of the same box.
     */
    fun placeholderAt(address: String, port: Int): KnownHost? =
        all().firstOrNull { it.address == address && it.port == port && it.fpHex.isEmpty() }

    /**
     * The record a dial is about. With a pin: the record pinned to [fpHex], else the placeholder
     * at [address]:[port] waiting for one — never a record pinned to another fingerprint, which
     * at a shared address is the other OS of a dual-boot box. An empty [fpHex] is a card saved
     * without a pin: its placeholder only. `null` is a bare typed address, and [get] answers.
     * Mirrors the Rust `KnownHosts::resolve`.
     */
    fun resolve(fpHex: String?, address: String, port: Int): KnownHost? =
        if (fpHex == null) get(address, port) else getByFp(fpHex) ?: placeholderAt(address, port)

    /**
     * The trusted record pinned to [fpHex], or `null`. An empty fingerprint is not a key: it
     * would match the first unpinned placeholder, which is never the one meant.
     */
    fun getByFp(fpHex: String): KnownHost? =
        if (fpHex.isEmpty()) null else all().firstOrNull { it.fpHex.equals(fpHex, true) }

    /** The trusted record with this stable [id], or `null` — the lookup a binding or link uses. */
    fun byId(id: String): KnownHost? = prefs.getString(id, null)?.let(::parse)

    /**
     * Pin (or update) a trusted host — upsert by [KnownHost.id]. An edit that moves the address or
     * port is a plain save now: the key is the identity, not the address.
     */
    fun save(host: KnownHost) {
        prefs.edit().putString(host.id, encode(host)).apply()
    }

    /**
     * Trust (or re-trust) the host at [address]:[port] with the fingerprint it presented.
     *
     * When a record already exists there — a re-pair after the host's identity changed, an
     * approval that upgrades a TOFU record to paired — it keeps its identity and everything the
     * user set on it: the stable [KnownHost.id] (so preset bindings, pinned cards and any
     * `punktfunk://` shortcut still point at it), the per-host clipboard decision, the binding,
     * the pins and the learned MACs. Only the pin and paired flag are refreshed, plus [name] on a
     * placeholder taking its first pin: a pinned record keeps the name the user knows it by, even
     * when a dial meant for another card at this address landed on it. Returns the stored record.
     *
     * The record is found by its PIN wherever it now answers, so re-pairing a host that moved
     * lease re-points the one record instead of forking a second with the same fingerprint;
     * failing that, an unpinned placeholder saved at this address takes the pin. A record
     * pinned to a DIFFERENT fingerprint is a different host and is left alone — a dual-boot box
     * answers at one lease with one MAC and a certificate per OS, so trusting the second OS
     * would otherwise overwrite the first one's record.
     */
    fun trust(address: String, port: Int, name: String, fpHex: String, paired: Boolean): KnownHost {
        val existing = getByFp(fpHex) ?: placeholderAt(address, port)
        val host = existing?.copy(
            address = address,
            port = port,
            name = if (existing.fpHex.isEmpty()) name else existing.name,
            fpHex = fpHex,
            paired = paired,
        ) ?: KnownHost(address, port, name, fpHex, paired, addedAt = nowSecs())
        save(host)
        return host
    }

    /** Stamp now as [host]'s last connect. No-op when the record is gone. */
    fun touchLastUsed(host: KnownHost) {
        val h = byId(host.id) ?: return
        save(h.copy(lastUsed = nowSecs()))
    }

    /**
     * Learn/refresh [host]'s Wake-on-LAN MAC(s) from its live advert (called while online).
     * No-op when the record is gone, the list is empty, or it's unchanged — so it doesn't churn
     * prefs on every discovery tick.
     *
     * Keyed by the record the caller matched, re-read by its id: an address names more than one
     * record once a dual-boot box has both its OS installs saved, and the advert of one used to
     * teach whichever of them the address answered with.
     */
    fun learnMac(host: KnownHost, mac: List<String>) {
        if (mac.isEmpty()) return
        val h = byId(host.id) ?: return
        if (h.mac == mac) return
        save(h.copy(mac = mac))
    }

    /**
     * Learn/refresh [host]'s OS-identity chain from its live advert — same contract as
     * [learnMac]: no-op when the record is gone, empty, or unchanged. Keyed by the record for
     * the same reason, and it matters most here: the chain draws the card's OS mark, so the
     * wrong record took the neighbouring OS's icon.
     */
    fun learnOs(host: KnownHost, os: String) {
        if (os.isEmpty()) return
        val h = byId(host.id) ?: return
        if (h.os == os) return
        save(h.copy(os = os))
    }

    /**
     * Learn/refresh a saved host's management-API port from its live advert — same contract as
     * [learnMac]. This is the one that keeps a moved mgmt port working once mDNS isn't reachable.
     */
    fun learnMgmtPort(host: KnownHost, mgmtPort: Int) {
        if (mgmtPort <= 0) return
        val h = byId(host.id) ?: return
        if (h.mgmtPort == mgmtPort) return
        save(h.copy(mgmtPort = mgmtPort))
    }

    /**
     * Re-point a pinned host at the address it just answered a probe from, matched by
     * fingerprint — the record's identity, which neither a new DHCP lease nor a VPN changes.
     * Every dial, library fetch and wake reads the saved address. The one it leaves is kept in
     * [KnownHost.prevAddresses], so the sweep can find the host there again. Unpinned records
     * are left alone: the address is all that names them. No-op, and no write, when unchanged.
     * Returns whether anything moved.
     */
    fun learnAddress(fpHex: String, address: String, port: Int): Boolean {
        if (fpHex.isEmpty() || address.isBlank() || port !in 1..65535) return false
        val h = getByFp(fpHex) ?: return false
        if (h.address == address && h.port == port) return false
        save(h.movedTo(address, port))
        return true
    }

    /** Forget [host] (the next connect re-pairs / re-TOFUs). */
    fun remove(host: KnownHost) {
        prefs.edit().remove(host.id).apply()
    }

    /** All trusted hosts, name-sorted — backs the saved-hosts list. */
    fun all(): List<KnownHost> = prefs.all
        .filterKeys { it != K_SCHEMA }
        .values
        .mapNotNull { (it as? String)?.let(::parse) }
        .sortedBy { it.name.lowercase() }

    /**
     * One-time move from the `"address:port"`-keyed schema to id-keyed records, run on first
     * construction after the upgrade and never again ([K_SCHEMA] records that it happened).
     *
     * It is deliberately ONE pass, not three: the store is being rewritten anyway, and every extra
     * migration pass is another chance to strand somebody's hosts. So the same pass mints the
     * stable id, re-keys the record onto it, and copies the retiring GLOBAL clipboard-sync setting
     * onto every host — behaviour-preserving, since every host was following that one value.
     */
    private fun migrateIfNeeded(context: Context) {
        if (prefs.getInt(K_SCHEMA, 0) >= SCHEMA_VERSION) return
        val settings =
            context.applicationContext.getSharedPreferences(PREFS_SETTINGS, Context.MODE_PRIVATE)
        // Fallback false: an install that never wrote the global gets the secure default, not
        // the old implicit on (security-review 2026-08-31 M-8). A global the user DID set — either
        // way — still lands on every host, which is the behaviour-preserving half below.
        val result = migrate(prefs.all, settings.getBoolean(K_GLOBAL_CLIPBOARD_SYNC, false))
        // `commit`, not `apply`: the re-keyed records and the schema flag are one atomic write to
        // disk, and the global below is only retired once that write has landed. With `apply` a
        // process death in between could drop the old global while the hosts that were supposed to
        // inherit it were still only in memory. Once, on one small file, on an upgrade.
        val written = prefs.edit().apply {
            result.removals.forEach(::remove)
            result.writes.forEach { (k, v) -> putString(k, v) }
            putInt(K_SCHEMA, SCHEMA_VERSION)
        }.commit()
        if (written && settings.contains(K_GLOBAL_CLIPBOARD_SYNC)) {
            settings.edit().remove(K_GLOBAL_CLIPBOARD_SYNC).apply()
        }
    }

    private fun parse(s: String): KnownHost? = decode(s)

    companion object {
        /** The prefs file holding the host records. */
        private const val PREFS_HOSTS = "punktfunk_hosts"

        /** The app's settings file — read once by [migrate] for the retiring global. */
        private const val PREFS_SETTINGS = "punktfunk_settings"

        /**
         * The global clipboard-sync key this migration retires. Clipboard sync is a decision about
         * a HOST (design §3, tier H), so it lives on the record now; the global is read once, to
         * seed every host, and then deleted.
         */
        private const val K_GLOBAL_CLIPBOARD_SYNC = "clipboard_sync"

        /** Schema marker inside the hosts file. Reserved — never a host record. */
        private const val K_SCHEMA = "__schema"

        /** 1 = id-keyed records with per-host clipboard sync, preset binding and pins. */
        private const val SCHEMA_VERSION = 1

        /** What [migrate] decided: entries to write, and (old) keys to drop. */
        data class Migration(val writes: Map<String, String>, val removals: Set<String>)

        /**
         * The pure half of the store migration, over a raw prefs snapshot ([entries] as returned by
         * `SharedPreferences.all`) — so it can be tested against a real pre-migration blob without
         * an Android runtime.
         *
         * Every host record survives with its address, port, name, pin, paired flag and MACs
         * intact, gains a minted [KnownHost.id], moves to that key, and takes
         * [globalClipboardSync] as its own [KnownHost.clipboardSync]. Entries that aren't parsable
         * host records are left alone: they were already invisible (`all()` skipped them), and
         * deleting things we don't understand is not this pass's job.
         */
        fun migrate(entries: Map<String, Any?>, globalClipboardSync: Boolean): Migration {
            val writes = mutableMapOf<String, String>()
            val removals = mutableSetOf<String>()
            for ((key, raw) in entries) {
                if (key == K_SCHEMA) continue
                val json = (raw as? String)?.let { runCatching { JSONObject(it) }.getOrNull() }
                    ?: continue
                if (!json.has("addr") || !json.has("port")) continue
                val id = json.optString("id", "").ifEmpty { newRecordId() }
                json.put("id", id)
                json.put("clip", json.optBoolean("clip", globalClipboardSync))
                writes[id] = json.toString()
                if (key != id) removals += key
            }
            return Migration(writes, removals)
        }

        /**
         * Parse a free-typed Wake-on-LAN field into normalized `aa:bb:cc:dd:ee:ff` entries (comma /
         * space / newline separated). Anything that isn't six colon-separated hex octets is dropped;
         * an empty result clears the host's MAC. Mirrors the Apple client's `AddHostSheet.parseMacs`.
         */
        fun parseMacs(s: String): List<String> = s
            .split(',', ';', ' ', '\n', '\t')
            .map { it.trim().lowercase() }
            .filter { m ->
                // Exactly six octets, each two literal hex digits. (Not toIntOrNull(16) — that accepts
                // a leading +/- sign, so "aa:bb:cc:dd:ee:-1" would wrongly pass.)
                m.split(":").let { o ->
                    o.size == 6 && o.all { it.length == 2 && it.all { c -> c in '0'..'9' || c in 'a'..'f' } }
                }
            }

        /** The stored JSON for one record — also the shape [migrate] upgrades into. */
        internal fun encode(host: KnownHost): String = JSONObject()
            .put("id", host.id)
            .put("addr", host.address)
            .put("port", host.port)
            .put("name", host.name)
            .put("fp", host.fpHex.lowercase())
            .put("paired", host.paired)
            .put("mac", host.mac.joinToString(","))
            .put("os", host.os)
            .put("mgmt", host.mgmtPort ?: 0)
            .put("clip", host.clipboardSync)
            .put("preset", host.presetId ?: "")
            .put("delivery", host.delivery)
            .put("pins", JSONArray(host.pinnedPresetIds))
            .put("game_presets", JSONObject(host.gamePresets))
            // The pre-rename keys too, so an older build of this app keeps the bindings.
            .put("profile", host.presetId ?: "")
            .put("game_profiles", JSONObject(host.gamePresets))
            .put("prev_addrs", JSONArray(host.prevAddresses))
            .put("added", host.addedAt ?: 0)
            .put("last_used", host.lastUsed ?: 0)
            .toString()

        /** One stored record, or null when it does not parse. */
        internal fun decode(s: String): KnownHost? = runCatching {
            val j = JSONObject(s)
            KnownHost(
                address = j.getString("addr"),
                port = j.getInt("port"),
                name = j.getString("name"),
                fpHex = j.getString("fp"),
                paired = j.optBoolean("paired", false),
                mac = j.optString("mac", "").split(",").map { it.trim() }.filter { it.isNotEmpty() },
                os = j.optString("os", ""),
                // 0 (or absent) = never learned. `optInt` cannot express "missing", hence the sentinel
                // rather than a bare default — a record written before this field existed must decode
                // to null and fall back to 47990, not to port 0.
                mgmtPort = j.optInt("mgmt", 0).takeIf { it > 0 },
                // A record without an id can only be one this build wrote before the migration ran, or
                // a hand-edited file; minting here keeps the parse total rather than dropping a host.
                id = j.optString("id", "").ifEmpty { newRecordId() },
                clipboardSync = j.optBoolean("clip", false),
                // `profile` and `game_profiles` are the pre-rename keys, read when the new key is absent.
                presetId = j.optString(newOrOld(j, "preset", "profile"), "").ifEmpty { null },
                delivery = j.optInt("delivery", 0),
                pinnedPresetIds = stringList(j.optJSONArray("pins")),
                gamePresets = stringMap(
                    j.optJSONObject(newOrOld(j, "game_presets", "game_profiles")),
                ),
                prevAddresses = stringList(j.optJSONArray("prev_addrs")),
                // 0 (or absent) = never stamped, the same sentinel as `mgmt`.
                addedAt = j.optLong("added", 0).takeIf { it > 0 },
                lastUsed = j.optLong("last_used", 0).takeIf { it > 0 },
            )
        }.getOrNull()

        /** [new] when the record has it, else its pre-rename spelling [old]. */
        private fun newOrOld(j: JSONObject, new: String, old: String): String =
            if (j.has(new)) new else old

        private fun stringList(a: JSONArray?): List<String> {
            if (a == null) return emptyList()
            return (0 until a.length()).mapNotNull { a.optString(it, "").ifEmpty { null } }
        }

        private fun stringMap(o: JSONObject?): Map<String, String> {
            if (o == null) return emptyMap()
            return o.keys().asSequence()
                .mapNotNull { k -> o.optString(k, "").ifEmpty { null }?.let { k to it } }
                .toMap()
        }
    }
}

/**
 * A fresh stable record identity: a lowercase UUID v4, the shape the Apple client's `StoredHost.id`
 * and the Rust `KnownHost.id` already use, so a `punktfunk://` host reference is one grammar
 * everywhere.
 */
fun newRecordId(): String = UUID.randomUUID().toString()

/** Unix seconds, the unit [KnownHost.addedAt] and [KnownHost.lastUsed] are stored in. */
fun nowSecs(): Long = System.currentTimeMillis() / 1000
