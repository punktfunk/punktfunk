package io.unom.punktfunk

import android.content.Context
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.library.MgmtReply
import io.unom.punktfunk.kit.library.mgmtCall
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody

/**
 * "Send logs to host" — this device's log ring ([NativeBridge.nativeRenderLogs], fed by
 * `pf_client_core::logring`) posted to a paired host's `POST /api/v1/client-logs` over the same
 * mTLS identity the library fetch uses. The bundle is then listed in that host's web console, on
 * its Logs page, beside the host's own log.
 *
 * ONE implementation for both Android shells — the Skia console's host menu
 * (`console.SkiaConsole`) and the touch home's card menu ([ConnectGrid]). It lived only in the
 * console, which made it unreachable on exactly the devices that most need it: a phone whose
 * console never comes up has no route to its own logs at all, and the touch UI is the shell a
 * reporter is looking at when something is wrong. The wording is the desktop console's verbatim
 * (`clients/session/src/console.rs`) so a quoted message means the same thing on every client.
 *
 * Blocking — call it off the main thread.
 */
object SendLogs {
    /** `punktfunk-android <ver> (android <rel>; <abi>) — client log bundle`, the desktop's shape. */
    fun header(context: Context): String {
        val version = runCatching {
            context.packageManager.getPackageInfo(context.packageName, 0).versionName
        }.getOrNull() ?: "?"
        return "punktfunk-android $version (android ${android.os.Build.VERSION.RELEASE}; " +
            "${android.os.Build.SUPPORTED_ABIS.firstOrNull() ?: "?"}) — client log bundle"
    }

    /** The user-facing outcome: the success line, or "Couldn't send logs — <why>". */
    fun toHost(context: Context, identity: ClientIdentity, host: KnownHost): String =
        toHost(
            context, identity,
            addr = host.address, mgmtPort = host.effectiveMgmtPort, fpHex = host.fpHex,
            hostName = host.name.ifBlank { host.address },
        )

    /**
     * The address-and-port form, for the console — its menu addresses a `HostRow`, which carries
     * the mgmt port the ADVERT taught it (fresher than the saved record's).
     */
    fun toHost(
        context: Context,
        identity: ClientIdentity,
        addr: String,
        mgmtPort: Int,
        fpHex: String,
        hostName: String,
    ): String {
        val reply = mgmtCall(identity, addr, mgmtPort, fpHex) { base ->
            val body = NativeBridge.nativeRenderLogs(header(context))
            Request.Builder()
                .url("$base/api/v1/client-logs")
                .post(body.toRequestBody("text/plain; charset=utf-8".toMediaType()))
                .build()
        }
        // The host answers 201 Created: the route STORES a bundle, so any 2xx is a success.
        return if (reply is MgmtReply.Answer && reply.ok) {
            "Logs sent to $hostName — download them from its web console's Logs page"
        } else {
            "Couldn't send logs — ${reply.why}"
        }
    }
}
