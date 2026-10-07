import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { type FC, useState } from "react";
import {
	getListPairedClientsQueryKey,
	useListPairedClients,
	useRenameClient,
	useUnpairAllClients,
	useUnpairClient,
} from "@/api/gen/clients/clients";
import { useGetDisplaySettings } from "@/api/gen/display/display";
import type { UpdateNativeAccess } from "@/api/gen/model/updateNativeAccess";
import {
	getListNativeClientsQueryKey,
	useListNativeClients,
	useUnpairAllNativeClients,
	useUnpairNativeClient,
	useUpdateNativeClientAccess,
} from "@/api/gen/native/native";
import { useDialogs } from "@/components/dialogs";
import { Badge } from "@/components/ui/badge";
import { MenuItem, MenuSeparator, RowMenu } from "@/components/ui/menu";
import { m } from "@/paraglide/messages";
import {
	ClientPolicySheet,
	overlaySummary,
} from "@/sections/Displays/ClientPolicySheet";
import { AccessChip, useNowUnix } from "./access";
import { EditAccessSheet, type EditAccessTarget } from "./EditAccessSheet";

/** The two pairing protocols a device can be paired over. */
export type PairedProtocol = "native" | "moonlight";

/** One paired device, normalized across the native + Moonlight lists. */
export interface PairedRow {
	protocol: PairedProtocol;
	fingerprint: string;
	/**
	 * What to show in the Name column. Native devices carry a name from pairing; a Moonlight client
	 * shows its operator-given label if it has one, and otherwise falls back to its cert subject —
	 * which is the same fixed string for every Moonlight client alive, hence [`label`].
	 */
	name: string;
	/**
	 * The operator-assigned label, Moonlight rows only — `null` when the device has never been
	 * named. Distinct from `name` because the rename dialog must open on the label alone: seeding
	 * it with the `CN=…` fallback would make every rename start by deleting boilerplate.
	 */
	label?: string | null;
	/**
	 * Access fields — native rows only, and only from hosts that have them (the console pairs
	 * against older hosts: all four stay `undefined` then, and the Access column shows "—").
	 * `access_level` is the presence sentinel — a current host always derives it for a
	 * NativeClient, so its absence means the host predates per-client access.
	 */
	accessLevel?: string | null;
	/** Grant bitmask; `null` = a pre-grants record = full control. */
	grants?: number | null;
	/** Absolute expiry (unix secs); `null` = permanent. "Expired" is our arithmetic. */
	expiresUnix?: number | null;
	/** The record goes when this device's last session does. Independent of `expiresUnix`. */
	untilDisconnect?: boolean | null;
}

/** Whether the host reported access fields for this row (⇒ the chip and editor exist). */
const hasAccess = (r: PairedRow): boolean =>
	r.protocol === "native" &&
	(r.accessLevel != null || r.grants != null || r.expiresUnix != null);

/**
 * ALL paired devices in one list: the native (punktfunk/1) clients and the GameStream/Moonlight
 * clients — two host endpoints — merged and tagged by protocol, each unpair routed back to its
 * own endpoint. `dialogs` are the access, display and rename sheets, mounted once by the page.
 */
