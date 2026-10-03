package io.unom.punktfunk

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.drawText
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import java.util.Locale

/*
 * The companion panel's graphs (design/android-dual-screen.md §6): the stream's last minute as
 * four figures and three time charts — frame rate against the refresh, capture-to-glass latency
 * as p50 with the p95 band, bitrate against the target. One hue per series, from a palette
 * checked for colour-blind separation on the black panel; the text keeps the theme's ink. A
 * finger on a chart reads the second under it.
 */

private val RECEIVED = Color(0xFF3987E5)
private val PRESENTED = Color(0xFFD95926)
private val LATENCY = Color(0xFF9085E9)
private val BITRATE = Color(0xFF199E70)

/** The charts' window, in seconds; a young stream fills it from the right. */
private const val SPAN_S = 60

private fun f0(v: Float) = String.format(Locale.ROOT, "%.0f", v)
private fun f1(v: Float) = String.format(Locale.ROOT, "%.1f", v)

@Composable
internal fun StatsGraphs(history: StatsHistory, wide: Boolean, modifier: Modifier = Modifier) {
    history.version // a push recomposes
    val latest = history.latest
    val samples = history.samples.takeLast(SPAN_S)
    Column(
        modifier.fillMaxSize().verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        val tiles: List<@Composable RowScope.() -> Unit> = listOf(
            {
                Tile(
                    "Frame rate", latest?.let { f0(it.presentedFps) } ?: "—", "fps",
                    latest?.let { s ->
                        (if (s.refreshHz > 0) "of ${s.refreshHz} · " else "") + "received ${f0(s.receivedFps)}"
                    }.orEmpty(),
                )
            },
            {
                Tile(
                    "Latency", latest?.let { f0(it.e2eMs) } ?: "—", "ms",
                    latest?.let { "p95 ${f0(it.e2eP95Ms)} ms" } ?: "capture to glass",
                )
            },
            {
                Tile(
                    "Bitrate", latest?.let { f1(it.mbps) } ?: "—", "Mb/s",
                    latest?.takeIf { it.targetMbps > 0f }?.let { "target ${f0(it.targetMbps)}" }.orEmpty(),
                )
            },
            {
                Tile(
                    "Loss", latest?.let { f1(it.lostPct) } ?: "—", "%",
                    latest?.let { s ->
                        listOfNotNull(
                            "skipped ${s.skipped}".takeIf { s.skipped > 0 },
                            s.rttMs?.let { "rtt ${f1(it)} ms" },
                        ).joinToString(" · ")
                    }.orEmpty(),
                )
            },
        )
        // Four across where there is width for a number and its unit; two by two on a Thor.
        for (row in tiles.chunked(if (wide) 4 else 2)) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                for (tile in row) tile()
            }
        }
        val height = if (wide) 150.dp else 112.dp
        val charts: List<@Composable () -> Unit> = listOf(
            {
                TimeChart(
                    "Frame rate", samples, unit = "fps", height = height,
                    series = listOf(
                        Series("received", RECEIVED) { it.receivedFps },
                        Series("presented", PRESENTED) { it.presentedFps },
                    ),
                    reference = latest?.refreshHz?.takeIf { it > 0 }?.toFloat(),
                    referenceLabel = "refresh",
                    // Headroom over the refresh, so its line and label sit inside the plot.
                    top = { ceilTo(it * 1.1f, 10f) },
                )
            },
            {
                TimeChart(
                    "Latency", samples, unit = "ms", height = height,
                    series = listOf(Series("p50", LATENCY) { it.e2eMs }),
                    band = Series("p95", LATENCY) { it.e2eP95Ms },
                    top = ::niceCeiling,
                )
            },
            {
                TimeChart(
                    "Bitrate", samples, unit = "Mb/s", height = height,
                    series = listOf(Series("goodput", BITRATE) { it.mbps }),
                    reference = latest?.targetMbps?.takeIf { it > 0f },
                    referenceLabel = "target",
                    area = true,
                    top = ::niceCeiling,
                )
            },
        )
        if (wide) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                for (chart in charts) Box(Modifier.weight(1f)) { chart() }
            }
        } else {
            for (chart in charts) chart()
        }
    }
}

