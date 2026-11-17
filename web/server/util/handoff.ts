import { readFileSync } from "node:fs";
import { join } from "node:path";
// Verifies the console handoff ticket `punktfunk-host ctl console-url` mints, keyed by the
// management token both sides already hold. The ticket never enters browser argv, where
// /proc/<pid>/cmdline would expose it to other local users: the host writes it into a 0600 page
// under $XDG_RUNTIME_DIR and the browser is launched at that file.

/** How long a ticket may be redeemed. Minting happens at click time, so a browser has seconds. */
export const HANDOFF_TTL_MS = 60_000;

export type HandoffVerdict =
	| { ok: true }
	| {
			ok: false;
			reason: "malformed" | "expired" | "replayed" | "bad-signature";
	  };

/** The signed message. Kept in one place because it is a cross-language contract: change it here
 * and `handoff_ticket` in the host's ctl.rs must change in the same commit. */
export function handoffMessage(ts: string, nonce: string): string {
	return `pf-console-handoff:v1:${ts}:${nonce}`;
}

async function macFor(key: string, ts: string, nonce: string): Promise<string> {
	const enc = new TextEncoder();
	const cryptoKey = await crypto.subtle.importKey(
		"raw",
		enc.encode(key),
		{ name: "HMAC", hash: "SHA-256" },
		false,
		["sign"],
	);
	const sig = await crypto.subtle.sign(
		"HMAC",
		cryptoKey,
		enc.encode(handoffMessage(ts, nonce)),
	);
	return [...new Uint8Array(sig)]
		.map((b) => b.toString(16).padStart(2, "0"))
		.join("");
}

/** Constant-time compare of two equal-length hex strings. Length is not secret (it is fixed by the
 * hash), so returning early on a mismatched length leaks nothing. */
export function safeEqualHex(a: string, b: string): boolean {
	if (a.length !== b.length) return false;
	let diff = 0;
	for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
	return diff === 0;
}

/**
 * Decide whether `ticket` may open a session.
 *
 * `seen` is the caller's replay set (ticket → redeemed-at ms); this function reads AND records, so
 * a second call with the same ticket is refused. Passing a fresh map per request would therefore
 * disable single-use — the route keeps one for the process lifetime on purpose.
 */
export async function verifyHandoff(
	ticket: string,
	keys: string | readonly string[],
	seen: Map<string, number>,
	now: number = Date.now(),
): Promise<HandoffVerdict> {
	for (const [t, at] of seen) if (now - at > HANDOFF_TTL_MS) seen.delete(t);

	const parts = ticket.split(".");
	if (parts.length !== 3) return { ok: false, reason: "malformed" };
	const [ts, nonce, mac] = parts;
	// `split` yields `string | undefined` per element under `noUncheckedIndexedAccess`, and the
	// length check above does not narrow a plain array — so this both proves it to the compiler and
	// rejects the empty segments a `"1..2"` ticket would otherwise sneak through.
	if (!ts || !nonce || !mac) return { ok: false, reason: "malformed" };
	if (
		!/^\d+$/.test(ts) ||
		!/^[0-9a-f]+$/.test(nonce) ||
		!/^[0-9a-f]+$/.test(mac)
	) {
		return { ok: false, reason: "malformed" };
	}

	const issued = Number(ts) * 1000;
	// Symmetric window: a ticket from the future is as wrong as an old one, and clock skew between
	// two processes on the SAME box is not a thing we need to forgive.
	if (!Number.isFinite(issued) || Math.abs(now - issued) > HANDOFF_TTL_MS) {
		return { ok: false, reason: "expired" };
	}
	// Reserve BEFORE the first await: the event loop can interleave two redemptions of one
	// ticket while the HMAC is in flight, and both would pass a check-then-record written the
	// obvious way round. A failed signature releases the reservation; that cannot burn someone
	// else's ticket, because a wrong-mac guess is a different map key than the real ticket.
	if (seen.has(ticket)) return { ok: false, reason: "replayed" };
	seen.set(ticket, now);
	for (const key of typeof keys === "string" ? [keys] : keys) {
		if (key && safeEqualHex(mac, await macFor(key, ts, nonce)))
			return { ok: true };
	}
	seen.delete(ticket);
	return { ok: false, reason: "bad-signature" };
}

/**
 * The owner's host's key, on a door: the door keeps it in `owner-handoff-key` beside its own
 * token, so the owner's `ctl console-url`, which signs with their own host's token, opens the
 * door's console. Never a seat's. Empty off a door. Read per call: the owner's row mints a new
 * token each time it starts.
 */
export function ownerHandoffKeys(
	dir: string = process.env.PUNKTFUNK_CONFIG_DIR ?? "",
): string[] {
	if (!dir) return [];
	try {
		const key = readFileSync(join(dir, "owner-handoff-key"), "utf8").trim();
		return key ? [key] : [];
	} catch {
		return [];
	}
}
