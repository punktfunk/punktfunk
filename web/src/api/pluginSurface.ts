// The console's calls to a plugin's own surfaces: `/api/plugin-metadata`, `/api/plugin-config` and
// `/api/plugin-game`. Hand-written, since the plugin serves them and not the host API. Built on
// `apiFetch`, so a lost session goes to /login like every other call.
import { m } from "@/paraglide/messages";
import { ApiError, apiFetch } from "./fetcher";

/** A surface's refusal: the plugin's own `issue`, the route's `error`, and its 404 markers. */
interface SurfaceRefusal {
	issue?: string;
	/** A string from the route; `true` on an h3 error, whose text is `statusMessage`. */
	error?: string | boolean;
	statusMessage?: string;
	/** The plugin serves no `__config`: its settings are its own page. */
	noConfig?: boolean;
	/** The plugin has no section for this library entry. */
	noSection?: boolean;
}

export type SurfaceAnswer<T> =
	| { ok: true; status: number; body: T }
	| { ok: false; status: number; body: SurfaceRefusal | undefined };

/**
 * One call to a plugin surface, JSON both ways. A refusal comes back as `{ok: false, status, body}`
 * rather than a throw, so the caller can read a 404 marker. A network failure still throws.
 */
export async function pluginSurface<T>(
	url: string,
	send?: { method: "PUT" | "POST"; body: unknown },
): Promise<SurfaceAnswer<T>> {
	try {
		const body = await apiFetch<T>(
			url,
			send && {
				method: send.method,
				headers: { "content-type": "application/json" },
				body: JSON.stringify(send.body),
			},
		);
		return { ok: true, status: 200, body };
	} catch (e) {
		if (!(e instanceof ApiError)) throw e;
		const body =
			typeof e.data === "object" && e.data !== null
				? (e.data as SurfaceRefusal)
				: undefined;
		return { ok: false, status: e.status, body };
	}
}

/** Why a surface refused: the plugin's own issue first, then the route's error. */
export function refusalText(body: SurfaceRefusal | undefined): string {
	if (body?.issue) return body.issue;
	if (typeof body?.error === "string") return body.error;
	return body?.statusMessage || m.library_source_settings_refused();
}

/** {@link pluginSurface}, throwing the refusal's text: for a caller with no marker to read. */
export async function pluginSurfaceOrThrow<T>(
	url: string,
	send?: { method: "PUT" | "POST"; body: unknown },
): Promise<T> {
	const r = await pluginSurface<T>(url, send);
	if (!r.ok) throw new Error(refusalText(r.body));
	return r.body;
}
