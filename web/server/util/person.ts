// A person's own console, held here. Their seat host traded a ticket only their account could
// sign for a session token; the browser gets a random id for it, and the token never leaves this
// process. Nothing here reads or sends the console's admin token: a person's call goes to their
// seat host with their token, or to the box with the person headers (personApi.ts).
import { createHash, randomBytes, X509Certificate } from "node:crypto";
import { connect } from "node:tls";
import { deleteCookie, getCookie, type H3Event, setCookie } from "h3";
import { cookieSecure } from "./auth";

/** The person's session id, httpOnly. */
export const PERSON_COOKIE = "pf_person";
/** Which console the page draws, readable by it. It grants nothing: the server decides each call. */
export const MODE_COOKIE = "pf_mode";

/** A ticket is good this long either side of its stamp, as on the seat host. */
export const TICKET_TTL_MS = 60_000;
/** The longest a session lives here; the seat host's own grant is the same or shorter. */
const MAX_SESSION_MS = 12 * 60 * 60 * 1000;
/** Sessions one profile holds at once, as many as its seat host keeps. Past this that profile's
 * own session that ends soonest goes, never another's. */
const MAX_SESSIONS_PER_PROFILE = 16;
/** A seat host on loopback that takes this long is not answering. */
const SEAT_TIMEOUT_MS = 15_000;

/** Where a seat host's management API listens, and its certificate's SHA-256, lowercase hex. */
export interface SeatEndpoint {
	port: number;
	pin: string;
}

export interface PersonSession {
	profile: string;
	/** The owner's display name: shared games are theirs to add. */
	owner: string | null;
	/** The seat runs on the person's own account, so a game's exit always returns to it. */
	own: boolean;
	seat: SeatEndpoint;
	/** The seat host's session token. */
	token: string;
	/** Unix ms. */
	expiresAt: number;
	/** Signs this session's plugin page paths (pluginPage.ts); random, and dies with it. */
	pageKey: string;
}

/** A person's call could not reach their seat host: it stopped, or another process holds its port. */
export class SeatGone extends Error {}

/** A peer on this computer: a person's console opens and runs only there. */
export const isLoopbackPeer = (ip: string): boolean =>
	/^(127\.\d{1,3}\.\d{1,3}\.\d{1,3}|::1|::ffff:127\.\d{1,3}\.\d{1,3}\.\d{1,3})$/.test(
		ip,
	);

const TICKET = /^(\d{1,20})\.([0-9a-f]{32})\.([0-9a-f]{12})\.([0-9a-f]{64})$/;

/**
 * The profile a ticket names, when it has the seat host's shape and is fresh. Only the seat host
 * can verify it; this keeps a stale or malformed one from costing the person a try there.
 */
export function ticketProfile(ticket: string, now = Date.now()): string | null {
	const m = TICKET.exec(ticket);
	if (!m) return null;
	const issued = Number(m[1]) * 1000;
	if (!Number.isSafeInteger(issued) || Math.abs(now - issued) > TICKET_TTL_MS)
		return null;
	return m[3] ?? null;
}

/** Tickets this console relayed, so a replayed one never reaches a seat host. */
const relayed = new Map<string, number>();

/** True the first time `ticket` is offered within its lifetime. */
export function firstRelay(ticket: string, now = Date.now()): boolean {
	for (const [t, at] of relayed)
		if (now - at > 2 * TICKET_TTL_MS) relayed.delete(t);
	if (relayed.has(ticket)) return false;
	relayed.set(ticket, now);
	return true;
}

/** Sessions by the SHA-256 of their cookie value. */
const live = new Map<string, PersonSession>();
const digest = (id: string) => createHash("sha256").update(id).digest("hex");

const cookieBase = () => ({
	path: "/",
	sameSite: "lax" as const,
	secure: cookieSecure(),
});

/** Opens `session` in this browser, replacing any person session it held. A profile at
 * `MAX_SESSIONS_PER_PROFILE` loses its own session that ends soonest. */
export function openSession(
	event: H3Event,
	session: Omit<PersonSession, "pageKey">,
): void {
	const old = getCookie(event, PERSON_COOKIE);
	if (old) live.delete(digest(old));
	const now = Date.now();
	for (const [k, s] of live) if (s.expiresAt <= now) live.delete(k);
	const own = [...live]
		.filter(([, s]) => s.profile === session.profile)
		.sort((a, b) => a[1].expiresAt - b[1].expiresAt);
	const over = Math.max(0, own.length + 1 - MAX_SESSIONS_PER_PROFILE);
	for (const [k] of own.slice(0, over)) live.delete(k);
	const expiresAt = Math.min(session.expiresAt, now + MAX_SESSION_MS);
	const id = randomBytes(32).toString("hex");
	const pageKey = randomBytes(32).toString("hex");
	live.set(digest(id), { ...session, expiresAt, pageKey });
	const maxAge = Math.max(1, Math.floor((expiresAt - now) / 1000));
	setCookie(event, PERSON_COOKIE, id, {
		...cookieBase(),
		httpOnly: true,
		maxAge,
	});
	setCookie(event, MODE_COOKIE, "person", {
		...cookieBase(),
		httpOnly: false,
		maxAge,
	});
}

