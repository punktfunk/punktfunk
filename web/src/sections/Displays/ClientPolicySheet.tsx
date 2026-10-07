// One device's display settings (design/web-console-overhaul.md §6.2).
//
// The host's questions (`PolicyQuestions`), each with **Follow host** first, naming what
// following means. An overlay pins only what the operator chooses; the rest tracks the host.
// Only the axes the host acts on PER DEVICE are asked (`client_enforced`).
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import type { FC } from "react";
import {
	getGetDisplaySettingsQueryKey,
	useDeleteDisplayClient,
	useGetDisplaySettings,
	useSetDisplayClient,
} from "@/api/gen/display/display";
import type { ClientOverlay, EffectivePolicy } from "@/api/gen/model";
import { Button } from "@/components/ui/button";
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { m } from "@/paraglide/messages";
import { describePolicy } from "./describePolicy";
import {
	conflictLabel,
	identityLabel,
	keepLabel,
	type PolicyPatch,
	PolicyQuestions,
	type PolicyValues,
	topologyLabel,
} from "./PolicyQuestions";

/**
 * The device row's short form: what this device does DIFFERENTLY, in a few words.
 *
 * Not the full sentence — that belongs in the sheet, where there is room for it. A row is
 * scanned, so it carries only the pinned axes, in the same words the sheet uses for them.
 */
export function overlaySummary(overlay: ClientOverlay | undefined): string {
	const pinned = overlay ? stripNulls(overlay) : {};
	const parts: string[] = [];
	if (pinned.topology) parts.push(topologyLabel(pinned.topology));
	if (pinned.mode_conflict) parts.push(conflictLabel(pinned.mode_conflict));
	if (pinned.identity) parts.push(identityLabel(pinned.identity));
	if (pinned.keep_alive) parts.push(keepLabel(pinned.keep_alive));
	if (overlay?.capture_monitor) {
		parts.push(m.display_mirrors({ connector: overlay.capture_monitor }));
	}
	if (overlay?.max_mode)
		parts.push(m.display_capped({ mode: overlay.max_mode }));
	if (overlay?.scale) parts.push(`${overlay.scale}×`);
	return parts.length === 0 ? m.display_follows_host() : parts.join(" · ");
}

/** orval types an absent field as `null | T`; the overlay's own meaning of absent is "follow". */
function stripNulls(
	overlay: ClientOverlay,
): Partial<EffectivePolicy> & PolicyValues {
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(overlay)) {
		if (v !== null && v !== undefined) out[k] = v;
	}
	return out as Partial<EffectivePolicy> & PolicyValues;
}

export const ClientPolicySheet: FC<{
	open: boolean;
	onOpenChange: (open: boolean) => void;
	/** Pairing fingerprint — what the overlay is keyed by, never an address. */
	fingerprint: string;
	deviceName: string;
}> = ({ open, onOpenChange, fingerprint, deviceName }) => {
	const qc = useQueryClient();
	const settings = useGetDisplaySettings();
	const save = useSetDisplayClient();
	const clear = useDeleteDisplayClient();

	const host = settings.data?.effective;
	const overlay: ClientOverlay = settings.data?.clients?.[fingerprint] ?? {};
	const enforced = settings.data?.client_enforced ?? [];
	const busy = save.isPending || clear.isPending;

	const invalidate = () =>
		qc.invalidateQueries({ queryKey: getGetDisplaySettingsQueryKey() });

	/** The whole overlay is the unit of write: a field dropped here stops being pinned. */
	const write = (patch: ClientOverlay | PolicyPatch) => {
		const next = { ...stripNulls(overlay), ...patch };
		for (const [k, v] of Object.entries(patch)) {
			if (v === null) delete (next as Record<string, unknown>)[k];
		}
		save.mutate(
			{ fingerprint, data: next },
			{
				onSuccess: () => {
					invalidate();
					toast.success(m.display_settings_saved());
				},
			},
		);
	};

	const followAll = () =>
		clear.mutate(
			{ fingerprint },
			{
				onSuccess: () => {
					invalidate();
					toast.success(m.display_settings_saved());
				},
			},
		);

	const pinned = Object.keys(stripNulls(overlay)).length > 0;

	return (
		<Dialog open={open} onOpenChange={onOpenChange}>
			<DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-xl">
				<DialogHeader>
					<DialogTitle>{deviceName}</DialogTitle>
				</DialogHeader>

				{host && (
					<div className="space-y-5">
						<PolicyQuestions
							value={stripNulls(overlay)}
							axes={enforced}
							inherited={host}
							onSet={write}
							busy={busy}
						/>

						{/* The sentence this device will actually get, assembled by the same
						    generator as the host's — so the two can never disagree. */}
						<p className="rounded-md border bg-muted/40 p-3 text-sm">
							{describePolicy(
								{ ...host, ...stripNulls(overlay) },
								{ deviceName },
							)}
						</p>

						{pinned && (
							<Button variant="ghost" disabled={busy} onClick={followAll}>
								{m.display_follow_host_all()}
							</Button>
						)}
					</div>
				)}
			</DialogContent>
		</Dialog>
	);
};
