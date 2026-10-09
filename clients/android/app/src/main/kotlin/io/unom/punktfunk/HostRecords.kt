package io.unom.punktfunk

import android.content.Context
import io.unom.punktfunk.kit.ProfilePick
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore

/**
 * The host-record edits both shells make: the touch home's card menu and the console's commands.
 * Each does the whole job on the store and on what is filed under the record's id; the caller
 * refreshes its own view and owns any notice or confirmation.
 */
object HostRecords {
    /**
     * Forget [kh]. A forgotten host leaves no list of what somebody plays, and no record of what
     * they were playing, behind on the device: both are filed under the record id, so this is the
     * last moment either can be found. Returns [settings] with the default host cleared when it
     * pointed here, for the caller to persist; null when the settings stand.
     */
    fun forget(context: Context, store: KnownHostStore, kh: KnownHost, settings: Settings): Settings? {
        store.remove(kh)
        LibraryCache.standard(context.cacheDir).forget(kh.id)
        LibraryPosition.forget(context, kh.id)
        return if (settings.defaultHost == kh.id) settings.copy(defaultHost = null) else null
    }

    /** Pin [presetId] as a card of [kh], or unpin it. It never touches the preset or the binding. */
    fun setPin(store: KnownHostStore, kh: KnownHost, presetId: String, pin: Boolean) {
        val pins = kh.pinnedPresetIds
        val next = when {
            pin && presetId !in pins -> pins + presetId
            !pin -> pins - presetId
            else -> pins
        }
        store.save(kh.copy(pinnedPresetIds = next))
    }

    fun togglePin(store: KnownHostStore, kh: KnownHost, presetId: String) =
        setPin(store, kh, presetId, pin = presetId !in kh.pinnedPresetIds)

    /** Keep the record, drop its pin, so the next connect pairs again. */
    fun unpair(store: KnownHostStore, kh: KnownHost) = store.save(kh.copy(fpHex = "", paired = false))

    /**
     * Bind [presetId] as [kh]'s default ([KnownHost.presetId]), or with [game] one title's
     * ([KnownHost.gamePresets]). A null [presetId] clears either; a cleared title leaves no key.
     */
    fun bindPreset(store: KnownHostStore, kh: KnownHost, presetId: String?, game: String?) {
        val next = when (game) {
            null -> kh.copy(presetId = presetId)
            else -> kh.copy(
                gamePresets = kh.gamePresets.toMutableMap()
                    .apply { if (presetId == null) remove(game) else put(game, presetId) },
            )
        }
        store.save(next)
    }

    /** The per-host clipboard trust toggle. */
    fun setClipboard(store: KnownHostStore, kh: KnownHost, on: Boolean) =
        store.save(kh.copy(clipboardSync = on))

    /** Save [pick] as [kh]'s profile (or clear it); a no-op when the record is gone. */
    fun savePick(store: KnownHostStore, kh: KnownHost, pick: ProfilePick?) {
        val h = store.byId(kh.id) ?: return
        if (h.asProfile != pick) store.save(h.copy(asProfile = pick))
    }
}