export function usePairedDevices() {
	const qc = useQueryClient();
	const { confirm, promptText } = useDialogs();
	const native = useListNativeClients();
	const moonlight = useListPairedClients();
	const unpairNative = useUnpairNativeClient();
	const unpairMoonlight = useUnpairClient();
	const unpairAllNative = useUnpairAllNativeClients();
	const unpairAllMoonlight = useUnpairAllClients();
	const renameMoonlight = useRenameClient();
	const patchAccess = useUpdateNativeClientAccess();
	const displaySettings = useGetDisplaySettings();
	// Whether this host acts on ANY per-device field: an older host has no `/display/clients`, and
	// a sheet with no questions in it is a dead control.
	const perDevice = (displaySettings.data?.client_enforced ?? []).length > 0;
	const [displayTarget, setDisplayTarget] = useState<PairedRow | null>(null);
	// One clock for every countdown in the list AND the sheet — recomputed client-side from
	// `expires_unix`, so the tick never refetches anything.
	const nowUnix = useNowUnix();
	// The row whose access is being edited — a snapshot, so a background refetch can't yank the
	// form out from under the operator.
	const [editing, setEditing] = useState<EditAccessTarget | null>(null);

	const rows: PairedRow[] = [
		...(native.data ?? []).map(
			(c): PairedRow => ({
				protocol: "native",
				fingerprint: c.fingerprint,
				name: c.name,
				accessLevel: c.access_level,
				grants: c.grants,
				expiresUnix: c.expires_unix,
				untilDisconnect: c.until_disconnect,
			}),
		),
		...(moonlight.data ?? []).map(
			(c): PairedRow => ({
				protocol: "moonlight",
				fingerprint: c.fingerprint,
				name: c.label ?? c.subject ?? "",
				label: c.label,
			}),
		),
	];

	const onUnpair = async (protocol: PairedProtocol, fingerprint: string) => {
		const ok = await confirm({
			title: m.pairing_native_unpair_confirm(),
			description: m.pairing_native_unpair_body(),
			confirmLabel: m.action_unpair(),
			destructive: true,
		});
		if (!ok) return;
		if (protocol === "native") {
			unpairNative.mutate(
				{ fingerprint },
				{
					onSuccess: () =>
						qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() }),
				},
			);
		} else {
			unpairMoonlight.mutate(
				{ fingerprint },
				{
					onSuccess: () =>
						qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() }),
				},
			);
		}
	};

	/**
	 * Name a Moonlight device. Every Moonlight client presents the identical certificate subject,
	 * so without this the list is a column of `CN=NVIDIA GameStream Client` rows and the only way
	 * to tell a phone from a TV — or to know which one you are about to unpair — is the
	 * fingerprint. Submitting an empty field clears the name (the host reads that as "unnamed"),
	 * which is why cancel (`null`) and empty are handled differently here.
	 */
	const onRename = async (row: PairedRow) => {
		const next = await promptText({
			title: m.clients_rename_title(),
			description: m.clients_rename_body(),
			label: m.clients_rename_label(),
			defaultValue: row.label ?? "",
			confirmLabel: m.action_rename(),
		});
		if (next === null) return;
		renameMoonlight.mutate(
			{ fingerprint: row.fingerprint, data: { label: next.trim() || null } },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() }),
				onError: () => toast.error(m.clients_rename_failed()),
			},
		);
	};

	const savedAccess = () => {
		setEditing(null);
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
	};
	const onSaveAccess = (fingerprint: string, body: UpdateNativeAccess) =>
		patchAccess.mutate(
			{ fingerprint, data: body },
			{
				onSuccess: savedAccess,
				onError: () => toast.error(m.access_edit_failed()),
			},
		);
	// "Expire now" = the same partial PATCH with a zero relative expiry — cuts live sessions
	// from this device with the typed AccessExpired close; the row stays listed as "Expired".
	const onExpireNow = (fingerprint: string) =>
		onSaveAccess(fingerprint, { expires_in_secs: 0 });
	const onRemoveFromSheet = (fingerprint: string) => {
		setEditing(null);
		void onUnpair("native", fingerprint);
	};

	/**
	 * Unpair EVERY device, in one confirmation.
	 *
	 * Two calls, not one per device: each plane owns a separate trust store behind its own
	 * collection DELETE, and each of those empties its store in a single persisted write host-side.
	 * Only the planes actually holding a row are called — the native endpoint answers 503 on a host
	 * built without it, which would otherwise report a failure for devices that were never there.
	 */
	const onUnpairAll = async () => {
		const ok = await confirm({
			title: m.pairing_native_unpair_all_confirm({ count: rows.length }),
			description: m.pairing_native_unpair_all_body(),
			confirmLabel: m.action_unpair_all(),
			destructive: true,
		});
		if (!ok) return;
		const calls: Promise<unknown>[] = [];
		if (rows.some((r) => r.protocol === "native")) {
			calls.push(unpairAllNative.mutateAsync());
		}
		if (rows.some((r) => r.protocol === "moonlight")) {
			calls.push(unpairAllMoonlight.mutateAsync());
		}
		// allSettled, not all: the two planes are independent, so one failing must neither cancel
		// the other nor throw past this handler.
		const settled = await Promise.allSettled(calls);
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
		qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() });
		if (settled.some((r) => r.status === "rejected")) {
			toast.error(m.pairing_native_unpair_all_failed());
		}
	};

	// The fingerprint of the row whose unpair is in flight (if any) — so only THAT row's button
	// disables, not every row's.
	const pendingFingerprint =
		(unpairNative.isPending
			? unpairNative.variables?.fingerprint
			: undefined) ??
		(unpairMoonlight.isPending
			? unpairMoonlight.variables?.fingerprint
			: undefined) ??
		null;

	// Derived, not state: the two bulk calls are launched together and awaited together, so their
	// pending flags cover the whole run without a gap in the middle to flicker through.
	const isUnpairingAll =
		unpairAllNative.isPending || unpairAllMoonlight.isPending;

	const busy = (r: PairedRow) =>
		isUnpairingAll || pendingFingerprint === r.fingerprint;
	return {
		rows,
		isLoading: native.isLoading || moonlight.isLoading,
		error: native.error ?? moonlight.error,
		refetch: () => {
			native.refetch();
			moonlight.refetch();
		},
		onUnpairAll,
		isUnpairingAll,
		/** What one row needs. */
		rowProps: (r: PairedRow) => ({
			row: r,
			nowUnix,
			display:
				perDevice && r.protocol === "native"
					? overlaySummary(displaySettings.data?.clients?.[r.fingerprint])
					: undefined,
			busy: busy(r),
			onEditAccess: hasAccess(r)
				? () =>
						setEditing({
							fingerprint: r.fingerprint,
							name: r.name,
							grants: r.grants,
							expiresUnix: r.expiresUnix,
							untilDisconnect: r.untilDisconnect,
						})
				: undefined,
			onDisplaySettings:
				perDevice && r.protocol === "native"
					? () => setDisplayTarget(r)
					: undefined,
			onRename: r.protocol === "moonlight" ? () => onRename(r) : undefined,
			onUnpair: () => onUnpair(r.protocol, r.fingerprint),
		}),
		dialogs: (
			<>
				{displayTarget && (
					<ClientPolicySheet
						open
						onOpenChange={(open) => !open && setDisplayTarget(null)}
						fingerprint={displayTarget.fingerprint}
						deviceName={
							displayTarget.name || displayTarget.fingerprint.slice(0, 12)
						}
					/>
				)}
				<EditAccessSheet
					target={editing}
					nowUnix={nowUnix}
					onCancel={() => setEditing(null)}
					onSave={onSaveAccess}
					onExpireNow={onExpireNow}
					onRemove={onRemoveFromSheet}
					isPending={patchAccess.isPending}
				/>
			</>
		),
	};
}
/**
 * One paired device. Its protocol and fingerprint ride the details line; Access, Display and the
 * session it is streaming sit beside it; Access · Display · Rename · Unpair live in ⋯ (R6).
 *
 * A Moonlight device has full control and says so: the GameStream plane is not governed by
 * grants. Rename is a Moonlight verb — its certificate carries nothing that names the device.
 */
