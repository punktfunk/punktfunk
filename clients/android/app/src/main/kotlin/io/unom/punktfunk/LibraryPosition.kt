package io.unom.punktfunk

import android.content.Context

// Where the player was in a host's library, so the round trip back from a stream doesn't lose it.
// The Android mirror of the Apple client's `LibraryScrollMemory`.
//
// Leaving a stream re-composes the library screen from scratch — new `remember`s, a new
// `LazyGridState` — so a library of any size would return at the top. For the loop this screen
// exists to serve (browse → play → quit → browse), that means re-scrolling to the same place on
// every lap.
//
// The position is remembered as the ID OF THE TITLE the player last opened, not as a scroll offset
// or an index. An offset is meaningless across the things that legitimately change between visits —
// a rotation, a window resize, a foldable unfolding, a host that gained or lost titles, or the
// running-first ordering this screen applies. A title id survives all of them, and the grid
// turns it back into a position at whatever the current layout is.

/**
 * Per-host "last title opened", in `SharedPreferences`.
 *
 * Small, non-sensitive and worth surviving a process death — the app being killed in the background
 * while a stream is up is exactly when this is most useful — so preferences rather than an in-memory
 * cache. One key per host record id, namespaced so nothing else can collide with it.
 */
object LibraryPosition {
    private const val PREFS = "punktfunk_library_position"

    private fun prefs(context: Context) =
        context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /** The title last opened from this host's library, if any is remembered. */
    fun last(context: Context, hostId: String): String? =
        prefs(context).getString(hostId, null)

    /**
     * Remember a title as this host's position. Called when one is LAUNCHED, which is the only
     * moment the player is definitely leaving the grid for it — remembering on mere focus would
     * make a scroll past a tile into a decision.
     */
    fun remember(context: Context, hostId: String, gameId: String) {
        prefs(context).edit().putString(hostId, gameId).apply()
    }

    /**
     * Forget a host's position — part of removing the host, so a forgotten host leaves no trace of
     * what somebody was playing behind on the device.
     */
    fun forget(context: Context, hostId: String) {
        prefs(context).edit().remove(hostId).apply()
    }
}
