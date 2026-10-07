import Section from "@unom/ui/section";
import type { FC, ReactNode } from "react";
import { QueryState } from "@/components/query-state";
import { Card, CardContent } from "@/components/ui/card";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

/**
 * The Devices LAYOUT — one list: what is waiting first, then what is paired. Pairing opens a
 * sheet from the header. The live page (`index.tsx`) and the stories fill the same slots.
 */
export const DevicesView: FC<{
	/** Pair a device, and the ⋯ that unpairs everything. */
	actions: ReactNode;
	/** Knocks, an armed PIN, a Moonlight client waiting for its PIN. */
	waiting: ReactNode[];
	paired: ReactNode[];
	pairedState: Omit<Loadable<unknown>, "data">;
}> = ({ actions, waiting, paired, pairedState }) => (
	<Section maxWidth={false}>
		<div className="flex flex-col gap-card">
			<div className="flex flex-wrap items-center justify-between gap-3">
				<h1 className="text-2xl font-semibold">{m.pairing_title()}</h1>
				<div className="flex items-center gap-1">{actions}</div>
			</div>
			<Card>
				<CardContent className="space-y-5">
					{waiting.length > 0 && (
						<Group label={m.pairing_pending_title()}>
							<ul className="divide-y">{waiting}</ul>
						</Group>
					)}
					<Group label={m.pairing_native_devices()}>
						<QueryState
							isLoading={pairedState.isLoading}
							error={pairedState.error}
							refetch={pairedState.refetch}
						>
							{paired.length === 0 ? (
								<p className="py-2 text-sm text-muted-foreground">
									{m.pairing_native_empty()}
								</p>
							) : (
								<ul className="divide-y">{paired}</ul>
							)}
						</QueryState>
					</Group>
				</CardContent>
			</Card>
		</div>
	</Section>
);

const Group: FC<{ label: string; children: ReactNode }> = ({
	label,
	children,
}) => (
	<section>
		<h2 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
			{label}
		</h2>
		{children}
	</section>
);
