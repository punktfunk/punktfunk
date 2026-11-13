package io.unom.punktfunk.kit

import android.content.res.Resources

/**
 * A line the kit hands to a screen. The kit has no Context: it returns the message id and its
 * figures, and the app words them with [words] where it holds [Resources].
 */
sealed interface Say {
    /** Message [id] (`R.string`). [args] fill its placeholders in order: a string, a number or a [Say], worded first. */
    data class Msg(val id: Int, val args: List<Any> = emptyList()) : Say

    /** Plural message [id] for [count]. [args] fill the placeholders in order, the count among them. */
    data class Plural(val id: Int, val count: Int, val args: List<Any> = emptyList()) : Say

    /** Words shown as they came: a name, or a sentence the host or the OS sent. */
    data class Text(val value: String) : Say

    /**
     * A sentence the host sent by the id of its message ([HostText]), with the values of its
     * placeholders. [english] is the host's own, for an id this build does not know.
     */
    data class Host(val id: String, val params: Map<String, String>, val english: String) : Say

    /** This line in the language [res] is in. */
    fun words(res: Resources): String = when (this) {
        is Text -> value
        is Host -> HostText.words(res, id, params) ?: english
        is Plural -> res.getQuantityString(id, count, *args.toTypedArray())
        is Msg ->
            if (args.isEmpty()) {
                res.getString(id)
            } else {
                res.getString(id, *args.map { (it as? Say)?.words(res) ?: it }.toTypedArray())
            }
    }
}

/** [Say.Msg] of message [id] with [args]. */
fun say(id: Int, vararg args: Any): Say = Say.Msg(id, args.toList())
