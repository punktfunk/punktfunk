// POST /api/v1/pair/pin — the GameStream lane's half of the same decision: delivering the PIN a
// Moonlight client is showing completes its pairing handshake, and the paired client then has
// keyboard and mouse on the host desktop. Anyone holding a session cookie can point their own
// Moonlight at the host and read the PIN off their own screen, so this is gated on the console
// password exactly like the native approve route (util/confirm.ts).
//
// Wins over the `/api/**` catch-all by h3 route specificity. Only reachable when the host runs the
// compat planes (`--gamestream`, off by default).
import { defineEventHandler, readBody } from "h3";
import { confirmPassword } from "../../../../util/confirm";
import { forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<{
		pin?: string;
		uniqueid?: string;
		fingerprint?: string;
		peer_ip?: string;
		label?: string;
		password?: string;
	}>(event);
	await confirmPassword(event, body?.password);
	// Rebuild from exactly the fields the host takes, so the password cannot leak upstream.
	// uniqueid/fingerprint address the PIN to one parked ceremony — the one the operator SAW in
	// the pairing status — instead of whichever handshake is parked at delivery time
	// (security-review 2026-08-31 H-4).
	// `label` names the device being paired; the host scrubs it and stores it on success.
	return forwardJson(event, "/api/v1/pair/pin", "POST", {
		pin: String(body?.pin ?? ""),
		uniqueid: String(body?.uniqueid ?? ""),
		fingerprint: String(body?.fingerprint ?? ""),
		peer_ip: String(body?.peer_ip ?? ""),
		...(body?.label ? { label: String(body.label) } : {}),
	});
});
