// Recharts visualisations for a captured stats series. Everything here is rendered
// CLIENT-ONLY (behind <ChartFrame>'s mounted guard): recharts' ResponsiveContainer
// measures its parent via ResizeObserver, which has no width during SSR and would
// otherwise render a 0×0 (or warn). Latency plots in milliseconds against one frame at
// the stream's rate, so a stage reads as a share of the time it has, not of the panel.
import { type ReactElement, useEffect, useMemo, useState } from "react";
import {
	Area,
	CartesianGrid,
	ComposedChart,
	Legend,
	Line,
	LineChart,
	ReferenceLine,
	ResponsiveContainer,
	Tooltip,
	XAxis,
	YAxis,
} from "recharts";
import type { StatsSample } from "@/api/gen/model/statsSample";
import { Button } from "@/components/ui/button";
import { fmtNumber } from "@/lib/format";
import { m } from "@/paraglide/messages";
import { dur, frameBudgetMs, latencyCeilingMs, msTick } from "./units";

const CHART_H = 240;

const axisTick = { fontSize: 11, fill: "var(--muted-foreground)" } as const;
const gridStroke = "var(--border)";
const tooltipStyle = {
	background: "var(--card)",
	border: "1px solid var(--border)",
	borderRadius: 8,
	fontSize: 12,
	color: "var(--foreground)",
} as const;
const legendStyle = { fontSize: 12 } as const;
const budgetLabel = {
	position: "insideTopRight",
	fontSize: 11,
	fill: "var(--muted-foreground)",
} as const;

// Known stages get a stable hue; anything else falls back to the palette by appearance order.
// The driver's lump and copy share encode's and packetize's hues: they are those stages there.
const STAGE_COLORS: Record<string, string> = {
	queue: "#64748b",
	capture: "#6c5bf3",
	submit: "#22a2f2",
	encode: "#f2a922",
	driver: "#f2a922",
	packetize: "#1fb6a8",
	copy: "#1fb6a8",
	send: "#f25c8a",
	send_spread: "#9b6cf3",
	pool: "#64748b",
	ipc: "#22a2f2",
};
const PALETTE = [
	"#6c5bf3",
	"#22a2f2",
	"#f2a922",
	"#1fb6a8",
	"#f25c8a",
	"#9b6cf3",
];

const STAGE_LABELS: Record<string, () => string> = {
	queue: m.stats_stage_queue,
	capture: m.stats_stage_capture,
	submit: m.stats_stage_submit,
	encode: m.stats_stage_encode,
	driver: m.stats_stage_driver,
	packetize: m.stats_stage_packetize,
	copy: m.stats_stage_copy,
	send: m.stats_stage_send,
	send_spread: m.stats_stage_send_spread,
	pool: m.stats_stage_pool,
	ipc: m.stats_stage_ipc,
};

/** A stage as the console names it; a name this build does not know shows as recorded. */
function stageLabel(name: string): string {
	return STAGE_LABELS[name]?.() ?? name;
}

/** True only after the first client-side effect — gates recharts off the server render. */
function useMounted(): boolean {
	const [mounted, setMounted] = useState(false);
	useEffect(() => setMounted(true), []);
	return mounted;
}

/** Reserves the chart's height during SSR / before mount, then swaps in the responsive chart. */
function ChartFrame({ children }: { children: ReactElement }) {
	const mounted = useMounted();
	if (!mounted) return <div style={{ height: CHART_H }} aria-hidden />;
	return (
		<ResponsiveContainer width="100%" height={CHART_H}>
			{children}
		</ResponsiveContainer>
	);
}

/** Stage names across all samples, in first-seen (pipeline) order. */
function stageNames(samples: StatsSample[]): string[] {
	const seen: string[] = [];
	for (const s of samples)
		for (const st of s.stages) if (!seen.includes(st.name)) seen.push(st.name);
	return seen;
}

function colorFor(name: string, i: number): string {
	return STAGE_COLORS[name] ?? PALETTE[i % PALETTE.length] ?? "#6c5bf3";
}

/**
 * Shared X-axis config for every chart here.
 *
 * `type="number"` + an explicit domain, NOT recharts' default category axis: as a category axis
 * every sample is one evenly-spaced slot, so a capture that idled for two minutes drew that gap
 * as a single step. As a number axis the spacing is the actual elapsed time.
 */
const timeAxis = {
	dataKey: "t",
	type: "number",
	domain: ["dataMin", "dataMax"],
	scale: "time",
	tick: axisTick,
	stroke: gridStroke,
	unit: "s",
	allowDecimals: false,
} as const;

