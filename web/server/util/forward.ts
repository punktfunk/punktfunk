// Calls to the management API. Every one goes through `mgmtFetch`, and the two relays share its
// pieces in util/mgmt.ts: server-side bearer injection, the loopback TLS pin, and 401 → 502 so a
// host-token misconfiguration can't bounce a logged-in user into a redirect loop.
import {
	createError,
	type H3Event,
	setResponseHeader,
	setResponseStatus,
} from "h3";
import {
	assertHostTokenAccepted,
	loopbackTls,
	mgmtBearer,
	mgmtUrl,
} from "./mgmt";
import { seatPath } from "./seatProxy";

/**
 * Every field of a host request model, each present and possibly `undefined`. A password route
 * rebuilds its upstream body as `{…} satisfies AllFields<Model>`, so `tsc` fails when the host
 * grows a field the rebuild would strip. `JSON.stringify` drops the `undefined` ones.
 */
export type AllFields<T> = { [K in keyof Required<T>]: T[K] | undefined };

/**
 * One call to `path` on the management API, with the host's bearer and the loopback TLS pin.
 * Throws 503 with no token, and 502 when the host is unreachable or rejects the token: a dead
 * host is not a console bug, and an escaped rejection would surface as a bare 500.
 */
export async function mgmtFetch(
	path: string,
	init: RequestInit = {},
): Promise<Response> {
	const base = mgmtUrl();
	const headers = new Headers(init.headers);
	headers.set("authorization", mgmtBearer());
	let res: Response;
	try {
		// `tls` is a Bun.fetch extension, pinned per request and never process-wide.
		res = await fetch(`${base}${path}`, {
			...init,
			headers,
			...loopbackTls(base),
		});
	} catch (cause) {
		throw createError({
			statusCode: 502,
			statusMessage: "management API unreachable",
			cause,
		});
	}
	assertHostTokenAccepted(res);
	return res;
}

/** Forward a JSON body to `path` on the management API and relay the upstream response verbatim.
 * Omit `body` for a bodiless method (GET) — a read whose RESPONSE we rewrite. A request that
 * came in for a seat (`event.context.seat`) goes to that seat's host. */
export async function forwardJson(
	event: H3Event,
	path: string,
	method: string,
	body?: unknown,
): Promise<string> {
	const seat: string | undefined = event.context.seat;
	const upstream = await mgmtFetch(
		seat ? seatPath(seat, path.slice("/api/v1/".length)) : path,
		{
			method,
			...(body === undefined
				? {}
				: {
						headers: { "content-type": "application/json" },
						body: JSON.stringify(body),
					}),
		},
	);
	setResponseStatus(event, upstream.status);
	setResponseHeader(event, "content-type", "application/json");
	return upstream.text();
}
