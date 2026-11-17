// The one outbound-HTTP helper a library plugin should use, with the posture of the host's art
// fetch (`fetch_art`, library/art.rs): **no redirects**, a size cap, a short timeout, and only
// http(s) URLs.
//
// The no-redirect rule is the important one and it is not paranoia: a scanner fetches URLs it read
// out of a launcher's cache — data the plugin did not author. A `3xx` chased automatically is an
// SSRF pivot from a process running on the operator's box (`http://169.254.169.254/…`, an internal
// service). The host refuses redirects for the same reason, and a plugin fetching the same class
// of URL follows the same rule. A rare legitimately-redirecting CDN just yields no art.

import { Effect } from "effect";
import { HostRequestError } from "../../errors.js";

export interface FetchLimits {
	/** Hard cap on the response body. Default 8 MiB — a cover never approaches it. */
	readonly maxBytes?: number;
	/** Wall-clock timeout in ms. Default 10 000. */
	readonly timeoutMs?: number;
}

const DEFAULT_MAX = 8 * 1024 * 1024;
const DEFAULT_TIMEOUT = 10_000;

export interface FetchedBytes {
	readonly bytes: Uint8Array;
	readonly contentType: string;
}

/**
 * GET an `http(s)` URL under the posture above. Fails with {@link HostRequestError} on any non-2xx,
 * a redirect, an empty or over-cap body, a timeout, or a non-http(s) scheme.
 *
 * Most scanners never need this: they emit CDN URLs and let the CLIENT fetch them, which is both
 * faster and keeps the host out of the loop. Reach for it only when a store's art requires an API
 * lookup the client cannot do (GOG's product API, Microsoft's display catalog).
 */
export const fetchBytes = (
	url: string,
	limits: FetchLimits = {},
): Effect.Effect<FetchedBytes, HostRequestError> =>
	Effect.tryPromise({
		try: async (): Promise<FetchedBytes> => {
			if (!/^https?:\/\//i.test(url)) {
				throw new Error("only http(s) URLs may be fetched");
			}
			const maxBytes = limits.maxBytes ?? DEFAULT_MAX;
			const signal = AbortSignal.timeout(limits.timeoutMs ?? DEFAULT_TIMEOUT);
			// `redirect: "manual"` rather than "error": we want to SEE the 3xx and report it as a
			// refusal, not have fetch throw something opaque.
			const res = await fetch(url, { redirect: "manual", signal });
			if (res.status >= 300 && res.status < 400) {
				throw new Error(`refusing to follow a ${res.status} redirect`);
			}
			if (!res.ok) throw new Error(`HTTP ${res.status}`);
			// Trust Content-Length when it is there (cheap rejection), but still check the bytes
			// received: a hostile server can lie about it or omit it entirely.
			const declared = Number(res.headers.get("content-length"));
			if (Number.isFinite(declared) && declared > maxBytes) {
				throw new Error(`body larger than ${maxBytes} bytes`);
			}
			const buf = new Uint8Array(await res.arrayBuffer());
			if (buf.byteLength === 0) throw new Error("empty body");
			if (buf.byteLength > maxBytes) {
				throw new Error(`body larger than ${maxBytes} bytes`);
			}
			return {
				bytes: buf,
				contentType: res.headers.get("content-type") ?? "image/jpeg",
			};
		},
		catch: (cause) =>
			new HostRequestError({
				method: "GET",
				path: url,
				cause,
			}),
	});

/** {@link fetchBytes}, JSON-decoded. Same posture; use for a store's public product API. */
export const fetchJson = <T = unknown>(
	url: string,
	limits: FetchLimits = {},
): Effect.Effect<T, HostRequestError> =>
	fetchBytes(url, limits).pipe(
		Effect.flatMap((r) =>
			Effect.try({
				try: () => JSON.parse(new TextDecoder().decode(r.bytes)) as T,
				catch: (cause) =>
					new HostRequestError({
						method: "GET",
						path: url,
						cause,
					}),
			}),
		),
	);
