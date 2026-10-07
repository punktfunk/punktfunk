import { useQueryClient } from "@tanstack/react-query";
import { KeyRound, Smartphone, Timer } from "lucide-react";
import { type FC, useEffect, useRef, useState } from "react";
import type { ArmNativePairing } from "@/api/gen/model/armNativePairing";
import type { NativePairStatus } from "@/api/gen/model/nativePairStatus";
import {
	getGetNativePairingQueryKey,
	getListNativeClientsQueryKey,
	useDisarmNativePairing,
	useGetNativePairing,
} from "@/api/gen/native/native";
import { useArmNativePairing } from "@/api/pairing";
import {
	PasswordConfirmField,
	type PasswordFailure,
	usePasswordFailure,
} from "@/components/password-confirm";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { fmtClockDuration } from "@/lib/format";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import {
	AccessControls,
	type AccessDraft,
	draftExpirySecs,
	draftUntilDisconnect,
	GRANT_ALL,
	PRESET_CONTROLLER,
} from "./access";

/**
 * The device an armed window is named for. A window carrying one is the only thing that admits
 * a knock from the internet, where a device's own name proves nothing.
 */
export interface BoundDevice {
	fingerprint: string;
	name: string;
}

/**
 * Container: native (punktfunk/1) pairing — arm a window, poll fast while armed
 * for the live countdown, slow otherwise.
 */
export const NativePairingSection: FC<{
	/** Name this window for one device — set by a WAN knock's "Arm PIN". */
	boundTo?: BoundDevice | null;
	onClearBound?: () => void;
	/** The approve dialog is open and owns the console password. */
	passwordLocked?: boolean;
}> = ({ boundTo = null, onClearBound, passwordLocked }) => {
	const qc = useQueryClient();
	const native = useGetNativePairing({
		query: { refetchInterval: (q) => (q.state.data?.armed ? 1_000 : 4_000) },
	});
	const arm = useArmNativePairing();
	const disarm = useDisarmNativePairing();

	// A device pairs via the QUIC PIN ceremony, NOT through approve/deny, so nothing else
	// invalidates the paired-devices list on the happy path — it would stay stale until remount.
	// The status poll's `paired_clients` count is the pairing signal: when it rises, refresh the
	// list so the newly paired device appears immediately.
	const pairedCount = native.data?.paired_clients;
	const prevPairedCount = useRef(pairedCount);
	useEffect(() => {
		if (
			prevPairedCount.current !== undefined &&
			pairedCount !== undefined &&
			pairedCount !== prevPairedCount.current
		) {
			qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
		}
		prevPairedCount.current = pairedCount;
	}, [pairedCount, qc]);

	const refresh = () =>
		qc.invalidateQueries({ queryKey: getGetNativePairingQueryKey() });
	// The armed PIN lives HERE, not in the polled status: the BFF strips it from that read
	// (server/routes/api/v1/native/pair.get.ts) so a session cookie alone cannot learn one, which
	// leaves the password-gated arm response as the console's only copy. A reload therefore loses
	// it and the card falls back to its arm form, where re-arming mints a fresh PIN.
	const [pin, setPin] = useState<string | null>(null);
	const refusal = usePasswordFailure();
	// `access` carries the window's device-access choice (grants + expiry) — NOT the window TTL;
	// whichever device completes this window's ceremony gets it.
	const onArm = (access: Partial<ArmNativePairing>, password: string) => {
		refusal.reset();
		arm.mutate(
			{
				ttl_secs: 120,
				...(boundTo ? { fingerprint: boundTo.fingerprint } : {}),
				...access,
				password,
			},
			{
				onSuccess: (status) => {
					setPin(status.pin ?? null);
					refresh();
				},
				onError: refusal.classify,
			},
		);
	};
	const onDisarm = () =>
		disarm.mutate(undefined, {
			onSuccess: () => {
				setPin(null);
				onClearBound?.();
				refresh();
			},
		});

	return (
		<NativePairingCard
			status={native}
			pin={pin}
			boundTo={boundTo}
			onClearBound={onClearBound}
			onArm={onArm}
			onDisarm={onDisarm}
			isArming={arm.isPending}
			failure={refusal.failure}
			isDisarming={disarm.isPending}
			passwordLocked={passwordLocked}
		/>
	);
};