const secondsLabel = (t: unknown) => `${fmtNumber(Number(t))} s`;

/**
 * Split a capture at every session boundary and insert a gap between the pieces.
 *
 * A capture can span more than one session (`StatsSample.session_id`), and joining those samples
 * into one continuous line implies a continuity that never existed. Recharts breaks a line
 * wherever a value is `null`, so one spacer row between sessions renders the discontinuity.
 */
function withSessionBreaks<T extends { t: number }>(
	samples: StatsSample[],
	rows: T[],
): (T | { t: number })[] {
	const out: (T | { t: number })[] = [];
	for (let i = 0; i < rows.length; i++) {
		const row = rows[i];
		if (!row) continue;
		const prev = samples[i - 1];
		const cur = samples[i];
		if (prev && cur && prev.session_id !== cur.session_id) {
			// A bare `t` row: every series key is absent ⇒ null ⇒ recharts lifts the pen.
			out.push({ t: row.t - 0.001 });
		}
		out.push(row);
	}
	return out;
}

/** Seconds since the capture began, as a number (see `timeAxis`). */
const tSeconds = (s: StatsSample): number => s.t_ms / 1000;

/** µs → ms, keeping an absent value absent. */
const toMs = (us: number | null | undefined): number | null =>
	us == null ? null : us / 1000;

/**
 * Latency by stage, stacked, in ms — the "where does the time go" view — against one frame at
 * `fps`. When the host reported its whole share, that total draws as a line over the stack, so
 * a stack that does not tile it cannot mislead. With `toggle`, a p50/p99 switch.
 */
export function LatencyChart({
	samples,
	fps,
	toggle,
}: {
	samples: StatsSample[];
	fps: number;
	toggle?: boolean;
}) {
	const [p99, setP99] = useState(false);
	const names = useMemo(() => stageNames(samples), [samples]);
	const hasHost = useMemo(
		() => samples.some((s) => s.host_p50_us != null),
		[samples],
	);
	const budget = frameBudgetMs(fps);
	// Memoised: this walks every sample × every stage, and the live card re-renders it on a 2 s
	// poll.
	const rows = useMemo(() => {
		const built = samples.map((s) => {
			const row: Record<string, number | null> & { t: number } = {
				t: tSeconds(s),
			};
			const byName = new Map(s.stages.map((st) => [st.name, st] as const));
			for (const n of names) {
				const st = byName.get(n);
				// A stage this sample lacks lifts the pen rather than drawing a zero band.
				row[n] = st ? (p99 ? st.p99_us : st.p50_us) / 1000 : null;
			}
			row.host = toMs(p99 ? s.host_p99_us : s.host_p50_us);
			return row;
		});
		return withSessionBreaks(samples, built);
	}, [samples, names, p99]);

	return (
		<div className="space-y-2">
			{toggle && (
				// Two explicit options with the active one pressed: a single button labelled with
				// the plotted percentile read as "click to show" the one already showing.
				<div className="flex justify-end gap-1">
					{([false, true] as const).map((wantP99) => (
						<Button
							key={String(wantP99)}
							variant={p99 === wantP99 ? "default" : "outline"}
							size="sm"
							aria-pressed={p99 === wantP99}
							onClick={() => setP99(wantP99)}
						>
							{wantP99 ? m.stats_p99() : m.stats_p50()}
						</Button>
					))}
				</div>
			)}
			<ChartFrame>
				<ComposedChart
					data={rows}
					margin={{ top: 6, right: 8, left: 0, bottom: 0 }}
				>
					<CartesianGrid strokeDasharray="3 3" stroke={gridStroke} />
					<XAxis {...timeAxis} />
					<YAxis
						tick={axisTick}
						stroke={gridStroke}
						width={56}
						unit={` ${m.stats_latency_axis()}`}
						domain={[0, (max: number) => latencyCeilingMs(max, budget)]}
						tickFormatter={(v: number) => msTick(v)}
					/>
					<Tooltip
						contentStyle={tooltipStyle}
						formatter={(v) => dur(Number(v) * 1000)}
						labelFormatter={secondsLabel}
					/>
					<Legend wrapperStyle={legendStyle} />
					{budget > 0 && (
						<ReferenceLine
							y={budget}
							stroke="var(--muted-foreground)"
							strokeDasharray="4 4"
							label={{
								...budgetLabel,
								value: m.stats_frame_budget({ ms: fmtNumber(budget, 1) }),
							}}
						/>
					)}
					{names.map((n, i) => (
						<Area
							key={n}
							type="monotone"
							dataKey={n}
							name={stageLabel(n)}
							stackId="lat"
							stroke={colorFor(n, i)}
							fill={colorFor(n, i)}
							fillOpacity={0.5}
							isAnimationActive={false}
						/>
					))}
					{hasHost && (
						<Line
							type="monotone"
							dataKey="host"
							name={m.stats_host_total()}
							stroke="var(--foreground)"
							strokeWidth={1.5}
							dot={false}
							isAnimationActive={false}
						/>
					)}
				</ComposedChart>
			</ChartFrame>
		</div>
	);
}

