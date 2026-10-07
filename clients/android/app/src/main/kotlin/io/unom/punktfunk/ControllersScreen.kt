package io.unom.punktfunk

import android.content.Context
import android.hardware.input.InputManager
import android.os.Build
import android.os.CombinedVibration
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.os.Vibrator
import android.util.Log
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.DsDevice
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.Sc2BleLink
import io.unom.punktfunk.kit.Sc2Capture
import io.unom.punktfunk.kit.bodyRumbleFor
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.padHasMotor
import kotlinx.coroutines.delay

/**
 * Connected-controllers debug view (Settings -> Controller -> Connected controllers): everything
 * the app can see about attached input devices, plus a live input test. This exists for exactly
 * the support case where a pad "doesn't work" - adapters and BT-to-USB dongles often enumerate
 * with a different identity than the physical pad, or not as a gamepad at all, and punktfunk only
 * forwards devices Android classifies as gamepad/joystick. This screen makes that visible on the
 * device itself.
 *
 * The TOUCH presentation, and since 2026-08 the only one: the console reaches the same answer
 * through its own Skia screen (`crates/pf-console-ui/src/screens/controllers.rs`), which keeps the
 * console's input on the page instead of suspending it behind a Compose takeover. What this screen
 * still owns alone is the live input test - the console receives only the aggregated navigation
 * sample, which is nowhere near a per-device axis/trigger readout. Everything the console DOES
 * need from here it asks for as a `ConsoleCmd::PadAction` (see [SkiaConsoleShell]), which is why
 * [padInfoOf] and [testRumble] are internal rather than private.
 */
