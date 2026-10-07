// The one HTTP implementation both surfaces share (RFC §7: two surfaces, one core): the
// Effect service wraps these with typed errors; the Promise facade calls them directly.
//
// Takes a `Connection`, not a resolved config: this file must not know how the credential was
// obtained, only how to present it — and how to present it again when the host says 401 and the
// credential is the kind that can be re-earned.
import type { Connection } from "./connection.js";

/** A non-2xx response, with the host's `ApiError` envelope message when present. */
export class HttpStatusError extends Error {
	constructor(
		readonly status: number,
		message: string,
	) {
		super(message);
	}
}

/**
 * One management-API request under `/api/v1`. Returns the parsed JSON body (or `undefined`
 * for 204/empty). Throws [`HttpStatusError`] on a non-2xx (401 included — callers type it).
 */
export const httpRequest = async (
	cfg: Connection,
	method: string,
	apiPath: string,
	body?: unknown,
	retryOn401 = cfg.credential.kind === "device",
): Promise<unknown> => {
	const headers: Record<string, string> = {};
	const auth = await cfg.credential.header();
	if (auth) headers.authorization = auth;
	if (body !== undefined) headers["content-type"] = "application/json";
	const resp = await cfg.fetch(`${cfg.url}/api/v1${apiPath}`, {
		method,
		headers,
		body: body !== undefined ? JSON.stringify(body) : undefined,
	});
	// A device token can lapse, or the host can restart and forget every token it issued. One
	// re-earned credential and one retry; then the caller sees the 401.
	if (resp.status === 401 && retryOn401) {
		cfg.credential.invalidate();
		return httpRequest(cfg, method, apiPath, body, false);
	}
	if (!resp.ok) {
		let message = `HTTP ${resp.status}`;
		try {
			const err = (await resp.json()) as { error?: string };
			if (typeof err.error === "string") message = err.error;
		} catch {
			// non-JSON error body — keep the status message
		}
		throw new HttpStatusError(resp.status, message);
	}
	if (resp.status === 204) return undefined;
	const text = await resp.text();
	return text.length === 0 ? undefined : JSON.parse(text);
};
