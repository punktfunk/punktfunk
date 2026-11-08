package io.unom.punktfunk.kit

import org.json.JSONArray
import org.json.JSONObject

/**
 * In-stream shortcuts: a press or hold of controller buttons or keys, bound to a quick action —
 * the `shortcuts` settings blob. The Kotlin twin of pf-client-core's `shortcuts` module
 * (design/client-shortcuts.md in the planning repo); `ShortcutVectorsTest` replays
 * `clients/shared/shortcut-vectors.json` as the Rust `tests.rs` does, so a rule changes there
 * first.
 *
 * [parse] never fails; an empty blob is this client's [defaults] ([resolve]). [Engine] is the pure
 * state machine that holds back, forwards and fires: one per pad, one for the keyboard.
 */
data class Shortcuts(val bindings: List<Binding> = emptyList()) {

    /** A client family with its own [defaults]. */
    enum class Client { DESKTOP, ANDROID, APPLE, MAC, WEBOS, WEB, TIZEN, PS5 }

    /** Where a trigger is pressed. */
    enum class Device { PAD, KEYBOARD }

    /** What the player presses. Pad buttons are wire bits in stored order; keys are [keyVk]
     *  names, modifiers in [MODIFIERS] order. A keyboard trigger is always a press. */
    sealed interface Trigger {
        val device: Device
        val hold: Boolean

        data class Pad(val buttons: List<Int>, override val hold: Boolean) : Trigger {
            override val device get() = Device.PAD
        }

        data class Key(val modifiers: List<String>, val key: String) : Trigger {
            override val device get() = Device.KEYBOARD
            override val hold get() = false
        }

        /** What the engine matches, sorted: wire bits, or the VKs of the modifiers and the key. */
        fun ids(): List<Int> = when (this) {
            is Pad -> buttons
            is Key -> (modifiers + key).mapNotNull(::keyVk)
        }.distinct().sorted()

        /** The keys alone: "Select + A", "Ctrl+Alt+Shift+Q". */
        fun chord(): String = when (this) {
            is Pad -> buttons.joinToString(" + ", transform = ::buttonLabel)
            is Key -> (modifiers + key).joinToString("+", transform = ::keyLegend)
        }

        /** The whole line: the chord, or "Hold …" for a hold. */
        fun label(): Say = if (hold) say(R.string.controls_hold, chord()) else Say.Text(chord())
    }

    /** How an action meets a hold: once, or down and up on the host like a key. */
    enum class Kind { ONCE, KEY_LIKE }

    /** A quick action, or opening the dial itself. [Unknown] is an id from a newer client: the
     *  row stays, dimmed, and the engine never sees it. */
    sealed interface Action {
        data class Slot(val slot: SlotId) : Action
        data object Ring : Action
        data class Unknown(val raw: String) : Action

        /** The inverse of [parse]. */
        val id: String
            get() = when (this) {
                is Slot -> slot.id
                Ring -> "ring"
                is Unknown -> raw
            }

        /** Guide, Quick access and a key combination keep their key down while held. */
        val kind: Kind
            get() = when ((this as? Slot)?.slot) {
                SlotId.Guide, SlotId.Qam, is SlotId.Shortcut -> Kind.KEY_LIKE
                else -> Kind.ONCE
            }

        companion object {
            fun parse(id: String): Action =
                if (id == "ring") Ring else SlotId.parse(id)?.let(::Slot) ?: Unknown(id)
        }
    }

    data class Binding(val trigger: Trigger, val action: Action)

    /** Why the editor refuses a new binding. Indices point into [bindings]. */
    sealed interface Conflict {
        /** The same trigger already exists: offer to replace it. */
        data class Identical(val index: Int) : Conflict

        /** One of the two is a press whose buttons the other's contain: it fires first. */
        data class PressSubset(val index: Int) : Conflict

        /** One button is a hold; a press needs two. */
        data object SinglePress : Conflict
        data object NoModifier : Conflict
        data object NoKey : Conflict
    }