/** Native (punktfunk/1) pairing: arm a window → DISPLAY the PIN the user enters on their device. */
export const NativePairingCard: FC<{
	status: Loadable<NativePairStatus>;
	/** The PIN of the window THIS console armed, or null — never from the polled status. */
	pin: string | null;
	/** The device this window is named for, or null for any device (trusted LAN only). The card
	 * never drops a binding on its own: the count it can see says a device paired, not WHICH,
	 * and unbinding on someone else's pairing would silently reset this form to Full · forever. */
	boundTo?: BoundDevice | null;
	onClearBound?: () => void;
	/** Arm, carrying the chosen device access (empty = today's full/permanent behavior) and the
	 * console password the BFF re-verifies. */
	onArm: (access: Partial<ArmNativePairing>, password: string) => void;
	onDisarm: () => void;
	isArming: boolean;
	/** Why the BFF refused the last arm's password, if it did. */
	failure: PasswordFailure;
	isDisarming: boolean;
	/** The approve dialog is open and owns the console password. */
	passwordLocked?: boolean;
}> = ({
	status,
	pin,
	boundTo = null,
	onClearBound,
	onArm,
	onDisarm,
	isArming,
	failure,
	isDisarming,
	passwordLocked,
}) => {
	const d = status.data;
	// What the pairing device will be allowed to do — same defaults as the approve dialog (D1):
	// Full · Forever, because arming for your OWN next device is the common case.
	const [draft, setDraft] = useState<AccessDraft>({
		grants: GRANT_ALL,
		expiry: "forever",
		customHours: 4,
	});
	// A window named for a device is being armed for someone else's — a knock from the internet
	// is the only way to get here today — so it starts at Controller · this session, which ends
	// when they do. Keyed on the fingerprint, not the object, so a re-render never wipes the
	// operator's edits mid-form.
	const boundFp = boundTo?.fingerprint;
	useEffect(() => {
		setDraft(
			boundFp
				? { grants: PRESET_CONTROLLER, expiry: "session", customHours: 4 }
				: { grants: GRANT_ALL, expiry: "forever", customHours: 4 },
		);
	}, [boundFp]);
	const [password, setPassword] = useState("");
	const arm = () => {
		const secs = draftExpirySecs(draft);
		const access: Partial<ArmNativePairing> = {};
		// The untouched Full · Forever default is omitted entirely: a re-pairing device then keeps
		// the access it already has (the API's omitted-fields contract), and an older host that
		// predates the fields sees exactly yesterday's request.
		const session = draftUntilDisconnect(draft);
		if (draft.grants !== GRANT_ALL || secs != null || session) {
			access.grants = draft.grants;
			if (secs != null) access.expires_in_secs = secs;
			access.until_disconnect = session;
		}
		onArm(access, password);
	};
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<Smartphone className="size-4" />
					{m.pairing_native_title()}
				</CardTitle>
			</CardHeader>
			<CardContent className="@container space-y-4">
				{/* The card is in the page's cascade from the first frame; only its body waits. */}
				<QueryState
					isLoading={status.isLoading}
					error={status.error}
					refetch={status.refetch}
				>
					{!d?.enabled ? (
						<p className="text-sm text-muted-foreground">
							{m.pairing_native_disabled()}
						</p>
					) : d.armed && pin ? (
						<div className="mx-auto w-full max-w-md space-y-3">
							<p className="text-sm">
								{boundTo
									? m.pairing_native_bound_for({ name: boundTo.name })
									: m.pairing_native_enter()}
							</p>
							<div className="rounded-lg border bg-muted/40 py-5 text-center font-mono text-4xl font-semibold tracking-[0.3em]">
								{pin}
							</div>
							{d.expires_in_secs != null && (
								<p className="flex items-center justify-center gap-1.5 text-sm text-muted-foreground">
									<Timer className="size-4" />
									{m.pairing_native_expires()}{" "}
									{fmtClockDuration(d.expires_in_secs)}
								</p>
							)}
							<Button
								variant="outline"
								className="w-full"
								disabled={isDisarming}
								onClick={onDisarm}
							>
								{m.pairing_native_cancel()}
							</Button>
						</div>
					) : (
						<>
							<p className="max-w-prose text-sm text-muted-foreground">
								{m.pairing_native_desc()}
							</p>
							{boundTo && (
								<div className="flex items-start justify-between gap-2 rounded-md border border-dashed p-3">
									<div>
										<p className="text-sm font-medium">
											{m.pairing_native_bound_for({ name: boundTo.name })}
										</p>
										<p className="text-xs text-muted-foreground">
											{m.pairing_native_bound_hint()}
										</p>
									</div>
									{onClearBound && (
										<Button
											variant="ghost"
											size="sm"
											className="shrink-0"
											onClick={onClearBound}
										>
											{m.pairing_native_bound_clear()}
										</Button>
									)}
								</div>
							)}
							{/* The window's device-access choice — whichever device completes the
							    ceremony gets exactly this (design §6.2). */}
							<AccessControls
								value={draft}
								onChange={setDraft}
								idPrefix="arm"
							/>
							{/* Whoever completes this window's ceremony gets keyboard and mouse on this
							    machine, so arming re-confirms the console password — the BFF verifies and
							    strips it (util/confirm.ts). */}
							<div className="grid gap-4 @xl:grid-cols-2">
								<PasswordConfirmField
									id="arm-password"
									value={password}
									onChange={setPassword}
									failure={failure}
									help={m.pairing_password_help()}
									disabled={passwordLocked}
								/>
							</div>
							<Button
								disabled={isArming || password.length === 0}
								onClick={arm}
							>
								<KeyRound className="size-4" />
								{m.pairing_native_arm()}
							</Button>
						</>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};