/** One headline figure: the label, the number with its unit, one line under it. */
@Composable
private fun RowScope.Tile(label: String, value: String, unit: String, caption: String) {
    val scheme = MaterialTheme.colorScheme
    Surface(
        shape = MaterialTheme.shapes.medium,
        color = scheme.surfaceVariant.copy(alpha = 0.6f),
        modifier = Modifier.weight(1f),
    ) {
        Column(Modifier.padding(horizontal = 12.dp, vertical = 10.dp)) {
            Text(label, style = MaterialTheme.typography.labelMedium, color = scheme.onSurfaceVariant, maxLines = 1)
            Row(verticalAlignment = Alignment.Bottom) {
                Text(
                    value,
                    style = MaterialTheme.typography.headlineSmall.copy(fontFeatureSettings = "tnum"),
                    color = scheme.onSurface, maxLines = 1, softWrap = false,
                )
                Spacer(Modifier.width(4.dp))
                Text(
                    unit, style = MaterialTheme.typography.labelMedium, color = scheme.onSurfaceVariant,
                    maxLines = 1, softWrap = false, modifier = Modifier.padding(bottom = 4.dp),
                )
            }
            Text(
                caption.ifEmpty { " " }, style = MaterialTheme.typography.labelSmall,
                color = scheme.onSurfaceVariant, maxLines = 1, overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

/** One line on a chart: what it is called, its hue, and the figure it reads off a sample. */
private class Series(val label: String, val color: Color, val pick: (StatsSample) -> Float)

/**
 * The last [SPAN_S] seconds of [series] on one axis, newest at the right. [band] fills from the
 * first series up to its own values (a p95 over a p50); [reference] is a dashed line with
 * [referenceLabel]; [area] fills under the first series. [top] turns the peak into the axis top.
 */
@Composable
private fun TimeChart(
    title: String,
    samples: List<StatsSample>,
    unit: String,
    height: androidx.compose.ui.unit.Dp,
    series: List<Series>,
    top: (Float) -> Float,
    band: Series? = null,
    reference: Float? = null,
    referenceLabel: String = "",
    area: Boolean = false,
) {
    val scheme = MaterialTheme.colorScheme
    val measurer = rememberTextMeasurer()
    val n = samples.size
    val values = series.map { s -> FloatArray(n) { s.pick(samples[it]) } }
    val bandValues = band?.let { b -> FloatArray(n) { b.pick(samples[it]) } }
    val peak = (values.flatMap { it.asList() } + bandValues?.asList().orEmpty() + listOfNotNull(reference))
        .maxOrNull() ?: 0f
    val yTop = top(peak).coerceAtLeast(1f)
    var hover by remember { mutableStateOf<Int?>(null) }
    val read = hover?.takeIf { it in 0 until n }
    Column {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(title, style = MaterialTheme.typography.labelMedium, color = scheme.onSurfaceVariant)
            // Identity is never colour alone: two or more lines carry a legend.
            if (series.size > 1 || band != null) {
                for (s in series + listOfNotNull(band)) Legend(s)
            }
            Spacer(Modifier.weight(1f))
            val readout = read?.let { i ->
                val ago = n - 1 - i
                series.joinToString(" · ") { "${it.label} ${f1(it.pick(samples[i]))}" } +
                    " $unit · ${if (ago == 0) "now" else "$ago s ago"}"
            }
            Text(
                readout ?: "${SPAN_S} s", style = MaterialTheme.typography.labelSmall,
                color = scheme.onSurfaceVariant, maxLines = 1,
            )
        }
        val ink = scheme.onSurface
        val muted = scheme.onSurfaceVariant
        val gutterDp = 40.dp
        Canvas(
            Modifier
                .fillMaxWidth()
                .height(height)
                .pointerInput(n) {
                    val gutter = gutterDp.toPx()
                    fun at(x: Float): Int? {
                        if (n == 0) return null
                        val w = size.width - gutter
                        val slot = ((x / w) * (SPAN_S - 1)).let { Math.round(it) } - (SPAN_S - n)
                        return slot.coerceIn(0, n - 1)
                    }
                    detectDragGestures(
                        onDragStart = { hover = at(it.x) },
                        onDrag = { change, _ -> change.consume(); hover = at(change.position.x) },
                    )
                }
                .pointerInput(n) {
                    // A finger down reads that second at once; a drag (above) walks it.
                    detectTapGestures(onPress = {
                        val gutter = gutterDp.toPx()
                        if (n > 0) {
                            val w = size.width - gutter
                            hover = (Math.round((it.x / w) * (SPAN_S - 1)) - (SPAN_S - n)).coerceIn(0, n - 1)
                        }
                    })
                },
        ) {
            val gutter = gutterDp.toPx()
            val left = 0f
            val right = size.width - gutter
            val topPx = 6.dp.toPx()
            val bottom = size.height - 14.dp.toPx()
            val plotH = bottom - topPx
            fun x(i: Int): Float = left + (i + (SPAN_S - n)).toFloat() / (SPAN_S - 1) * (right - left)
            fun y(v: Float): Float = bottom - (v / yTop).coerceIn(0f, 1f) * plotH
            val label = TextStyle(color = muted, fontSize = 10.sp, fontFeatureSettings = "tnum")
            // The grid recedes: three lines, two labels on the right.
            for (k in 0..2) {
                val v = yTop * k / 2
                drawLine(ink.copy(alpha = 0.10f), Offset(left, y(v)), Offset(right, y(v)), strokeWidth = 1f)
                if (k > 0) {
                    val t = measurer.measure(if (k == 2) "${f0(v)} $unit" else f0(v), label)
                    drawText(t, topLeft = Offset(right + 6.dp.toPx(), (y(v) - t.size.height / 2f).coerceAtLeast(0f)))
                }
            }
            val axis = measurer.measure("−${SPAN_S} s", label)
            drawText(axis, topLeft = Offset(left, bottom + 2.dp.toPx()))
            val now = measurer.measure("now", label)
            drawText(now, topLeft = Offset(right - now.size.width, bottom + 2.dp.toPx()))
            if (reference != null && reference <= yTop) {
                drawLine(
                    muted.copy(alpha = 0.55f), Offset(left, y(reference)), Offset(right, y(reference)),
                    strokeWidth = 1.dp.toPx(),
                    pathEffect = PathEffect.dashPathEffect(floatArrayOf(6.dp.toPx(), 4.dp.toPx())),
                )
                // The label rides above its line, or under it when the line is near the top.
                val t = measurer.measure("$referenceLabel ${f0(reference)}", label)
                val above = y(reference) - t.size.height - 1.dp.toPx()
                drawText(t, topLeft = Offset(left + 4.dp.toPx(), if (above >= topPx) above else y(reference) + 1.dp.toPx()))
            }
            if (n >= 1) {
                // The p95 band: up from the first series to the band's values and back.
                if (bandValues != null) {
                    val p = Path()
                    for (i in 0 until n) {
                        if (i == 0) p.moveTo(x(i), y(bandValues[i])) else p.lineTo(x(i), y(bandValues[i]))
                    }
                    for (i in n - 1 downTo 0) p.lineTo(x(i), y(values[0][i]))
                    p.close()
                    drawPath(p, band!!.color.copy(alpha = 0.18f))
                }
                if (area) {
                    val p = Path()
                    p.moveTo(x(0), bottom)
                    for (i in 0 until n) p.lineTo(x(i), y(values[0][i]))
                    p.lineTo(x(n - 1), bottom)
                    p.close()
                    drawPath(p, series[0].color.copy(alpha = 0.18f))
                }
                for ((k, s) in series.withIndex()) {
                    val p = Path()
                    for (i in 0 until n) {
                        if (i == 0) p.moveTo(x(i), y(values[k][i])) else p.lineTo(x(i), y(values[k][i]))
                    }
                    if (n == 1) drawCircle(s.color, 3.dp.toPx(), Offset(x(0), y(values[k][0])))
                    drawPath(p, s.color, style = Stroke(2.dp.toPx(), cap = StrokeCap.Round, join = StrokeJoin.Round))
                }
                read?.let { i ->
                    drawLine(ink.copy(alpha = 0.45f), Offset(x(i), topPx), Offset(x(i), bottom), strokeWidth = 1.dp.toPx())
                    for ((k, s) in series.withIndex()) {
                        drawCircle(Color.Black, 6.dp.toPx(), Offset(x(i), y(values[k][i])))
                        drawCircle(s.color, 4.dp.toPx(), Offset(x(i), y(values[k][i])))
                    }
                }
            }
        }
    }
}

@Composable
private fun Legend(s: Series) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(4.dp)) {
        Box(Modifier.size(8.dp).background(s.color, CircleShape))
        Text(s.label, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
}
