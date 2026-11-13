// A person's `/api/**`, answered before any route file runs (middleware/auth.ts). Each call goes
// where personRoutes.ts says, with the person's credentials and nothing of the console's: their
// seat host gets their session token, the box gets the person headers. No admin bearer exists here.
import { getRequestHeader, getRequestURL, type H3Event, readRawBody } from "h3";
import { isLoopbackUrl, loopbackTls, mgmtUrl } from "./mgmt";
import { normalizeSegments } from "./paths";
import {
	forgetSession,
	isTimeout,
	type PersonSession,
	SeatGone,
	seatCall,
} from "./person";
import { type Lane, personLane } from "./personRoutes";
import { encodeRest } from "./seatProxy";

/** The headers the box reads a person session from (`mgmt/person_session.rs`). */
export const SESSION_HEADER = "x-punktfunk-person-session";
export const PROFILE_HEADER = "x-punktfunk-person-profile";

/** What a person's call carries upstream, and what comes back. Cookies never cross. */
const REQUEST_HEADERS = ["accept", "content-type", "if-none-match"];
const RESPONSE_HEADERS = [
	"content-type",
	"etag",
	"cache-control",
	"last-modified",
];

/** Why a person's console closed under them; the page says each in its own words. */
export type Ended = "person_session_ended" | "person_seat_stopped";

const json = (status: number, body: unknown) =>
	new Response(JSON.stringify(body), {
		status,
		headers: { "content-type": "application/json" },
	});

/** The answer to a person whose session is over. The cookies go with it. */
export function ended(event: H3Event, code: Ended, error?: string): Response {
	forgetSession(event);
	return json(401, { code, error: error ?? code });
}

const closed = () =>
	json(403, {
		code: "person_route_closed",
		error: "not part of a person's console",
	});

/** `/api/v1/<rest>` resolved, or `null` for a path that leaves it or hides a separator. */
function apiRest(pathname: string): string | null {
	const [api, v1, ...rest] = normalizeSegments(pathname);
	if (api !== "api" || v1 !== "v1" || rest.length === 0) return null;
	if (rest.some((s) => /^\.{0,2}$|\//.test(s))) return null;
	return rest.join("/");
}

/** The box, from loopback, with the person headers and no bearer. */
async function boxCall(
	session: PersonSession,
	path: string,
	init: RequestInit,
): Promise<Response> {
	const base = mgmtUrl();
	// The session token is the person's: it never leaves this machine.
	if (!isLoopbackUrl(base)) throw new SeatGone("the box is not on loopback");
	const headers = new Headers(init.headers);
	headers.set(SESSION_HEADER, session.token);
	headers.set(PROFILE_HEADER, session.profile);
	return fetch(`${base}${path}`, {
		...init,
		headers,
		redirect: "manual",
		signal: AbortSignal.timeout(15_000),
		...loopbackTls(base),
	} as RequestInit);
}

/** Answers a person's `/api/**` call. */
export async function personApi(
	event: H3Event,
	session: PersonSession,
): Promise<Response> {
	const url = getRequestURL(event);
	const method = (event.method ?? "GET").toUpperCase();
	const rest = apiRest(url.pathname);
	const lane: Lane | null = rest ? personLane(method, rest) : null;
	if (!rest || !lane) return closed();

	// `as` is the session's, never the browser's.
	const query = new URLSearchParams(url.search);
	query.delete("as");
	if (lane === "view") query.set("as", session.profile);
	const qs = query.toString();
	const path = `/api/v1/${encodeRest(rest)}${qs ? `?${qs}` : ""}`;

	const headers = new Headers();
	for (const name of REQUEST_HEADERS) {
		const value = getRequestHeader(event, name);
		if (value) headers.set(name, value);
	}
	// Every write in the table is JSON.
	const body =
		method === "GET" || method === "HEAD"
			? undefined
			: ((await readRawBody(event)) ?? undefined);
	const init: RequestInit = { method, headers, body };

	let res: Response;
	try {
		res =
			lane === "seat"
				? await seatCall(session, path, init)
				: await boxCall(session, path, init);
	} catch (e) {
		if (e instanceof SeatGone) return ended(event, "person_seat_stopped");
		if (isTimeout(e))
			return json(504, {
				code: "seat_host_no_answer",
				error: "That host didn't answer. Try again in a moment.",
			});
		throw e;
	}
	// The seat host answers 401 only for a session that is over; the box says so by its code.
	if (res.status === 401) {
		const said = (await res.json().catch(() => null)) as {
			code?: unknown;
			error?: unknown;
		} | null;
		if (lane === "seat" || said?.code === "person_session_ended")
			return ended(
				event,
				"person_session_ended",
				typeof said?.error === "string" ? said.error : undefined,
			);
		return json(401, said ?? {});
	}
	const out = new Headers();
	for (const name of RESPONSE_HEADERS) {
		const value = res.headers.get(name);
		if (value) out.set(name, value);
	}
	return new Response(res.body, { status: res.status, headers: out });
}
