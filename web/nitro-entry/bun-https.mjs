// Custom Nitro server entry for the punktfunk web console.
//
// It is the stock Nitro `bun` preset entry
// (node_modules/nitropack/dist/presets/bun/runtime/bun.mjs) plus **TLS**, so the console is served
// over **HTTPS (HTTP/1.1 over TLS)** using the HOST's own identity cert (the cert native clients
// already pin). One trust anchor across the data plane, the management API, and this console. Wired
// in via `entry:` in vite.config.ts on top of Nitro's `bun` preset (which bundles the handler in).
//
// NOTE on HTTP/2 + HTTP/3: NOT offered here, on purpose. `Bun.serve` has no HTTP/2 server, and
// HTTP/3 (which Bun *can* do) is useless to a browser against this cert: QUIC refuses any cert error,
// and the host identity is SELF-SIGNED whichever pair we serve — the native one carries real SANs, so
// a browser gets past the name check, but never past the untrusted issuer (and the legacy fallback is
// CN-only with no SAN, which fails both). So browsers stay on HTTP/1.1 regardless — advertising h3 would just
// dangle an `Alt-Svc` no browser can use. Real h2/h3 would need a browser-TRUSTED, SAN-matching cert
// (a local CA installed per device) fronted by a server that speaks them (e.g. Caddy) — deliberately
// out of scope for a LAN console; TLS (no cleartext login/session) is the win.
//
// TWO LISTENERS, on purpose — see `PLUGIN ORIGIN` below.
//
// Env (set by the launchers / the systemd unit — see web.env.example):
//   PUNKTFUNK_UI_TLS_CERT / _KEY   PEM file paths (the host's cert.pem / key.pem — the native
//                                  sibling pair is preferred when present, see tls-paths.mjs).
//                                  BOTH set ⇒ HTTPS. Unset ⇒ plain HTTP (local dev only).
//   PORT / HOST                    standard Nitro bind (3000 / 0.0.0.0).
//   PUNKTFUNK_UI_PLUGIN_PORT       the plugin-UI origin's port (default: console port + 1).
import "#nitro-internal-pollyfills";
import wsAdapter from "crossws/adapters/bun";
import { useNitroApp } from "nitropack/runtime";
import { startScheduleRunner } from "nitropack/runtime/internal";
import { isLocalPeer } from "./peer-scope.mjs";
import { resolveUiTlsPaths } from "./tls-paths.mjs";

const nitroApp = useNitroApp();
const ws = import.meta._websocket
	? wsAdapter(nitroApp.h3App.websocket)
	: undefined;

// The socket peer, handed to the app as a trusted header.
//
// Nitro's `localFetch` (below) hands the app a SYNTHETIC request whose socket has no
// `remoteAddress`, so h3's `getRequestIP()` returns undefined *inside* the app and every
// per-peer decision collapses onto one shared bucket. That silently defeated the login
// throttle: five wrong passwords from anywhere locked out everyone, including the operator
// (and, since the update-apply route shares that budget, locked out host updates too).
// `server.requestIP(req)` is the only place the real peer is knowable, so we stamp it here.
// Any inbound copy is deleted first, so a client cannot forge it.
// Read back by `peerAddress()` in server/util/auth.ts — keep the two names in sync.
const PEER_IP_HEADER = "x-pf-peer-ip";

// PLUGIN ORIGIN — which listener a request arrived on, stamped the same unforgeable way.
//
// A plugin's UI used to be reverse-proxied onto the CONSOLE's own origin and framed with
// `allow-same-origin`, which means plugin JS ran as first-party code on the console origin: it
// could `fetch('/api/**', {credentials:'same-origin'})` and the BFF would attach the operator's
// ADMIN mgmt bearer. That reached everything `plugin_may_access` withholds — arm pairing, read the
// host PIN, approve a device, read `/hooks` — i.e. any plugin was one line of JS away from full
// operator admin (2026-08-05 review H-3). The "open in new tab" link was the same escalation with
// no iframe involved at all, so no sandbox attribute could have fixed it.
//
// The fix is to make the browser's own same-origin policy the boundary, by serving plugin UIs from
// a DIFFERENT ORIGIN: a second listener on its own port.
//
//   different ORIGIN — scheme+host+PORT — so SOP applies: plugin JS cannot read the console's DOM,
//                      and its cross-origin `fetch` of `/api/**` is unreadable (we emit no CORS) and
//                      unable to mutate (the Sec-Fetch-Site guard sees `same-site`, not
//                      `same-origin`).
//   same SITE        — because a cookie's scope ignores the port, and SameSite is computed on the
//                      site, not the origin. So the `SameSite=Lax` session cookie still flows to the
//                      plugin origin, and plugin pages keep loading their assets while logged in.
//
// That combination is why this works and why the obvious alternative does not: dropping
// `allow-same-origin` gives the frame an OPAQUE origin, which makes its subresource requests
// cross-site, which stops the Lax cookie, which 302s every plugin asset to /login — a blank frame.
//
// The console listener refuses `/plugin-ui/**` and the plugin listener refuses everything else
// (server/middleware/auth.ts). Both halves matter: without the first the old path still works;
// without the second, plugin JS could call `/api/**` on its OWN origin and get the admin bearer
// attached right back.
const LISTENER_HEADER = "x-pf-listener";