/**
 * New vs repeat fps against the stream's rate (left axis), and Mb/s against the encoder target
 * (right axis). Both axes reach at least their reference, so a quiet desktop reads as quiet.
 */
export function ThroughputChart({
	samples,
	fps,
}: {
	samples: StatsSample[];
	fps: number;
}) {
	const rows = useMemo(
		() =>
			withSessionBreaks(
				samples,
				samples.map((s) => ({
					t: tSeconds(s),
					fps: s.fps,
					repeat: s.repeat_fps,
					mbps: s.mbps,
					// The encoder target (kbps → Mb/s) so goodput reads against it.
					target: s.bitrate_kbps / 1000,
				})),
			),
		[samples],
	);
	const targetMax = useMemo(
		() => samples.reduce((top, s) => Math.max(top, s.bitrate_kbps / 1000), 0),
		[samples],
	);
	return (
		<ChartFrame>
			<LineChart data={rows} margin={{ top: 6, right: 8, left: 0, bottom: 0 }}>
				<CartesianGrid strokeDasharray="3 3" stroke={gridStroke} />
				<XAxis {...timeAxis} />
				<YAxis
					yAxisId="fps"
					tick={axisTick}
					stroke={gridStroke}
					width={40}
					allowDecimals={false}
					domain={[
						0,
						(max: number) => Math.max(Math.ceil(max), Math.ceil(fps * 1.1)),
					]}
					tickFormatter={(v: number) => fmtNumber(v)}
				/>
				<YAxis
					yAxisId="mbps"
					orientation="right"
					tick={axisTick}
					stroke={gridStroke}
					width={48}
					domain={[0, (max: number) => Math.max(max, targetMax * 1.2, 1)]}
					tickFormatter={(v: number) => fmtNumber(v)}
				/>
				<Tooltip
					contentStyle={tooltipStyle}
					formatter={(v) => fmtNumber(Number(v), 1)}
					labelFormatter={secondsLabel}
				/>
				<Legend wrapperStyle={legendStyle} />
				{fps > 0 && (
					<ReferenceLine
						yAxisId="fps"
						y={fps}
						stroke="var(--muted-foreground)"
						strokeDasharray="4 4"
						label={{ ...budgetLabel, value: m.stats_fps_budget({ fps }) }}
					/>
				)}
				<Line
					yAxisId="fps"
					type="monotone"
					dataKey="fps"
					name={m.stats_fps_new()}
					stroke="#6c5bf3"
					dot={false}
					isAnimationActive={false}
				/>
				<Line
					yAxisId="fps"
					type="monotone"
					dataKey="repeat"
					name={m.stats_fps_repeat()}
					stroke="#f2a922"
					strokeDasharray="4 3"
					dot={false}
					isAnimationActive={false}
				/>
				<Line
					yAxisId="mbps"
					type="monotone"
					dataKey="mbps"
					name={m.stats_mbps()}
					stroke="#1fb6a8"
					dot={false}
					isAnimationActive={false}
				/>
				<Line
					yAxisId="mbps"
					type="monotone"
					dataKey="target"
					name={m.stats_bitrate_target()}
					stroke="#94a3b8"
					strokeDasharray="2 3"
					dot={false}
					isAnimationActive={false}
				/>
			</LineChart>
		</ChartFrame>
	);
}

type Counter = {
	key: string;
	pick: (s: StatsSample) => number | null | undefined;
	name: () => string;
	stroke: string;
};

