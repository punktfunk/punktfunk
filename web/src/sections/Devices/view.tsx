import Section from "@unom/ui/section";
import { motion } from "motion/react";
import type { FC, ReactNode } from "react";
import { QueryState } from "@/components/query-state";
import { ROW_GAP, staggerProps } from "@/components/stagger";
import { Card, CardContent } from "@/components/ui/card";
import { m } from "@/paraglide/messages";
import type { PairedListView } from "./pairedRows";

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
	pairedState: Omit<PairedListView, "rows"> & { refetch?: () => void };
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
							<motion.ul {...staggerProps(ROW_GAP)} className="divide-y">
								{waiting}
							</motion.ul>
						</Group>
					)}
					<Group label={m.pairing_native_devices()}>
						<QueryState isLoading={pairedState.loading} error={null}>
							<ListError
								error={pairedState.nativeError}
								refetch={pairedState.refetch}
							/>
							{pairedState.empty ? (
								<p className="py-2 text-sm text-muted-foreground">
									{m.pairing_native_empty()}
								</p>
							) : (
								paired.length > 0 && (
									<>
										<div className="mt-3 hidden gap-x-4 border-b pb-2 text-xs font-medium text-muted-foreground md:grid md:grid-cols-[minmax(0,1.3fr)_minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,0.6fr)_auto]">
											<span>{m.devices_col_device()}</span>
											<span>{m.devices_col_access()}</span>
											<span>{m.devices_col_display()}</span>
											<span />
											<span className="w-9" />
										</div>
										<motion.ul {...staggerProps(ROW_GAP)} className="divide-y">
											{paired}
										</motion.ul>
									</>
								)
							)}
							<ListError
								error={pairedState.moonlightError}
								refetch={pairedState.refetch}
							/>
						</QueryState>
					</Group>
				</CardContent>
			</Card>
		</div>
	</Section>
);

/** A failing list's error and Retry, where its rows would sit. */
const ListError: FC<{ error: unknown; refetch?: () => void }> = ({
	error,
	refetch,
}) =>
	error ? (
		<div className="mt-3">
			<QueryState isLoading={false} error={error} refetch={refetch}>
				{null}
			</QueryState>
		</div>
	) : null;

const Group: FC<{ label: string; children: ReactNode }> = ({
	label,
	children,
}) => (
	<section>
		<h2 className="text-xs font-medium text-muted-foreground">{label}</h2>
		{children}
	</section>
);
