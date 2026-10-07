import { Radio } from "lucide-react";
import { type FC, useMemo } from "react";
import { ApiError } from "@/api/fetcher";
import type { Capture } from "@/api/gen/model/capture";
import {
	useStatsCaptureLive,
	useStatsCaptureStatus,
} from "@/api/gen/stats/stats";
import { QueryState } from "@/components/query-state";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import {
	HealthChart,
	hasRtt,
	hasSendSplit,
	LatencyChart,
	RttChart,
	SendSplitChart,
	ThroughputChart,
} from "./charts";
import { ChartBlock } from "./helpers";

/**
 * Container: the live graphs. Self-gates on the capture being armed — it shares the status query
 * (same key) with the control card, and only fetches the in-progress capture while armed (it 404s
 * when idle). Renders nothing when no capture is running.
 */
export const LiveSection: FC = () => {
	const status = useStatsCaptureStatus({ query: { refetchInterval: 2_000 } });
	const armed = status.data?.armed ?? false;
	const live = useStatsCaptureLive({
		query: { refetchInterval: 2_000, enabled: armed },
	});
	if (!armed) return null;
	return <LiveCard live={live} />;
};

/**
 * How many samples the live charts plot.
 *
 * The live endpoint returns the capture SO FAR, which grows without bound — a capture left running
 * over an evening is tens of thousands of samples, re-serialised and re-plotted every 2 s. The tail
 * is also the only part anyone watches live (the full series is what the saved recording is for),
 * so plot a bounded window and leave the rest to the detail view.
 */
const LIVE_WINDOW = 600;

/** Live graphs while a capture is armed: the set a saved recording shows, over the last window. */
export const LiveCard: FC<{ live: Loadable<Capture> }> = ({ live }) => {
	const all = live.data?.samples;
	// Memoised on the array identity: React Query keeps it stable when a poll changed nothing, so
	// an unchanged poll costs no re-slice and — because `samples` keeps its identity — no chart
	// rebuild either (the charts memoise on exactly this).
	const samples = useMemo(
		() =>
			all && all.length > LIVE_WINDOW ? all.slice(-LIVE_WINDOW) : (all ?? []),
		[all],
	);
	// A 404 is the expected transient right after arming (the capture isn't there yet) — treat it as
	// "waiting". Surface any OTHER error (500, network drop) instead of silently showing "waiting".
	const error =
		live.error instanceof ApiError && live.error.status === 404
			? null
			: live.error;
	const fps = live.data?.meta?.fps ?? 0;
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Radio className="size-4" />
					{m.stats_live_title()}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-8">
				<QueryState isLoading={false} error={error} refetch={live.refetch}>
					{samples.length === 0 ? (
						<p className="py-8 text-center text-sm text-muted-foreground">
							{m.stats_live_waiting()}
						</p>
					) : (
						<>
							<ChartBlock
								title={m.stats_latency_title()}
								desc={m.stats_latency_desc()}
							>
								<LatencyChart samples={samples} fps={fps} toggle />
							</ChartBlock>
							<ChartBlock title={m.stats_throughput_title()}>
								<ThroughputChart samples={samples} fps={fps} />
							</ChartBlock>
							{/* Loss is what a live capture is watched FOR, so it plots here too, not
							    only in the saved recording. */}
							<ChartBlock title={m.stats_health_title()}>
								<HealthChart samples={samples} />
							</ChartBlock>
							{hasRtt(samples) && (
								<ChartBlock title={m.stats_rtt_title()}>
									<RttChart samples={samples} />
								</ChartBlock>
							)}
							{hasSendSplit(samples) && (
								<ChartBlock title={m.stats_send_title()}>
									<SendSplitChart samples={samples} />
								</ChartBlock>
							)}
							{(live.data?.samples?.length ?? 0) > LIVE_WINDOW && (
								<p className="text-xs text-muted-foreground">
									{m.stats_live_window({ count: LIVE_WINDOW })}
								</p>
							)}
						</>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};