    fun toJson(): String = JSONObject().put("v", SCHEMA_VERSION).put(
        "bindings",
        JSONArray().also { arr ->
            bindings.forEach { b ->
                val t = b.trigger
                arr.put(
                    JSONObject().apply {
                        when (t) {
                            is Trigger.Pad -> {
                                put("on", "pad")
                                put("keys", JSONArray(t.buttons.mapNotNull(::buttonName)))
                                put("hold", t.hold)
                            }
                            is Trigger.Key -> {
                                put("on", "key")
                                put("keys", JSONArray(t.modifiers + t.key))
                            }
                        }
                        put("do", b.action.id)
                    },
                )
            }
        },
    ).toString()

    /** Whether [candidate] may join these bindings; null when it may. An edit is checked against
     *  the list without the binding it changes. */
    fun validate(candidate: Binding): Conflict? {
        when (val t = candidate.trigger) {
            is Trigger.Key -> {
                if (t.key.isEmpty() || modifier(t.key) != null) return Conflict.NoKey
                // A function key types nothing, so it alone is no clash with typing.
                val function = t.key.length >= 2 && t.key[0] == 'f' && t.key.substring(1).toIntOrNull() in 1..24
                if (t.modifiers.isEmpty() && !function) return Conflict.NoModifier
            }
            is Trigger.Pad -> if (t.buttons.size < 2 && !(t.hold && t.buttons.size == 1)) return Conflict.SinglePress
        }
        val ids = candidate.trigger.ids()
        val device = candidate.trigger.device
        val same = bindings.withIndex().filter { it.value.trigger.device == device }
        same.firstOrNull { it.value.trigger.ids() == ids && it.value.trigger.hold == candidate.trigger.hold }
            ?.let { return Conflict.Identical(it.index) }
        // Modifiers are down before the key, so only pad presses can cut a chord short.
        if (device == Device.PAD) {
            fun strict(small: List<Int>, big: List<Int>) = small.size < big.size && big.containsAll(small)
            same.firstOrNull {
                val other = it.value.trigger.ids()
                (!candidate.trigger.hold && strict(ids, other)) || (!it.value.trigger.hold && strict(other, ids))
            }?.let { return Conflict.PressSubset(it.index) }
        }
        return null
    }

    /** The first trigger on [device] bound to [action], as one line: "Hold L1 + R1 + Start +
     *  Select", "Ctrl+Alt+Shift+O". For the start banner and the settings captions. */
    fun hint(action: Action, device: Device): Say? =
        bindings.firstOrNull { it.action == action && it.trigger.device == device }?.trigger?.label()

    /** Which device a group's rows are pressed on. */
    enum class GroupKind { REMOTE, KEYBOARD, TOUCH, PAD }

    /** One row: what to press, what it does, and the binding it edits. [text] is null on a binding
     *  row, which the screen labels by its action; [binding] is null on a fixed row. */
    data class Row(val press: Say, val text: Say?, val binding: Int?)

    data class Group(val kind: GroupKind, val rows: List<Row>) {
        /** Title and note as one line. */
        val heading: Say
            get() = when (kind) {
                GroupKind.REMOTE -> say(R.string.controls_group_remote)
                GroupKind.KEYBOARD -> say(R.string.controls_group_keyboard)
                GroupKind.TOUCH -> say(R.string.controls_group_touch_full)
                GroupKind.PAD -> say(R.string.controls_group_pad_full, "Select", "Back", "View")
            }
    }

