package io.unom.punktfunk.kit

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothGattCharacteristic
import android.bluetooth.BluetoothGattDescriptor
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.BluetoothStatusCodes
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import java.util.UUID
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * BLE transport for a Steam Controller 2 paired directly with the device (no Puck). The standard
 * HID service (0x1812) is claimed by the OS (and would feed the pad through the ordinary input
 * stack in lizard-crippled form), so this talks Valve's vendor GATT service instead — the same
 * approach Steam itself uses on hosts without a dongle.
 *
 * GATT operations are serialized by a small state machine (connect → MTU → discover → subscribe
 * each notify char → lizard-off → ready); duplicate callbacks (the Android stack sometimes fires
 * `onMtuChanged` twice) are ignored. Once ready, every write waits its turn in [writes]: Android
 * takes one GATT operation per connection, unacked writes included, until its write callback, and
 * a write refused while another is in flight is lost. Every framing rule lives device-free in
 * [Sc2Device], where tests reach it: the `0x45` re-prepend on the way up, and on the way down
 * the per-report characteristic each output id is routed to and the feature characteristic
 * every command goes to, id byte stripped in both directions.
 *
 * The link re-acquires by itself — `autoConnect` leaves the request with the stack, so a pad that
 * powers off mid-session reconnects on its own and [onClosed] only means "release the slot".
 *
 * Requires BLUETOOTH_CONNECT (the caller gates on it); connection priority is bumped to HIGH to
 * pull the connection interval from ~50 ms down to ~11 ms.
 */
