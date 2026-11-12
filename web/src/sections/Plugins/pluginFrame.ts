/** What fills the plugin page below its header. */
export type PluginBody =
	| "unavailable"
	| "untrusted"
	| "offline"
	| "frame"
	| "probing";

/**
 * The plugin origin: `undefined` while the config loads, `null` when the plugin listener did
 * not bind, `""` under `vite dev` (same origin), else the plugin listener's origin.
 */
export type PluginOrigin = string | null | undefined;

/**
 * Which panel the page shows. No origin beats everything: the frame never falls back to the
 * console's origin. The frame waits for the origin probe to settle, but only when one runs: an
 * empty origin disables the probe, which then stays pending.
 */
export function pluginBody(
	origin: PluginOrigin,
	reachable: { isError: boolean; isPending: boolean },
	health: { isError: boolean; isSuccess: boolean },
): PluginBody {
	if (origin === null) return "unavailable";
	if (reachable.isError) return "untrusted";
	if (health.isError) return "offline";
	const probing = !!origin && reachable.isPending;
	return health.isSuccess && !probing && origin !== undefined
		? "frame"
		: "probing";
}

/**
 * Why a health answer is not a live plugin, or `null` when it is. A redirect is the gate sending
 * an expired session to /login, whose HTML would otherwise pass for a healthy plugin.
 */
export function healthFailure(r: {
	type: string;
	ok: boolean;
	status: number;
}): string | null {
	if (r.type === "opaqueredirect") return "session expired";
	return r.ok ? null : `health ${r.status}`;
}

/** Probe a plugin that is down more often, so it recovers on its own. */
export const healthIntervalMs = (status: string): number =>
	status === "error" ? 5_000 : 20_000;

/** What a message from the plugin frame asks of the console. */
export type FrameAction =
	| { kind: "theme" }
	| { kind: "navigate"; splat: string };

/**
 * The request in a `message` event, or `null` when it is not the plugin frame's own. Only the
 * frame's window may ask for the palette or move the address bar; its sandboxed page has an
 * opaque origin, so `event.origin` is always `"null"` and the window is the identity.
 */
export function frameAction(
	fromFrame: boolean,
	data: unknown,
): FrameAction | null {
	if (!fromFrame) return null;
	const msg = data as { type?: string; path?: unknown } | null | undefined;
	if (msg?.type === "pf-ui:theme-request") return { kind: "theme" };
	if (msg?.type === "pf-ui:navigate" && typeof msg.path === "string") {
		return { kind: "navigate", splat: msg.path.replace(/^\//, "") };
	}
	return null;
}
