// GET /_auth/person?ticket=… — the page `ctl console-url` writes inside a seat opens this. The
// profile's seat host, found from the box and never from the ticket or the browser, redeems the
// ticket; its session token stays here. Any refusal lands on one plain sentence and opens nothing.
import { defineEventHandler, getQuery, sendRedirect } from "h3";
import { peerAddress } from "../../../util/auth";
import {
	firstRelay,
	isLoopbackPeer,
	openSession,
	seatFetch,
	ticketProfile,
} from "../../../util/person";
import { lookupSeat } from "../../../util/personLookup";

interface Grant {
	token?: unknown;
	expires_at?: unknown;
	profile?: unknown;
}

export default defineEventHandler(async (event) => {
	const refuse = (why: "refused" | "wait") =>
		sendRedirect(event, `/closed?why=${why}`, 302);
	const ticket = String(getQuery(event).ticket ?? "");
	if (!isLoopbackPeer(peerAddress(event))) return refuse("refused");
	const profile = ticketProfile(ticket);
	if (!profile || !firstRelay(ticket)) return refuse("refused");
	const found = await lookupSeat(profile);
	if (!found) return refuse("refused");

	let res: Response;
	try {
		res = await seatFetch(found.seat, null, "/api/v1/auth/person/session", {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({ ticket }),
			signal: AbortSignal.timeout(10_000),
		});
	} catch {
		return refuse("refused");
	}
	if (res.status === 429) return refuse("wait");
	if (!res.ok) return refuse("refused");
	const grant = (await res.json().catch(() => null)) as Grant | null;
	const token = grant?.token;
	const expires = grant?.expires_at;
	if (
		typeof token !== "string" ||
		!/^[0-9a-f]{64}$/.test(token) ||
		grant?.profile !== profile ||
		typeof expires !== "number"
	)
		return refuse("refused");

	openSession(event, {
		profile,
		owner: found.owner,
		own: found.own,
		seat: found.seat,
		token,
		expiresAt: expires * 1000,
	});
	return sendRedirect(event, "/", 302);
});
