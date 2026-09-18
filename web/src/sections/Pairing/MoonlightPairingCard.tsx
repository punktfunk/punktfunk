import { useQueryClient } from "@tanstack/react-query";
import { Info, KeyRound } from "lucide-react";
import { type FC, useEffect, useRef, useState } from "react";
import { ApiError } from "@/api/fetcher";
import { getListPairedClientsQueryKey } from "@/api/gen/clients/clients";
import type { PairingStatus } from "@/api/gen/model/pairingStatus";
import {
	getGetPairingStatusQueryKey,
	useGetPairingStatus,
} from "@/api/gen/pairing/pairing";
import { useSubmitPairingPin } from "@/api/pairing";
import { QueryState } from "@/components/query-state";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
	Select,
	SelectContent,
	SelectItem,
	SelectTrigger,
	SelectValue,
} from "@/components/ui/select";
import type { Loadable } from "@/lib/query";
import { m } from "@/paraglide/messages";

const ceremonyKey = (c: PairingStatus["pending"][number]) =>
	`${c.uniqueid}\u0000${c.fingerprint}\u0000${c.peer_ip}`;

/** Container: GameStream/Moonlight pairing — poll status, own the PIN entry, submit it. */
export const MoonlightPairingSection: FC = () => {
	const qc = useQueryClient();
	const [pin, setPin] = useState("");
	const [label, setLabel] = useState("");
	const [password, setPassword] = useState("");
	const [wrongPassword, setWrongPassword] = useState(false);
	// Fingerprint of the ceremony the PIN is addressed to; "" = the first (sole) one.
	const [target, setTarget] = useState("");
	const pairing = useGetPairingStatus({ query: { refetchInterval: 2_000 } });
	const submit = useSubmitPairingPin();

	// Clear the previous attempt's outcome when a NEW pairing knock arrives.
	//
	// The mutation's success flag outlives the form — the section never unmounts, only the inner
	// <form> is conditional — so the green "PIN sent" note was still on screen above an empty PIN
	// box the next time Moonlight asked. Resetting inside `onSubmit` (the first attempt at this)
	// does nothing: `mutate` moves the status to pending in the same update, so `isSuccess` was
	// already about to go false. The transition that matters is `pin_pending` going false → true.
	const pending = pairing.data?.pin_pending ?? false;
	const wasPending = useRef(pending);
	useEffect(() => {
		if (pending && !wasPending.current) {
			submit.reset();
			setPin("");
			setLabel("");
			setPassword("");
			setWrongPassword(false);
			setTarget("");
		}
		wasPending.current = pending;
	}, [pending, submit.reset]);

	const onSubmit = () => {
		setWrongPassword(false);
		// Address the PIN to the ceremony the operator saw (the selected one, else the sole
		// one) — never to whichever handshake is parked at delivery time (security-review
		// 2026-08-31 H-4).
		const ceremonies = pairing.data?.pending ?? [];
		const chosen =
			ceremonies.find((c) => ceremonyKey(c) === target) ?? ceremonies[0];
		if (!chosen) return;
		submit.mutate(
			{
				pin,
				password,
				uniqueid: chosen.uniqueid,
				fingerprint: chosen.fingerprint,
				peerIp: chosen.peer_ip,
				label,
			},
			{
				onSuccess: () => {
					setPin("");
					setLabel("");
					setPassword("");
					qc.invalidateQueries({ queryKey: getGetPairingStatusQueryKey() });
					// The success message tells the operator to check the paired list, so refresh it —
					// both planes, since this card's count spans them.
					qc.invalidateQueries({ queryKey: getListPairedClientsQueryKey() });
				},
				onError: (e) => {
					if (e instanceof ApiError && e.status === 401) setWrongPassword(true);
				},
			},
		);
	};

	return (
		<MoonlightPairing
			pairing={pairing}
			pin={pin}
			onPinChange={setPin}
			label={label}
			onLabelChange={setLabel}
			password={password}
			onPasswordChange={setPassword}
			wrongPassword={wrongPassword}
			target={target}
			onTargetChange={setTarget}
			onSubmit={onSubmit}
			isSubmitting={submit.isPending}
			isSuccess={submit.isSuccess}
			isError={submit.isError}
		/>
	);
};

