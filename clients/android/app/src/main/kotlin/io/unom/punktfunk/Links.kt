package io.unom.punktfunk

import android.content.ClipData
import android.content.ClipboardManager
import android.content.ContentResolver
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.net.Uri
import android.os.Build
import androidx.annotation.StringRes
import androidx.core.content.pm.ShortcutInfoCompat
import androidx.core.content.pm.ShortcutManagerCompat
import androidx.core.graphics.drawable.IconCompat
import io.unom.punktfunk.R as AppR
import io.unom.punktfunk.console.fetchArt
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.LinkError
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost

// Where a `punktfunk://` URL goes besides a browser (design/client-deep-links.md §4/§5): the
// clipboard, a home-screen shortcut, and a launcher file a frontend such as Cocoon or ES-DE opens.
// Each surface that hands a link out builds its own URL; whether the platform took it, and what
// to say about that, is answered here once for every shell.

/** What a link handed out by a title (or host) can become. */
internal enum class LinkAction { Copy, HomeScreen, LauncherFile }

/** Put a `punktfunk://` URL on the clipboard. False = no clipboard service, or it refused. */
internal fun putLinkOnClipboard(context: Context, url: String): Boolean {
    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
        ?: return false
    return runCatching {
        clipboard.setPrimaryClip(ClipData.newPlainText("Punktfunk link", url))
    }.isSuccess
}

/**
 * The message id to tell the user about a copy, or null for "say nothing".
 *
 * Android 13 draws its own clipboard confirmation, and stacking a second one on top of it is the
 * platform's own documented anti-pattern. Below it nothing visible happens at all unless we say
 * so — a silent menu item reads as a broken one.
 */
@StringRes
internal fun linkCopyMessage(copied: Boolean): Int? = when {
    copied && Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU -> null
    copied -> R.string.link_copied
    else -> R.string.link_copy_failed
}

/** False on Android TV's launchers and any home screen that takes no pinned shortcuts. */
internal fun canPinShortcuts(context: Context): Boolean =
    ShortcutManagerCompat.isRequestPinShortcutSupported(context)

/**
 * Ask the home screen to pin a shortcut that opens [url]; the OS asks the user to confirm. [id]
 * keys it, so pinning the same thing again updates the old one. The intent names MainActivity,
 * so another app claiming `punktfunk://` can't take the tap. False = the launcher refused.
 */
internal fun pinLinkShortcut(
    context: Context,
    id: String,
    label: String,
    url: String,
    art: Bitmap?,
): Boolean {
    val icon = art?.let { IconCompat.createWithAdaptiveBitmap(squareIcon(it)) }
        ?: IconCompat.createWithResource(context, AppR.mipmap.ic_launcher)
    val info = ShortcutInfoCompat.Builder(context, id)
        .setShortLabel(label)
        .setIcon(icon)
        .setIntent(Intent(Intent.ACTION_VIEW, Uri.parse(url), context, MainActivity::class.java))
        .build()
    return runCatching { ShortcutManagerCompat.requestPinShortcut(context, info, null) }
        .getOrDefault(false)
}

/**
 * Pin a title (or a host's desktop) as a shortcut to [url], its first poster that loads as the
 * icon. Blocking — the poster may come off the network — so call it off the main thread.
 * Returns the line to show, or null when the OS's own confirmation is the answer.
 */
internal fun pinTitleShortcut(
    context: Context,
    identity: ClientIdentity?,
    host: KnownHost,
    posters: List<String>,
    url: String,
    title: String,
    id: String,
): String? {
    val bytes = identity?.let { me ->
        runCatching {
            fetchArt(posters, posterHttp(context, me, host.address, host.fpHex, host.port), offline = false)
        }.getOrNull()
    }
    val art = bytes?.let { BitmapFactory.decodeByteArray(it, 0, it.size) }
    return if (pinLinkShortcut(context, id, title, url, art)) {
        null
    } else {
        context.getString(R.string.android_shortcut_unsupported)
    }
}

/** The shortcut id for one link target: the same title pinned twice is one shortcut. */
internal fun shortcutId(hostId: String, launch: String?, preset: String?): String =
    listOf("link", hostId, launch.orEmpty(), preset.orEmpty()).joinToString(":")

/** The centre square of a poster, at launcher size; the launcher masks the edges. */
private fun squareIcon(art: Bitmap): Bitmap {
    val side = minOf(art.width, art.height)
    val square = Bitmap.createBitmap(art, (art.width - side) / 2, (art.height - side) / 2, side, side)
    return Bitmap.createScaledBitmap(square, 256, 256, true)
}

/** Any other type makes the file picker append its own extension to `.punktfunk`. */
internal const val LAUNCHER_FILE_MIME = "application/octet-stream"

/** `Halo: Reach` → `Halo Reach.punktfunk`: a name every SD-card file system takes. */
internal fun launcherFileName(title: String): String {
    val base = title.replace(Regex("[\\\\/:*?\"<>|\\p{Cntrl}]"), " ")
        .replace(Regex("\\s+"), " ")
        .trim()
        .trimEnd('.')
    return "${base.ifEmpty { "Punktfunk" }}.punktfunk"
}

/** A launcher file is the link alone on one line: the format [linkFromLauncherFile] reads. */
internal fun writeLauncherFile(context: Context, uri: Uri, url: String): Boolean = runCatching {
    val out = context.contentResolver.openOutputStream(uri, "wt") ?: error("no output stream")
    out.use { it.write("$url\n".toByteArray()) }
}.isSuccess

/**
 * The link inside a launcher file a frontend hands over as `content://`, or null when it holds
 * none of ours. Read on the caller's (main) thread: frontends keep these on local storage.
 */
internal fun linkFromLauncherFile(context: Context, uri: Uri): String? {
    if (uri.scheme != ContentResolver.SCHEME_CONTENT) return null
    // The read runs with this app's own permissions, so its own providers are off limits.
    val authority = uri.authority ?: return null
    val owner = context.packageManager.resolveContentProvider(authority, 0)?.packageName
    if (owner == context.packageName) return null
    val head = runCatching {
        context.contentResolver.openInputStream(uri)?.use { stream ->
            val buf = ByteArray(2 * DeepLinks.MAX_URL_LEN)
            var n = 0
            while (n < buf.size) {
                val r = stream.read(buf, n, buf.size - n)
                if (r < 0) break
                n += r
            }
            String(buf, 0, n, Charsets.UTF_8)
        }
    }.getOrNull() ?: return null
    return launcherFileLink(head)
}

/**
 * A launcher file's link: its first non-blank line (a byte-order mark from a Windows editor
 * dropped), if that is one of ours. The routers vet the rest like any other URL.
 */
internal fun launcherFileLink(text: String): String? {
    val line = text.removePrefix("﻿").lineSequence().map(String::trim)
        .firstOrNull(String::isNotEmpty) ?: return null
    val parsed = DeepLinks.parse(line)
    return line.takeUnless { parsed is DeepLinkResult.Refused && parsed.error == LinkError.NOT_OUR_SCHEME }
}
