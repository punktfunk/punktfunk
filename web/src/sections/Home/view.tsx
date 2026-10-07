import Section from "@unom/ui/section";
import { motion } from "motion/react";
import type { FC, ReactNode } from "react";
import type { RuntimeStatus } from "@/api/gen/model/runtimeStatus";
import { QueryState } from "@/components/query-state";
import { ROW_GAP, staggerProps } from "@/components/stagger";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/**
 * The Home LAYOUT — three sections, one question: what needs attention? The live page
 * (`index.tsx`) and the stories fill the same slots.
 *
 * `attention` sits above the status query on purpose: a host whose `/status` fails is exactly
 * when its health checks are worth reading. With nothing live, *Now* holds `last` instead of rows.
 */
export const HomeView: FC<{
	attention?: ReactNode;
	status: Loadable<RuntimeStatus>;
	live: boolean;
	now: ReactNode;
	last: ReactNode;
	recent?: ReactNode;
}> = ({ attention, status, live, now, last, recent }) => (
	<Section maxWidth={false}>
		<div className="flex flex-col gap-card">
			<h1 className="text-2xl font-semibold">{m.nav_home()}</h1>
			{attention}
			<Card>
				<CardHeader>
					<CardTitle>{m.home_now()}</CardTitle>
				</CardHeader>
				<CardContent>
					<QueryState
						isLoading={status.isLoading}
						error={status.error}
						refetch={status.refetch}
					>
						{/* Rows mount once /status answers: their own container staggers them. */}
						{live ? (
							<motion.ul
								{...staggerProps(ROW_GAP)}
								className="flex flex-col divide-y"
							>
								{now}
							</motion.ul>
						) : (
							last
						)}
					</QueryState>
				</CardContent>
			</Card>
			{recent}
		</div>
	</Section>
);
