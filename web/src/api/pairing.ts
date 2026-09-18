// The pairing WRITES. Reads keep using the generated queries; these three are hand-rolled — like
// `api/hooks.ts` — because they carry the console password, which the BFF verifies and strips
// (server/routes/api/v1/native/pair/arm.post.ts, .../native/pending/[id]/approve.post.ts,
// .../pair/pin.post.ts). Admitting a device gives it keyboard and mouse on the host desktop, so a
// 7-day session cookie on its own must not be able to do it.
import { useMutation } from "@tanstack/react-query";
import { apiFetch } from "@/api/fetcher";
import type { ApprovePending } from "@/api/gen/model/approvePending";
import type { ArmNativePairing } from "@/api/gen/model/armNativePairing";
import type { NativeClient } from "@/api/gen/model/nativeClient";
import type { NativePairStatus } from "@/api/gen/model/nativePairStatus";

const json = (body: unknown): RequestInit => ({
	method: "POST",
	headers: { "Content-Type": "application/json" },
	body: JSON.stringify(body),
});

/**
 * Arm a native pairing window.
 *
 * The PIN comes back HERE and nowhere else: the polled status has it stripped at the BFF
 * (server/routes/api/v1/native/pair.get.ts), so this response is the console's only copy of it.
 */
export function useArmNativePairing() {
	return useMutation({
		mutationFn: (
			body: Partial<ArmNativePairing> & { password: string },
		): Promise<NativePairStatus> =>
			apiFetch<NativePairStatus>("/api/v1/native/pair/arm", json(body)),
	});
}

/** Pair a knocking device outright (no PIN ceremony) — hence the password. */
export function useApprovePendingDevice() {
	return useMutation({
		mutationFn: ({
			id,
			data,
			password,
		}: {
			id: number;
			data: ApprovePending;
			password: string;
		}): Promise<NativeClient> =>
			apiFetch<NativeClient>(
				`/api/v1/native/pending/${id}/approve`,
				json({ ...data, password }),
			),
	});
}

/**
 * Deliver the PIN a Moonlight client is showing, completing its handshake.
 *
 * `uniqueid`/`fingerprint` address the PIN to the parked ceremony the operator SAW in the
 * pairing status — never to whichever handshake happens to be parked at delivery time
 * (security-review 2026-08-31 H-4).
 */
export function useSubmitPairingPin() {
	return useMutation({
		mutationFn: ({
			pin,
			password,
			uniqueid,
			fingerprint,
			peerIp,
			label,
		}: {
			pin: string;
			password: string;
			uniqueid: string;
			fingerprint: string;
			peerIp: string;
			/** Name for the device; the host stores it once the pairing completes. */
			label?: string;
		}) =>
			apiFetch<void>(
				"/api/v1/pair/pin",
				json({
					pin,
					password,
					uniqueid,
					fingerprint,
					peer_ip: peerIp,
					...(label?.trim() ? { label: label.trim() } : {}),
				}),
			),
	});
}