    /**
     * What the Android Shortcuts screen draws: the bindings as rows, plus the fixed touch rows on
     * a phone or the remote's on a TV ([touchscreen] false), whose Back and held Select
     * ([RemotePointer]) are not shortcuts. The Rust `rows` lists no remote for Android.
     */
    fun rows(touchscreen: Boolean): List<Group> {
        val out = mutableListOf<Group>()
        if (!touchscreen) {
            out += Group(
                GroupKind.REMOTE,
                listOf(
                    Row(say(R.string.controls_hold, "Select"), say(R.string.android_controls_remote_pointer), null),
                    Row(Say.Text("Back"), say(R.string.controls_open_dial), null),
                ),
            )
        }
        items(Device.KEYBOARD).takeIf { it.isNotEmpty() }?.let { out += Group(GroupKind.KEYBOARD, it) }
        if (touchscreen) {
            out += Group(
                GroupKind.TOUCH,
                listOf(
                    Row(Say.Text("Back"), say(R.string.controls_open_dial), null),
                    Row(say(R.string.controls_gesture_three_finger_tap), say(R.string.controls_cycle_stats), null),
                    Row(say(R.string.controls_gesture_three_finger_swipe_up), say(R.string.controls_show_keyboard), null),
                    Row(say(R.string.controls_gesture_three_finger_swipe_down), say(R.string.controls_hide_keyboard), null),
                    Row(say(R.string.controls_gesture_two_finger_twist), say(R.string.controls_open_dial), null),
                ),
            )
        }
        out += Group(GroupKind.PAD, items(Device.PAD))
        return out
    }

    /** One row per binding on [device]; two presses of one action share an "or" row. */
    private fun items(device: Device): List<Row> {
        val shown = BooleanArray(bindings.size)
        val rows = mutableListOf<Row>()
        bindings.forEachIndexed { i, b ->
            if (shown[i] || b.trigger.device != device) return@forEachIndexed
            val first = b.trigger.chord()
            val twin = (i + 1 until bindings.size).firstOrNull { j ->
                val o = bindings[j]
                !b.trigger.hold && o.action == b.action && o.trigger.device == device && !o.trigger.hold
            }
            val press = when {
                b.trigger.hold -> say(R.string.controls_hold, first)
                twin != null -> {
                    shown[twin] = true
                    say(R.string.controls_either, first, bindings[twin].trigger.chord())
                }
                else -> Say.Text(first)
            }
            rows += Row(press, null, i)
        }
        return rows
    }

    /**
     * One device's shortcuts, as the rules of design §4. Feed every transition to [step], call
     * [tick] by [nextDue] (late only delays), and [flush] when the device goes away. Pass only the
     * actions this session can perform. Ids are wire bits, or VKs with left and right modifiers
     * folded onto `keyVk("ctrl" | "alt" | "shift")`. Pure: the caller owns the clock and the wire.
     */
    class Engine(bindings: List<Binding>, private val device: Device) {
        private class Rule(val ids: List<Int>, val hold: Boolean, val action: Action)

        /** [twin] is the press with the same buttons: it fires when the chord lifts before [HOLD_MS]. */
        private class Armed(val since: Long, val hold: Int, val twin: Int?)

        // No unknown actions, no keyboard holds, and no one-button pad press (that tap is the game's).
        private val rules = bindings
            .filter { it.trigger.device == device && it.action !is Action.Unknown }
            .map { Rule(it.trigger.ids(), it.trigger.hold, it.action) }
            .filter { if (device == Device.PAD) it.hold || it.ids.size >= 2 else !it.hold && it.ids.isNotEmpty() }

        /** Physically down, in press order. */
        private val down = mutableListOf<Int>()

        /** Down on the host because of an [Out.Forward]. */
        private val forwarded = mutableListOf<Int>()

        /** Pressed, not sent: a held chord's presses, only while its hold is armed. */
        private val heldBack = mutableListOf<Int>()
        private var armed: Armed? = null

        /** Buttons of a chord that fired: their releases are dropped. */
        private val claimed = mutableListOf<Int>()

        /** A key-like hold that went down, and the buttons whose first lift ends it. */
        private var keyDown: Pair<Action, List<Int>>? = null

        /** A tap's release, due at the time. */
        private val owed = mutableListOf<Pair<Long, Int>>()

        /** The hold that fires at [nextDue] unless a button lifts first; the "hold to quit" cue. */
        val armedAction: Action? get() = armed?.let { rules[it.hold].action }

