// A person's own plugin pages: the plugins their seat host runs, served on the plugin listener as
// the operator's are. A page path is signed with the session's own key, so it dies with the
// session. The plugin's `{port, secret}` comes from their seat host on their session and stays
// in this process; a plugin with no listener of its own is reached through that host's relay.
import { createHmac } from "node:crypto";
import { safeEqualHex } from "./handoff";
import {
	isTimeout,
	liveSessions,
	type PersonSession,
	SeatGone,
	seatCall,
} from "./person";
import { type PluginPage, pageParts } from "./pluginPage";
import {
	isDialablePort,
	type PersonReach,
	PLUGIN_ID_RE,
	type UiCredential,
} from "./pluginProxy";

const tag = (session: PersonSession, id: string, exp: string): string =>
	createHmac("sha256", session.pageKey)
		.update(`pf-plugin-ui:person:v1:${id}:${exp}`)
		.digest("hex")
		.slice(0, 32);

/** `/plugin-ui/<segment>/` for plugin `id` in `session`, good while the session lives. */
export function mintPersonPagePath(id: string, session: PersonSession): string {
	const exp = Math.ceil(session.expiresAt / 1000).toString(36);
	return `/plugin-ui/${id}-${exp}-${tag(session, id, exp)}/`;
}

/** The person page `pathname` names, and whose session signed it; `null` for none live. */
export function personPluginPage(
	pathname: string,
	nowMs = Date.now(),
): (PluginPage & { session: PersonSession }) | null {
	const page = pageParts(pathname, nowMs);
	if (!page) return null;
	const session = liveSessions(nowMs).find((s) =>
		safeEqualHex(page.mac, tag(s, page.id, page.exp)),
	);
	if (!session) return null;
	return { id: page.id, prefix: page.prefix, rest: page.rest, session };
}

const TTL_MS = 15_000;
const cache = new WeakMap<
	PersonSession,
	Map<string, { cred: UiCredential | null; at: number }>
>();

/**
 * Plugin `id`'s UI credential on the session's seat host, cached as the operator's is. Port 0 is a
 * plugin that host relays itself. `null`: not running, no UI, or a port the console won't dial.
 */
export async function personUiCredential(
	session: PersonSession,
	id: string,
	{ bustCache }: { bustCache: boolean },
): Promise<UiCredential | null> {
	if (!PLUGIN_ID_RE.test(id)) return null;
	const now = Date.now();
	const kept = cache.get(session) ?? new Map();
	cache.set(session, kept);
	const hit = kept.get(id);
	if (!bustCache && hit && now - hit.at < TTL_MS) return hit.cred;
	let res: Response;
	try {
		res = await seatCall(session, `/api/v1/plugins/${id}/ui-credential`, {
			signal: AbortSignal.timeout(5_000),
		});
	} catch (e) {
		if (e instanceof SeatGone || isTimeout(e)) return null;
		throw e;
	}
	if (!res.ok) {
		if (res.status === 404) kept.set(id, { cred: null, at: now });
		return null;
	}
	const body = (await res
		.json()
		.catch(() => null)) as Partial<UiCredential> | null;
	const port = body?.port;
	const secret = body?.secret;
	const cred =
		typeof port === "number" &&
		(port === 0 || isDialablePort(port)) &&
		typeof secret === "string" &&
		secret !== ""
			? { port, secret }
			: null;
	kept.set(id, { cred, at: now });
	return cred;
}

/** Plugin `id` as the session's person reaches it ({@link PersonReach}). */
export const personReach = (
	session: PersonSession,
	id: string,
): PersonReach => ({
	credential: (opts) => personUiCredential(session, id, opts),
	// A page may stream: no deadline, as the operator's relay has none.
	relay: (path, init) =>
		seatCall(session, path, { ...init, signal: new AbortController().signal }),
});
