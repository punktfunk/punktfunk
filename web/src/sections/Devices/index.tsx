import { KeyRound, Plus, Smartphone } from "lucide-react";
import { type FC, useCallback, useState } from "react";
import { useGetHostInfo, useGetStatus } from "@/api/gen/host/host";
import { useGetPairingStatus } from "@/api/gen/pairing/pairing";
import { Button } from "@/components/ui/button";
import { MenuItem, RowMenu } from "@/components/ui/menu";
import { fmtClockDuration } from "@/lib/format";
import { useLocale } from "@/lib/i18n";
import { m } from "@/paraglide/messages";
import { MoonlightPairingSection } from "./MoonlightPairingCard";
import { PairedRowView, usePairedDevices } from "./PairedDevices";
import { type BoundDevice, PairSheet, useNativePairing } from "./PairSheet";
import { PendingRow, usePendingDevices, WaitingRow } from "./PendingDevices";
import { DevicesView } from "./view";

/**
 * Devices: pair a device, see what is paired. One list, one Pair sheet; the approve, access and
 * display dialogs mount once here.
 */
export const SectionDevices: FC = () => {
	useLocale();
	// A knock from the internet is admitted only by a PIN bound to its fingerprint: its row hands
	// the device to the Pair sheet, which arms for it.
	const [boundTo, setBoundTo] = useState<BoundDevice | null>(null);
	const clearBound = useCallback(() => setBoundTo(null), []);
	const native = useNativePairing(boundTo, clearBound);
	const [sheet, setSheet] = useState<"app" | "moonlight" | null>(null);
	// Moonlight pairs only while the host runs the compat planes (`--gamestream`, off by default).
	const host = useGetHostInfo();
	const gamestream = host.data?.gamestream === true;
	const moonlight = useGetPairingStatus({
		query: { enabled: gamestream, refetchInterval: 2_000 },
	});
	const pending = usePendingDevices();
	const paired = usePairedDevices();
	// Who is streaming right now: a session's `client` is its fingerprint's prefix.
	const status = useGetStatus({ query: { refetchInterval: 15_000 } });
	const streaming = (fingerprint: string) =>
		(status.data?.sessions ?? []).some(
			(s) => s.plane !== "gamestream" && fingerprint.startsWith(s.client),
		);

	const armed = native.status.data?.armed && native.pin;
	const waiting = [
		armed && (
			<WaitingRow
				key="armed"
				lead={<KeyRound className="size-4" />}
				title={m.devices_armed({
					pin: native.pin ?? "",
					time: fmtClockDuration(native.status.data?.expires_in_secs ?? 0),
				})}
				details={
					boundTo ? m.pairing_native_bound_for({ name: boundTo.name }) : null
				}
				actions={
					<Button
						size="sm"
						variant="outline"
						disabled={native.isDisarming}
						onClick={native.onDisarm}
					>
						{m.pairing_native_cancel()}
					</Button>
				}
			/>
		),
		gamestream && moonlight.data?.pin_pending && (
			<WaitingRow
				key="moonlight"
				lead={<Smartphone className="size-4" />}
				title={m.devices_moonlight_waiting()}
				actions={
					<Button size="sm" onClick={() => setSheet("moonlight")}>
						{m.devices_moonlight_enter()}
					</Button>
				}
			/>
		),
		...(pending.pending.data ?? []).map((p) => (
			<PendingRow
				key={p.id}
				device={p}
				onApprove={pending.onApprove}
				onArmFor={(d) => {
					setBoundTo({ fingerprint: d.fingerprint, name: d.name });
					setSheet("app");
				}}
				onDeny={pending.onDeny}
				busy={pending.pendingId === p.id}
			/>
		)),
	].filter(Boolean);

	return (
		<>
			<DevicesView
				actions={
					<>
						<Button onClick={() => setSheet("app")}>
							<Plus className="size-4" />
							{m.pairing_native_title()}
						</Button>
						{paired.rows.length > 0 && (
							<RowMenu
								label={m.common_more_actions()}
								disabled={paired.isUnpairingAll}
							>
								<MenuItem destructive onSelect={paired.onUnpairAll}>
									{m.action_unpair_all()}
								</MenuItem>
							</RowMenu>
						)}
					</>
				}
				waiting={waiting}
				paired={paired.rows.map((r) => (
					<PairedRowView
						key={`${r.protocol}:${r.fingerprint}`}
						{...paired.rowProps(r)}
						streaming={r.protocol === "native" && streaming(r.fingerprint)}
					/>
				))}
				pairedState={{
					isLoading: paired.isLoading,
					error: paired.error,
					refetch: paired.refetch,
				}}
			/>
			<PairSheet
				open={sheet !== null}
				onOpenChange={(open) => {
					if (open) return;
					setSheet(null);
					// A bound window that was never armed has nothing left to name.
					if (!native.pin) clearBound();
				}}
				pairing={native}
				boundTo={boundTo}
				onClearBound={clearBound}
				moonlight={gamestream ? <MoonlightPairingSection /> : undefined}
				step={sheet ?? "app"}
				onStep={setSheet}
			/>
			{pending.dialog}
			{paired.dialogs}
		</>
	);
};
