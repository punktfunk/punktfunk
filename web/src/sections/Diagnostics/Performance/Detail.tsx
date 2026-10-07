import { ChartColumn, X } from "lucide-react";
import type { FC } from "react";
import type { Capture } from "@/api/gen/model/capture";
import { useStatsRecordingGet } from "@/api/gen/stats/stats";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
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

/** Container: the full graph set for the selected recording — fetched by id. */
export const DetailSection: FC<{ id: string; onClose: () => void }> = ({
	id,
	onClose,
}) => {
	const detail = useStatsRecordingGet(id, { query: { enabled: !!id } });
	return <DetailCard detail={detail} onClose={onClose} />;
};

/** One recording's graphs: latency (p99 toggle), throughput, health, and the round trip and
 * sealing split when the recording carries them. */
export const DetailCard: FC<{
	detail: Loadable<Capture>;
	onClose: () => void;
}> = ({ detail, onClose }) => {
	const cap = detail.data;
	const samples = cap?.samples ?? [];
	const fps = cap?.meta.fps ?? 0;
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center justify-between gap-3">
					<span className="flex items-center gap-2">
						<ChartColumn className="size-4" />
						{m.stats_detail_title()}
						{cap && (
							// Encoder + GPU ride along with the mode: the stage split below can't be
							// read without knowing which backend produced it (a 10 ms `submit` means
							// very different things on NVENC and on Vulkan). Older recordings predate
							// the fields and simply omit them.
							<span className="ml-2 text-sm font-normal text-muted-foreground">
								{cap.meta.width}×{cap.meta.height}@{cap.meta.fps} ·{" "}
								{cap.meta.codec.toUpperCase()}
								{cap.meta.encoder_backend && ` · ${cap.meta.encoder_backend}`}
								{cap.meta.gpu && ` · ${cap.meta.gpu}`}
							</span>
						)}
					</span>
					<Button
						variant="ghost"
						size="icon"
						aria-label={m.stats_close()}
						onClick={onClose}
					>
						<X className="size-4" />
					</Button>
				</CardTitle>
			</CardHeader>
			<CardContent>
				<QueryState
					isLoading={detail.isLoading}
					error={detail.error}
					refetch={detail.refetch}
				>
					{samples.length === 0 ? (
						<p className="py-8 text-center text-sm text-muted-foreground">
							{m.stats_no_samples()}
						</p>
					) : (
						<div className="space-y-8">
							{cap?.meta.truncated && (
								<p className="text-xs text-muted-foreground">
									{m.stats_truncated_note()}
								</p>
							)}
							<ChartBlock
								title={m.stats_latency_title()}
								desc={m.stats_latency_desc()}
							>
								<LatencyChart samples={samples} fps={fps} toggle />
							</ChartBlock>
							<ChartBlock title={m.stats_throughput_title()}>
								<ThroughputChart samples={samples} fps={fps} />
							</ChartBlock>
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
						</div>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};
