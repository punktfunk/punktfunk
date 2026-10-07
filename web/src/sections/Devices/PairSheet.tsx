import { useQueryClient } from "@tanstack/react-query";
import { ArrowLeft, KeyRound, Timer } from "lucide-react";
import { type FC, type ReactNode, useEffect, useRef, useState } from "react";
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
import {
	Dialog,
	DialogContent,
	DialogHeader,
	DialogTitle,
} from "@/components/ui/dialog";
import { fmtClockDuration } from "@/lib/format";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";
import {
	type AccessDraft,
	AccessPicker,
	GRANT_ALL,
	grantFields,
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
 * Native (punktfunk/1) pairing: arm a window, poll fast while armed for the countdown. Shared by
 * the sheet and the list's armed row.
 *
 * The armed PIN lives here, not in the polled status: the BFF strips it from that read
 * (server/routes/api/v1/native/pair.get.ts), so the password-gated arm response is the console's
 * only copy. A reload loses it, and re-arming mints a fresh one.
 */
export function useNativePairing(
	boundTo: BoundDevice | null,
	onClearBound: () => void,
) {
	const qc = useQueryClient();
	const status = useGetNativePairing({
		query: { refetchInterval: (q) => (q.state.data?.armed ? 1_000 : 4_000) },
	});
	const arm = useArmNativePairing();
	const disarm = useDisarmNativePairing();
	// A device pairs over the QUIC ceremony, not through approve, so nothing else refreshes the
	// paired list on the happy path. The status count rising is the signal.
	const pairedCount = status.data?.paired_clients;
	const prevPairedCount = useRef(pairedCount);
	useEffect(() => {
		if (
			prevPairedCount.current !== undefined &&
			pairedCount !== undefined &&
			pairedCount !== prevPairedCount.current
		)
			qc.invalidateQueries({ queryKey: getListNativeClientsQueryKey() });
		prevPairedCount.current = pairedCount;
	}, [pairedCount, qc]);
	const refresh = () =>
		qc.invalidateQueries({ queryKey: getGetNativePairingQueryKey() });
	const [pin, setPin] = useState<string | null>(null);
	const refusal = usePasswordFailure();
	return {
		status,
		/** The PIN of the window THIS console armed, while it is armed. */
		pin: status.data?.armed ? pin : null,
		/** `access` is whatever the device that completes this window's ceremony gets. */
		onArm: (access: Partial<ArmNativePairing>, password: string) => {
			refusal.reset();
			arm.mutate(
				{
					ttl_secs: 120,
					...(boundTo ? { fingerprint: boundTo.fingerprint } : {}),
					...access,
					password,
				},
				{
					onSuccess: (s) => {
						setPin(s.pin ?? null);
						refresh();
					},
					onError: refusal.classify,
				},
			);
		},
		onDisarm: () =>
			disarm.mutate(undefined, {
				onSuccess: () => {
					setPin(null);
					onClearBound();
					refresh();
				},
			}),
		isArming: arm.isPending,
		failure: refusal.failure,
		isDisarming: disarm.isPending,
	};
}

export type NativePairing = ReturnType<typeof useNativePairing>;

/**
 * Pair a device: show a one-time PIN for a Punktfunk app, or — with GameStream on — enter the PIN
 * a Moonlight client shows, as the sheet's second step.
 */
export const PairSheet: FC<{
	open: boolean;
	onOpenChange: (open: boolean) => void;
	pairing: NativePairing;
	boundTo: BoundDevice | null;
	onClearBound: () => void;
	/** The Moonlight step. Absent without GameStream. */
	moonlight?: ReactNode;
	step: "app" | "moonlight";
	onStep: (step: "app" | "moonlight") => void;
}> = ({
	open,
	onOpenChange,
	pairing,
	boundTo,
	onClearBound,
	moonlight,
	step,
	onStep,
}) => (
	<Dialog open={open} onOpenChange={onOpenChange}>
		<DialogContent className="@container max-w-xl">
			<DialogHeader>
				<DialogTitle className="flex items-center gap-2">
					{step === "moonlight" && moonlight && (
						<Button
							variant="ghost"
							size="icon"
							aria-label={m.common_back()}
							onClick={() => onStep("app")}
						>
							<ArrowLeft className="size-4" />
						</Button>
					)}
					{step === "moonlight" && moonlight
						? m.pairing_moonlight_title()
						: m.pairing_native_title()}
				</DialogTitle>
			</DialogHeader>
			{step === "moonlight" && moonlight ? (
				moonlight
			) : (
				<div className="space-y-4">
					<NativePairingBody
						status={pairing.status}
						pin={pairing.pin}
						boundTo={boundTo}
						onClearBound={onClearBound}
						onArm={pairing.onArm}
						onDisarm={pairing.onDisarm}
						isArming={pairing.isArming}
						failure={pairing.failure}
						isDisarming={pairing.isDisarming}
					/>
					{moonlight && !pairing.pin && (
						<button
							type="button"
							className="text-sm text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
							onClick={() => onStep("moonlight")}
						>
							{m.pairing_sheet_moonlight()}
						</button>
					)}
				</div>
			)}
		</DialogContent>
	</Dialog>
);

/** Arm a window → DISPLAY the PIN the user enters on their device. */
export const NativePairingBody: FC<{
	status: Loadable<NativePairStatus>;
	/** The PIN of the window THIS console armed, or null — never from the polled status. */
	pin: string | null;
	/** The device this window is named for, or null for any device (trusted LAN only). The form
	 * never drops a binding on its own: the count it can see says a device paired, not WHICH. */
	boundTo?: BoundDevice | null;
	onClearBound?: () => void;
	/** Arm, carrying the chosen device access (empty = full and permanent) and the console
	 * password the BFF re-verifies. */
	onArm: (access: Partial<ArmNativePairing>, password: string) => void;
	onDisarm: () => void;
	isArming: boolean;
	/** Why the BFF refused the last arm's password, if it did. */
	failure: PasswordFailure;
	isDisarming: boolean;
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
}) => {
	const d = status.data;
	// Full · Forever by default: arming for your OWN next device is the common case. A window named
	// for a device is for someone else's — a knock from the internet — so it starts at Controller ·
	// this session. Keyed on the fingerprint, so a re-render never wipes the operator's edits.
	const [draft, setDraft] = useState<AccessDraft>({
		grants: GRANT_ALL,
		expiry: "forever",
		customHours: 4,
	});
	const boundFp = boundTo?.fingerprint;
	useEffect(() => {
		setDraft(
			boundFp
				? { grants: PRESET_CONTROLLER, expiry: "session", customHours: 4 }
				: { grants: GRANT_ALL, expiry: "forever", customHours: 4 },
		);
	}, [boundFp]);
	const [password, setPassword] = useState("");
	return (
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
							{m.pairing_native_expires()} {fmtClockDuration(d.expires_in_secs)}
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
				<div className="space-y-4">
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
					{/* Whichever device completes this window's ceremony gets exactly this. */}
					<AccessPicker value={draft} onChange={setDraft} idPrefix="arm" />
					{/* The completing device gets keyboard and mouse on this machine, so arming
					    re-confirms the console password; the BFF verifies and strips it. */}
					<form
						className="space-y-4"
						onSubmit={(e) => {
							e.preventDefault();
							if (!isArming && password.length > 0)
								onArm(grantFields(draft), password);
						}}
					>
						<PasswordConfirmField
							id="arm-password"
							value={password}
							onChange={setPassword}
							failure={failure}
							help={m.pairing_password_help()}
						/>
						<Button type="submit" disabled={isArming || password.length === 0}>
							<KeyRound className="size-4" />
							{m.pairing_native_arm()}
						</Button>
					</form>
				</div>
			)}
		</QueryState>
	);
};
