package io.unom.punktfunk.console

import io.unom.punktfunk.HostServices
import io.unom.punktfunk.Settings
import io.unom.punktfunk.models.ActiveSession
import kotlin.coroutines.EmptyCoroutineContext
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.launch
import org.json.JSONObject

/**
 * State only the main thread touches, held where nothing else can reach it. [launch] runs a block
 * on the main dispatcher with the state as its receiver, at once when already there; [now] runs
 * one in place and throws off the main thread.
 */
internal class MainConfined<S>(private val state: S) {
    private val main = Dispatchers.Main.immediate
    private val scope = CoroutineScope(SupervisorJob() + main)

    fun launch(block: suspend S.() -> Unit): Job = scope.launch { state.block() }

    fun <R> now(block: S.() -> R): R {
        check(!main.isDispatchNeeded(EmptyCoroutineContext)) { "main-thread state touched off the main thread" }
        return state.block()
    }
}

/**
 * What the console changes once [SkiaConsole.ensure] has built it. Only [SkiaConsole.state] holds
 * one, so only the main thread reaches it.
 */
internal class ConsoleState {
    var settings = Settings()

    // What the composable hands over while it is on screen; null while parked.
    var onConnected: ((ActiveSession) -> Unit)? = null
    var onSettingsChange: ((Settings) -> Unit)? = null
    var onQuit: (() -> Unit)? = null
    var onPadAction: ((String, String) -> Unit)? = null
    var onPulse: ((String) -> Unit)? = null

    /** The console's focus, in words, whenever it changes — the shell speaks it to TalkBack. */
    var onAnnounce: ((String) -> Unit)? = null

    /** "Save launcher file" (title, url): the activity owns the save dialog. */
    var onSaveLauncherFile: ((String, String) -> Unit)? = null

    /** The connect in flight, if any — cancelable through `OverlayAction::CancelConnect`. */
    var dial: HostServices.Ticket? = null

    /**
     * The connected session the console's launch hold is still standing in front of.
     *
     * Handed to the app on `ShowStream`, dropped on a cancel. Null whenever the shell holds no
     * launch — a desktop-session connect, a launcher tile or a link.
     */
    var pendingSession: ActiveSession? = null

    /** The driving pad's reading while the console's input test is on (`ConsoleCmd::PadTest`). */
    var padTest: PadTestReading? = null

    /** A link that arrived before the first identity load ended; replayed once it has. */
    var parkedLink: String? = null

    /** The last `FetchLibrary`'s host and `req`. A refetch this shell starts answers under it. */
    var libraryReq: Pair<String, JSONObject>? = null
}
