import { Link } from "@tanstack/react-router";
import { AlertTriangle, ArrowRight } from "lucide-react";
import { motion } from "motion/react";
import type { FC } from "react";
import { useGetDiagnostics } from "@/api/gen/diagnostics/diagnostics";
import type { AudioWiring } from "@/api/gen/model/audioWiring";
import type { HostCheck } from "@/api/gen/model/hostCheck";
import { useListPendingDevices } from "@/api/gen/native/native";
import { ROW, ROW_GAP, staggerProps } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import {
	checkTitle,
	needsAttention,
	statusLabel,
	statusVariant,
	worstFirst,
} from "@/lib/diagnostics";
import { m } from "@/paraglide/messages";

/**
 * Home's *Attention*: "something about this host needs you". Renders nothing when nothing does,
 * so a healthy host sees no chrome. A pointer, not a manual: each line links to the page that
 * acts on it.
 */

/** Health checks shown before deferring to Troubleshooting. Three keeps it a strip. */
const MAX_CHECKS = 3;

export const Attention: FC<{ audio?: AudioWiring | null }> = ({ audio }) => {
	// Startup-static checks: one cache entry shared with Troubleshooting, not a poll. A host
	// without `/diagnostics` answers 404, which is a supported pairing, not a fault.
	const diagnostics = useGetDiagnostics({
		query: { staleTime: 5 * 60_000, retry: false },
	});
	const pending = useListPendingDevices({ query: { refetchInterval: 10_000 } });
	return (
		<AttentionStrip
			checks={diagnostics.data?.checks ?? []}
			waiting={(pending.data ?? []).map((p) => p.name)}
			audio={audio}
		/>
	);
};

/** No game audio leaves the host: the endpoint is missing, not merely the microphone. */
const audioUnready = (a?: AudioWiring | null) =>
	a != null && (a.readiness === "mic_only" || a.readiness === "none");

/** The pure half — fed fixtures by the stories, so the empty state is provable. */
export const AttentionStrip: FC<{
	checks: HostCheck[];
	/** Names of the devices waiting for approval. */
	waiting?: string[];
	audio?: AudioWiring | null;
}> = ({ checks, waiting = [], audio }) => {
	const problems = worstFirst(checks.filter(needsAttention));
	const deaf = audioUnready(audio);
	if (problems.length === 0 && waiting.length === 0 && !deaf) return null;
	const shown = problems.slice(0, MAX_CHECKS);
	const hidden = problems.length - shown.length;
	return (
		<Card className="border-amber-600/40 dark:border-amber-500/40">
			<CardContent className="flex items-start gap-3">
				<AlertTriangle className="mt-0.5 size-5 shrink-0 text-amber-600 dark:text-amber-500" />
				<div className="min-w-0 flex-1 space-y-3">
					<p className="text-sm font-medium text-amber-600 dark:text-amber-500">
						{m.diag_attention_title()}
					</p>
					<motion.ul
						{...staggerProps(ROW_GAP)}
						className="flex flex-col gap-2 text-sm"
					>
						{waiting.length > 0 && (
							<Line to="/devices" link={m.nav_devices()}>
								{m.home_attention_pending({ names: waiting.join(", ") })}
							</Line>
						)}
						{deaf && audio && (
							<Line to="/host" link={m.nav_host()}>
								{m.home_attention_audio({
									state:
										audio.readiness === "none"
											? m.audio_none()
											: m.audio_no_output(),
								})}
							</Line>
						)}
						{shown.map((check) => (
							<motion.li
								variants={ROW}
								key={check.id}
								className="flex flex-wrap items-baseline gap-x-2 gap-y-1"
							>
								{/* Text, not colour alone: a screen reader reads the badge. */}
								<Badge variant={statusVariant(check)}>
									{statusLabel(check)}
								</Badge>
								<span className="font-medium">{checkTitle(check)}</span>
								<span className="min-w-0 text-muted-foreground">
									{check.summary}
								</span>
							</motion.li>
						))}
					</motion.ul>
					{shown.length > 0 && (
						<div className="flex flex-wrap items-center gap-x-3 gap-y-1">
							<Link
								to="/diagnostics/logs"
								className="inline-flex items-center gap-1 text-sm font-medium hover:underline"
							>
								{m.diag_attention_link()}
								<ArrowRight className="size-3.5" />
							</Link>
							{hidden > 0 && (
								<span className="text-xs text-muted-foreground">
									{m.diag_attention_more({ count: hidden })}
								</span>
							)}
						</div>
					)}
				</div>
			</CardContent>
		</Card>
	);
};

const Line: FC<{
	to: "/devices" | "/host";
	link: string;
	children: string;
}> = ({ to, link, children }) => (
	<motion.li
		variants={ROW}
		className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1"
	>
		<span className="min-w-0 font-medium">{children}</span>
		<Link
			to={to}
			className="inline-flex items-center gap-1 font-medium hover:underline"
		>
			{link}
			<ArrowRight className="size-3.5" />
		</Link>
	</motion.li>
);
