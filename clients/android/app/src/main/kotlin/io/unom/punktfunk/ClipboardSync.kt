package io.unom.punktfunk

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import io.unom.punktfunk.kit.NativeBridge

/**
 * Text clipboard sync for the active session (the desktop-client model, text-only v1):
 *  * **Device → host**: a local copy (the primary-clip listener, plus one probe at start) is
 *    announced as a lazy offer — the text crosses only when the host actually pastes (a
 *    `fetch:` event, answered with the clipboard's current content).
 *  * **Host → device**: a host copy arrives as an `offer:` event and is fetched eagerly into
 *    the system clipboard (Android apps can't lazily materialize a paste from the network
 *    without a content-provider round-trip that isn't worth it here).
 *
 * Loop guard: text set from a host fetch is remembered ([lastFromHost]) so the resulting
 * primary-clip-changed callback doesn't bounce it straight back as a new offer. A clip a password
 * manager marks secret ([isSecretClip]) is never offered or served. Clipboard reads happen while
 * the stream is foreground (Android only allows focused-app reads). The native events are drained
 * on a dedicated thread and applied on the main thread; [stop] joins it.
 */
class ClipboardSync(
    private val context: Context,
    private val handle: Long,
) {
    private val main = Handler(Looper.getMainLooper())
    private val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager

    @Volatile private var running = true
    private var seq = 0
    private var lastOffered: String? = null
    private var lastFromHost: String? = null
    private var pendingFetch = -1
    private var thread: Thread? = null

    private val clipListener = ClipboardManager.OnPrimaryClipChangedListener { offerLocal() }

    fun start() {
        NativeBridge.nativeClipControl(handle, true)
        cm.addPrimaryClipChangedListener(clipListener)
        thread = Thread({ pollLoop() }, "pf-clipboard").also { it.start() }
        offerLocal() // whatever is already on the clipboard is pasteable host-side right away
    }

    fun stop() {
        running = false
        cm.removePrimaryClipChangedListener(clipListener)
        thread?.join(600) // one poll timeout (250 ms) + slack
        thread = null
    }

    /** Announce the current local text (if it's new and not an echo of a host copy). */
    private fun offerLocal() {
        if (!running) return
        val text = currentClipText() ?: return
        if (text == lastOffered || text == lastFromHost) return
        lastOffered = text
        seq += 1
        NativeBridge.nativeClipOfferText(handle, seq)
    }

    /** The clip's text, or null when there is none or it is marked secret. */
    private fun currentClipText(): String? = runCatching {
        if (isSecretClip(cm.primaryClipDescription)) return@runCatching null
        cm.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)
            ?.coerceToText(context)?.toString()?.takeIf { it.isNotEmpty() }
    }.getOrNull()

    private fun pollLoop() {
        while (running) {
            val ev = NativeBridge.nativeNextClip(handle) ?: continue
            if (ev == "closed") return
            main.post { handleEvent(ev) }
        }
    }

    private fun handleEvent(ev: String) {
        if (!running) return
        val parts = ev.split(":", limit = 3)
        when (parts[0]) {
            "offer" -> {
                val offerSeq = parts.getOrNull(1)?.toIntOrNull() ?: return
                if (parts.getOrNull(2) == "1") {
                    pendingFetch = NativeBridge.nativeClipFetchText(handle, offerSeq)
                }
            }
            "fetch" -> {
                val req = parts.getOrNull(1)?.toIntOrNull() ?: return
                val text = currentClipText()
                if (text != null) {
                    NativeBridge.nativeClipServeText(handle, req, text)
                } else {
                    NativeBridge.nativeClipCancel(handle, req)
                }
            }
            "data" -> {
                val xfer = parts.getOrNull(1)?.toIntOrNull() ?: return
                if (xfer != pendingFetch) return // stale/unknown transfer
                pendingFetch = -1
                val text = parts.getOrNull(2)?.takeIf { it.isNotEmpty() } ?: return
                lastFromHost = text
                // Android checks focus only for clipboard reads (AOSP 10 through main), so a
                // background write goes through. A throw here must reach the log, or
                // "clipboard sync stopped" has nothing to point at.
                runCatching { cm.setPrimaryClip(ClipData.newPlainText("Punktfunk", text)) }
                    .onFailure { Log.w("ClipboardSync", "setPrimaryClip refused", it) }
            }
            // "state"/"cancel"/"error": nothing to drive in the text-only v1.
        }
    }
}

/** `ClipDescription.EXTRA_IS_SENSITIVE`, spelled out: the constant is API 33, the key is not. */
private const val EXTRA_IS_SENSITIVE = "android.content.extra.IS_SENSITIVE"

/** A password manager marked this clip secret. */
internal fun isSecretClip(description: ClipDescription?): Boolean =
    description?.extras?.getBoolean(EXTRA_IS_SENSITIVE) == true