// TLS from the host's identity cert (file PATHS → Bun.file, not PEM-in-env). Absent ⇒ plain HTTP.
//
// The launchers all name the LEGACY cert.pem/key.pem pair and cannot express a fallback, so the
// choice between the host's two identities is made here — see tls-paths.mjs for why the native
// pair is the right one to serve (SANs a browser accepts; the cert the tray and native clients
// already pin).
const { cert: certPath, key: keyPath } = resolveUiTlsPaths(
	process.env.PUNKTFUNK_UI_TLS_CERT,
	process.env.PUNKTFUNK_UI_TLS_KEY,
);
const tls =
	certPath && keyPath
		? { cert: Bun.file(certPath), key: Bun.file(keyPath) }
		: undefined;

// Half-configured TLS is not a warning, it is a refusal.
//
// Two silent failures hide here, and both end with the operator staring at a console that looks
// fine. One path set and the other missing drops to plain HTTP — the login password then crosses
// the LAN in the clear on a server the operator believes is TLS. And PUNKTFUNK_UI_SECURE without
// TLS marks the session cookie Secure, which a browser refuses to store over http://, so login
// "succeeds" and every request after it is unauthenticated, forever.
//
// Neither state can serve a working console, so exiting is strictly better than serving a broken
// one: a supervisor logs the reason and the operator sees a stopped service instead of a subtly
// wrong one.
const secureFlag = /^(1|true)$/i.test(process.env.PUNKTFUNK_UI_SECURE ?? "");
if (Boolean(certPath) !== Boolean(keyPath)) {
	console.error(
		`punktfunk web console: only ${certPath ? "PUNKTFUNK_UI_TLS_CERT" : "PUNKTFUNK_UI_TLS_KEY"} is set — ` +
			"TLS needs BOTH. Refusing to start rather than serve the login password in the clear.",
	);
	process.exit(1);
}
if (!tls && secureFlag) {
	console.error(
		"punktfunk web console: PUNKTFUNK_UI_SECURE is set but TLS is not configured. The session " +
			"cookie would be marked Secure and dropped by the browser over http://, so login could " +
			"never stick. Refusing to start — set PUNKTFUNK_UI_TLS_CERT/_KEY, or unset PUNKTFUNK_UI_SECURE.",
	);
	process.exit(1);
}

// Where BOTH listeners bind. `PUNKTFUNK_UI_BIND` (host.env, the file every other host setting
// lives in) first, then whatever a launcher set, then every interface. `fetch` refuses any peer
// off the local network, so a wide bind is the LAN, never the internet. The plugin-UI origin
// inherits both through `listenerOptions`, so 47993 is never wider than 47992.
const bind =
	process.env.PUNKTFUNK_UI_BIND?.trim() ||
	process.env.NITRO_HOST ||
	process.env.HOST ||
	"0.0.0.0";
// Read back by the app (server/routes/_auth/ui-config.get.ts) so Settings can say when the console
// is reachable from the network. Set here, never trusted from the environment we started with.
process.env.PUNKTFUNK_UI_BIND_ACTIVE = bind;

