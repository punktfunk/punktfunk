// The single server-side gate. Runs for EVERY request to the deployed Bun/Nitro server
// (pages, the /api proxy, everything) before routing. Unauthenticated requests are
// redirected to /login (page navigations) or rejected 401 (/api); the plugin listener takes a
// page path instead of the session, and a person's cookie its own gate (`personGate`). Fails
// CLOSED when no password is configured either way.
import {
	defineEventHandler,
	getCookie,
	getQuery,
	getRequestHeader,
	getRequestURL,
	type H3Event,
	sendRedirect,
	setResponseHeader,
	setResponseHeaders,
	setResponseStatus,
	useSession,
} from "h3";
import {
	authConfigured,
	csrfRequestOrigin,
	isCrossSiteMutation,
	peerAddress,
	SESSION_NAME,
	type SessionData,
	sessionConfig,
	sessionEpoch,
} from "../util/auth";
import { isPublicPath, safeNextPath } from "../util/paths";
import { forgetSession, isLoopbackPeer, personCookie } from "../util/person";
import { ended, personApi } from "../util/personApi";
import { personPluginPage } from "../util/personPlugins";
import {
	consoleOriginPort,
	consoleOriginScheme,
	frameAncestorSource,
	isPluginUiPath,
	listenerOf,
} from "../util/pluginOrigin";
import {
	PLUGIN_PAGE_CORS,
	PLUGIN_PAGE_SANDBOX,
	pluginPage,
	pluginPagePreflight,
} from "../util/pluginPage";

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
	//  - the plugin origin must not serve anything ELSE — above all not `/api/**`, where the BFF
	//    would attach the operator's admin bearer to whatever a plugin page sends there.
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
	// The plugin origin names the console as its only framer, where 'self' would name itself, and
	// sandboxes every document it serves so each plugin page gets an opaque origin of its own.
	setResponseHeader(
		event,
		"Content-Security-Policy",
		listener === "plugin"
			? `${PLUGIN_PAGE_SANDBOX}; frame-ancestors ${consoleFrameAncestor(event)}; object-src 'none'; base-uri 'self'`
			: "frame-ancestors 'self'; object-src 'none'; base-uri 'self'",
	);

	const method = event.method?.toUpperCase?.() ?? "GET";
	if (listener === "plugin") return pluginGate(event, pathname, method);

	// Mutating requests must be same-origin. Origin covers browsers that omit
	// Sec-Fetch-Site; Fetch-Site still rejects `same-site` (another port).
	// Applies to public paths too (login CSRF), before any session logic.
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

	const person = await personGate(event, pathname);
	if (person !== undefined) return person === PASS ? undefined : person;

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
	// Page navigation → bounce to the login screen, remembering where they were headed.
	return sendRedirect(
		event,
		`/login?next=${encodeURIComponent(pathname)}`,
		302,
	);
});

/** `personGate`'s word for "a person's request, let it through to its page or `/_auth/` route". */
const PASS = Symbol("pass");

/**
 * A browser holding a person's cookie is a person's console. Their `/api/**` is answered here by
 * personApi, so no route file ever runs for them, and the operator's session is not read. A
 * person cookie that ended gives way to an operator signed in on the same browser; without one,
 * the person sees that it ended. `undefined`: no person cookie, the operator's gate decides.
 */
async function personGate(
	event: H3Event,
	pathname: string,
): Promise<unknown | typeof PASS> {
	const person = personCookie(event);
	if (!person) return undefined;
	const api = pathname === "/api" || pathname.startsWith("/api/");
	if (person.session && isLoopbackPeer(peerAddress(event))) {
		event.context.person = person.session;
		if (api) return personApi(event, person.session);
		return PASS;
	}
	if (await operatorSignedIn(event)) {
		forgetSession(event);
		return undefined;
	}
	if (api) return ended(event, "person_session_ended");
	if (isPublicPath(pathname)) return PASS;
	forgetSession(event);
	return new Response(null, {
		status: 302,
		headers: { location: "/closed?why=ended" },
	});
}

/** A live operator session, read only when its cookie exists: `useSession` would mint one. */
async function operatorSignedIn(event: H3Event): Promise<boolean> {
	if (!authConfigured() || !getCookie(event, SESSION_NAME)) return false;
	const session = await useSession<SessionData>(event, sessionConfig());
	return (
		session.data.authenticated === true && session.data.epoch === sessionEpoch()
	);
}

/**
 * The plugin listener's gate: a page path minted for that plugin, by the operator's session or a
 * person's live one, and never a cookie, which a sandboxed page sends to some requests and not
 * others. Every refusal is the same 401. Requests come from `Origin: null`, so a valid path
 * answers CORS and its preflight, and needs no CSRF check: a page that cannot name the path
 * cannot reach it.
 */
function pluginGate(event: H3Event, pathname: string, method: string) {
	if (!authConfigured()) {
		setResponseStatus(event, 503);
		return { error: "auth not configured: set PUNKTFUNK_UI_PASSWORD_HASH" };
	}
	// A person's pages, like their console, answer this computer alone.
	const person = () =>
		isLoopbackPeer(peerAddress(event)) && personPluginPage(pathname) !== null;
	if (!pluginPage(pathname) && !person()) {
		setResponseStatus(event, 401);
		return { error: "unauthorized" };
	}
	setResponseHeaders(event, PLUGIN_PAGE_CORS);
	const asked = getRequestHeader(event, "access-control-request-method");
	if (method !== "OPTIONS" || !asked) return;
	setResponseHeaders(
		event,
		pluginPagePreflight(
			getRequestHeader(event, "access-control-request-headers"),
		),
	);
	return null;
}

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