@Composable
internal fun ControllersScreen(
    gamepadSetting: Int,
    onBack: () -> Unit,
    padsOverride: List<PadInfo>? = null,
) {
    BackHandler(onBack = onBack)
    val scroll = rememberScrollState()
    // Events are OBSERVED (not consumed) while the test is off, which is what keeps the
    // "Last input" line live while browsing. Nothing else here wants the pad.
    var testing by remember { mutableStateOf(false) }
    val onTestingChange: (Boolean) -> Unit = { testing = it }
    val contentPadding = PaddingValues(horizontal = 20.dp, vertical = 24.dp)
    val context = LocalContext.current
    val activity = context as? MainActivity

    // Device list, re-read on every hot-plug event. [padsOverride] replaces it wholesale: the
    // screenshot harness runs where no InputDevice can exist, and the connected-pad card is the
    // point of that shot.
    var generation by remember { mutableIntStateOf(0) }
    val body = remember { deviceBodyVibrator(context) }
    val pads = padsOverride ?: remember(generation) { Gamepad.pads() }.map { padInfoOf(it, body) }
    val others = remember(generation) {
        InputDevice.getDeviceIds()
            .toList()
            .mapNotNull { InputDevice.getDevice(it) }
            // Everything real that is NOT counted as a controller — including a device that claims
            // a pad source with no pad hardware behind it, which the Gamepads list above now
            // rejects. One list or the other, never neither: this screen is where someone looks
            // when the client's idea of "a pad is attached" disagrees with the room.
            .filter { !it.isVirtual && !Gamepad.looksLikeController(it) }
    }
    DisposableEffect(Unit) {
        val im = context.getSystemService(InputManager::class.java)
        val listener = object : InputManager.InputDeviceListener {
            override fun onInputDeviceAdded(deviceId: Int) { generation++ }
            override fun onInputDeviceRemoved(deviceId: Int) { generation++ }
            override fun onInputDeviceChanged(deviceId: Int) { generation++ }
        }
        im.registerInputDeviceListener(listener, Handler(Looper.getMainLooper()))
        onDispose { im.unregisterInputDeviceListener(listener) }
    }

    val test = rememberInputTest(activity, testing, onTestingChange)

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(scroll)
            .padding(contentPadding),
        verticalArrangement = Arrangement.spacedBy(24.dp),
    ) {
        Text("Controllers", style = MaterialTheme.typography.headlineMedium)

        // Capture-side detection, re-checked on USB hot-plug. The SC2 is never an InputDevice
        // (lizard mode is kb/mouse; the capture claims even those away) so it's enumerated from
        // the USB device list + bonded BLE; a Sony pad IS an InputDevice until claimed, so its
        // row supplements the PadRow below with the capture status + the USB grant.
        var usbGeneration by rememberUsbGeneration(context)
        val sc2Probe = remember { Sc2Capture(context) }
        val sc2Usb = remember(usbGeneration) { sc2Probe.findUsbDevice() }
        // Answers null without the Bluetooth grant (and logs why) — see Sc2BleLink.
        val sc2Ble = remember(usbGeneration) { sc2Probe.pairedBleAddress() }
        val sc2Present = sc2Usb != null || sc2Ble != null
        // A BLE-paired SC2 cannot be seen at all until Bluetooth is granted, so "no controller
        // detected" would be the wrong thing to print at someone who has one paired. This is the
        // screen a user opens when a pad is missing, so the grant belongs here — see
        // [sc2BluetoothGrantOffered] for when it is worth offering, and the lizard-mode
        // InputDevice probe (no permission of its own) for how we word it.
        val btPermitted = remember(usbGeneration) { Sc2BleLink.permissionGranted(context) }
        val sc2OnBluetooth = remember(usbGeneration) { Gamepad.sc2InputDevicePresent() }
        val dsUsb = remember(usbGeneration) {
            (context.getSystemService(Context.USB_SERVICE) as android.hardware.usb.UsbManager)
                .deviceList.values.firstOrNull {
                    it.vendorId == DsDevice.VID_SONY && it.productId in DsDevice.USB_PIDS
                }
        }

        Group("Gamepads") {
            if (sc2Present) Sc2Row(sc2Usb, activity)
            dsUsb?.let { DsRow(it) }
            if (pads.isEmpty() && !sc2Present) {
                Text(
                    "No controller detected. Punktfunk can only forward devices Android " +
                        "classifies as a gamepad or joystick — a pad connected through an adapter " +
                        "or hub may show up under \"Other input devices\" below with the adapter's " +
                        "identity, or not at all.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            // After that paragraph on purpose: when nothing was detected, this is the actionable
            // half of the same answer — the one pad we are blind to rather than one Android has
            // simply classified oddly.
            if (
                sc2BluetoothGrantOffered(
                    permissionGranted = btPermitted,
                    usbSc2 = sc2Usb != null,
                    sc2Attached = sc2OnBluetooth,
                    anyPadDetected = pads.isNotEmpty(),
                )
            ) {
                Sc2BluetoothRow(attached = sc2OnBluetooth, activity = activity) { usbGeneration++ }
            }
            // Every real controller is forwarded now (Automatic forwards them all, each on its own
            // wire pad index) — not just the first. A joystick-only device Android doesn't classify as
            // a gamepad still can't be forwarded (the host wants a gamepad), so gate the badge on it.
            pads.forEach { info ->
                PadRow(info, gamepadSetting = gamepadSetting)
            }
        }

        Group("Input test") {
            Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Test inputs", style = MaterialTheme.typography.bodyLarge)
                    Text(
                        when {
                            test.holdSatisfied -> "Release B to finish"
                            testing -> "Controller input stays on this screen — hold B to finish"
                            else -> "Show button presses and stick motion live"
                        },
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Switch(
                    checked = testing,
                    onCheckedChange = { on -> onTestingChange(on); if (!on) test.held.clear() },
                )
            }
            if (testing) {
                ButtonGrid(test.held)
                AXIS_LABELS.forEach { label -> AxisBar(label, test.axes[label] ?: 0f) }
            }
            test.lastInput?.let {
                Text(
                    "Last input — $it",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }

        Group("Other input devices") {
            if (others.isEmpty()) {
                Text(
                    "None",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            others.forEach { dev ->
                Column {
                    Text(dev.name, style = MaterialTheme.typography.bodyMedium)
                    Text(
                        deviceDetail(dev),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

/** What the live input test shows: the lit buttons, the axis bars, the last raw event line. */
private class InputTest {
    val held = mutableStateMapOf<Int, Boolean>()
    val axes = mutableStateMapOf<String, Float>()
    var lastInput by mutableStateOf<String?>(null)
    var bHeld by mutableStateOf(false)
    /** The hold has lasted long enough; the test ends when B is let go (see the probe). */
    var holdSatisfied by mutableStateOf(false)
}

/**
 * The live input test. While [testing], the MainActivity probes consume pad events (so they show
 * up here instead of driving focus navigation); holding B releases, since the pad can no longer
 * reach the Switch. Off, events are OBSERVED, which keeps the "Last input" line live while browsing.
 */
@Composable
private fun rememberInputTest(
    activity: MainActivity?,
    testing: Boolean,
    onTestingChange: (Boolean) -> Unit,
): InputTest {
    val test = remember { InputTest() }
    // The probes below are built ONCE and then read these for the life of the screen, so
    // capturing `testing` plainly would freeze the value it had when the probe was made — the
    // test would consume nothing.
    val consuming by rememberUpdatedState(testing)
    // The console's refusal thud, on whatever actuator the driving pad or this device has.
    val haptics by rememberUpdatedState(rememberConsoleHaptics())
    DisposableEffect(Unit) {
        // One entry on the MainActivity probe stack, removed by identity on the way out, so a
        // teardown never takes another screen's claim with it.
        val keyProbe: (KeyEvent) -> Boolean = probe@{ event ->
            if (!Gamepad.isPad(event.device)) return@probe false
            // Read ONCE, up front: the test can end inside this very event, and the release that
            // ended it still has to be swallowed here — see the B branch below.
            val consume = consuming
            // The CORRECTED keycode, so this screen shows the button the stream will send and not
            // the one Android guessed for a pad it has no key layout for — the two differ on every
            // controller [Gamepad.padKeyCode] exists for, and a tester that disagrees with the
            // stream is worse than no tester. The raw pair is still reported in "Last input".
            val code = Gamepad.padKeyCode(event)
            when (event.action) {
                KeyEvent.ACTION_DOWN -> {
                    test.held[code] = true
                    if (code == KeyEvent.KEYCODE_BUTTON_B) test.bHeld = true
                }
                KeyEvent.ACTION_UP -> {
                    test.held[code] = false
                    if (code == KeyEvent.KEYCODE_BUTTON_B) {
                        test.bHeld = false
                        if (consume) {
                            if (event.eventTime - event.downTime >= HOLD_TO_FINISH_MS) {
                                // The hold ends the test HERE, on the release, and NOT the moment
                                // the 1.2 s elapsed: end it a moment earlier and this release falls
                                // through unconsumed to the activity's B→BACK remap, which takes the
                                // whole screen with it. Finishing the test and leaving the screen on
                                // one press is not what "hold B to finish" says.
                                onTestingChange(false)
                                test.held.clear()
                            } else {
                                // A short B is not swallowed either. While the test owns the pad, B
                                // is a BUTTON UNDER TEST — it lights its chip like every other — so
                                // a tap can't also mean "leave", and in the console B is otherwise
                                // the universal back. The press gets the boundary thud instead, the
                                // same answer a refused step gets on the settings screen: heard, and
                                // it means something else here.
                                haptics.boundary()
                            }
                        }
                    }
                }
            }
            // Raw scancode AND keycode, plus the correction when one fired: this line is what a
            // field report needs to pin an unmapped pad's report order without the device in hand.
            val raw = KeyEvent.keyCodeToString(event.keyCode).removePrefix("KEYCODE_")
            val fixed = KeyEvent.keyCodeToString(code).removePrefix("KEYCODE_")
            test.lastInput = "${event.device?.name}: scan 0x%X · %s%s".format(
                event.scanCode,
                raw,
                if (code != event.keyCode) " → $fixed" else "",
            )
            consume
        }
        val motionProbe: (MotionEvent) -> Boolean = probe@{ event ->
            if (!Gamepad.isPad(event.device)) return@probe false
            test.axes.putAll(padAxes(event))
            consuming
        }
        val probes = MainActivity.PadProbes(keyProbe, motionProbe)
        activity?.pushPadProbes(probes)
        onDispose { activity?.removePadProbes(probes) }
    }
    // Hold-B-to-exit: with events consumed, the pad can't reach the Switch — a 1.2 s hold ends the
    // test instead (touch still works). This half only ANSWERS the hold once it is long enough; the
    // release is what ends the test (see the probe). Letting go early cancels the effect before the
    // delay fires, so nothing is announced.
    LaunchedEffect(test.bHeld, testing) {
        if (test.bHeld && testing) {
            delay(HOLD_TO_FINISH_MS)
            test.holdSatisfied = true
            // A hold with no answer at the moment it lands is a hold you keep holding. Say it in
            // both channels a couch user has: a pulse in the hands, a changed line on the screen.
            haptics.confirm()
        } else {
            test.holdSatisfied = false
        }
    }

    return test
}

/**
 * The USB hot-plug generation: bumps on every attach/detach so capture-side detection re-runs.
 */
@Composable
private fun rememberUsbGeneration(context: Context): MutableState<Int> {
    val generation = remember { mutableIntStateOf(0) }
    DisposableEffect(Unit) {
        val receiver = object : android.content.BroadcastReceiver() {
            override fun onReceive(c: Context?, i: android.content.Intent?) { generation.intValue++ }
        }
        val filter = android.content.IntentFilter().apply {
            addAction(android.hardware.usb.UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(android.hardware.usb.UsbManager.ACTION_USB_DEVICE_DETACHED)
        }
        if (Build.VERSION.SDK_INT >= 33) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            context.registerReceiver(receiver, filter)
        }
        onDispose { runCatching { context.unregisterReceiver(receiver) } }
    }
    return generation
}

/**
 * The eight test axes, read through the device's resolved map exactly as `Gamepad.AxisMapper`
 * reads it while streaming — on a pad Android has no key layout for, the right stick and the
 * triggers are not on the axes their names suggest.
 */
internal fun padAxes(event: MotionEvent): Map<String, Float> {
    val map = Gamepad.padMap(event.device)
    fun trigger(mapped: Int, a: Int, b: Int) = if (mapped == Gamepad.AXIS_NONE) {
        maxOf(event.getAxisValue(a), event.getAxisValue(b))
    } else {
        map.level(event.getAxisValue(mapped))
    }
    return mapOf(
        "LX" to event.getAxisValue(MotionEvent.AXIS_X),
        "LY" to event.getAxisValue(MotionEvent.AXIS_Y),
        "RX" to event.getAxisValue(map.rightStickX),
        "RY" to event.getAxisValue(map.rightStickY),
        "LT" to trigger(map.leftTrigger, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE),
        "RT" to trigger(map.rightTrigger, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS),
        "HX" to event.getAxisValue(MotionEvent.AXIS_HAT_X),
        "HY" to event.getAxisValue(MotionEvent.AXIS_HAT_Y),
    )
}

/**
 * Whether to offer the Bluetooth grant for a directly-paired Steam Controller 2.
 *
 * Only when it could change the answer ([permissionGranted] false), and only when there is reason
 * to think it would: an SC2 is visibly attached in lizard mode ([sc2Attached] — the permission-free
 * probe), or nothing was detected at all ([anyPadDetected] false) and a Bluetooth SC2 is precisely
 * the pad this client cannot see without the grant. A [usbSc2] is already captured over USB and
 * needs no Bluetooth, and someone with working controllers and no sign of an SC2 is shown nothing.
 */
fun sc2BluetoothGrantOffered(
    permissionGranted: Boolean,
    usbSc2: Boolean,
    sc2Attached: Boolean,
    anyPadDetected: Boolean,
): Boolean = !permissionGranted && !usbSc2 && (sc2Attached || !anyPadDetected)

/**
 * The Bluetooth grant for a directly-paired Steam Controller 2 — the card that exists because a
 * BLE SC2 is invisible without it.
 *
 * A wired or Puck SC2 is enumerated over USB with no permission at all, so it shows up in this
 * screen either way; the bonded list a BLE one lives in is behind `BLUETOOTH_CONNECT` from API 31
 * and answers "nothing is paired" rather than "ask me first" when the permission is missing. Until
 * this existed, nothing in the client ever requested it, so a Bluetooth SC2 was silently absent
 * everywhere — no capture, no controller layout, no forwarding — while the same pad over USB
 * worked (field report, 2026-08-15).
 *
 * [attached] distinguishes "we can see one sitting in lizard mode" from "you may have one paired",
 * which is the difference between a statement and a guess. [onGranted] re-probes the caller's
 * device state; the menu capture is engaged from here too, so the pad starts driving the UI on the
 * grant rather than at the next resume.
 */
@Composable
private fun Sc2BluetoothRow(
    attached: Boolean,
    activity: MainActivity?,
    onGranted: () -> Unit,
) {
    val launcher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        if (granted) {
            activity?.startSc2MenuNav()
            onGranted()
        }
    }
    val permission = Sc2BleLink.CONNECT_PERMISSION ?: return
    OutlinedCard(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(
                if (attached) "Steam Controller 2" else "Steam Controller 2 over Bluetooth",
                style = MaterialTheme.typography.bodyLarge,
            )
            Text(
                when {
                    attached ->
                        "Paired over Bluetooth. Punktfunk needs Bluetooth access to capture it — " +
                            "until then it stays in its built-in keyboard/mouse mode and no game " +
                            "sees a controller."
                    else ->
                        "A Steam Controller 2 paired over Bluetooth can't be detected without " +
                            "Bluetooth access. Wired and Puck-dongle controllers need no " +
                            "permission and are already listed above."
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            OutlinedButton(onClick = { launcher.launch(permission) }) {
                Text("Grant Bluetooth access")
            }
        }
    }
}

/**
 * The Steam Controller 2 card — capture-side state, since a (claimed or lizard-mode) SC2 never
 * appears as a gamepad InputDevice. Shows the transport, whether the capture is live (driving
 * these menus now; streamed as-is in a session), a grant button when USB access is missing, and
 * the rumble test a captured pad has no vibrator for.
 */
@Composable
private fun Sc2Row(usbDev: android.hardware.usb.UsbDevice?, activity: MainActivity?) {
    val context = LocalContext.current
    val active = activity?.sc2MenuActive == true
    val usbManager = context.getSystemService(Context.USB_SERVICE) as android.hardware.usb.UsbManager
    val permitted = usbDev != null && usbManager.hasPermission(usbDev)
    OutlinedCard(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    "Steam Controller 2",
                    style = MaterialTheme.typography.bodyLarge,
                    modifier = Modifier.weight(1f),
                )
                if (active) {
                    Text(
                        "navigating this UI",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
            Text(
                when {
                    usbDev == null -> "Paired via Bluetooth"
                    usbDev.productId == io.unom.punktfunk.kit.Sc2Device.PID_WIRED -> "Wired (USB)"
                    else -> "Puck dongle (USB)"
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            when {
                active -> {
                    Text(
                        "Captured — streams as-is: the host presents a real Steam Controller 2 " +
                            "that its Steam drives directly (trackpads, gyro, haptics).",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    // The pad has left the input stack, so [testRumble]'s vibrator path cannot
                    // reach it; the capture writes the grip-rumble report itself. Nothing reads
                    // back, so both messages say what was done, never that a motor ran. `active`
                    // is only ever true with an activity behind it.
                    OutlinedButton(onClick = {
                        val sent = activity.testSc2Rumble()
                        val msg =
                            if (sent) "Sent a rumble to the controller."
                            else "Couldn't reach the controller."
                        Toast.makeText(context, msg, Toast.LENGTH_SHORT).show()
                    }) { Text("Test rumble") }
                }
                usbDev != null && !permitted -> {
                    Text(
                        "Needs USB access to be captured.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    OutlinedButton(onClick = { activity?.startSc2MenuNav(forceAsk = true) }) {
                        Text("Grant USB access")
                    }
                }
                else -> Text(
                    "Detected — capture engages automatically.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

/**
 * Broadcast action for the Sony-pad USB grants — fired by both the menu-time auto-ask
 * ([MainActivity.maybeAskDsPermission]) and [DsRow]'s explicit button, so an open card
 * refreshes whichever dialog was answered.
 */
internal const val DS_USB_PERMISSION_ACTION = "io.unom.punktfunk.DS_CONTROLLERS_USB_PERMISSION"

/**
 * The Sony USB pad card — capture status + the USB grant. The grant normally arrives via the
 * menu-time auto-ask the moment the pad attaches ([MainActivity.maybeAskDsPermission]); the
 * button here is the recovery path after a deny (the auto-ask fires once per attach). Shown
 * ALONGSIDE the pad's ordinary [PadRow] (unclaimed it is still an InputDevice); the capture
 * itself only runs inside a stream, so at menu time this card is pure status.
 */
@Composable
private fun DsRow(usbDev: android.hardware.usb.UsbDevice) {
    val context = LocalContext.current
    val settingOn = remember { SettingsStore(context).load().dsCapture }
    val usbManager = context.getSystemService(Context.USB_SERVICE) as android.hardware.usb.UsbManager
    var permitted by remember(usbDev) { mutableStateOf(usbManager.hasPermission(usbDev)) }
    val model = DsDevice.modelFor(usbDev.productId)
    val label = when (model) {
        DsDevice.Model.DUALSENSE -> "DualSense"
        DsDevice.Model.DUALSENSE_EDGE -> "DualSense Edge"
        DsDevice.Model.DUALSHOCK4 -> "DualShock 4"
        null -> return
    }
    // Refresh `permitted` when the grant dialog answers (the grant itself is system-recorded;
    // this receiver only updates the card).
    val action = DS_USB_PERMISSION_ACTION
    DisposableEffect(usbDev) {
        val receiver = object : android.content.BroadcastReceiver() {
            override fun onReceive(c: Context?, i: android.content.Intent?) {
                if (i?.action == action) permitted = usbManager.hasPermission(usbDev)
            }
        }
        androidx.core.content.ContextCompat.registerReceiver(
            context,
            receiver,
            android.content.IntentFilter(action),
            androidx.core.content.ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        onDispose { runCatching { context.unregisterReceiver(receiver) } }
    }
    OutlinedCard(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text("$label passthrough", style = MaterialTheme.typography.bodyLarge)
            Text(
                "Wired (USB)",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            when {
                !settingOn -> Text(
                    "Passthrough is disabled in Settings — enable \"DualSense / DualShock " +
                        "passthrough (USB)\" to capture it.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                !permitted -> {
                    Text(
                        "Needs USB access — grant it now and streams capture the pad silently.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    OutlinedButton(onClick = {
                        usbManager.requestPermission(
                            usbDev,
                            android.app.PendingIntent.getBroadcast(
                                context, 3, // requestCode 3 — 0/1/2 are the SC2/stream grants
                                android.content.Intent(action).setPackage(context.packageName),
                                // MUTABLE: the USB stack appends the grant extras to this intent.
                                android.app.PendingIntent.FLAG_MUTABLE,
                            ),
                        )
                    }) {
                        Text("Grant USB access")
                    }
                }
                else -> {
                    Text(
                        if (model == DsDevice.Model.DUALSHOCK4) {
                            "Ready — captured at stream start: rumble, lightbar and gyro are " +
                                "driven directly."
                        } else {
                            "Ready — captured at stream start: rumble, adaptive triggers, lightbar " +
                                "and gyro are driven directly."
                        },
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    // Pad-audio self test. Deliberately reachable WITHOUT a stream: it exists to
                    // answer "can this phone drive this pad's audio endpoint at all", and gating
                    // that behind a live session would make it depend on the very thing one wants
                    // to rule out when a session misbehaves. DualSense only — the DS4 has no
                    // 4-channel haptics device.
                    if (model != DsDevice.Model.DUALSHOCK4) {
                        var testing by remember { mutableStateOf(false) }
                        var result by remember { mutableStateOf<String?>(null) }
                        result?.let {
                            Text(
                                it,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                        OutlinedButton(
                            enabled = !testing,
                            onClick = {
                                testing = true
                                result = null
                                Thread({
                                    // Its OWN connection: the renderer's descriptor must never be
                                    // shared with another transfer engine, and that applies to
                                    // this test as much as to the real path.
                                    val conn = runCatching { usbManager.openDevice(usbDev) }.getOrNull()
                                    val fd = conn?.fileDescriptor ?: -1
                                    val r = if (fd >= 0) {
                                        io.unom.punktfunk.kit.NativeBridge.nativePadAudioSelfTest(fd, 3, 60)
                                    } else {
                                        -1
                                    }
                                    conn?.close()
                                    val msg = when {
                                        r > 0 -> "Haptics test passed — $r frames to the pad."
                                        r == -1 -> "Couldn't open the pad's haptics — the pad still works normally"
                                        r == -2 -> "The audio stream stopped part-way."
                                        else -> "The stream opened but no audio reached the pad."
                                    }
                                    android.os.Handler(android.os.Looper.getMainLooper()).post {
                                        result = msg
                                        testing = false
                                    }
                                }, "pf-pad-selftest-ui").start()
                            },
                        ) {
                            Text(if (testing) "Testing…" else "Test haptics")
                        }
                    }
                }
            }
        }
    }
}

/** One detected gamepad: identity, what it streams as, and a rumble test. */
@Composable
private fun PadRow(info: PadInfo, gamepadSetting: Int) {
    OutlinedCard(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(info.name, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
                if (info.forwarded) {
                    // Android's own controller number (1-based; 0 = unassigned), shown so a multi-pad
                    // user can tell which physical pad is which. The stream's wire pad index is
                    // assigned separately (lowest-free per device) once streaming starts.
                    val number = info.controllerNumber
                    Text(
                        if (number > 0) "forwarded · player $number" else "forwarded to host",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
            Text(
                info.detail,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            val resolved = info.resolvedPref
            Text(
                if (gamepadSetting == Gamepad.PREF_AUTO) {
                    "Streams as: ${Gamepad.prefLabel(resolved)} (automatic)"
                } else {
                    "Streams as: ${Gamepad.prefLabel(gamepadSetting)} (set in Settings; " +
                        "automatic would pick ${Gamepad.prefLabel(resolved)})"
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            // Only when a correction is actually in force: on a pad Android has a key layout for
            // there is nothing to say, and a line that says "normal" on every device teaches
            // nobody anything. Named rather than merely flagged, so a field report can quote it.
            padButtonsNote(info.buttons)?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (info.canRumble) {
                val context = LocalContext.current
                OutlinedButton(onClick = {
                    if (info.dev?.let { testRumble(it, deviceBodyVibrator(context)) } != true) {
                        Toast.makeText(context, "No motor answered", Toast.LENGTH_SHORT).show()
                    }
                }) { Text("Test rumble") }
            } else {
                Text(
                    "No rumble motors reported — host rumble will be silent",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

/** The forwarded buttons as chips that light up while held. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ButtonGrid(held: Map<Int, Boolean>) {
    FlowRow(
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        TEST_BUTTONS.forEach { (label, keyCode) ->
            val active = held[keyCode] == true
            Text(
                label,
                style = MaterialTheme.typography.labelMedium,
                color = if (active) MaterialTheme.colorScheme.onPrimary
                else MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier
                    .background(
                        if (active) MaterialTheme.colorScheme.primary
                        else MaterialTheme.colorScheme.surfaceVariant,
                        RoundedCornerShape(6.dp),
                    )
                    .padding(horizontal = 10.dp, vertical = 6.dp),
            )
        }
    }
}

/** A labelled live axis bar; sticks/HAT are −1..1 (centre = half), triggers 0..1. */
@Composable
private fun AxisBar(label: String, value: Float) {
    val progress = if (label == "LT" || label == "RT") value else (value + 1f) / 2f
    Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Text(label, style = MaterialTheme.typography.labelMedium, modifier = Modifier.width(32.dp))
        LinearProgressIndicator(
            progress = { progress.coerceIn(0f, 1f) },
            modifier = Modifier.weight(1f),
        )
        Text(
            "%+.2f".format(value),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(start = 8.dp),
        )
    }
}

/** A titled section — same look as the Settings groups. */
@Composable
private fun Group(title: String, content: @Composable ColumnScope.() -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Text(
            title,
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.padding(start = 4.dp),
        )
        Column(verticalArrangement = Arrangement.spacedBy(12.dp), content = content)
    }
}

/**
 * Whether this device is actually forwarded to the host — the same rule the stream's [GamepadRouter]
 * applies: a real, non-virtual controller whose source classes include GAMEPAD. A joystick-only node
 * (e.g. a DualSense motion-sensor sibling, or an adapter that enumerates as bare joystick) shows in
 * the list but isn't forwarded.
 */
private fun isForwarded(dev: InputDevice): Boolean =
    !dev.isVirtual && dev.sources and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD

/**
 * Everything [PadRow] renders, decoupled from [InputDevice] so the screenshot harness can compose
 * the connected-pad card at all — Robolectric enumerates no input devices, and a marketing shot of
 * "no controller detected" sells nothing. Production always maps a real device via [padInfoOf];
 * [dev] powers the rumble test and is absent only in the harness (the button then no-ops).
 */
internal data class PadInfo(
    val name: String,
    val detail: String,
    val forwarded: Boolean,
    val controllerNumber: Int,
    val resolvedPref: Int,
    val canRumble: Boolean,
    /**
     * The report order this pad's buttons were resolved to ([Gamepad.padButtons]). Defaults to
     * the pad Android already knows, which is what a screenshot scene wants and what the note
     * under the card stays silent about.
     */
    val buttons: Gamepad.PadButtons = Gamepad.PadButtons.NATIVE,
    val dev: InputDevice? = null,
)

/** [body] is this device's own vibrator, which plays a built-in pad's rumble ([bodyRumbleFor]). */
internal fun padInfoOf(dev: InputDevice, body: Vibrator?): PadInfo = PadInfo(
    name = dev.name,
    detail = deviceDetail(dev),
    forwarded = isForwarded(dev),
    controllerNumber = dev.controllerNumber,
    resolvedPref = Gamepad.prefFor(dev),
    buttons = Gamepad.padMap(dev).buttons, // via padMap so the list refresh reuses the cache
    canRumble = padHasMotor(dev) || bodyRumbleFor(dev, body) != null,
    dev = dev,
)

/**
 * A short pulse on the motor that plays the pad's rumble: its own, or [body] for a built-in pad
 * without one ([bodyRumbleFor]). Also the console's `PadAction::Rumble`. `false` when nothing
 * was pulsed — no motor, or the platform refused — so the caller can say so instead of leaving
 * "Test" indistinguishable from a pad that has no motor.
 */
internal fun testRumble(dev: InputDevice, body: Vibrator?): Boolean = runCatching {
    bodyRumbleFor(dev, body)?.let {
        it.vibrate(VibrationEffect.createOneShot(300, 200))
        return true
    }
    if (Build.VERSION.SDK_INT >= 31) {
        val vm = dev.vibratorManager
        if (vm.vibratorIds.isEmpty()) return false
        vm.vibrate(CombinedVibration.createParallel(VibrationEffect.createOneShot(300, 200)))
    } else {
        @Suppress("DEPRECATION")
        val v = dev.vibrator
        if (!v.hasVibrator()) return false
        v.vibrate(VibrationEffect.createOneShot(300, 200))
    }
    true
}.getOrElse {
    Log.w("Controllers", "rumble on ${dev.name}", it)
    false
}

/** Identity line: VID:PID + the source classes Android assigned. */
/**
 * What to say about a pad whose buttons had to be resolved from their scancodes because Android
 * has no key layout for it — null for a pad it does know, which needs no explanation.
 */
private fun padButtonsNote(buttons: Gamepad.PadButtons): String? = when (buttons) {
    Gamepad.PadButtons.NATIVE -> null
    Gamepad.PadButtons.GENERIC_SONY ->
        "Android has no button layout for this controller — read as a PlayStation pad"
    Gamepad.PadButtons.GENERIC_XBOX ->
        "Android has no button layout for this controller — read as an Xbox pad"
    Gamepad.PadButtons.SONY_MODERN ->
        "Android has no button layout for this controller — face buttons corrected"
}

internal fun deviceDetail(dev: InputDevice): String =
    "%04X:%04X · %s".format(dev.vendorId, dev.productId, sourcesLabel(dev.sources))

private fun sourcesLabel(sources: Int): String {
    fun has(flag: Int) = sources and flag == flag
    val names = buildList {
        if (has(InputDevice.SOURCE_GAMEPAD)) add("gamepad")
        if (has(InputDevice.SOURCE_JOYSTICK)) add("joystick")
        if (has(InputDevice.SOURCE_DPAD)) add("dpad")
        if (has(InputDevice.SOURCE_KEYBOARD)) add("keyboard")
        if (has(InputDevice.SOURCE_MOUSE)) add("mouse")
        if (has(InputDevice.SOURCE_TOUCHSCREEN)) add("touchscreen")
        if (has(InputDevice.SOURCE_TOUCHPAD)) add("touchpad")
        if (has(InputDevice.SOURCE_STYLUS)) add("stylus")
        if (has(InputDevice.SOURCE_ROTARY_ENCODER)) add("rotary")
    }
    return if (names.isEmpty()) "sources 0x%08X".format(sources) else names.joinToString(" · ")
}

/** Buttons shown in the test grid (label → Android keycode). */
private val TEST_BUTTONS = listOf(
    "A" to KeyEvent.KEYCODE_BUTTON_A,
    "B" to KeyEvent.KEYCODE_BUTTON_B,
    "X" to KeyEvent.KEYCODE_BUTTON_X,
    "Y" to KeyEvent.KEYCODE_BUTTON_Y,
    "LB" to KeyEvent.KEYCODE_BUTTON_L1,
    "RB" to KeyEvent.KEYCODE_BUTTON_R1,
    "L2" to KeyEvent.KEYCODE_BUTTON_L2,
    "R2" to KeyEvent.KEYCODE_BUTTON_R2,
    "LS" to KeyEvent.KEYCODE_BUTTON_THUMBL,
    "RS" to KeyEvent.KEYCODE_BUTTON_THUMBR,
    "Select" to KeyEvent.KEYCODE_BUTTON_SELECT,
    "Start" to KeyEvent.KEYCODE_BUTTON_START,
    "Guide" to KeyEvent.KEYCODE_BUTTON_MODE,
    // The two buttons Android has no keycode for, on the keycodes [Gamepad.buttonBit] borrows for
    // them. Only a driverless Sony pad reaches these; every other controller leaves them dark.
    "Touch" to KeyEvent.KEYCODE_BUTTON_15,
    "Mute" to KeyEvent.KEYCODE_BUTTON_16,
    "↑" to KeyEvent.KEYCODE_DPAD_UP,
    "↓" to KeyEvent.KEYCODE_DPAD_DOWN,
    "←" to KeyEvent.KEYCODE_DPAD_LEFT,
    "→" to KeyEvent.KEYCODE_DPAD_RIGHT,
)

/** Axis bars shown in the test view, in display order. */
private val AXIS_LABELS = listOf("LX", "LY", "RX", "RY", "LT", "RT", "HX", "HY")

/**
 * How long B must be held to end the input test — and, below that, how long a press still counts as
 * a tap that gets answered rather than ignored. One constant, because a hold that ends at 1.2 s
 * while the "you tapped" answer stops at some other number leaves a window where a press does
 * nothing at all.
 */
private const val HOLD_TO_FINISH_MS = 1_200L
