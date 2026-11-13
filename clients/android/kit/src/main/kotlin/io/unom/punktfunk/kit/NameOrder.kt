package io.unom.punktfunk.kit

import java.text.Collator
import java.util.Locale

/**
 * Orders by [name] the way [locale]'s language does, so `Ä` sits with `A` in German and after `Z`
 * in Swedish. Case does not count; an accent does, as a tie-break. A tie keeps the input order.
 */
fun <T> nameOrder(locale: Locale = Locale.getDefault(), name: (T) -> String): Comparator<T> {
    val collator = Collator.getInstance(locale).apply { strength = Collator.SECONDARY }
    return Comparator { a, b -> collator.compare(name(a), name(b)) }
}