export const PairedRowView: FC<{
	row: PairedRow;
	nowUnix: number;
	/** The device's display overlay in words; absent where the host has no per-device policy. */
	display?: string;
	streaming?: boolean;
	busy: boolean;
	onEditAccess?: () => void;
	onDisplaySettings?: () => void;
	onRename?: () => void;
	onUnpair: () => void;
}> = ({
	row: r,
	nowUnix,
	display,
	streaming,
	busy,
	onEditAccess,
	onDisplaySettings,
	onRename,
	onUnpair,
}) => (
	<li className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-1.5 py-3 md:grid-cols-[minmax(0,1.3fr)_minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,0.6fr)_auto]">
		<div className="order-1 min-w-0">
			<div className="truncate font-medium">{r.name || "—"}</div>
			<div className="truncate text-xs text-muted-foreground">
				{r.protocol === "native"
					? m.pairing_protocol_native()
					: m.pairing_protocol_moonlight()}{" "}
				· <span className="font-mono">{r.fingerprint.slice(0, 16)}…</span>
			</div>
		</div>
		<div className="order-2 justify-self-end md:order-5">
			<RowMenu label={m.common_more_actions()} disabled={busy}>
				{onEditAccess && (
					<MenuItem onSelect={onEditAccess}>{m.access_edit_title()}</MenuItem>
				)}
				{onDisplaySettings && (
					<MenuItem onSelect={onDisplaySettings}>
						{m.display_device_settings()}
					</MenuItem>
				)}
				{onRename && (
					<MenuItem onSelect={onRename}>{m.action_rename()}</MenuItem>
				)}
				{(onEditAccess || onDisplaySettings || onRename) && <MenuSeparator />}
				<MenuItem destructive onSelect={onUnpair}>
					{m.action_unpair()}
				</MenuItem>
			</RowMenu>
		</div>
		<div className="order-3 col-span-1 flex flex-wrap items-center gap-2 md:contents">
			<div className="md:order-2">
				{r.protocol === "moonlight" ? (
					<Badge
						variant="outline"
						className="whitespace-nowrap text-muted-foreground"
					>
						{m.access_ungoverned()}
					</Badge>
				) : hasAccess(r) ? (
					<AccessChip
						grants={r.grants}
						expiresUnix={r.expiresUnix}
						untilDisconnect={r.untilDisconnect}
						nowUnix={nowUnix}
					/>
				) : (
					// A host older than per-client access reports nothing; say nothing.
					<span className="text-muted-foreground">—</span>
				)}
			</div>
			<div className="truncate text-sm text-muted-foreground md:order-3">
				{display}
			</div>
			<div className="md:order-4">
				{streaming && <Badge variant="success">{m.devices_streaming()}</Badge>}
			</div>
		</div>
	</li>
);
