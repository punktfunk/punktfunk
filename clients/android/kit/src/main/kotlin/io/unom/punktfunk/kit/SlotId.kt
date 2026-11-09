package io.unom.punktfunk.kit

/*
 * The quick-action ids and the key names the dial and the shortcuts both speak: the ids of
 * pf-client-core's `overlay_actions.rs` and the names of its `key_vk`. In the kit so the pad
 * router's [Shortcuts.Engine] can name an action; the ring's blob lives in the app's
 * `OverlayActions.kt`.
 */

/** What a ring slot or a shortcut does. [Host] carries a host-advertised action id; [Shortcut]
 *  refers into the ring blob's key combinations by id. */
sealed class SlotId {
    object EndStream : SlotId()

    /** End the game this device launched, then the stream. */
    object EndGame : SlotId()
    object DisconnectLinger : SlotId()
    object TouchMode : SlotId()
    object Keyboard : SlotId()
    object Stats : SlotId()
    object Mic : SlotId()
    object Pad : SlotId()
    object SendText : SlotId()

    /** The host's guide button (Xbox / PS / Steam), as a synthetic pad tap. */
    object Guide : SlotId()

    /** The host's Quick Access: the Deck's `…` where the pad has it, Steam's chord elsewhere. */
    object Qam : SlotId()

    /** Controller mouse: the pad drives the host pointer instead of its virtual pad. */
    object PadMouse : SlotId()

    /** Step the controller type the host emulates, live. */
    object PadType : SlotId()

    /** Silence this device's speakers. Local: the host keeps playing for anyone joined to it. */
    object StreamMute : SlotId()

    /** A dual-screen handheld's two screens trade the picture and the companion panel. */
    object SwapScreens : SlotId()

    /** Release the captured mouse, or capture it again. */
    object ReleaseInput : SlotId()

    /** Switch the mouse between capture and desktop mode. */
    object MouseMode : SlotId()

    /** A desktop window's fullscreen toggle; Android has no window. */
    object Fullscreen : SlotId()

    /** Minimize a desktop window; Android has no window. */
    object Minimize : SlotId()

    /** Start or stop clipboard sharing mid-stream; the Apple client's. */
    object Clipboard : SlotId()
    data class Host(val actionId: String) : SlotId()
    data class Shortcut(val shortcutId: String) : SlotId()

    /** The wire id, the inverse of [parse]. */
    val id: String
        get() = when (this) {
            EndStream -> "end_stream"
            EndGame -> "end_game"
            DisconnectLinger -> "disconnect_linger"
            TouchMode -> "touch_mode"
            Keyboard -> "keyboard"
            Stats -> "stats"
            Mic -> "mic"
            Pad -> "pad"
            SendText -> "send_text"
            Guide -> "guide"
            Qam -> "qam"
            PadMouse -> "pad_mouse"
            PadType -> "pad_type"
            StreamMute -> "stream_mute"
            SwapScreens -> "swap_screens"
            ReleaseInput -> "release_input"
            MouseMode -> "mouse_mode"
            Fullscreen -> "fullscreen"
            Minimize -> "minimize"
            Clipboard -> "clipboard"
            is Host -> "host:$actionId"
            is Shortcut -> "shortcut:$shortcutId"
        }

    companion object {
        /** An id from a blob; `null` for one this build does not know. */
        fun parse(s: String): SlotId? = when (s) {
            "end_stream" -> EndStream
            "end_game" -> EndGame
            "disconnect_linger" -> DisconnectLinger
            "touch_mode" -> TouchMode
            "keyboard" -> Keyboard
            "stats" -> Stats
            "mic" -> Mic
            "pad" -> Pad
            "send_text" -> SendText
            "guide" -> Guide
            "qam" -> Qam
            "pad_mouse" -> PadMouse
            "pad_type" -> PadType
            "stream_mute" -> StreamMute
            "swap_screens" -> SwapScreens
            "release_input" -> ReleaseInput
            "mouse_mode" -> MouseMode
            "fullscreen" -> Fullscreen
            "minimize" -> Minimize
            "clipboard" -> Clipboard
            else -> when {
                s.startsWith("host:") && s.length > 5 -> Host(s.substring(5))
                s.startsWith("shortcut:") && s.length > 9 -> Shortcut(s.substring(9))
                else -> null
            }
        }
    }
}

/**
 * The Windows virtual-key code a key name stands for (the wire speaks VKs); `null` for a name
 * this build does not know. Twin of the Rust `key_vk`: `OverlayActionsTest` replays
 * `crates/core/punktfunk-core/testdata/key-vk-vectors.json`, so a new name lands there first.
 */
fun keyVk(name: String): Int? {
    val n = name.trim().lowercase()
    return when (n) {
        "ctrl", "control" -> 0x11
        "shift" -> 0x10
        "alt", "option" -> 0x12
        "win", "cmd", "super", "meta" -> 0x5B
        "escape", "esc" -> 0x1B
        "tab" -> 0x09
        "enter", "return" -> 0x0D
        "space" -> 0x20
        "backspace" -> 0x08
        "delete", "del" -> 0x2E
        "insert" -> 0x2D
        "home" -> 0x24
        "end" -> 0x23
        "pageup" -> 0x21
        "pagedown" -> 0x22
        "up" -> 0x26
        "down" -> 0x28
        "left" -> 0x25
        "right" -> 0x27
        "printscreen" -> 0x2C
        "pause" -> 0x13
        "capslock" -> 0x14
        else -> when {
            n.length == 1 && n[0] in 'a'..'z' -> 0x41 + (n[0] - 'a')
            n.length == 1 && n[0] in '0'..'9' -> 0x30 + (n[0] - '0')
            n.length >= 2 && n[0] == 'f' ->
                n.substring(1).toIntOrNull()?.takeIf { it in 1..24 }?.let { 0x70 + it - 1 }
            else -> null
        }
    }
}

/** A chord as a legend reads it: `Ctrl+Shift+Esc`, or `Win` alone. */
fun chordChip(keys: List<String>): String = keys.joinToString("+", transform = ::keyLegend)

/**
 * One key's legend: the word a keyboard prints on it (`Ctrl`, `Esc`, `PgUp`), arrows as arrows.
 * Symbols like ❖ or ⇧ read as nothing to most people, so none are used here.
 */
fun keyLegend(k: String): String = when (k.trim().lowercase()) {
    "ctrl", "control" -> "Ctrl"
    "shift" -> "Shift"
    "alt", "option" -> "Alt"
    "win", "cmd", "super", "meta" -> "Win"
    "escape", "esc" -> "Esc"
    "enter", "return" -> "Enter"
    "backspace" -> "Backspace"
    "delete", "del" -> "Del"
    "insert" -> "Ins"
    "pageup" -> "PgUp"
    "pagedown" -> "PgDn"
    "printscreen" -> "PrtSc"
    "capslock" -> "Caps"
    "up" -> "↑"
    "down" -> "↓"
    "left" -> "←"
    "right" -> "→"
    else -> k.trim().lowercase().replaceFirstChar { it.uppercase() }
}