/** One line per series against time, the y axis starting at zero and reaching at least `floor`. */
function SeriesChart({
	samples,
	series,
	y,
	format,
	legend = true,
}: {
	samples: StatsSample[];
	series: readonly Counter[];
	y: {
		width: number;
		floor: number;
		unit?: string;
		integer?: boolean;
		tick: (v: number) => string;
	};
	format: (v: number) => string;
	legend?: boolean;
}) {
	const rows = useMemo(
		() =>
			withSessionBreaks(
				samples,
				samples.map((s) => {
					const row: Record<string, number | null> & { t: number } = {
						t: tSeconds(s),
					};
					for (const c of series) row[c.key] = c.pick(s) ?? null;
					return row;
				}),
			),
		[samples, series],
	);
	return (
		<ChartFrame>
			<LineChart data={rows} margin={{ top: 6, right: 8, left: 0, bottom: 0 }}>
				<CartesianGrid strokeDasharray="3 3" stroke={gridStroke} />
				<XAxis {...timeAxis} />
				<YAxis
					tick={axisTick}
					stroke={gridStroke}
					width={y.width}
					unit={y.unit}
					allowDecimals={!y.integer}
					domain={[0, (max: number) => Math.max(max, y.floor)]}
					tickFormatter={y.tick}
				/>
				<Tooltip
					contentStyle={tooltipStyle}
					formatter={(v) => format(Number(v))}
					labelFormatter={secondsLabel}
				/>
				{legend && <Legend wrapperStyle={legendStyle} />}
				{series.map((c) => (
					<Line
						key={c.key}
						type="monotone"
						dataKey={c.key}
						name={c.name()}
						stroke={c.stroke}
						dot={false}
						isAnimationActive={false}
					/>
				))}
			</LineChart>
		</ChartFrame>
	);
}

const COUNTERS: Counter[] = [
	{
		key: "frames",
		pick: (s) => s.frames_dropped,
		name: m.stats_frames_dropped,
		stroke: "#f25c8a",
	},
	{
		key: "packets",
		pick: (s) => s.packets_dropped,
		name: m.stats_packets_dropped,
		stroke: "#f2a922",
	},
	{
		key: "send",
		pick: (s) => s.send_dropped,
		name: m.stats_send_dropped,
		stroke: "#22a2f2",
	},
	{
		key: "fec",
		pick: (s) => s.fec_recovered,
		name: m.stats_fec_recovered,
		stroke: "#1fb6a8",
	},
];

/**
 * Loss and recovery counters per window. A counter the path does not measure is absent from
 * its samples and gets no line: a flat zero would read as "no loss".
 */
export function HealthChart({ samples }: { samples: StatsSample[] }) {
	const present = useMemo(
		() => COUNTERS.filter((c) => samples.some((s) => c.pick(s) != null)),
		[samples],
	);
	if (present.length === 0)
		return (
			<p className="text-xs text-muted-foreground">{m.stats_health_none()}</p>
		);
	return (
		<SeriesChart
			samples={samples}
			series={present}
			y={{ width: 40, floor: 5, integer: true, tick: (v) => fmtNumber(v) }}
			format={(v) => fmtNumber(v)}
		/>
	);
}

/** Whether any sample carries the connection's round trip. */
export function hasRtt(samples: StatsSample[]): boolean {
	return samples.some((s) => s.rtt_us != null);
}

const RTT: readonly Counter[] = [
	{
		key: "rtt",
		pick: (s) => toMs(s.rtt_us),
		name: m.stats_rtt_title,
		stroke: "#22a2f2",
	},
];

/** The QUIC round trip to the client, in ms — the network's share of the picture. */
export function RttChart({ samples }: { samples: StatsSample[] }) {
	return (
		<SeriesChart
			samples={samples}
			series={RTT}
			y={{
				width: 56,
				floor: 1,
				unit: ` ${m.stats_latency_axis()}`,
				tick: (v) => msTick(v),
			}}
			format={(v) => dur(v * 1000)}
			legend={false}
		/>
	);
}

/** Whether any sample carries the sealing split (native captures). */
export function hasSendSplit(samples: StatsSample[]): boolean {
	return samples.some(
		(s) => s.fec_us != null || s.seal_us != null || s.sock_us != null,
	);
}

const SEND_SPLIT: readonly Counter[] = [
	{
		key: "fec_us",
		pick: (s) => s.fec_us,
		name: m.stats_send_fec,
		stroke: "#8b5cf6",
	},
	{
		key: "seal_us",
		pick: (s) => s.seal_us,
		name: m.stats_send_seal,
		stroke: "#f59e0b",
	},
	{
		key: "sock_us",
		pick: (s) => s.sock_us,
		name: m.stats_send_sock,
		stroke: "#10b981",
	},
];

/** What sealing one frame costs, in µs: parity, encryption, the socket. Sub-millisecond by nature. */
export function SendSplitChart({ samples }: { samples: StatsSample[] }) {
	return (
		<SeriesChart
			samples={samples}
			series={SEND_SPLIT}
			y={{ width: 56, floor: 10, unit: " µs", tick: (v) => fmtNumber(v) }}
			format={(v) => dur(v)}
		/>
	);
}