        /** When [tick] next has work: an armed hold or an owed release. */
        fun nextDue(): Long? = (owed.map { it.first } + listOfNotNull(armed?.let { it.since + HOLD_MS })).minOrNull()

        /** One transition at [nowMs]. A tick that was due runs first. */
        fun step(id: Int, down: Boolean, nowMs: Long): List<Out> {
            val out = tick(nowMs).toMutableList()
            if (down) press(id, nowMs, out) else release(id, nowMs, out)
            settle(nowMs, out)
            return out
        }

        /** Owed tap releases and an armed hold that came due. */
        fun tick(nowMs: Long): List<Out> {
            val out = mutableListOf<Out>()
            val i = owed.iterator()
            while (i.hasNext()) {
                val (due, b) = i.next()
                if (nowMs >= due) {
                    i.remove()
                    out += Out.Forward(b, false)
                }
            }
            armed?.let { a ->
                if (nowMs - a.since >= HOLD_MS) fire(a.hold, if (kind(a.hold) == Kind.ONCE) Phase.ONCE else Phase.DOWN, out)
            }
            return out
        }

        /** The device is gone: a key-like hold comes up, and the host lets go of everything this
         *  engine sent down or still owes. Forgets every press. */
        fun flush(): List<Out> {
            val out = mutableListOf<Out>()
            keyDown?.let { out += Out.Fire(it.first, Phase.UP) }
            keyDown = null
            val lift = owed.map { it.second } + forwarded
            owed.clear()
            forwarded.clear()
            if (lift.isNotEmpty()) out += Out.Lift(lift)
            down.clear()
            heldBack.clear()
            claimed.clear()
            armed = null
            return out
        }

        private fun kind(rule: Int) = rules[rule].action.kind

        private fun press(b: Int, now: Long, out: MutableList<Out>) {
            // Key repeat, or a press the engine already counts.
            if (b in down) return
            val o = owed.indexOfFirst { it.second == b }
            if (o >= 0) {
                owed.removeAt(o)
                out += Out.Forward(b, false)
            }
            down += b
            armed = null
            // While a key-like hold is down, a press is the host's: Guide+A is Steam's chord.
            if (!isModifier(b) && keyDown == null) {
                val press = exact(hold = false)
                val hold = exact(hold = true)
                if (press != null && hold == null) {
                    return fire(press, if (kind(press) == Kind.ONCE) Phase.ONCE else Phase.TAP, out)
                }
                if (hold != null) {
                    heldBack += b
                    armed = Armed(now, hold, press)
                    return
                }
            }
            // A partner that completes nothing makes a held-back press real, sent first.
            sendHeldBack(out)
            forwarded += b
            out += Out.Forward(b, true)
        }

        private fun release(b: Int, now: Long, out: MutableList<Out>) {
            if (!down.remove(b)) {
                // Pressed before this engine: the host may hold it.
                out += Out.Forward(b, false)
                return
            }
            keyDown?.takeIf { b in it.second }?.let {
                keyDown = null
                out += Out.Fire(it.first, Phase.UP)
            }
            if (claimed.remove(b)) return
            // Armed means down was the hold's chord, so `b` is one of its buttons.
            val a = armed
            armed = null
            if (a != null) {
                a.twin?.let { return fire(it, if (kind(it) == Kind.ONCE) Phase.ONCE else Phase.TAP, out) }
                if (kind(a.hold) == Kind.KEY_LIKE && now - a.since >= KEY_TAP_MS) return fire(a.hold, Phase.TAP, out)
            }
            if (heldBack.remove(b)) {
                out += Out.Forward(b, true)
                owed += (now + TAP_PRESS_MS) to b
                return
            }
            forwarded.removeAll { it == b }
            out += Out.Forward(b, false)
        }