@SuppressLint("MissingPermission")
class Sc2BleLink(
    private val context: Context,
    private val onReport: (report: ByteArray, len: Int) -> Unit,
    private val onClosed: () -> Unit,
) {
    private enum class State { IDLE, CONNECTING, MTU_REQUESTED, DISCOVERING, SUBSCRIBING, READY }

    private val manager = context.getSystemService(Context.BLUETOOTH_SERVICE) as BluetoothManager

    private var gatt: BluetoothGatt? = null
    private val pendingSubs = mutableListOf<BluetoothGattCharacteristic>()
    private var subsIndex = 0

    /** One characteristic write: where it goes, what it carries, and whether it wants the ack. */
    private class GattWrite(
        val ch: BluetoothGattCharacteristic,
        val payload: ByteArray,
        val acked: Boolean,
        /** A read of [ch] instead: its value lands in [reads]. */
        val read: Boolean = false,
    )

    /** Values of the feature characteristic read by [exchange]. */
    private val reads = LinkedBlockingQueue<ByteArray>()

    /** Writes waiting for the link. Rumble supersedes rumble; the rest are never dropped. */
    private val writes = OutReportQueue<GattWrite>()

    /** A write is with the stack; its callback clears this and sends the next. */
    private val inFlight = AtomicBoolean(false)

    /** A write the stack refused as busy: it goes before the queue. Touched only under [inFlight]. */
    @Volatile private var held: GattWrite? = null
    private var lizardTicker: Thread? = null

    /** Output ids this firmware has no characteristic for — one line each, not one per resend. */
    private val unmappedIds = HashSet<Int>()

    /** Lowercase characteristic uuid → characteristic. Rebuilt whole on discovery and read-only
     *  after, so the write path reaches it from the feedback thread without a lock. */
    @Volatile private var chars: Map<String, BluetoothGattCharacteristic> = emptyMap()

    @Volatile private var featureChar: BluetoothGattCharacteristic? = null

    /** Set by [stop] so a disconnect tears the link down instead of re-arming it. */
    @Volatile private var stopped = false

    @Volatile private var state = State.IDLE

    /**
     * Bonded devices that look like a Steam Controller (name heuristic — BLE exposes no PID here).
     *
     * Gates on [permissionGranted] itself rather than trusting callers to: without the permission
     * `bondedDevices` throws, and the `runCatching` below turns that into an empty list —
     * indistinguishable from "no controller is paired". A capture that never engaged for want of a
     * permission nobody had asked for is exactly the silence this logs its way out of.
     */
    fun pairedControllers(): List<BluetoothDevice> {
        if (!permissionGranted(context)) {
            Log.i(TAG, "BLE controllers not enumerated: $CONNECT_PERMISSION not granted")
            return emptyList()
        }
        return runCatching {
            manager.adapter?.bondedDevices.orEmpty().filter { dev ->
                val n = runCatching { dev.name }.getOrNull() ?: return@filter false
                NAME_HINTS.any { n.contains(it, ignoreCase = true) }
            }
        }.getOrDefault(emptyList())
    }

    /** Connect to the bonded controller at [address]. Reports start flowing once READY. */
    fun start(address: String): Boolean {
        if (!permissionGranted(context)) {
            Log.i(TAG, "BLE capture not started: $CONNECT_PERMISSION not granted")
            return false
        }
        val adapter = manager.adapter ?: return false
        if (!adapter.isEnabled) return false
        val device = runCatching { adapter.getRemoteDevice(address) }.getOrNull() ?: return false
        stopped = false
        state = State.CONNECTING
        // autoConnect: the stack keeps the request and connects whenever the pad appears, so a
        // controller that is bonded but asleep costs nothing instead of failing a 30 s attempt.
        // Deprecated at 37 for a flag-gated overload this one forwards to; below 37 it is the API.
        @Suppress("DEPRECATION")
        gatt = device.connectGatt(context, true, callback, BluetoothDevice.TRANSPORT_LE)
        return true
    }

    /**
     * Replay one raw host report on the pad. [kind] is the C ABI's HID_RAW_OUTPUT (0) /
     * HID_RAW_FEATURE (1) and [data] is id-first, exactly as Steam wrote it. The id selects the
     * destination and never rides along: an output goes to its own characteristic trimmed to that
     * id's declared length, a feature command to [Sc2Device.BLE_FEATURE_CHAR] whole.
     */
    fun writeRaw(kind: Int, data: ByteArray) {
        if (state != State.READY) return
        if (kind == 0) {
            val out = Sc2Device.outputWrite(data) ?: return
            val ch = chars[out.charUuid]
                ?: return noteUnmapped(data[0].toInt() and 0xFF, out.charUuid)
            write(ch, out.payload, acked = false, Sc2Device.outputCoalesceKey(data))
        } else {
            val payload = Sc2Device.featurePayload(data) ?: return
            write(featureChar ?: return, payload, acked = true, OutReportQueue.NO_COALESCE)
        }
    }

    /**
     * One feature query: written to the feature characteristic, then read back, as SDL reads a
     * feature over GATT. A reply that does not echo the query (a keep-alive slipped in) asks again,
     * twice at most. Blocks up to a second per try; never call it from a GATT callback.
     */
    fun exchange(request: ByteArray): ByteArray? {
        val ch = featureChar ?: return null
        val payload = Sc2Device.featurePayload(request.copyOf(64)) ?: return null
        repeat(3) {
            reads.clear()
            writes.offer(GattWrite(ch, payload, acked = true), OutReportQueue.NO_COALESCE)
            writes.offer(GattWrite(ch, ByteArray(0), acked = true, read = true), OutReportQueue.NO_COALESCE)
            pump()
            val value = reads.poll(1, TimeUnit.SECONDS) ?: return null
            val reply = byteArrayOf(request[0]) + value
            if (reply.size > 1 && reply[1] == request[1]) return reply
        }
        return null
    }

    /**
     * Queue one GATT write, honouring what the characteristic offers — output prefers unacked,
     * feature prefers acked — and send it when the link is free.
     */
    private fun write(ch: BluetoothGattCharacteristic, payload: ByteArray, acked: Boolean, key: Int) {
        val canAck = ch.properties and BluetoothGattCharacteristic.PROPERTY_WRITE != 0
        val canFire = ch.properties and BluetoothGattCharacteristic.PROPERTY_WRITE_NO_RESPONSE != 0
        writes.offer(GattWrite(ch, payload, if (acked) canAck else !canFire), key)
        pump()
    }

    /** Send the next write unless one is with the stack. Any thread; the write callback too. */
    private fun pump() {
        while (state == State.READY) {
            val g = gatt ?: return
            if (!inFlight.compareAndSet(false, true)) return
            val w = held?.also { held = null } ?: writes.poll()
            if (w == null) {
                inFlight.set(false)
                if (writes.size == 0) return // else an offer raced the empty poll: go again
                continue
            }
            when (send(g, w)) {
                Sent.OK -> return // the write callback frees the link
                Sent.BUSY -> {
                    held = w
                    inFlight.set(false)
                    return // whatever holds the link calls back, and that pumps again
                }
                Sent.FAILED -> inFlight.set(false) // this one is lost; try the next
            }
        }
    }

    private enum class Sent { OK, BUSY, FAILED }

    private fun send(g: BluetoothGatt, w: GattWrite): Sent {
        if (w.read) {
            return if (runCatching { g.readCharacteristic(w.ch) }.getOrDefault(false)) Sent.OK else Sent.FAILED
        }
        val type = if (w.acked) {
            BluetoothGattCharacteristic.WRITE_TYPE_DEFAULT
        } else {
            BluetoothGattCharacteristic.WRITE_TYPE_NO_RESPONSE
        }
        return runCatching {
            if (Build.VERSION.SDK_INT >= 33) {
                when (g.writeCharacteristic(w.ch, w.payload, type)) {
                    BluetoothStatusCodes.SUCCESS -> Sent.OK
                    BluetoothStatusCodes.ERROR_GATT_WRITE_REQUEST_BUSY -> Sent.BUSY
                    else -> Sent.FAILED
                }
            } else {
                // Before 33 the payload rides the shared characteristic object, and `false` does
                // not say why — this queue is the only writer, so it is not "busy".
                @Suppress("DEPRECATION")
                w.ch.value = w.payload
                w.ch.writeType = type
                @Suppress("DEPRECATION")
                if (g.writeCharacteristic(w.ch)) Sent.OK else Sent.FAILED
            }
        }.getOrDefault(Sent.FAILED)
    }

    /**
     * A firmware that does not put this output id at `id + 0x35`. Nothing here can recover it
     * live: the output characteristics share one property mask, so a guess is as likely to drive
     * the wrong actuator as the right one. Drop the write and say so once per id.
     */
    private fun noteUnmapped(id: Int, want: String) {
        if (!unmappedIds.add(id)) return
        Log.w(
            TAG,
            "no characteristic for output 0x%02x (want %s) — that actuator is silent"
                .format(id, want),
        )
    }

    /** Write one lizard-mode setting ([Sc2Device.DISABLE_LIZARD] / [Sc2Device.ENABLE_LIZARD]) on
     *  the feature characteristic. Off feeds the firmware watchdog; on hands the pad back. */
    private fun sendLizard(frame: ByteArray) {
        if (state != State.READY) return
        val ch = featureChar ?: return
        val payload = Sc2Device.featurePayload(frame) ?: return
        // One key for both settings: the newest supersedes a pending one.
        write(ch, payload, acked = true, KEY_LIZARD)
    }

    /** Wait until every queued write went out, at most 30 × 5 ms — a GATT write is asynchronous
     *  and a disconnect drops one still queued. Returns early the moment the link is idle. */
    private fun awaitWriteIdle() {
        repeat(30) {
            if (!inFlight.get() && held == null && writes.size == 0) return
            runCatching { Thread.sleep(5) }
        }
    }

    /**
     * Restore lizard mode, then disconnect for good and stop the ticker. Idempotent; does not
     * fire [onClosed]. The restore blocks the caller for as long as [awaitWriteIdle] allows.
     */
    fun stop() {
        stopped = true // a disconnect from here must not re-arm the autoConnect request
        // Join the ticker, or a refresh caught mid-loop lands its lizard-off after the restore
        // below and the pad is dead again. It is interrupted, so it leaves at its next sleep.
        lizardTicker?.interrupt()
        runCatching { lizardTicker?.join(50) }
        lizardTicker = null
        // Still READY here, so the setting can go out: lizard's kb/mouse is what drives the OS
        // once this link lets go, and waiting for the watchdog leaves the pad dead for seconds.
        awaitWriteIdle()
        sendLizard(Sc2Device.ENABLE_LIZARD)
        awaitWriteIdle()
        state = State.IDLE // before the disconnect, so its callback cannot report a live drop
        runCatching { gatt?.disconnect() }
        runCatching { gatt?.close() }
        gatt = null
        forgetConnection()
    }

    /** Drop everything scoped to one connection. The GATT handle itself outlives this — it is
     *  what the stack re-arms onto. */
    private fun forgetConnection() {
        chars = emptyMap()
        featureChar = null
        pendingSubs.clear()
        subsIndex = 0
        writes.clear()
        held = null
        inFlight.set(false) // a callback that can never land now would wedge every later write
    }

    private val callback = object : BluetoothGattCallback() {
        override fun onConnectionStateChange(g: BluetoothGatt, status: Int, newState: Int) {
            when (newState) {
                BluetoothProfile.STATE_CONNECTED -> {
                    // ~11 ms connection interval instead of the ~50 ms default — input latency.
                    g.requestConnectionPriority(BluetoothGatt.CONNECTION_PRIORITY_HIGH)
                    if (state == State.CONNECTING) {
                        state = State.MTU_REQUESTED
                        if (!g.requestMtu(DESIRED_MTU)) {
                            state = State.DISCOVERING
                            g.discoverServices()
                        }
                    }
                }
                BluetoothProfile.STATE_DISCONNECTED -> {
                    val wasLive = state != State.IDLE
                    lizardTicker?.interrupt()
                    lizardTicker = null
                    forgetConnection()
                    state = State.IDLE
                    if (wasLive) onClosed()
                    // A pad power-cycles many times in one session, so re-arm rather than tear
                    // down: with autoConnect that is one call, idle until the pad comes back.
                    if (stopped) return
                    state = State.CONNECTING
                    if (!g.connect()) state = State.IDLE
                }
            }
        }

        override fun onMtuChanged(g: BluetoothGatt, mtu: Int, status: Int) {
            if (state != State.MTU_REQUESTED) return // fired twice on some stacks — act once
            state = State.DISCOVERING
            g.discoverServices()
        }

        override fun onServicesDiscovered(g: BluetoothGatt, status: Int) {
            if (state != State.DISCOVERING || status != BluetoothGatt.GATT_SUCCESS) return
            val valve = g.getService(VALVE_SERVICE) ?: run {
                Log.e(TAG, "Valve vendor service missing — not an SC2?")
                return
            }
            pendingSubs.clear()
            val found = HashMap<String, BluetoothGattCharacteristic>()
            for (ch in valve.characteristics) {
                found[ch.uuid.toString().lowercase()] = ch
                val short = shortUuid(ch.uuid) ?: continue
                val canNotify = ch.properties and BluetoothGattCharacteristic.PROPERTY_NOTIFY != 0
                if (canNotify && short in NOTIFY_LOW..NOTIFY_HIGH) pendingSubs.add(ch)
            }
            chars = found
            featureChar = found[Sc2Device.BLE_FEATURE_CHAR]
            if (featureChar == null) {
                Log.w(TAG, "no feature characteristic — lizard mode and the gyro stay as they are")
            }
            subsIndex = 0
            state = State.SUBSCRIBING
            subscribeNext(g)
        }

        override fun onDescriptorWrite(g: BluetoothGatt, d: BluetoothGattDescriptor, status: Int) {
            if (state == State.SUBSCRIBING) subscribeNext(g)
        }

        override fun onCharacteristicWrite(g: BluetoothGatt, ch: BluetoothGattCharacteristic, status: Int) {
            inFlight.set(false)
            pump()
        }

        override fun onCharacteristicRead(
            g: BluetoothGatt,
            ch: BluetoothGattCharacteristic,
            value: ByteArray,
            status: Int,
        ) {
            if (status == BluetoothGatt.GATT_SUCCESS) reads.offer(value)
            inFlight.set(false)
            pump()
        }

        /** Below 33 only; from 33 the stack calls the overload above. */
        @Deprecated("Replaced at API 33 by the overload that carries the value")
        override fun onCharacteristicRead(g: BluetoothGatt, ch: BluetoothGattCharacteristic, status: Int) {
            if (Build.VERSION.SDK_INT >= 33) return
            @Suppress("DEPRECATION")
            val value = ch.value
            if (status == BluetoothGatt.GATT_SUCCESS && value != null) reads.offer(value)
            inFlight.set(false)
            pump()
        }

        override fun onCharacteristicChanged(g: BluetoothGatt, ch: BluetoothGattCharacteristic, value: ByteArray) {
            val framed = Sc2Device.frameIncoming(value)
            onReport(framed, framed.size)
        }

        /** Below 33 only; from 33 the stack calls both, and the overload above has the value. */
        @Deprecated("Replaced at API 33 by the overload that carries the value")
        override fun onCharacteristicChanged(g: BluetoothGatt, ch: BluetoothGattCharacteristic) {
            if (Build.VERSION.SDK_INT >= 33) return
            @Suppress("DEPRECATION")
            val data = ch.value ?: return
            val framed = Sc2Device.frameIncoming(data)
            onReport(framed, framed.size)
        }
    }

    private fun subscribeNext(g: BluetoothGatt) {
        if (subsIndex >= pendingSubs.size) {
            state = State.READY
            Log.i(TAG, "SC2 BLE link up (${pendingSubs.size} notify chars)")
            sendLizard(Sc2Device.DISABLE_LIZARD)
            // The firmware watchdog re-enables lizard mode; refresh on SDL's cadence until the
            // host's Steam takes over via the raw plane (its writes land through writeRaw too).
            lizardTicker = Thread({
                while (state == State.READY) {
                    try {
                        Thread.sleep(Sc2Device.LIZARD_REFRESH_MS)
                    } catch (_: InterruptedException) {
                        return@Thread
                    }
                    sendLizard(Sc2Device.DISABLE_LIZARD)
                }
            }, "pf-sc2-lizard").apply { isDaemon = true; start() }
            return
        }
        val ch = pendingSubs[subsIndex++]
        g.setCharacteristicNotification(ch, true)
        val cccd = ch.getDescriptor(CCCD) ?: return subscribeNext(g)
        val on = BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE
        val sent = if (Build.VERSION.SDK_INT >= 33) {
            g.writeDescriptor(cccd, on) == BluetoothStatusCodes.SUCCESS
        } else {
            @Suppress("DEPRECATION")
            cccd.value = on
            @Suppress("DEPRECATION")
            g.writeDescriptor(cccd)
        }
        if (!sent) subscribeNext(g) // lose this one, try the rest
    }

    /** The 32-bit short id of a Valve vendor UUID, or null for foreign UUIDs. */
    private fun shortUuid(uuid: UUID): Long? {
        val s = uuid.toString()
        if (!s.endsWith(VALVE_UUID_TAIL)) return null
        return s.substring(0, 8).toLongOrNull(16)
    }

    companion object {
        private const val TAG = "Sc2BleLink"

        private val VALVE_SERVICE: UUID = UUID.fromString(Sc2Device.BLE_SERVICE)
        private const val VALVE_UUID_TAIL = "-1735-4313-b402-38567131e5f3"
        private const val NOTIFY_LOW = 0x100f6c75L
        private const val NOTIFY_HIGH = 0x100f6c7aL
        private val CCCD: UUID = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")

        private val NAME_HINTS =
            listOf("Steam Ctrl", "Steam Controller", "SteamController", "Valve")

        /** Enough for a state payload (45 B) + ATT header with margin. */
        private const val DESIRED_MTU = 100

        /** [writes] key for the lizard-mode setting (rumble is [OutReportQueue.KEY_RUMBLE]). */
        private const val KEY_LIZARD = 2

        /**
         * The runtime permission this transport needs, or null where the platform grants Bluetooth
         * at install time.
         *
         * From API 31 both operations a capture makes — reading the bonded list and `connectGatt`
         * — sit behind the runtime `BLUETOOTH_CONNECT`. Below it the manifest's legacy `BLUETOOTH`
         * (normal-level, granted on install) covers exactly those two, and `BLUETOOTH_CONNECT` is
         * not a permission that platform version knows: `checkSelfPermission` answers DENIED for
         * it and a request is refused without a dialog. Gating on it unconditionally is therefore
         * not merely redundant on old releases — it is a permanent refusal, which is what this
         * null arm exists to avoid.
         */
        val CONNECT_PERMISSION: String? =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                Manifest.permission.BLUETOOTH_CONNECT
            } else {
                null
            }

        /**
         * Whether a BLE capture may run: [CONNECT_PERMISSION] held, or not required on this
         * release. Callers that can offer the user a grant ask this first, so the offer appears
         * only when it would change something.
         */
        fun permissionGranted(context: Context): Boolean {
            val permission = CONNECT_PERMISSION ?: return true
            return context.checkSelfPermission(permission) == PackageManager.PERMISSION_GRANTED
        }
    }
}
