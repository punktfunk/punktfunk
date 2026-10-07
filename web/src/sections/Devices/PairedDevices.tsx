import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import {
	MonitorPlay,
	MonitorSmartphone,
	Pencil,
	SlidersHorizontal,
	Trash2,
} from "lucide-react";
import { type FC, useState } from "react";
import {
	getListPairedClientsQueryKey,
	useListPairedClients,
	useRenameClient,
	useUnpairAllClients,
	useUnpairClient,
} from "@/api/gen/clients/clients";
import { useGetDisplaySettings } from "@/api/gen/display/display";
import type { DisplaySettingsState } from "@/api/gen/model";
import type { UpdateNativeAccess } from "@/api/gen/model/updateNativeAccess";
import {
	getListNativeClientsQueryKey,
	useListNativeClients,
	useUnpairAllNativeClients,
	useUnpairNativeClient,
	useUpdateNativeClientAccess,
} from "@/api/gen/native/native";
import { useDialogs } from "@/components/dialogs";
import { QueryState } from "@/components/query-state";
import { ROW, ROW_GAP, staggerProps } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
	MotionTableBody,
	MotionTableRow,
	RowDetails,
	Table,
	TableCell,
	TableHead,
	TableHeader,
	TableRow,
	WIDE,
} from "@/components/ui/table";
import { cn } from "@/lib/utils";
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
 * Container: ALL paired devices in one list. Merges the native (punktfunk/1) clients and the
 * GameStream/Moonlight clients — two separate host endpoints — into a single table tagged by
 * protocol, and routes each unpair back to the right endpoint.
 */
export const PairedDevicesSection: FC = () => {
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
	const [displayTarget, setDisplayTarget] = useState<PairedRow | null>(null);
	// One clock for every countdown in the card AND the sheet — recomputed client-side from
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

	return (
		<>
			<PairedDevices
				rows={rows}
				isLoading={native.isLoading || moonlight.isLoading}
				error={native.error ?? moonlight.error}
				refetch={() => {
					native.refetch();
					moonlight.refetch();
				}}
				nowUnix={nowUnix}
				onEditAccess={(r) =>
					setEditing({
						fingerprint: r.fingerprint,
						name: r.name,
						grants: r.grants,
						expiresUnix: r.expiresUnix,
						untilDisconnect: r.untilDisconnect,
					})
				}
				onRename={onRename}
				onUnpair={onUnpair}
				onUnpairAll={onUnpairAll}
				onDisplaySettings={setDisplayTarget}
				settings={displaySettings.data}
				perDevice={(displaySettings.data?.client_enforced ?? []).length > 0}
				pendingFingerprint={pendingFingerprint}
				isUnpairingAll={isUnpairingAll}
			/>
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
	);
};

