// The single server-side gate. Runs for EVERY request to the deployed Bun/Nitro server
// (pages, the /api proxy, everything) before routing. Unauthenticated requests are
// redirected to /login (page navigations) or rejected 401 (/api). Fails CLOSED when no password
// is configured either way, so a misconfigured LAN-exposed server admits no one.
import {
	defineEventHandler,
	getCookie,
	getQuery,
	getRequestHeader,
	getRequestURL,
	type H3Event,
	sendRedirect,
	setResponseHeader,
	setResponseStatus,
	useSession,
} from "h3";
import {
	authConfigured,
	csrfRequestOrigin,
	isCrossSiteMutation,
	SESSION_NAME,
	type SessionData,
	sessionConfig,
	sessionEpoch,
} from "../util/auth";
import { isPublicPath, safeNextPath } from "../util/paths";
import {
	consoleOriginPort,
	consoleOriginScheme,
	frameAncestorSource,
	isPluginUiPath,
	listenerOf,
} from "../util/pluginOrigin";

export default defineEventHandler(async (event) => {
	const url = getRequestURL(event);
	const { pathname } = url;
	const listener = listenerOf(event);
	const isPluginPath = isPluginUiPath(pathname);

	// ── the origin split (2026-08-05 review H-3) ────────────────────────────────────────────────
	//
	// Plugin UIs live on their own origin (see nitro-entry/bun-https.mjs). Enforcing that is two
	// refusals, and BOTH are load-bearing:
	//
	//  - the console origin must not serve `/plugin-ui/**`, or the old same-origin path still works
	//    and nothing has changed;
	//  - the plugin origin must not serve anything ELSE — above all not `/api/**`. Plugin JS is
	//    same-origin with the plugin listener, so if that listener proxied `/api/**` the BFF would
	//    attach the operator's admin bearer to the plugin's own fetch and hand back exactly the
	//    escalation we just moved.
	//
	// Unconditional, not conditional on the plugin listener having bound: if it did not, plugin UIs
	// are disabled and refusing here is the correct answer, not a reason to fall back. (`vite dev`
	// serves one origin, but its own middleware answers `/plugin-ui` before Nitro is reached, so
	// this never fires there.)
	if (listener === "console" && isPluginPath) {
		setResponseStatus(event, 404);
		return { error: "plugin UIs are served from their own origin" };
	}
	if (listener === "plugin" && !isPluginPath) {
		setResponseStatus(event, 404);
		return { error: "this origin serves plugin UIs only" };
	}

	// Baseline response headers for everything this server emits. Deliberately modest: a plugin's
	// own UI is third-party code we don't control, so a script-src policy tight enough to be worth
	// having would break the pages it serves. What is safe to assert unconditionally still closes
	// the cheap holes:
	//   nosniff        — a plugin serving text/plain that "looks like" HTML can't be sniffed into it
	//   frame-ancestors— who may frame this; see below, it differs per origin
	//   object-src     — no Flash/applet embedding anywhere
	//   base-uri       — a stray <base> can't repoint every relative URL on the page
	//   Referrer-Policy— never leak a console path (which can carry ids) to an external homepage link
	setResponseHeader(event, "X-Content-Type-Options", "nosniff");
	setResponseHeader(event, "Referrer-Policy", "no-referrer");
	// `frame-ancestors 'self'` is right for the console and WRONG for the plugin origin: 'self'
	// there means the plugin origin, and the console — now a different origin — is precisely who
	// needs to frame it. So the plugin origin names the console explicitly, and nobody else.
	setResponseHeader(
		event,
		"Content-Security-Policy",
		`frame-ancestors ${listener === "plugin" ? consoleFrameAncestor(event) : "'self'"}; object-src 'none'; base-uri 'self'`,
	);

	// Mutating requests must be same-origin. Origin covers browsers that omit
	// Sec-Fetch-Site; Fetch-Site still rejects `same-site` (another port).
	// Applies to public paths too (login CSRF), before any session logic.
	const method = event.method?.toUpperCase?.() ?? "GET";
	if (
		isCrossSiteMutation({
			method,
			fetchSite: getRequestHeader(event, "sec-fetch-site"),
			origin: getRequestHeader(event, "origin"),
			requestOrigin: csrfRequestOrigin({
				forwardedProto: getRequestHeader(event, "x-forwarded-proto"),
				listenerScheme: consoleOriginScheme(),
				requestScheme: url.protocol,
				host: url.host,
			}),
		})
	) {
		setResponseStatus(event, 403);
		return { error: "cross-site request refused" };
	}

	// A signed-in visitor on the login page is sent on to `next`. This is also what makes a login
	// land when the browser lost the client's redirect: Firefox aborts in-flight chunk imports on
	// navigation, and the stale-chunk recovery then reloads /login over the target.
	//
	// Gated on the cookie EXISTING, not just on the path: `useSession` issues a sealed one when it
	// finds none, and the login page is the one place a visitor with no session is expected.
	if (
		pathname === "/login" &&
		authConfigured() &&
		getCookie(event, SESSION_NAME)
	) {
		const session = await useSession<SessionData>(event, sessionConfig());
		if (session.data.authenticated && session.data.epoch === sessionEpoch()) {
			const next = getQuery(event).next;
			return sendRedirect(
				event,
				safeNextPath(typeof next === "string" ? next : undefined),
				302,
			);
		}
	}

	if (isPublicPath(pathname)) return;

	// Misconfigured: refuse everything rather than serve open on the LAN.
	if (!authConfigured()) {
		setResponseStatus(event, 503);
		return { error: "auth not configured: set PUNKTFUNK_UI_PASSWORD_HASH" };
	}

	const session = await useSession<SessionData>(event, sessionConfig());
	// The epoch check is what makes logout mean something: a cookie sealed before the last
	// revocation unseals fine but no longer matches, so it is refused like any other bad session.
	if (session.data.authenticated && session.data.epoch === sessionEpoch())
		return; // authenticated — let it through

	if (pathname.startsWith("/api")) {
		setResponseStatus(event, 401);
		return { error: "unauthorized" };
	}
	// The plugin origin has no /login to bounce to — it serves plugin UIs and nothing else, so a
	// redirect there would land on this middleware's own 404. Answer plainly instead; the console
	// probes plugin liveness server-side and renders the session-expired state itself.
	if (listener === "plugin") {
		setResponseStatus(event, 401);
		return { error: "unauthorized" };
	}
	// Page navigation → bounce to the login screen, remembering where they were headed.
	return sendRedirect(
		event,
		`/login?next=${encodeURIComponent(pathname)}`,
		302,
	);
});

