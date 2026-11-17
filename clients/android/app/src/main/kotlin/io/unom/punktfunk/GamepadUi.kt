package io.unom.punktfunk

import android.app.UiModeManager
import android.content.Context
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.hardware.input.InputManager
import android.os.Handler
import android.os.Looper
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import io.unom.punktfunk.kit.Gamepad

/**
 * [Settings.gamepadUiMode]: take over only while a controller is attached. The default.
 */
const val GAMEPAD_UI_WHEN_CONNECTED = "connected"

/**
 * [Settings.gamepadUiMode]: take over whenever the switch is on, pad or no pad — for a phone or
 * tablet that lives docked to a TV, where the console layout is the one wanted and the pad is not
 * always awake.
 */
const val GAMEPAD_UI_ALWAYS = "always"

/**
 * Whether the controller-optimized "console" home (the host carousel + gamepad chrome) should
 * replace the touch UI — the Android mirror of the Apple client's `GamepadUIEnvironment.isActive`:
 * the user's [enabled] setting AND (the [mode] is [GAMEPAD_UI_ALWAYS] OR a controller is attached
 * OR this is a TV OR the dev [forced] flag). A TV counts unconditionally — its remote/gamepad is
 * the only input, so it's always the console UI (as long as the setting is on), which is why the
 * mode row means nothing there. An unrecognized [mode] waits for a controller, so a value a newer
 * client wrote can never strand this one in a layout it has no way back out of.
 */
fun gamepadUiActive(
    enabled: Boolean,
    mode: String,
    controllerConnected: Boolean,
    tv: Boolean,
    forced: Boolean,
): Boolean = enabled && (mode == GAMEPAD_UI_ALWAYS || controllerConnected || tv || forced)

/** True on a TV: the leanback/television feature or the TELEVISION ui-mode. */
fun isTvDevice(context: Context): Boolean {
    val pm = context.packageManager
    if (pm.hasSystemFeature(PackageManager.FEATURE_LEANBACK) ||
        pm.hasSystemFeature(PackageManager.FEATURE_TELEVISION)
    ) {
        return true
    }
    val uiMode = context.getSystemService(Context.UI_MODE_SERVICE) as? UiModeManager
    return uiMode?.currentModeType == Configuration.UI_MODE_TYPE_TELEVISION
}

/**
 * Live "is a game controller attached" state, updated as pads connect/disconnect via
 * [InputManager]'s device listener — so the home screen flips to the console UI the instant a pad is
 * plugged in or paired, and back to touch when it's removed. Mirrors the reactivity the Apple client
 * gets from observing `GamepadManager.shared`.
 */
@Composable
fun rememberControllerConnected(): State<Boolean> {
    val context = LocalContext.current
    // A menu-captured Steam Controller 2 counts as connected: it drives the console UI through
    // the capture link, but never surfaces as an Android InputDevice (lizard mode is kb/mouse,
    // and the claim removes even those) — the InputManager path below can't see it.
    val activity = context as? MainActivity
    val connected = remember { mutableStateOf(Gamepad.firstPad() != null) }
    DisposableEffect(Unit) {
        val im = context.getSystemService(Context.INPUT_SERVICE) as InputManager
        val listener = object : InputManager.InputDeviceListener {
            private fun refresh() { connected.value = Gamepad.firstPad() != null }
            override fun onInputDeviceAdded(deviceId: Int) = refresh()
            override fun onInputDeviceRemoved(deviceId: Int) = refresh()
            override fun onInputDeviceChanged(deviceId: Int) = refresh()
        }
        im.registerInputDeviceListener(listener, Handler(Looper.getMainLooper()))
        connected.value = Gamepad.firstPad() != null
        onDispose { im.unregisterInputDeviceListener(listener) }
    }
    return remember {
        derivedStateOf { connected.value || activity?.sc2MenuActive == true }
    }
}
