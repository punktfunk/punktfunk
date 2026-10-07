package io.unom.punktfunk.kit

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.hardware.usb.UsbRequest
import android.os.Build
import android.util.Log
import java.nio.ByteBuffer
import java.util.concurrent.TimeoutException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Generic USB transport for a client-captured HID controller — the device-agnostic half of what
 * [Sc2UsbLink] pioneered, now shared with the Sony capture ([DsCapture]). Claims the controller
 * interface(s) — `force = true` detaches the kernel/OS driver, so a captured pad can't
 * double-drive the ordinary InputDevice path — runs a multiplexed [UsbRequest] read loop, and
 * writes the host/capture's reports back to the device (interrupt-OUT when the interface has one,
 * else EP0 `SET_REPORT`).
 *
 * Everything device-specific is [Config]: which attached device to pick, which of its interfaces
 * to claim, and an optional keep-alive (feature reports re-sent on a firmware-watchdog cadence —
 * the SC2's lizard-mode refresh; a DualSense needs none).
 *
 * **Unplug is signalled, never inferred from silence:** a quiet controller is not a missing one
 * (an SC2 on-glass round tripped exactly this — a 5 s silence heuristic firing on an idle pad).
 * The real signals are [UsbManager.ACTION_USB_DEVICE_DETACHED] for this device, or `requestWait`
 * returning sustained hard errors (every transfer fails instantly once the fd is dead).
 */
class HidUsbLink(
    private val context: Context,
    private val config: Config,
    /** One input report and the interface it came in on (a Puck's slots are interfaces 2–5). */
    private val onReport: (report: ByteArray, len: Int, iface: Int) -> Unit,
    private val onClosed: () -> Unit,
) {
    /**
     * The per-device knowledge this transport is parameterized by. [ifaceFilter] narrows WHICH
     * HID/vendor-class interfaces get claimed (the class check itself is built in) — e.g. the SC2
     * Puck's controller slots, or the DualSense's single HID interface among its audio siblings.
     * [keepAliveFeatures] are full feature reports (id byte first) re-sent to the streaming
     * interface every [keepAliveMs] AND once at claim time; empty = no keep-alive.
     * [releaseFeatures] are written once as the claim goes back, undoing what the keep-alive held
     * (the SC2's lizard mode) so the OS gets the device in the state it expects.
     */
    class Config(
        val tag: String,
        val threadName: String,
        val deviceMatch: (UsbDevice) -> Boolean,
        val ifaceFilter: (UsbDevice, UsbInterface) -> Boolean = { _, _ -> true },
        val keepAliveFeatures: List<ByteArray> = emptyList(),
        val keepAliveMs: Long = 0,
        val releaseFeatures: List<ByteArray> = emptyList(),
    )

    private val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager

    /** One claimed interface: its endpoints + the read state the reader thread owns. */
    private class Claim(
        val iface: UsbInterface,
        val epIn: UsbEndpoint,
        val epOut: UsbEndpoint?,
    ) {
        // Direct: UsbRequest.queue copies a heap buffer into a fresh direct one on every report.
        val inBuf: ByteBuffer = ByteBuffer.allocateDirect(64)
        var inReq: UsbRequest? = null
        var outReq: UsbRequest? = null
        var outBusy = false
        /** Read by the keep-alive thread to find the interfaces that carry a pad. */
        @Volatile var reports = 0L

        /** Pending OUT reports, submitted by the reader thread — only one thread may drive a
         *  connection's [UsbRequest]s ([UsbDeviceConnection.requestWait] returns ANY completed
         *  request; a second waiter would steal the reader's completions). See [OutReportQueue]
         *  for what gets discarded when it fills, and why that is not simply "the oldest". */
        val outQueue = OutReportQueue<ByteArray>()
    }

    private var connection: UsbDeviceConnection? = null
    private var device: UsbDevice? = null
    private var claims: List<Claim> = emptyList()

    /** The claim whose IN endpoint last produced data — where output/feature writes go.
     *  Written by the reader thread, read by the feedback thread (feature control transfers). */
    @Volatile private var activeClaim: Claim? = null

    private var reader: Thread? = null
    private var keepAlive: Thread? = null
    private var detachReceiver: BroadcastReceiver? = null

    @Volatile private var running = false

    /** Latches on the first "this link is down" signal so [onClosed] fires exactly once, however
     *  many of the racing detectors (detach broadcast, reader error streak, failed re-queue) see
     *  it. Reset by [start]. */
    private val down = AtomicBoolean(false)

    /** First attached matching device, or null. Does not need USB permission to enumerate. */
    fun findDevice(): UsbDevice? = usb.deviceList.values.firstOrNull(config.deviceMatch)

    /**
     * Open a SECOND connection to the same device, for a consumer that needs its own descriptor.
     *
     * **Not a convenience — a correctness requirement.** `UsbDeviceConnection.requestWait()`
     * returns *any* completed request on that connection, and the same is true of the usbfs reap
     * ioctl underneath it: two independent transfer engines sharing one descriptor steal each
     * other's completions. This link's reader owns its connection exclusively (see the note on
     * [Claim.outQueue]), so anything else driving transfers on this device — the isochronous audio
     * renderer — must open its own.
     *
     * usbfs allows the same device to be opened many times, and claims are per (descriptor,
     * interface), so a claim made on this connection does not conflict with one made on that.
     *
     * The caller owns the returned connection and must close it.
     */
    fun openAuxConnection(): UsbDeviceConnection? {
        val dev = device ?: return null
        return usb.openDevice(dev)
    }

    /**
     * The open connection's usbfs file descriptor, or -1 when the link is not running.
     *
     * Handed to native code that drives interfaces this link deliberately does NOT claim — the
     * pad's isochronous audio endpoint (see `pad_audio` on the native side), which Android's own
     * USB API cannot reach because `UsbRequest` rejects anything that is not bulk or interrupt.
     * usbfs claims are per interface, so a native claim of the audio interface leaves this link's
     * HID claim untouched.
     *
     * **The borrower must stop using it before [stop] runs**: closing the connection while a
     * transfer is in flight pulls the descriptor out from under the kernel.
     */
    val fileDescriptor: Int get() = connection?.fileDescriptor ?: -1

    /**
     * Claim [dev]'s controller interface(s), then start the read loop and, when configured, the
     * keep-alive thread. The caller has already obtained USB permission. Returns false when
     * nothing could be claimed.
     */
    fun start(dev: UsbDevice): Boolean {
        if (!usb.hasPermission(dev)) {
            Log.e(config.tag, "no USB permission for ${dev.deviceName}")
            return false
        }
        val conn = usb.openDevice(dev) ?: run {
            Log.e(config.tag, "openDevice failed for ${dev.deviceName}")
            return false
        }
        val claimed = claimControllerInterfaces(dev, conn)
        if (claimed.isEmpty()) {
            Log.e(config.tag, "no claimable interface on ${dev.deviceName} (PID=0x%04x)".format(dev.productId))
            conn.close()
            return false
        }
        connection = conn
        device = dev
        claims = claimed
        down.set(false)
        running = true
        Log.i(
            config.tag,
            "USB link up: PID=0x%04x ifaces=%s".format(
                dev.productId,
                claimed.joinToString {
                    "%d(in=0x%02x out=%s)".format(
                        it.iface.id, it.epIn.address,
                        it.epOut?.let { e -> "0x%02x".format(e.address) } ?: "-",
                    )
                },
            ),
        )
        // The REAL unplug signal — silence never is (an idle pad may simply stop streaming).
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context?, intent: Intent?) {
                if (intent?.action != UsbManager.ACTION_USB_DEVICE_DETACHED) return
                val gone: UsbDevice? = intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
                if (gone?.deviceName == dev.deviceName) {
                    Log.i(config.tag, "USB detached (${dev.deviceName})")
                    linkDown()
                }
            }
        }
        detachReceiver = receiver
        val filter = IntentFilter(UsbManager.ACTION_USB_DEVICE_DETACHED)
        if (Build.VERSION.SDK_INT >= 33) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            context.registerReceiver(receiver, filter)
        }
        if (config.keepAliveFeatures.isNotEmpty()) {
            claimed.forEach { sendKeepAlive(conn, it.iface.id) }
        }
        reader = Thread({ readLoop(conn, claimed) }, config.threadName).apply {
            isDaemon = true
            start()
        }
        if (config.keepAliveFeatures.isNotEmpty() && config.keepAliveMs > 0) {
            keepAlive = Thread({ keepAliveLoop(conn, claimed) }, "${config.threadName}-keepalive").apply {
                isDaemon = true
                start()
            }
        }
        return true
    }

    /**
     * Re-send the keep-alive features every [Config.keepAliveMs] to every interface that streams
     * (else every claimed one) — each pad on a Puck has its own; replaying also repairs settings
     * another consumer changed. Its own thread, because each EP0 transfer blocks up to
     * [WRITE_TIMEOUT_MS] and the reader must not.
     */
    private fun keepAliveLoop(conn: UsbDeviceConnection, claims: List<Claim>) {
        while (running) {
            try {
                Thread.sleep(config.keepAliveMs)
            } catch (_: InterruptedException) {
                return
            }
            if (!running) return
            streaming(claims).forEach { sendKeepAlive(conn, it.iface.id) }
        }
    }

    /**
     * Claim every candidate controller interface: HID (or vendor-class) interfaces that pass the
     * config's [Config.ifaceFilter], with an INT/BULK IN endpoint (OUT optional — the fallback is
     * EP0 `SET_REPORT`). `force = true` detaches the kernel/OS driver, so the pad also vanishes
     * from Android's own input stack while captured.
     */
    private fun claimControllerInterfaces(dev: UsbDevice, conn: UsbDeviceConnection): List<Claim> {
        val out = mutableListOf<Claim>()
        for (i in 0 until dev.interfaceCount) {
            val iface = dev.getInterface(i)
            if (!config.ifaceFilter(dev, iface)) continue
            val hidOrVendor = iface.interfaceClass == UsbConstants.USB_CLASS_HID ||
                iface.interfaceClass == 0xFF
            if (!hidOrVendor) continue
            var inEp: UsbEndpoint? = null
            var outEp: UsbEndpoint? = null
            for (e in 0 until iface.endpointCount) {
                val ep = iface.getEndpoint(e)
                val usable = ep.type == UsbConstants.USB_ENDPOINT_XFER_INT ||
                    ep.type == UsbConstants.USB_ENDPOINT_XFER_BULK
                if (!usable) continue
                if (ep.direction == UsbConstants.USB_DIR_IN && inEp == null) inEp = ep
                if (ep.direction == UsbConstants.USB_DIR_OUT && outEp == null) outEp = ep
            }
            if (inEp == null) continue
            if (conn.claimInterface(iface, true)) {
                out.add(Claim(iface, inEp, outEp))
            } else {
                Log.w(config.tag, "claimInterface(iface ${iface.id}) failed")
            }
        }
        return out
    }

    /**
     * The multiplexed read loop: one IN request queued per claimed interface at all times, OUT
     * writes submitted from each [Claim.outQueue], completions routed via [UsbRequest.getClientData]. It
     * never blocks on EP0, so input keeps flowing while a control transfer is in flight.
     */
    private fun readLoop(conn: UsbDeviceConnection, claims: List<Claim>) {
        val live = claims.filter { c ->
            val req = UsbRequest()
            if (!req.initialize(conn, c.epIn)) {
                Log.w(config.tag, "UsbRequest.initialize(IN, iface ${c.iface.id}) failed")
                return@filter false
            }
            req.clientData = c
            c.inReq = req
            c.epOut?.let { ep ->
                val o = UsbRequest()
                if (o.initialize(conn, ep)) {
                    o.clientData = c
                    c.outReq = o
                } else {
                    Log.w(config.tag, "UsbRequest.initialize(OUT, iface ${c.iface.id}) failed — output reports via EP0")
                }
            }
            c.inBuf.clear()
            req.queue(c.inBuf)
        }
        if (live.isEmpty()) {
            Log.e(config.tag, "no IN request could be queued")
            finishReader(claims)
            // `start` already returned true, so without this the owner would sit waiting on a
            // capture that never streams and never reports itself dead.
            linkDown()
            return
        }
        val scratch = ByteArray(64)
        var errorsSince = 0L // elapsedRealtime of the first hard error in the current streak
        try {
            while (running) {
                val now = android.os.SystemClock.elapsedRealtime()
                // Submit each interface's next pending OUT report.
                for (c in live) {
                    val req = c.outReq ?: continue
                    if (c.outBusy) continue
                    c.outQueue.poll()?.let { data -> if (req.queue(ByteBuffer.wrap(data))) c.outBusy = true }
                }
                val done = try {
                    conn.requestWait(READ_TIMEOUT_MS)
                } catch (_: TimeoutException) {
                    // A quiet controller is NOT an unplug — keep listening indefinitely; the
                    // detach broadcast is the real signal.
                    errorsSince = 0L
                    continue
                }
                if (done == null) {
                    // Hard error. On a real unplug these storm continuously (the detach
                    // broadcast usually beats us to it); tolerate transient ones.
                    if (errorsSince == 0L) errorsSince = now
                    if (now - errorsSince >= ERROR_UNPLUG_MS) {
                        Log.i(config.tag, "USB request errors persisting ${now - errorsSince} ms — treating as unplug")
                        break
                    }
                    continue
                }
                errorsSince = 0L
                val claim = done.clientData as? Claim ?: continue
                if (done === claim.inReq) {
                    val n = claim.inBuf.position()
                    if (n > 0) {
                        claim.inBuf.flip()
                        claim.inBuf.get(scratch, 0, n)
                        if (claim.reports++ == 0L) {
                            Log.i(
                                config.tag,
                                "first report on iface %d: id=0x%02x len=%d".format(
                                    claim.iface.id, scratch[0].toInt() and 0xFF, n,
                                ),
                            )
                        }
                        activeClaim = claim
                        onReport(scratch, n, claim.iface.id)
                    }
                    claim.inBuf.clear()
                    if (!claim.inReq!!.queue(claim.inBuf)) {
                        Log.i(config.tag, "re-queue(IN, iface ${claim.iface.id}) failed — treating as unplug")
                        break
                    }
                } else if (done === claim.outReq) {
                    claim.outBusy = false
                }
            }
        } finally {
            finishReader(claims)
        }
        linkDown()
    }

    /**
     * Report the link down, exactly once, from whichever detector noticed first — the detach
     * broadcast (main thread) or the reader thread on its way out.
     *
     * This only *signals*; releasing the connection and the interfaces stays the owner's job, via
     * the [stop] its `onClosed` handler calls. Previously nothing released them on this path: the
     * detach receiver flipped a flag and fired the callback, so an unplug left the connection open,
     * the interfaces claimed (the pad could not return to Android's own input stack) and the
     * receiver still registered — and a re-plug overwrote the field holding it, leaking a receiver
     * that stayed live for the process's lifetime.
     */
    private fun linkDown() {
        running = false
        if (down.compareAndSet(false, true)) onClosed()
    }

    private fun finishReader(claims: List<Claim>) {
        for (c in claims) {
            runCatching { c.inReq?.cancel(); c.inReq?.close() }
            runCatching { c.outReq?.cancel(); c.outReq?.close() }
            c.inReq = null
            c.outReq = null
        }
    }

    /**
     * Write one raw report to the device: kind 0 = output report (the active interface's
     * interrupt-OUT, else a `SET_REPORT(Output)` control transfer), kind 1 = feature report
     * (`SET_REPORT(Feature)`). [data] is the full report, id byte first, hidapi framing.
     *
     * [coalesce] tells the pending-OUT queue whether a newer report of the same kind may replace
     * this one — [OutReportQueue.KEY_RUMBLE] for motor levels, the default [OutReportQueue.NO_COALESCE]
     * for one-shots (lightbar, player LEDs, trigger effects) the sender will not repeat.
     *
     * Returns whether the report reached the device or is queued for it. A caller that is writing
     * a **stop** needs this: a discarded stop has nothing behind it, so it must not be mistaken
     * for one that landed.
     */
    fun writeRaw(
        kind: Int,
        data: ByteArray,
        coalesce: Int = OutReportQueue.NO_COALESCE,
        iface: Int = ANY_IFACE,
    ): Boolean {
        if (data.isEmpty()) return false
        val target = claimFor(iface) ?: return false
        return when (kind) {
            0 -> {
                if (target.outReq != null) {
                    // Interrupt-OUT rides UsbRequests submitted by the reader thread.
                    target.outQueue.offer(data, coalesce)
                } else {
                    setReport(REPORT_TYPE_OUTPUT, data, iface)
                }
            }
            1 -> setReport(REPORT_TYPE_FEATURE, data, iface)
            else -> false
        }
    }

    /** The claim for [iface]; [ANY_IFACE] is the one that last reported, else the first. */
    private fun claimFor(iface: Int): Claim? {
        val named = if (iface == ANY_IFACE) null else claims.firstOrNull { it.iface.id == iface }
        return named ?: activeClaim ?: claims.firstOrNull()
    }

    /** The claims that have carried a report, else all of them. */
    private fun streaming(claims: List<Claim>): List<Claim> = claims.filter { it.reports > 0 }.ifEmpty { claims }

    private fun setReport(type: Int, data: ByteArray, iface: Int = ANY_IFACE): Boolean {
        val conn = connection ?: return false
        val ifId = claimFor(iface)?.iface?.id ?: return false
        return sendReport(conn, ifId, type, data)
    }

    /**
     * Write one output report EP0-direct (`SET_REPORT(Output)`), bypassing the interrupt-OUT
     * queue — for a teardown write that must land while the reader thread is stopping and the
     * queue would never drain (e.g. a rumble stop before the interfaces release). Safe from any
     * thread: EP0 control transfers are independent of the reader's `requestWait`.
     */
    fun writeControl(data: ByteArray, iface: Int = ANY_IFACE): Boolean =
        data.isNotEmpty() && setReport(REPORT_TYPE_OUTPUT, data, iface)

    private fun sendKeepAlive(conn: UsbDeviceConnection, ifaceId: Int) {
        for (f in config.keepAliveFeatures) sendReport(conn, ifaceId, REPORT_TYPE_FEATURE, f)
    }

    /**
     * HID `SET_REPORT` control transfer with hidapi's report-id framing: a non-zero leading byte
     * is the report id (sent in wValue AND kept in the payload); a zero leading byte means
     * "unnumbered" (id 0 in wValue, id byte stripped from the payload). EP0 is independent of
     * the interrupt endpoints, so this is safe alongside the reader thread's requestWait.
     */
    private fun sendReport(
        conn: UsbDeviceConnection,
        ifaceId: Int,
        type: Int,
        data: ByteArray,
    ): Boolean {
        val id = data[0].toInt() and 0xFF
        val payload = if (id == 0) data.copyOfRange(1, data.size) else data
        // controlTransfer returns the byte count, or a negative value on failure — a failed write
        // must be reported as such, not swallowed (a dropped rumble stop has nothing behind it).
        val n = runCatching {
            conn.controlTransfer(
                0x21, // host→device, class, interface
                0x09, // SET_REPORT
                (type shl 8) or id,
                ifaceId,
                payload,
                payload.size,
                WRITE_TIMEOUT_MS,
            )
        }.getOrDefault(-1)
        return n >= 0
    }

    /**
     * Read one report back OUT of the device — HID `GET_REPORT`, the EP0 mirror of [sendReport].
     * [type] is [REPORT_TYPE_FEATURE] (or output), [id] the report number, [len] the report's full
     * declared size INCLUDING its leading id byte, which a numbered report echoes back in byte 0
     * (hidapi framing). Returns what arrived — truncated if the device answered short — or null
     * when the device refuses the request or the link is down.
     *
     * ⚠ **Once, at claim time; never per input report.** EP0 is independent of the interrupt
     * endpoints (see [sendReport]), so this is safe alongside the reader thread — but it BLOCKS the
     * calling thread for up to [WRITE_TIMEOUT_MS], and a blocking control transfer in the report
     * path would wreck capture latency. The one caller reads a Sony pad's fixed motion calibration
     * when the capture engages ([DsCapture]).
     */
    fun getReport(type: Int, id: Int, len: Int, iface: Int = ANY_IFACE): ByteArray? {
        if (len <= 0) return null
        val conn = connection ?: return null
        val ifId = claimFor(iface)?.iface?.id ?: return null
        val buf = ByteArray(len)
        val n = runCatching {
            conn.controlTransfer(
                0xA1, // device→host, class, interface
                0x01, // GET_REPORT
                (type shl 8) or id,
                ifId,
                buf,
                buf.size,
                WRITE_TIMEOUT_MS,
            )
        }.getOrDefault(-1)
        return when {
            n >= len -> buf
            n > 0 -> buf.copyOf(n)
            else -> null
        }
    }

    /** The device's USB serial; null before [start] or when the device withholds it. */
    fun serialNumber(): String? = runCatching { device?.serialNumber }.getOrNull()

    /**
     * One feature query: SET [request] zero-padded to [len], then GET its reply, retried for up
     * to a second — a Puck holds the GET while it relays the query over the radio. Blocks the
     * caller; never on the report path.
     */
    fun exchange(request: ByteArray, len: Int = 64, iface: Int = ANY_IFACE): ByteArray? {
        if (request.isEmpty()) return null
        val frame = request.copyOf(len)
        if (!setReport(REPORT_TYPE_FEATURE, frame, iface)) return null
        repeat(50) {
            getReport(REPORT_TYPE_FEATURE, frame[0].toInt() and 0xFF, len, iface)?.let { return it }
            Thread.sleep(20)
        }
        return null
    }

    /**
     * Stop the read loop and the keep-alive, write [Config.releaseFeatures], then release the
     * interfaces. Idempotent; does not fire [onClosed].
     *
     * Safe to call from the `onClosed` handler itself — that is how an unplug gets cleaned up,
     * and it arrives on the reader thread, which must not try to join itself. Both threads are
     * joined before the connection closes: a transfer in flight must not lose its descriptor.
     */
    fun stop() {
        running = false
        // Claim the down-latch so the reader's own exit does not report a close the owner asked for.
        down.set(true)
        detachReceiver?.let { runCatching { context.unregisterReceiver(it) } }
        detachReceiver = null
        keepAlive?.let { it.interrupt(); runCatching { it.join(1000) } }
        keepAlive = null
        if (reader !== Thread.currentThread()) {
            runCatching { reader?.join(1000) }
            // Only forget the thread once it is actually gone: clearing it while it still runs
            // would let a later stop() skip the join and free the connection under it.
            reader = null
        }
        // Hand the device back before the claim goes: both threads are joined, so EP0 is ours
        // alone and no keep-alive can follow. Best effort — a detached device answers an error,
        // which is exactly as much as this needs to do about it.
        for (c in streaming(claims)) {
            for (f in config.releaseFeatures) setReport(REPORT_TYPE_FEATURE, f, c.iface.id)
            c.outQueue.clear()
        }
        activeClaim = null
        for (c in claims) runCatching { connection?.releaseInterface(c.iface) }
        claims = emptyList()
        runCatching { connection?.close() }
        connection = null
        device = null
    }

    companion object {
        private const val READ_TIMEOUT_MS = 100L
        private const val WRITE_TIMEOUT_MS = 250
        /** Hard `requestWait` ERRORS (not timeouts) persisting this long = the fd is dead. */
        private const val ERROR_UNPLUG_MS = 2000L
        private const val REPORT_TYPE_OUTPUT = 0x02
        /** HID feature-report type — public for [getReport] callers ([writeRaw] takes a kind). */
        const val REPORT_TYPE_FEATURE = 0x03
        /** No interface named: the one that last reported, else the first claimed. */
        const val ANY_IFACE = -1
    }
}