/** The shared `Bun.serve` options both listeners use — only the port and the stamped lane differ. */
const listenerOptions = (lane) => ({
	host: bind,
	// Bun defaults this to 10 s, which is SHORTER than the host's 15 s SSE keep-alive comment — so a
	// proxied `/api/v1/events` stream (or any other quiet long-lived response) gets cut by us and
	// reconnects on a loop. 120 s is comfortably above any keep-alive we forward; still overridable.
	idleTimeout: Number.parseInt(process.env.NITRO_BUN_IDLE_TIMEOUT, 10) || 120,
	// Cap the request body an UNAUTHENTICATED peer can make us hold in memory.
	//
	// `fetch` below buffers the whole body with `await req.arrayBuffer()` before Nitro — and
	// therefore before the auth gate — has seen the request, so Bun's 128 MB default was the only
	// bound on what a LAN peer could push into console RSS by POSTing to /login (2026-08-05 review
	// L-10). Nothing the console legitimately accepts is remotely this large: the biggest real body
	// is a hooks/library JSON edit, kilobytes. 4 MiB leaves several orders of headroom and still
	// makes the memory cost of an unauthenticated request negligible.
	maxRequestBodySize:
		Number.parseInt(process.env.NITRO_BUN_MAX_BODY_BYTES, 10) ||
		4 * 1024 * 1024,
	// `tls: undefined` ⇒ plain HTTP (dev); otherwise HTTPS over HTTP/1.1.
	tls,
	websocket: import.meta._websocket ? ws.websocket : undefined,
	async fetch(req, server) {
		// Before the body is buffered or a socket upgraded: an internet peer gets nothing to hold.
		const peer = server.requestIP(req)?.address;
		if (!isLocalPeer(peer)) {
			return new Response(
				"This console only answers on its own network. Connect from that network or over a VPN.\n",
				{ status: 403 },
			);
		}
		if (import.meta._websocket && req.headers.get("upgrade") === "websocket") {
			return ws.handleUpgrade(req, server);
		}
		const url = new URL(req.url);
		let body;
		if (req.body) {
			body = await req.arrayBuffer();
		}
		// Strip any client-supplied value BEFORE stamping the real one (see PEER_IP_HEADER).
		const headers = new Headers(req.headers);
		headers.delete(PEER_IP_HEADER);
		headers.delete(LISTENER_HEADER);
		headers.set(PEER_IP_HEADER, peer);
		headers.set(LISTENER_HEADER, lane);
		return nitroApp.localFetch(url.pathname + url.search, {
			host: url.hostname,
			protocol: url.protocol,
			headers,
			method: req.method,
			redirect: req.redirect,
			body,
		});
	},
});

const consolePort = Number(process.env.NITRO_PORT || process.env.PORT || 3000);
const server = Bun.serve({ ...listenerOptions("console"), port: consolePort });
console.log(
	`punktfunk web console listening on ${server.url} (tls=${!!tls}, bind=${bind})`,
);
// Name the bind out loud on a loopback listener. A console that answers only here looks exactly
// like one that is down when you try it from another device, and this line is the difference
// between "it broke" and "it is configured that way". Same rule as `isLoopbackBind`
// (server/util/pluginOrigin.ts), which decides whether Settings shows the network notice.
if (/^(127\.|::1$|localhost$)/.test(bind)) {
	console.log(
		"punktfunk web console: this machine only. To reach it from your network, put " +
			"PUNKTFUNK_UI_BIND=0.0.0.0 in host.env and restart the console.",
	);
}

// The plugin-UI origin. Its own port, everything else identical.
//
// A bind failure does NOT fall back to serving plugin UIs on the console origin — that is the hole
// this exists to close, and a security boundary that disappears when a port is busy is not one. It
// degrades to "plugin UIs unavailable": the console reads the state below and renders an
// explanation instead of a frame, and everything else about the console keeps working.
const pluginPort = Number(
	process.env.PUNKTFUNK_UI_PLUGIN_PORT || consolePort + 1,
);
let pluginServer;
try {
	pluginServer = Bun.serve({ ...listenerOptions("plugin"), port: pluginPort });
	// Read back by the app (server/util/pluginOrigin.ts) — same process, so process.env is the
	// simplest channel, and it is only ever SET here, never trusted from the environment we started
	// with (a stale inherited value would otherwise advertise a port nothing is listening on).
	process.env.PUNKTFUNK_UI_PLUGIN_PORT_ACTIVE = String(pluginPort);
	process.env.PUNKTFUNK_UI_CONSOLE_PORT_ACTIVE = String(consolePort);
	// …and the SCHEME, which the app cannot recover from a request: `localFetch` synthesises one with
	// no TLS socket, so h3 reports `http:` on an HTTPS listener. `frame-ancestors` needs the scheme
	// the operator's address bar actually shows, or the browser refuses to frame the plugin at all
	// (see consoleOriginScheme() in server/util/pluginOrigin.ts). Both listeners share this `tls`
	// object, so one stamp is correct for both.
	process.env.PUNKTFUNK_UI_SCHEME_ACTIVE = tls ? "https" : "http";
	console.log(
		`punktfunk plugin-UI origin listening on ${pluginServer.url} (tls=${!!tls})`,
	);
} catch (e) {
	delete process.env.PUNKTFUNK_UI_PLUGIN_PORT_ACTIVE;
	console.error(
		`punktfunk web console: could not bind the plugin-UI origin on port ${pluginPort} ` +
			`(${e?.message ?? e}). Plugin UIs are DISABLED until this is resolved — they are ` +
			"deliberately not served on the console's own origin, because a plugin sharing that " +
			"origin can act as the logged-in operator. Set PUNKTFUNK_UI_PLUGIN_PORT to a free port.",
	);
}

if (import.meta._tasks) {
	startScheduleRunner();
}