/** All paired devices (native + Moonlight) in one table, differentiated by a protocol badge. */
export const PairedDevices: FC<{
	rows: PairedRow[];
	isLoading: boolean;
	error: unknown;
	refetch: () => void;
	/** Wall clock (unix secs) for the countdown chips — ONE ticking value for the whole table. */
	nowUnix: number;
	/** Open the access editor for a native row (only offered where `hasAccess`). */
	onEditAccess: (row: PairedRow) => void;
	/** Open this device's display settings. Native only — the overlay is keyed by the
	 * pairing fingerprint the native plane presents, which a GameStream cert is not. */
	onDisplaySettings: (row: PairedRow) => void;
	/** Display policy + stored overlays, so the Display column can be rendered without a
	 * second fetch per row. */
	settings?: DisplaySettingsState;
	/** Whether this host acts on ANY per-device field. False hides the column and its
	 * control — an older host has no `/display/clients` at all, and a sheet with no
	 * questions in it is the dead control D1 exists to prevent. */
	perDevice: boolean;
	/**
	 * Name a Moonlight row. Offered only on those: a native device already carries the name it gave
	 * at pairing, while a Moonlight certificate carries nothing that identifies the device at all.
	 */
	onRename: (row: PairedRow) => void;
	onUnpair: (protocol: PairedProtocol, fingerprint: string) => void;
	/** Unpair every row, behind one confirmation. */
	onUnpairAll: () => void;
	/** Fingerprint of the row whose unpair is in flight, or null — only that row disables. */
	pendingFingerprint: string | null;
	/** A bulk unpair is walking the list — every control in the card disables until it finishes. */
	isUnpairingAll: boolean;
}> = ({
	rows,
	isLoading,
	error,
	refetch,
	nowUnix,
	onEditAccess,
	onRename,
	onUnpair,
	onUnpairAll,
	onDisplaySettings,
	settings,
	perDevice,
	pendingFingerprint,
	isUnpairingAll,
}) => (
	<Card>
		{/* flex-row: CardHeader stacks by default, and this one carries a trailing action. */}
		<CardHeader className="flex-row items-center justify-between gap-4 space-y-0">
			<CardTitle>
				<h2 className="flex items-center gap-2">
					<MonitorSmartphone className="size-4" />
					{m.pairing_native_devices()}
				</h2>
			</CardTitle>
			{/* Nothing to unpair in bulk when the list is empty (or still loading) — an enabled
			    button there would open a confirmation reading "Unpair all 0 devices?". */}
			{rows.length > 0 && (
				<Button
					variant="outline"
					size="sm"
					disabled={isUnpairingAll}
					onClick={onUnpairAll}
				>
					<Trash2 className="size-4 text-destructive" />
					{m.action_unpair_all()}
				</Button>
			)}
		</CardHeader>

		<CardContent>
			<QueryState isLoading={isLoading} error={error} refetch={refetch}>
				{rows.length === 0 ? (
					m.pairing_native_empty()
				) : (
					<Table>
						<TableHeader>
							<TableRow>
								<TableHead>{m.clients_name()}</TableHead>
								<TableHead className={WIDE}>{m.pairing_protocol()}</TableHead>
								<TableHead>{m.pairing_access()}</TableHead>
								{perDevice && (
									<TableHead className={WIDE}>
										{m.display_device_column()}
									</TableHead>
								)}
								<TableHead className={WIDE}>
									{m.clients_fingerprint()}
								</TableHead>
								<TableHead className="w-20" />
							</TableRow>
						</TableHeader>
						<MotionTableBody {...staggerProps(ROW_GAP)}>
							{rows.map((r) => (
								<MotionTableRow
									key={`${r.protocol}:${r.fingerprint}`}
									variants={ROW}
								>
									<TableCell className="font-medium">
										{r.name || "—"}
										<RowDetails>
											{r.protocol === "native"
												? m.pairing_protocol_native()
												: m.pairing_protocol_moonlight()}{" "}
											·{" "}
											<span className="font-mono">
												{r.fingerprint.slice(0, 16)}…
											</span>
										</RowDetails>
									</TableCell>
									<TableCell className={WIDE}>
										<Badge
											variant={
												r.protocol === "native" ? "default" : "secondary"
											}
										>
											{r.protocol === "native"
												? m.pairing_protocol_native()
												: m.pairing_protocol_moonlight()}
										</Badge>
									</TableCell>
									<TableCell>
										{r.protocol === "moonlight" ? (
											// Honest: the GameStream plane isn't governed by grants
											// (yet) — a Moonlight device has full control, and this
											// chip says so instead of offering a fake editor.
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
											// A host older than per-client access reports nothing —
											// say nothing rather than guessing.
											<span className="text-muted-foreground">—</span>
										)}
									</TableCell>
									{perDevice && (
										<TableCell
											className={cn(
												WIDE,
												"max-w-[22rem] text-sm text-muted-foreground",
											)}
										>
											{/* Only a native device has an overlay: the map is keyed by
											    the pairing fingerprint the native plane presents, and a
											    GameStream client's cert is not that. */}
											{r.protocol === "native" ? (
												<span>
													{overlaySummary(settings?.clients?.[r.fingerprint])}
												</span>
											) : (
												<span>—</span>
											)}
										</TableCell>
									)}
									<TableCell
										className={cn(
											WIDE,
											"font-mono text-xs text-muted-foreground",
										)}
									>
										{r.fingerprint.slice(0, 16)}…
									</TableCell>
									<TableCell>
										<div className="flex justify-end">
											{r.protocol === "moonlight" && (
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.action_rename()}
													disabled={
														isUnpairingAll ||
														pendingFingerprint === r.fingerprint
													}
													onClick={() => onRename(r)}
												>
													<Pencil className="size-4" />
												</Button>
											)}
											{perDevice && r.protocol === "native" && (
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.display_device_settings()}
													title={m.display_device_settings()}
													disabled={
														isUnpairingAll ||
														pendingFingerprint === r.fingerprint
													}
													onClick={() => onDisplaySettings(r)}
												>
													<MonitorPlay className="size-4" />
												</Button>
											)}
											{hasAccess(r) && (
												<Button
													variant="ghost"
													size="icon"
													aria-label={m.access_edit_title()}
													disabled={
														isUnpairingAll ||
														pendingFingerprint === r.fingerprint
													}
													onClick={() => onEditAccess(r)}
												>
													<SlidersHorizontal className="size-4" />
												</Button>
											)}
											<Button
												variant="ghost"
												size="icon"
												aria-label={m.action_unpair()}
												disabled={
													isUnpairingAll || pendingFingerprint === r.fingerprint
												}
												onClick={() => onUnpair(r.protocol, r.fingerprint)}
											>
												<Trash2 className="size-4 text-destructive" />
											</Button>
										</div>
									</TableCell>
								</MotionTableRow>
							))}
						</MotionTableBody>
					</Table>
				)}
			</QueryState>
		</CardContent>
	</Card>
);
