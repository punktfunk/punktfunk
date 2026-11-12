import { useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { Pencil, Unlink } from "lucide-react";
import { motion } from "motion/react";
import { type FC, type ReactNode, useState } from "react";
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
import { PasswordConfirmDialog } from "@/components/password-confirm";
import { ROW } from "@/components/stagger";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { widensAccess } from "@/lib/password-gates";
import { m } from "@/paraglide/messages";
import {
	ClientPolicySheet,
	overlaySummary,
} from "@/sections/Displays/ClientPolicySheet";
import { AccessChip, useNowUnix } from "./access";
import { EditAccessSheet, type EditAccessTarget } from "./EditAccessSheet";
import {
	deviceName,
	hasAccess,
	type PairedProtocol,
	type PairedRow,
	pairedListView,
	pendingFingerprint,
	planesWithRows,
	renameBody,
	rowControls,
	storedAccess,
} from "./pairedRows";

export type { PairedProtocol, PairedRow };

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

	const list = pairedListView(native, moonlight);
	const { rows } = list;

	const onUnpair = async (
		protocol: PairedProtocol,
		fingerprint: string,
		name: string,
	) => {
		const ok = await confirm({
			title: m.pairing_native_unpair_confirm(),
			description: m.pairing_native_unpair_body(),
			confirmLabel: m.action_unpair(),
			destructive: true,
		});
		if (!ok) return;
		const onError = () => toast.error(m.pairing_native_unpair_failed({ name }));
		if (protocol === "native") {
			unpairNative.mutate(
				{ fingerprint },
				{
					onSuccess: () =>
						qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() }),
					onError,
				},
			);
		} else {
			unpairMoonlight.mutate(
				{ fingerprint },
				{
					onSuccess: () =>
						qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() }),
					onError,
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
		const data = renameBody(next);
		if (!data) return;
		renameMoonlight.mutate(
			{ fingerprint: row.fingerprint, data },
			{
				onSuccess: () =>
					qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() }),
				onError: () => toast.error(m.clients_rename_failed()),
			},
		);
	};

	// A change that widens a device's access waits for the console password; the sheet stays open.
	const [widening, setWidening] = useState<{
		fingerprint: string;
		body: UpdateNativeAccess;
	} | null>(null);
	const savedAccess = () => {
		setEditing(null);
		qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
	};
	const onSaveAccess = (fingerprint: string, body: UpdateNativeAccess) => {
		const stored = storedAccess(rows, fingerprint);
		if (widensAccess(stored, body, Math.floor(Date.now() / 1000))) {
			setWidening({ fingerprint, body });
			return;
		}
		patchAccess.mutate(
			{ fingerprint, data: body },
			{
				onSuccess: savedAccess,
				onError: () => toast.error(m.access_edit_failed()),
			},
		);
	};
	// "Expire now" = the same partial PATCH with a zero relative expiry — cuts live sessions
	// from this device with the typed AccessExpired close; the row stays listed as "Expired".
	const onExpireNow = (fingerprint: string) =>
		onSaveAccess(fingerprint, { expires_in_secs: 0 });
	const onRemoveFromSheet = (fingerprint: string) => {
		setEditing(null);
		void onUnpair(
			"native",
			fingerprint,
			deviceName({ name: editing?.name ?? "", fingerprint }),
		);
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
		const planes = planesWithRows(rows);
		const calls: Promise<unknown>[] = [];
		if (planes.native) calls.push(unpairAllNative.mutateAsync());
		if (planes.moonlight) calls.push(unpairAllMoonlight.mutateAsync());
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
	const unpairing = pendingFingerprint(unpairNative, unpairMoonlight);

	// Derived, not state: the two bulk calls are launched together and awaited together, so their
	// pending flags cover the whole run without a gap in the middle to flicker through.
	const isUnpairingAll =
		unpairAllNative.isPending || unpairAllMoonlight.isPending;

	const busy = (r: PairedRow) => isUnpairingAll || unpairing === r.fingerprint;
	const editAccess = (r: PairedRow) =>
		setEditing({
			fingerprint: r.fingerprint,
			name: r.name,
			grants: r.grants,
			expiresUnix: r.expiresUnix,
			untilDisconnect: r.untilDisconnect,
		});
	return {
		rows,
		list,
		refetch: () => {
			native.refetch();
			moonlight.refetch();
		},
		onUnpairAll,
		isUnpairingAll,
		/** What one row needs. */
		rowProps: (r: PairedRow) => {
			const can = rowControls(r, perDevice);
			return {
				row: r,
				nowUnix,
				display: can.display
					? overlaySummary(displaySettings.data?.clients?.[r.fingerprint])
					: undefined,
				busy: busy(r),
				onEditAccess: can.access ? () => editAccess(r) : undefined,
				onDisplaySettings: can.display ? () => setDisplayTarget(r) : undefined,
				onRename: can.rename ? () => onRename(r) : undefined,
				onUnpair: () => onUnpair(r.protocol, r.fingerprint, deviceName(r)),
			};
		},
		dialogs: (
			<>
				{displayTarget && (
					<ClientPolicySheet
						open
						onOpenChange={(open) => !open && setDisplayTarget(null)}
						fingerprint={displayTarget.fingerprint}
						deviceName={deviceName(displayTarget)}
						grants={
							hasAccess(displayTarget)
								? (displayTarget.grants ?? null)
								: undefined
						}
						onEditAccess={
							hasAccess(displayTarget)
								? () => {
										setDisplayTarget(null);
										editAccess(displayTarget);
									}
								: undefined
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
				<PasswordConfirmDialog
					open={widening !== null}
					id="access-widen-password"
					title={m.access_edit_title()}
					body={m.access_widen_confirm_body()}
					submitLabel={m.common_save()}
					failedText={m.access_edit_failed()}
					onSubmit={async (password) => {
						if (!widening) return;
						// The console's server reads the password from the body and strips it.
						const data = { ...widening.body, password } as UpdateNativeAccess;
						await patchAccess.mutateAsync({
							fingerprint: widening.fingerprint,
							data,
						});
						savedAccess();
					}}
					onClose={() => setWidening(null)}
				/>
			</>
		),
	};
}
/**
 * One paired device. Its protocol and fingerprint ride the details line. What can be changed is
 * what you click, each with a pencil: the name (a Moonlight device's, whose certificate carries
 * none), the access chip, the display line. Unpair is the row's one button. A Moonlight device
 * has full control and says so: the GameStream plane is not governed by grants.
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
	<motion.li
		variants={ROW}
		className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-1.5 py-3 md:grid-cols-[minmax(0,1.3fr)_minmax(0,1fr)_minmax(0,1.3fr)_minmax(0,0.6fr)_auto]"
	>
		<div className="order-1 min-w-0">
			<Setting label={m.action_rename()} disabled={busy} onClick={onRename}>
				<span className="truncate font-medium">{r.name || "—"}</span>
			</Setting>
			<div className="truncate text-xs text-muted-foreground">
				{r.protocol === "native"
					? m.pairing_protocol_native()
					: m.pairing_protocol_moonlight()}{" "}
				· <span className="font-mono">{r.fingerprint.slice(0, 16)}…</span>
			</div>
		</div>
		<div className="order-2 justify-self-end md:order-5">
			<Button
				variant="ghost"
				size="icon"
				aria-label={m.action_unpair()}
				title={m.action_unpair()}
				disabled={busy}
				className="text-muted-foreground hover:text-destructive"
				onClick={onUnpair}
			>
				<Unlink className="size-4" />
			</Button>
		</div>
		{/* On a phone the heading row is hidden: each value names its own column. */}
		<div className="order-3 col-span-1 flex flex-wrap items-end gap-x-6 gap-y-2 md:contents">
			<div className="flex flex-col items-start gap-1 md:order-2 md:block">
				<CellLabel>{m.devices_col_access()}</CellLabel>
				{r.protocol === "moonlight" ? (
					<Badge
						variant="outline"
						className="whitespace-nowrap text-muted-foreground"
					>
						{m.access_ungoverned()}
					</Badge>
				) : hasAccess(r) ? (
					<Setting
						label={m.access_edit_title()}
						disabled={busy}
						onClick={onEditAccess}
					>
						<AccessChip
							grants={r.grants}
							expiresUnix={r.expiresUnix}
							untilDisconnect={r.untilDisconnect}
							nowUnix={nowUnix}
						/>
					</Setting>
				) : (
					// A host older than per-client access reports nothing; say nothing.
					<span className="text-muted-foreground">—</span>
				)}
			</div>
			<div className="flex min-w-0 flex-col items-start gap-1 md:order-3 md:block">
				{(onDisplaySettings || display) && (
					<CellLabel>{m.devices_col_display()}</CellLabel>
				)}
				{onDisplaySettings ? (
					<Setting
						label={m.display_device_settings()}
						disabled={busy}
						onClick={onDisplaySettings}
					>
						<Badge variant="secondary" className="max-w-full">
							<span className="truncate">
								{display ?? m.display_device_settings()}
							</span>
						</Badge>
					</Setting>
				) : (
					display && (
						<Badge variant="secondary" className="max-w-full">
							<span className="truncate">{display}</span>
						</Badge>
					)
				)}
			</div>
			<div className="md:order-4">
				{streaming && <Badge variant="success">{m.devices_streaming()}</Badge>}
			</div>
		</div>
	</motion.li>
);

/** A value's column name, over it on a phone; the heading row names it from `md` up. */
const CellLabel: FC<{ children: ReactNode }> = ({ children }) => (
	<span className="text-[11px] text-muted-foreground md:hidden">
		{children}
	</span>
);

/** A value that opens its own editor on a click; a pencil says so. */
const Setting: FC<{
	label: string;
	disabled?: boolean;
	onClick?: () => void;
	children: ReactNode;
}> = ({ label, disabled, onClick, children }) =>
	onClick ? (
		<button
			type="button"
			title={label}
			aria-label={label}
			disabled={disabled}
			onClick={onClick}
			className="group/setting inline-flex max-w-full items-center gap-1.5 rounded-full outline-none ring-offset-2 ring-offset-background focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50"
		>
			{children}
			<Pencil className="size-3 shrink-0 text-muted-foreground opacity-60 transition-opacity group-hover/setting:opacity-100 group-focus-visible/setting:opacity-100" />
		</button>
	) : (
		children
	);