/** GameStream/Moonlight pairing: the client shows a PIN, the operator submits it here. */
export const MoonlightPairing: FC<{
	pairing: Loadable<PairingStatus>;
	pin: string;
	onPinChange: (v: string) => void;
	/** Name for the device being paired. Optional, and the only name it will have. */
	label: string;
	onLabelChange: (v: string) => void;
	/** The console password, re-confirmed because delivering the PIN completes a pairing. */
	password: string;
	onPasswordChange: (v: string) => void;
	wrongPassword: boolean;
	/** Fingerprint of the ceremony the PIN is addressed to; "" = the first (sole) one. */
	target: string;
	onTargetChange: (v: string) => void;
	onSubmit: () => void;
	isSubmitting: boolean;
	isSuccess: boolean;
	isError: boolean;
}> = ({
	pairing,
	pin,
	onPinChange,
	label,
	onLabelChange,
	password,
	onPasswordChange,
	wrongPassword,
	target,
	onTargetChange,
	onSubmit,
	isSubmitting,
	isSuccess,
	isError,
}) => {
	const pending = pairing.data?.pin_pending ?? false;
	const ceremonies = pairing.data?.pending ?? [];
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<KeyRound className="size-4" />
					{m.pairing_moonlight_title()}
				</CardTitle>
			</CardHeader>
			<CardContent>
				{/* The card is in the page's cascade from the first frame; only its body waits. */}
				<QueryState
					isLoading={pairing.isLoading}
					error={pairing.error}
					refetch={pairing.refetch}
				>
					{!pending ? (
						<p className="text-sm text-muted-foreground">{m.pairing_idle()}</p>
					) : (
						<form
							onSubmit={(e) => {
								e.preventDefault();
								onSubmit();
							}}
							className="space-y-4"
						>
							<p className="text-sm">{m.pairing_waiting()}</p>
							{/* Name the ceremony the PIN answers, so the operator pairs the device
							    they can SEE — and, with several parked, picks the one they mean; the
							    host delivers the PIN only to the named handshake (security-review
							    2026-08-31 H-4). */}
							{ceremonies.length === 1 && ceremonies[0] && (
								<p className="font-mono text-xs text-muted-foreground">
									{m.pairing_ceremony_device({
										uniqueid: ceremonies[0].uniqueid,
										ip: ceremonies[0].peer_ip,
										fp: ceremonies[0].fingerprint.slice(-10),
									})}
								</p>
							)}
							{ceremonies.length > 1 && (
								<div className="space-y-2">
									<p className="text-sm">{m.pairing_ceremony_select()}</p>
									<Select
										value={
											target || (ceremonies[0] && ceremonyKey(ceremonies[0]))
										}
										onValueChange={onTargetChange}
									>
										<SelectTrigger id="pair-target">
											<SelectValue />
										</SelectTrigger>
										<SelectContent>
											{ceremonies.map((c) => (
												<SelectItem key={ceremonyKey(c)} value={ceremonyKey(c)}>
													{m.pairing_ceremony_device({
														uniqueid: c.uniqueid,
														ip: c.peer_ip,
														fp: c.fingerprint.slice(-10),
													})}
												</SelectItem>
											))}
										</SelectContent>
									</Select>
								</div>
							)}
							<div className="space-y-2">
								<Label htmlFor="pin">{m.pairing_pin_label()}</Label>
								<Input
									id="pin"
									inputMode="numeric"
									autoComplete="off"
									maxLength={16}
									value={pin}
									onChange={(e) =>
										onPinChange(e.target.value.replace(/\D/g, ""))
									}
									placeholder="0000"
									className="font-mono text-lg tracking-widest"
								/>
							</div>
							{/* Moonlight identifies every client as the same device, so a name typed
							    here is the only one this one gets. Optional: skipping it lists the
							    device by fingerprint, renameable later from Paired devices. */}
							<div className="space-y-2">
								<Label htmlFor="pair-label">{m.clients_rename_label()}</Label>
								<Input
									id="pair-label"
									autoComplete="off"
									maxLength={64}
									value={label}
									onChange={(e) => onLabelChange(e.target.value)}
									placeholder={m.pairing_pending_name_prompt()}
								/>
							</div>
							{/* Delivering the PIN completes the handshake and pairs the client, which is the
							    same trust decision as approving a native knock — so the same password gate
							    (util/confirm.ts). Anyone can point their OWN Moonlight at this host and read
							    the PIN off their own screen; the password is what they don't have. */}
							<div className="space-y-2">
								<Label htmlFor="pair-password">{m.store_spec_password()}</Label>
								<Input
									id="pair-password"
									type="password"
									autoComplete="current-password"
									value={password}
									onChange={(e) => onPasswordChange(e.target.value)}
								/>
								<p className="text-xs text-muted-foreground">
									{m.pairing_password_help()}
								</p>
								{wrongPassword && (
									<p role="alert" className="text-xs text-destructive">
										{m.update_apply_wrong_password()}
									</p>
								)}
							</div>
							<Button
								type="submit"
								disabled={
									pin.length < 4 || password.length === 0 || isSubmitting
								}
							>
								{m.pairing_submit()}
							</Button>
							{/* A 204 means the PIN was DELIVERED to the waiting handshake, not that pairing
							    succeeded — the ceremony verifies it out-of-band. So report "sent", not
							    "paired", and let the operator confirm via the Paired devices list. */}
							{isSuccess && (
								<p className="flex items-center gap-1.5 text-sm text-muted-foreground">
									<Info className="size-4" />
									{m.pairing_pin_sent()}
								</p>
							)}
							{isError && (
								<p className="text-sm text-destructive">{m.pairing_failed()}</p>
							)}
						</form>
					)}
				</QueryState>
			</CardContent>
		</Card>
	);
};