        /** The rule fires: the host lets go of the chord, the held-back press is never sent, and
         *  every button still down is the chord's until it lifts. */
        private fun fire(rule: Int, phase: Phase, out: MutableList<Out>) {
            if (forwarded.isNotEmpty()) out += Out.Lift(forwarded.toList())
            forwarded.clear()
            val action = rules[rule].action
            out += Out.Fire(action, phase)
            if (phase == Phase.DOWN) keyDown = action to rules[rule].ids
            heldBack.clear()
            armed = null
            for (d in down) if (d !in claimed) claimed += d
        }

        /** A press stays held back only while its hold is armed. When a release disarms it, a
         *  hold still exactly down (a nested one) arms from [now]; otherwise the press goes out. */
        private fun settle(now: Long, out: MutableList<Out>) {
            if (armed != null || heldBack.isEmpty()) return
            val hold = exact(hold = true)
            if (hold != null) armed = Armed(now, hold, exact(hold = false)) else sendHeldBack(out)
        }

        private fun sendHeldBack(out: MutableList<Out>) {
            for (h in heldBack) {
                forwarded += h
                out += Out.Forward(h, true)
            }
            heldBack.clear()
        }

        /** The rule whose buttons are exactly what is down. */
        private fun exact(hold: Boolean): Int? = rules.indexOfFirst {
            it.hold == hold && it.ids.size == down.size && it.ids.containsAll(down)
        }.takeIf { it >= 0 }

        /** A modifier never completes a keyboard shortcut and is never held back. */
        private fun isModifier(b: Int) = device == Device.KEYBOARD && b in MODIFIER_VKS
    }

    sealed interface Out {
        /** Send this transition on, as input to the host (it obeys the input mask). */
        data class Forward(val id: Int, val down: Boolean) : Out
        data class Fire(val action: Action, val phase: Phase) : Out

        /** Release these on the host, mask or not: the rest of a chord that fired. */
        data class Lift(val ids: List<Int>) : Out
    }

    enum class Phase {
        ONCE,

        /** A key-like action held: down on the host until [UP]. */
        DOWN,
        UP,

        /** A key-like action pressed and released at once. */
        TAP,
    }

