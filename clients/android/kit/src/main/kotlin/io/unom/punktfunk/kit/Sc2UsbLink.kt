package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbDevice

/**
 * USB transport for a Steam Controller 2 — wired (`28DE:1302`) or through the wireless Puck
 * dongle (`1304`/`1305`). The SC2 specialization of the shared [HidUsbLink] transport (which owns
 * the claim, read loop, write queue, and unplug handling); this class contributes only what is
 * SC2-specific:
 *
 * **The Puck claims ALL controller interfaces (2..5):** the dongle hosts up to four pads, one
 * HID interface each, and there is no way to know which slot a controller bonded to — claiming
 * only interface 2 read silence while Android's input stack kept the others (the round-2
 * on-glass symptom: the pad surfaced as a generic InputDevice → Xbox360). Each report names its
 * interface, and each pad's rumble and settings go back to its own.
 *
 * **Lizard keep-alive:** the firmware watchdog re-enables lizard mode (built-in kb/mouse
 * emulation) after a few seconds of silence, so [Sc2Device.DISABLE_LIZARD] +
 * [Sc2Device.NORMALIZE_JOYSTICKS] are re-sent on SDL's cadence — the generic link's keep-alive.
 * [Sc2Device.ENABLE_LIZARD] goes back on release, so the pad drives the OS again at once rather
 * than after the watchdog.
 */
class Sc2UsbLink(
    context: Context,
    onReport: (report: ByteArray, len: Int, iface: Int) -> Unit,
    onClosed: () -> Unit,
) {
    private val link = HidUsbLink(
        context,
        HidUsbLink.Config(
            tag = "Sc2UsbLink",
            threadName = "pf-sc2-usb",
            deviceMatch = {
                it.vendorId == Sc2Device.VID_VALVE && it.productId in Sc2Device.USB_PIDS
            },
            // Wired: every HID/vendor interface; dongle: only the controller slots 2..5.
            ifaceFilter = { dev, iface ->
                dev.productId == Sc2Device.PID_WIRED || iface.id in Sc2Device.DONGLE_IFACES
            },
            keepAliveFeatures = listOf(Sc2Device.DISABLE_LIZARD, Sc2Device.NORMALIZE_JOYSTICKS),
            keepAliveMs = Sc2Device.LIZARD_REFRESH_MS,
            releaseFeatures = listOf(Sc2Device.ENABLE_LIZARD),
        ),
        onReport,
        onClosed,
    )

    /** First attached SC2 (wired or Puck), or null. Does not need USB permission to enumerate. */
    fun findDevice(): UsbDevice? = link.findDevice()

    /**
     * Claim [dev]'s controller interface(s) and start the read loop. The caller has already
     * obtained USB permission. Returns false when nothing could be claimed.
     */
    fun start(dev: UsbDevice): Boolean = link.start(dev)

    /**
     * Replay one raw report from the host on the device: kind 0 = output report (Steam's `0x80`
     * rumble & friends), kind 1 = feature report. [data] is the full report, id byte first,
     * exactly as hidapi framed it host-side, for the pad on [iface]. Rumble coalesces per
     * [Sc2Device.outputCoalesceKey].
     */
    fun writeRaw(kind: Int, data: ByteArray, iface: Int) =
        link.writeRaw(kind, data, Sc2Device.outputCoalesceKey(data), iface)

    /** One feature query and its reply on [iface] ([HidUsbLink.exchange]). Blocks up to a second. */
    fun exchange(request: ByteArray, iface: Int): ByteArray? = link.exchange(request, iface = iface)

    fun serialNumber(): String? = link.serialNumber()

    /** One output report on EP0 to [iface], for a write that must land while the link stops. */
    fun writeControl(frame: ByteArray, iface: Int): Boolean = link.writeControl(frame, iface)

    /** Restore lizard mode, stop the read loop, release the interfaces. Idempotent; fires no callback. */
    fun stop() = link.stop()
}
