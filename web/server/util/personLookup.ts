// Sign-in's one read of the box with the console's admin token, before the person's cookie exists
// (routes/_auth/person/index.get.ts): which seat host is the profile's. The only input is the
// profile id the ticket names. Who the person is, they read from their seat host with their own
// session (routes/_auth/person/me.get.ts). A person's own calls never come here.
import { mgmtFetch } from "./forward";
import type { SeatEndpoint } from "./person";

/** The slice of a `ProfileAdmin` row sign-in reads. */
interface Row {
	id?: unknown;
	display_name?: unknown;
	owner?: unknown;
	seat?: {
		kind?: unknown;
		pin?: unknown;
		account?: unknown;
		mgmt_port?: unknown;
	} | null;
}

export interface SeatLookup {
	seat: SeatEndpoint;
	owner: string | null;
	own: boolean;
}

const PIN = /^[0-9a-f]{64}$/i;

/**
 * The seat host of `profile`, a seat of its own, as the box lists it; `null` when it has none,
 * or when the box is too old to name the seat host's management port.
 */
export async function lookupSeat(profile: string): Promise<SeatLookup | null> {
	let rows: unknown;
	try {
		const res = await mgmtFetch("/api/v1/profiles", {
			signal: AbortSignal.timeout(5_000),
		});
		if (!res.ok) return null;
		rows = await res.json();
	} catch {
		return null;
	}
	if (!Array.isArray(rows)) return null;
	const list = rows as Row[];
	const row = list.find((r) => r.id === profile);
	const seat = row?.seat;
	if (!row || row.owner === true || seat?.kind !== "desktop") return null;
	const port = seat.mgmt_port;
	const pin = seat.pin;
	if (typeof pin !== "string" || !PIN.test(pin)) return null;
	if (
		typeof port !== "number" ||
		!Number.isInteger(port) ||
		port < 1 ||
		port > 65_535
	)
		return null;
	const owner = list.find((r) => r.owner === true)?.display_name;
	return {
		seat: { port, pin: pin.toLowerCase() },
		owner: typeof owner === "string" ? owner : null,
		own: typeof seat.account === "string" && seat.account !== "",
	};
}
