import type { FC, ReactNode } from "react";
import { m } from "@/paraglide/messages";

/**
 * The Troubleshooting segment's LAYOUT — the live page (`index.tsx`) and the Storybook stories fill
 * the slots, so the arrangement can never drift between them (same pattern as StatsView).
 *
 * This is the troubleshooting home: it is where someone already goes when something is wrong, so
 * the host's health checks meet them here rather than on a nav entry that is empty on a healthy
 * host.
 *
 * Order is deliberate: checks first (structured, actionable), the log stream underneath. When the
 * checks are green and something is still broken, the log is the natural next step — now one scroll
 * away instead of a separate destination.
 *
 * `actions` sits in the heading rather than in the viewer's toolbar because what lives there is a
 * PAGE-level export — every check, every producer, every stored bundle, regardless of what the
 * viewer is currently filtered to. The toolbar's own download means "what I am looking at"; keeping
 * the two apart in space is what keeps them apart in meaning.
 */
export const LogsView: FC<{
	checks?: ReactNode;
	actions?: ReactNode;
	viewer: ReactNode;
}> = ({ checks, actions, viewer }) => (
	<div className="flex flex-col gap-card">
		<div className="flex flex-wrap items-start justify-between gap-2">
			<p className="text-sm text-muted-foreground">
				{m.troubleshooting_subtitle()}
			</p>
			{actions}
		</div>

		{checks}
		{viewer}
	</div>
);
