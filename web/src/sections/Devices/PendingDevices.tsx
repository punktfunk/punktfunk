import { useQueryClient } from "@tanstack/react-query";
import { Globe, KeyRound, X } from "lucide-react";
import { type FC, type ReactNode, useState } from "react";
import type { ApprovePending } from "@/api/gen/model/approvePending";
import type { PendingDevice } from "@/api/gen/model/pendingDevice";
import {
	getListNativeClientsQueryKey,
	getListPendingDevicesQueryKey,
	useDenyPendingDevice,
	useListPendingDevices,
} from "@/api/gen/native/native";
import { useApprovePendingDevice } from "@/api/pairing";
import { usePasswordFailure } from "@/components/password-confirm";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { fmtAge } from "@/lib/utils";
import { m } from "@/paraglide/messages";
import { ApproveDialog } from "./ApproveDialog";

/**
 * Devices awaiting delegated approval: polled so a knock appears while looking (the
 * `pairing.pending` event refreshes it sooner). Approving pairs the device, so it also refreshes
 * the paired list. `dialog` is the approve dialog, mounted once by the page.
 */
export function usePendingDevices() {
	const qc = useQueryClient();
	const pending = useListPendingDevices({ query: { refetchInterval: 10_000 } });
	const approve = useApprovePendingDevice();
	const deny = useDenyPendingDevice();
	// A snapshot, so the 10 s poll can't reset the form mid-edit.
	const [approving, setApproving] = useState<PendingDevice | null>(null);
	const refusal = usePasswordFailure();
	const refresh = () => {
		qc.invalidateQueries({ queryKey: getListPendingDevicesQueryKey() });
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
	};
	const open = (device: PendingDevice | null) => {
		refusal.reset();
		setApproving(device);
	};
	const onApprove = (id: number, body: ApprovePending, password: string) => {
		refusal.reset();
		approve.mutate(
			{ id, data: body, password },
			{
				onSuccess: () => {
					open(null);
					refresh();
				},
				onError: refusal.classify,
			},
		);
	};
	return {
		pending,
		onApprove: open,
		onDeny: (id: number) => deny.mutate({ id }, { onSuccess: refresh }),
		/** The row whose approve/deny is in flight — only its buttons disable. */
		pendingId:
			(approve.isPending ? approve.variables?.id : undefined) ??
			(deny.isPending ? deny.variables?.id : undefined) ??
			null,
		dialog: (
			<ApproveDialog
				device={approving}
				onCancel={() => open(null)}
				onApprove={onApprove}
				isPending={approve.isPending}
				failure={refusal.failure}
			/>
		),
	};
}

/** A waiting row: what it is, one line of detail, at most two actions. */
export const WaitingRow: FC<{
	lead: ReactNode;
	title: ReactNode;
	details?: ReactNode;
	actions: ReactNode;
}> = ({ lead, title, details, actions }) => (
	<li className="flex items-center gap-3 py-3">
		<div className="flex size-6 shrink-0 items-center justify-center text-muted-foreground">
			{lead}
		</div>
		<div className="min-w-0 flex-1">
			<div className="flex flex-wrap items-center gap-2 font-medium">
				{title}
			</div>
			{details && (
				<div className="break-all text-xs text-muted-foreground">{details}</div>
			)}
		</div>
		<div className="flex shrink-0 items-center gap-1">{actions}</div>
	</li>
);

/**
 * A device that knocked. A knock from the internet has no Approve: the host refuses one (409),
 * because its name proves nothing. Arming a PIN bound to its fingerprint is the way in.
 */
export const PendingRow: FC<{
	device: PendingDevice;
	onApprove: (device: PendingDevice) => void;
	onArmFor: (device: PendingDevice) => void;
	onDeny: (id: number) => void;
	busy: boolean;
}> = ({ device: p, onApprove, onArmFor, onDeny, busy }) => (
	<WaitingRow
		lead={<span className="size-2 rounded-full bg-amber-500" />}
		title={
			<>
				<span className="truncate">{p.name}</span>
				{p.source === "wan" && (
					<Badge variant="outline" className="shrink-0 gap-1">
						<Globe className="size-3" />
						{m.pairing_pending_from_internet()}
					</Badge>
				)}
			</>
		}
		details={[
			fmtAge(p.age_secs),
			p.profile
				? m.devices_will_play_as({ name: p.profile.display_name })
				: null,
			`${p.fingerprint.slice(0, 16)}…`,
		]
			.filter(Boolean)
			.join(" · ")}
		actions={
			<>
				{p.source === "wan" ? (
					<Button
						size="sm"
						title={m.pairing_pending_wan_hint()}
						disabled={busy}
						onClick={() => onArmFor(p)}
					>
						<KeyRound className="size-4" />
						{m.pairing_pending_arm()}
					</Button>
				) : (
					<Button size="sm" disabled={busy} onClick={() => onApprove(p)}>
						{m.pairing_pending_approve()}
					</Button>
				)}
				<Button
					size="sm"
					variant="ghost"
					aria-label={m.pairing_pending_deny()}
					title={m.pairing_pending_deny()}
					disabled={busy}
					onClick={() => onDeny(p.id)}
				>
					<X className="size-4" />
				</Button>
			</>
		}
	/>
);
