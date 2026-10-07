// The fetch mutator orval-generated hooks call: `apiFetch<T>(url, RequestInit)`. orval is
// configured (includeHttpResponseReturnType: false) so `T` is the response BODY; on an HTTP
// error we THROW an `ApiError` so React Query's `isError` works (the query client skips
// retries on 4xx — see src/router.tsx).
//
// Auth: requests are same-origin to `/api/...`; the browser sends only the session cookie
// (the server-side proxy injects the management bearer token — the token never lives in the
// browser). The auth middleware's 401 body means the session is gone → bounce to /login.
// Password-confirmed actions also use 401, but leave the valid session in place.

/** A failed API call. `status` is the HTTP code; `data` is the parsed `ApiError` body if any. */
export class ApiError extends Error {
	status: number;
	data: unknown;
	constructor(status: number, data: unknown, message?: string) {
		super(message ?? `API error ${status}`);
		this.name = "ApiError";
		this.status = status;
		this.data = data;
	}
}

const V1 = "/api/v1/";

/** The profile whose seat the call being started belongs to; `null` is this box. */
let seat: string | null = null;

/** `/api/v1/x` as the box forwards it to a seat's own host; any other URL is the box's. */
export function seatUrl(url: string, id: string | null): string {
	if (!id || !url.startsWith(V1)) return url;
	return `${V1}profiles/${encodeURIComponent(id)}/proxy/${url.slice(V1.length)}`;
}

/**
 * Runs `fn` with each `apiFetch` it starts aimed at seat `id` (`null`: the box).
 *
 * The seat is read as `apiFetch` begins, so the call must start before `fn`'s first await.
 */
export function inSeat<T>(id: string | null, fn: () => T): T {
	const outer = seat;
	seat = id;
	try {
		return fn();
	} finally {
		seat = outer;
	}
}

export async function apiFetch<T>(
	url: string,
	options?: RequestInit,
): Promise<T> {
	const headers = new Headers(options?.headers);
	headers.set("Accept", "application/json");

	const res = await fetch(seatUrl(url, seat), {
		...options,
		headers,
		credentials: "same-origin",
	});

	const text = await res.text();
	const body = text ? safeJson(text) : undefined;
	if (res.status === 401 && isSessionUnauthorized(body)) redirectToLogin();
	if (!res.ok) throw new ApiError(res.status, body, res.statusText);
	return body as T;
}

/** The auth middleware's exact refusal, distinct from a wrong password confirmation. */
function isSessionUnauthorized(body: unknown): boolean {
	return (
		typeof body === "object" &&
		body !== null &&
		"error" in body &&
		body.error === "unauthorized"
	);
}

/**
 * On lost session, send the user to the login screen, remembering where they were.
 *
 * Deferred by a beat rather than navigating inline. This runs inside whichever call noticed the
 * 401 — very often a background poll the user never asked for — and a synchronous
 * `location.href =` there tears the page down mid-render, taking any unsaved editing state with it
 * (the Displays page models exactly such a draft). Letting the current task finish first means the
 * caller's own error handling still runs, and a `beforeunload` guard can still speak up.
 *
 * Guarded so a burst of parallel 401s (every card on a page polling at once) schedules one
 * navigation, not one per request.
 */
let redirecting = false;
function redirectToLogin(): void {
	if (typeof window === "undefined") return;
	if (window.location.pathname === "/login") return;
	if (redirecting) return;
	redirecting = true;
	// Keep the full path (query + hash too), so re-login returns to the exact view.
	const next = encodeURIComponent(
		window.location.pathname + window.location.search + window.location.hash,
	);
	setTimeout(() => {
		window.location.href = `/login?next=${next}`;
	}, 0);
}

function safeJson(text: string): unknown {
	try {
		return JSON.parse(text);
	} catch {
		return text;
	}
}

export default apiFetch;