/**
 * The console origin, as a `frame-ancestors` source, derived from the request the PLUGIN origin is
 * answering: the hostname is whatever name the operator actually browsed to (an IP, an mDNS name, a
 * hostname — so the policy matches their address bar), plus the console's port and scheme.
 *
 * ⚠ The scheme must NOT come from the request. `getRequestURL` reports `http:` on an HTTPS listener
 * here — Nitro hands the app a synthetic request with no TLS socket — so this named
 * `http://host:47992` as the only permitted ancestor of a console the operator was reading over
 * HTTPS, and every plugin UI came up as an empty panel with `ERR_BLOCKED_BY_RESPONSE`. It is taken
 * from the listener's own TLS state instead (`consoleOriginScheme`), with `x-forwarded-proto`
 * winning when something in front terminated TLS for us — that is the one case where the browser's
 * scheme differs from this process's.
 *
 * Falls back to `'none'` rather than `'self'` or `*` when the console port is unknown: an unframable
 * plugin page is a visible, harmless failure, and the alternatives are a policy that either does
 * nothing or lets any page on the LAN frame a logged-in plugin UI.
 */
function consoleFrameAncestor(event: H3Event): string {
	const port = consoleOriginPort();
	if (!port) return "'none'";
	const url = getRequestURL(event);
	return frameAncestorSource({
		forwardedProto: getRequestHeader(event, "x-forwarded-proto"),
		listenerScheme: consoleOriginScheme(),
		requestScheme: url.protocol,
		hostname: url.hostname,
		port,
	});
}
