import { Link } from "@tanstack/react-router";
import { AlertTriangle, ArrowRight } from "lucide-react";
import type { FC } from "react";
import { useGetDiagnostics } from "@/api/gen/diagnostics/diagnostics";
import type { HostCheck } from "@/api/gen/model/hostCheck";
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
 * The dashboard's attention strip: "something about this host needs you".
 *
 * Follows the `ConflictsCard` rule — **renders nothing at all when there is nothing**, so a healthy
 * host sees zero extra chrome. It is deliberately a pointer, not a manual: severity, the check's
 * name and the host's one-line summary, then a link. Remedies live on the troubleshooting page,
 * because a dashboard that starts explaining how to fix things stops being a dashboard.
 */

/** Rows shown before deferring to the troubleshooting page. Three keeps it a strip. */
const MAX_ROWS = 3;

export const AttentionCard: FC = () => {
	// The v1 checks are startup-static (a group membership, an installed udev rule), so this shares
	// one generous-`staleTime` cache entry with the troubleshooting page rather than polling.
	const diagnostics = useGetDiagnostics({
		query: {
			staleTime: 5 * 60_000,
			// A host older than this console has no `/diagnostics` route and answers 404. That is a
			// supported pairing, not a fault, so don't retry it and don't surface it here — the
			// troubleshooting page is where "this host can't report checks" gets explained.
			retry: false,
		},
	});
	return <AttentionStrip checks={diagnostics.data?.checks ?? []} />;
};

/** The pure half — fed fixtures by the stories, so the empty state is provable. */
export const AttentionStrip: FC<{ checks: HostCheck[] }> = ({ checks }) => {
	const problems = worstFirst(checks.filter(needsAttention));
	if (problems.length === 0) return null;
	const shown = problems.slice(0, MAX_ROWS);
	const hidden = problems.length - shown.length;
	return (
		<Card className="border-amber-600/40 dark:border-amber-500/40">
			<CardContent className="flex items-start gap-3">
				<AlertTriangle className="mt-0.5 size-5 shrink-0 text-amber-600 dark:text-amber-500" />
				<div className="min-w-0 flex-1 space-y-3">
					<p className="text-sm font-medium text-amber-600 dark:text-amber-500">
						{m.diag_attention_title()}
					</p>
					<ul className="flex flex-col gap-2">
						{shown.map((check) => (
							<li
								key={check.id}
								className="flex flex-wrap items-baseline gap-x-2 gap-y-1 text-sm"
							>
								{/* Text, not colour alone — the badge has to be readable to a screen
								    reader and to anyone who cannot tell the two tints apart. */}
								<Badge variant={statusVariant(check)}>
									{statusLabel(check)}
								</Badge>
								<span className="font-medium">{checkTitle(check)}</span>
								<span className="min-w-0 text-muted-foreground">
									{check.summary}
								</span>
							</li>
						))}
					</ul>
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
				</div>
			</CardContent>
		</Card>
	);
};