    companion object {
        const val SCHEMA_VERSION = 1

        /** A held trigger fires after this long. */
        const val HOLD_MS = 1000L

        /** A held-back tap's release follows its press by this, so the two don't fold into one
         *  pad state on the wire. */
        const val TAP_PRESS_MS = 50L

        /** A key-like hold released after this long is a tap of its key, not of the button. */
        const val KEY_TAP_MS = 350L

        /** Pad buttons by their stored name: the console's input test names, lowercased, `select`
         *  for Back, and the wire's own names for the rest. */
        val BUTTONS: List<Pair<String, Int>> = listOf(
            "a" to Gamepad.BTN_A, "b" to Gamepad.BTN_B, "x" to Gamepad.BTN_X, "y" to Gamepad.BTN_Y,
            "lb" to Gamepad.BTN_LB, "rb" to Gamepad.BTN_RB, "ls" to Gamepad.BTN_LS_CLICK, "rs" to Gamepad.BTN_RS_CLICK,
            "start" to Gamepad.BTN_START, "select" to Gamepad.BTN_BACK, "guide" to Gamepad.BTN_GUIDE,
            "up" to Gamepad.BTN_DPAD_UP, "down" to Gamepad.BTN_DPAD_DOWN,
            "left" to Gamepad.BTN_DPAD_LEFT, "right" to Gamepad.BTN_DPAD_RIGHT,
            "touchpad" to Gamepad.BTN_TOUCHPAD, "misc1" to Gamepad.BTN_MISC1, "misc2" to Gamepad.BTN_MISC2,
            "paddle1" to Gamepad.BTN_PADDLE1, "paddle2" to Gamepad.BTN_PADDLE2,
            "paddle3" to Gamepad.BTN_PADDLE3, "paddle4" to Gamepad.BTN_PADDLE4,
        )

        /** The keyboard modifiers a shortcut may hold, in display order. ⌘ and Win are the host's. */
        val MODIFIERS = listOf("ctrl", "alt", "shift")
        private val MODIFIER_VKS = MODIFIERS.mapNotNull(::keyVk).toSet()

        /** The wire bit of a stored button name, any case; `back` is `select`, and the pad art's
         *  grip names (`r4` `l4` `r5` `l5`) are the paddles. */
        fun buttonBit(name: String): Int? {
            val n = when (val raw = name.trim().lowercase()) {
                "back" -> "select"
                "r4" -> "paddle1"
                "l4" -> "paddle2"
                "r5" -> "paddle3"
                "l5" -> "paddle4"
                else -> raw
            }
            return BUTTONS.firstOrNull { it.first == n }?.second
        }

        fun buttonName(bit: Int): String? = BUTTONS.firstOrNull { it.second == bit }?.first

        /** A button as the screens name it: Xbox positions with Select and Start and
         *  PlayStation shoulders. */
        fun buttonLabel(bit: Int): String = when (bit) {
            Gamepad.BTN_A -> "A"
            Gamepad.BTN_B -> "B"
            Gamepad.BTN_X -> "X"
            Gamepad.BTN_Y -> "Y"
            Gamepad.BTN_LB -> "L1"
            Gamepad.BTN_RB -> "R1"
            Gamepad.BTN_LS_CLICK -> "L3"
            Gamepad.BTN_RS_CLICK -> "R3"
            Gamepad.BTN_START -> "Start"
            Gamepad.BTN_BACK -> "Select"
            Gamepad.BTN_GUIDE -> "Guide"
            Gamepad.BTN_DPAD_UP -> "Up"
            Gamepad.BTN_DPAD_DOWN -> "Down"
            Gamepad.BTN_DPAD_LEFT -> "Left"
            Gamepad.BTN_DPAD_RIGHT -> "Right"
            Gamepad.BTN_TOUCHPAD -> "Touchpad"
            Gamepad.BTN_MISC1 -> "Share"
            Gamepad.BTN_MISC2 -> "C"
            Gamepad.BTN_PADDLE1 -> "R4"
            Gamepad.BTN_PADDLE2 -> "L4"
            Gamepad.BTN_PADDLE3 -> "R5"
            Gamepad.BTN_PADDLE4 -> "L5"
            else -> "?"
        }

        private fun modifier(name: String): String? = when (name.trim().lowercase()) {
            "ctrl", "control" -> "ctrl"
            "alt", "option" -> "alt"
            "shift" -> "shift"
            else -> null
        }

        /** Never fails: an unparseable blob is empty, and a binding with a name this build does
         *  not know is dropped. An unknown action keeps its row as [Action.Unknown]. */
        fun parse(json: String?): Shortcuts = read(json) ?: Shortcuts()

        /** Null when the blob is not a shortcuts blob at all; the caller takes the defaults. */
        internal fun read(json: String?): Shortcuts? {
            val arr = runCatching { JSONObject(json ?: "").optJSONArray("bindings") }.getOrNull() ?: return null
            return Shortcuts((0 until arr.length()).mapNotNull { stored(arr.opt(it)) })
        }

        /** One binding as the blob stores it; null for one this build cannot read. */
        private fun stored(v: Any?): Binding? {
            val o = v as? JSONObject ?: return null
            val on = o.opt("on") as? String ?: return null
            val keys = o.opt("keys") as? JSONArray ?: return null
            val names = (0 until keys.length()).map { keys.opt(it) as? String ?: return null }
            val h = o.opt("hold")
            val hold = h as? Boolean ?: if (h == null || h == JSONObject.NULL) false else return null
            val action = o.opt("do") as? String ?: return null
            val trigger = when (on) {
                "pad" -> Trigger.Pad(names.map { buttonBit(it) ?: return null }.distinct(), hold)
                "key" -> {
                    val key = names.lastOrNull()?.trim()?.lowercase() ?: return null
                    keyVk(key) ?: return null
                    val mods = names.dropLast(1).map { modifier(it) ?: return null }
                    Trigger.Key(MODIFIERS.filter { it in mods }, key)
                }
                else -> return null
            }
            if (trigger is Trigger.Pad && trigger.buttons.isEmpty()) return null
            return Binding(trigger, Action.parse(action))
        }

        private val EXIT = listOf("lb", "rb", "start", "select")
        private val PS5_EXIT = listOf("lb", "rb", "start", "touchpad")

        /** This client's shortcuts when the player has changed none: the chords it always had.
         *  [legacyGuideGesture] is the old Hold Select for guide value: `on` and `off` decide the
         *  Hold Select row, anything else is the client's automatic. */
        fun defaults(client: Client, gameMode: Boolean, legacyGuideGesture: String): Shortcuts {
            val guide = when (legacyGuideGesture) {
                "on" -> true
                "off" -> false
                else -> when (client) {
                    Client.DESKTOP -> gameMode
                    Client.APPLE -> true
                    else -> false
                }
            }
            // The clients with their own pad path; the others' dial is their only pad chord.
            val native = client in listOf(Client.DESKTOP, Client.ANDROID, Client.APPLE, Client.MAC)
            val (open, exit) = if (client == Client.PS5) "touchpad" to PS5_EXIT else "select" to EXIT
            fun pad(buttons: List<String>, hold: Boolean, action: Action) =
                Binding(Trigger.Pad(buttons.mapNotNull(::buttonBit), hold), action)
            fun key(mods: List<String>, key: String, action: Action) = Binding(Trigger.Key(mods, key), action)
            fun slot(s: SlotId) = Action.Slot(s)
            fun chord(k: String, action: Action) = key(MODIFIERS, k, action)
            val b = mutableListOf<Binding>()
            // Pad rows first, then keys, each in the order the reference screen lists them.
            b += pad(listOf(open, "a"), false, Action.Ring)
            if (native) b += pad(listOf("select", "x"), false, slot(SlotId.Stats))
            if (client == Client.ANDROID) b += pad(listOf("select", "y"), false, slot(SlotId.Mic))
            if (native && guide) b += pad(listOf("select"), true, slot(SlotId.Guide))
            if (client in listOf(Client.DESKTOP, Client.WEB, Client.TIZEN)) b += pad(exit, false, slot(SlotId.ReleaseInput))
            b += pad(exit, true, slot(SlotId.EndStream))
            when (client) {
                Client.DESKTOP, Client.WEB, Client.TIZEN -> {
                    b += chord("q", slot(SlotId.ReleaseInput))
                    b += chord("d", slot(SlotId.EndStream))
                    b += chord("s", slot(SlotId.Stats))
                    b += chord("o", Action.Ring)
                    b += chord("v", slot(SlotId.Mic))
                    b += chord("m", slot(SlotId.MouseMode))
                    b += key(emptyList(), "f11", slot(SlotId.Fullscreen))
                    b += key(listOf("alt"), "enter", slot(SlotId.Fullscreen))
                }
                Client.ANDROID -> {
                    b += chord("q", slot(SlotId.ReleaseInput))
                    b += chord("o", Action.Ring)
                }
                Client.APPLE, Client.MAC -> {
                    b += chord("q", slot(SlotId.ReleaseInput))
                    b += chord("d", slot(SlotId.EndStream))
                    b += chord("s", slot(SlotId.Stats))
                    b += chord("o", Action.Ring)
                    if (client == Client.MAC) b += chord("m", slot(SlotId.MouseMode))
                    b += chord("c", slot(SlotId.Clipboard))
                    b += chord("a", slot(SlotId.Mic))
                }
                Client.WEBOS, Client.PS5 -> {}
            }
            return Shortcuts(b)
        }

        /** The stored blob, or this client's defaults when it is empty or unreadable. */
        fun resolve(blob: String?, client: Client, gameMode: Boolean, legacyGuideGesture: String): Shortcuts =
            read(blob) ?: defaults(client, gameMode, legacyGuideGesture)
    }
}