/** The browser's person cookie: its live session, or `null` for one that ended. */
export interface PersonCookie {
	id: string;
	session: PersonSession | null;
}

export function personCookie(event: H3Event): PersonCookie | null {
	const id = getCookie(event, PERSON_COOKIE);
	if (!id) return null;
	const session = live.get(digest(id)) ?? null;
	if (session && session.expiresAt <= Date.now()) {
		live.delete(digest(id));
		return { id, session: null };
	}
	return { id, session };
}

/** Every session still live, for a plugin page whose path one of them signed. */
export function liveSessions(now = Date.now()): PersonSession[] {
	return [...live.values()].filter((s) => s.expiresAt > now);
}

/** Forgets the browser's person session here and drops both cookies. The seat host's stays. */
export function forgetSession(event: H3Event): PersonSession | null {
	const id = getCookie(event, PERSON_COOKIE);
	const session = id ? (live.get(digest(id)) ?? null) : null;
	if (id) live.delete(digest(id));
	deleteCookie(event, PERSON_COOKIE, cookieBase());
	deleteCookie(event, MODE_COOKIE, cookieBase());
	return session;
}

/** Ends the browser's person session: here, and on the seat host when it still answers. */
export async function endSession(event: H3Event): Promise<void> {
	const session = forgetSession(event);
	if (!session) return;
	try {
		await seatCall(session, "/api/v1/auth/person/session", {
			method: "DELETE",
			signal: AbortSignal.timeout(3_000),
		});
	} catch {
		// A seat host that doesn't answer has no session left to end.
	}
}

/** PEMs of seat host certificates that matched their pin, by `port/pin`. */
const pinned = new Map<string, string>();

/** The leaf certificate whoever listens on `port` presents. Sends no application byte. */
function peerCertificate(port: number): Promise<Buffer> {
	return new Promise((resolve, reject) => {
		const socket = connect({
			host: "127.0.0.1",
			port,
			rejectUnauthorized: false,
		});
		socket.setTimeout(3_000, () => socket.destroy(new Error("timeout")));
		socket.once("secureConnect", () => {
			const raw = socket.getPeerCertificate(true)?.raw;
			socket.destroy();
			if (raw) resolve(raw);
			else reject(new Error("no certificate"));
		});
		socket.once("error", reject);
	});
}

/**
 * The seat host's certificate as a PEM, once whoever answers on its port presents the one the box
 * pinned. Every call then trusts that PEM alone, so a process that takes the port later fails
 * the handshake before it sees the token.
 */
async function seatPem(seat: SeatEndpoint): Promise<string> {
	const key = `${seat.port}/${seat.pin}`;
	const kept = pinned.get(key);
	if (kept) return kept;
	const raw = await peerCertificate(seat.port);
	const got = createHash("sha256").update(raw).digest("hex");
	if (got !== seat.pin.toLowerCase()) throw new SeatGone("pin mismatch");
	const pem = new X509Certificate(raw).toString();
	pinned.set(key, pem);
	return pem;
}

/** `path` on the person's seat host, with their session token. Throws `SeatGone` when it isn't there. */
export const seatCall = (
	session: PersonSession,
	path: string,
	init: RequestInit = {},
): Promise<Response> => seatFetch(session.seat, session.token, path, init);

/** `path` on the seat host at `seat`, with `token` as the bearer or none. */
export async function seatFetch(
	seat: SeatEndpoint,
	token: string | null,
	path: string,
	init: RequestInit = {},
): Promise<Response> {
	let pem: string;
	try {
		pem = await seatPem(seat);
	} catch (cause) {
		throw new SeatGone("no seat host", { cause });
	}
	const headers = new Headers(init.headers);
	headers.delete("authorization");
	if (token) headers.set("authorization", `Bearer ${token}`);
	try {
		return await fetch(`https://127.0.0.1:${seat.port}${path}`, {
			...init,
			headers,
			redirect: "manual",
			signal: init.signal ?? AbortSignal.timeout(SEAT_TIMEOUT_MS),
			// `tls` is a Bun.fetch extension, pinned per request.
			tls: {
				ca: [pem],
				rejectUnauthorized: true,
				checkServerIdentity: () => undefined,
			},
		} as RequestInit);
	} catch (cause) {
		if (isTimeout(cause)) throw cause;
		throw new SeatGone("seat host unreachable", { cause });
	}
}

export const isTimeout = (e: unknown): boolean =>
	e instanceof Error && (e.name === "TimeoutError" || e.name === "AbortError");

/** For tests: forget every session, relayed ticket and pinned certificate. */
export function resetPersonState(): void {
	live.clear();
	relayed.clear();
	pinned.clear();
}
