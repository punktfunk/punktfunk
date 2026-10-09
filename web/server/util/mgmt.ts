// The hop to the management API: where it listens, the bearer the BFF holds for it, and the loopback
// TLS pin. The token never reaches the browser: `relayToMgmt` and forward.ts's `mgmtFetch` inject
// it server-side.
import { readFileSync } from "node:fs";
import { createError, type H3Event, proxyRequest } from "h3";
import { dirPrefix } from "../../nitro-entry/tls-paths.mjs";

/** The management API the proxy forwards to (loopback by default — never LAN-exposed). It serves
 * HTTPS with the host's self-signed identity cert, so the proxy relaxes verification for that ONE
 * loopback hop via Bun's per-request `tls` option (`relayToMgmt`, util/forward.ts). There is
 * deliberately no process-wide NODE_TLS_REJECT_UNAUTHORIZED — see .env.example. */
export function mgmtUrl(): string {
	// Blank counts as UNSET, which `??` alone would not do. On a packaged install this value comes
	// from ~/.config/punktfunk/mgmt-endpoint (written by the host's `serve` with the port it really
	// bound, so a host moved off 47990 to coexist with a Sunshine fork carries the console with it),
	// sourced as a systemd EnvironmentFile. An empty or truncated file would otherwise set the
	// variable to "" and send every proxy hop to a URL that cannot parse.
	const url = process.env.PUNKTFUNK_MGMT_URL?.trim();
	return url ? url : "https://127.0.0.1:47990";
}

/** Bearer token for the management API, injected server-side. */
export function mgmtToken(): string {
	return process.env.PUNKTFUNK_MGMT_TOKEN ?? "";
}

/** Bun's per-request `tls` for the loopback hop to the management API, or `undefined` to
 * verify normally (a non-loopback `PUNKTFUNK_MGMT_URL` must present a real chain).
 *
 * Loopback is pinned, not relaxed: the host's identity certs are the CA and the name check is
 * off, since the legacy cert carries no SAN. Whatever answers on the port must hold the host's
 * private key, so a squatter on 127.0.0.1 never sees the bearer token. When neither cert is
 * readable the old relaxation stays, with one warning, rather than a console that cannot load. */
export function loopbackTls(base: string): { tls: object } | undefined {
	if (!isLoopbackUrl(base)) return undefined;
	const ca = hostIdentityCerts();
	if (ca.length === 0) {
		if (!warnedUnpinned) {
			warnedUnpinned = true;
			console.warn(
				"[punktfunk-web] PUNKTFUNK_UI_TLS_CERT names no readable host cert — the loopback hop to the management API is not pinned",
			);
		}
		return { tls: { rejectUnauthorized: false } };
	}
	return {
		tls: {
			ca,
			rejectUnauthorized: true,
			checkServerIdentity: () => undefined,
		},
	};
}
let warnedUnpinned = false;

/** The host's identity cert PEMs, the one the management API serves FIRST: the
 * `native-cert.pem` sibling of the `cert.pem` the launcher names when it exists (a host that
 * took the identity split serves it), then the legacy cert. The sibling is found by the entry's
 * own `dirPrefix` (nitro-entry/tls-paths.mjs), so a cert under any other name has none.
 * Bun's fetch honours only the first `ca` entry, so the order is the pin. Re-read every few
 * seconds so a re-minted identity is picked up without a restart. */
function hostIdentityCerts(): string[] {
	const now = Date.now();
	if (now - certCache.at < CERT_CACHE_MS) return certCache.pems;
	const pems: string[] = [];
	const legacy = process.env.PUNKTFUNK_UI_TLS_CERT?.trim();
	if (legacy) {
		const dir = dirPrefix(legacy, "cert.pem");
		for (const p of [dir === null ? null : `${dir}native-cert.pem`, legacy]) {
			if (!p) continue;
			try {
				const pem = readFileSync(p, "utf8");
				if (pem.includes("-----BEGIN CERTIFICATE-----")) pems.push(pem);
			} catch {
				// unreadable: skip
			}
		}
	}
	certCache = { at: now, pems };
	return pems;
}
const CERT_CACHE_MS = 10_000;
let certCache: { at: number; pems: string[] } = { at: 0, pems: [] };

/** Whether `url`'s host is a loopback address — the only place the proxy relaxes TLS verification
 * for the host's self-signed cert. IPv4 127.0.0.0/8, IPv6 ::1, and the `localhost` name. */
export function isLoopbackUrl(url: string): boolean {
	let host: string;
	try {
		host = new URL(url).hostname;
	} catch {
		return false;
	}
	// URL wraps IPv6 in brackets in .host but strips them in .hostname; normalize anyway.
	const h = host.replace(/^\[|\]$/g, "").toLowerCase();
	if (h === "localhost" || h === "::1") return true;
	return /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(h);
}

/** `Bearer <token>` for the management API. 503 when no token is configured: the host requires
 * one, so an empty bearer would only bounce as 401. */
export function mgmtBearer(): string {
	const token = mgmtToken();
	if (!token) {
		throw createError({
			statusCode: 503,
			statusMessage:
				"management token not configured (PUNKTFUNK_MGMT_TOKEN / ~/.config/punktfunk/mgmt-token)",
		});
	}
	return `Bearer ${token}`;
}

/** 502 for the management API's 401. The session gate already passed, so a 401 is the host
 * rejecting OUR token; relayed as-is it would send the browser to /login in a loop. */
export function assertHostTokenAccepted(res: { status: number }): void {
	if (res.status === 401) {
		throw createError({
			statusCode: 502,
			statusMessage:
				"management API rejected the host token (check PUNKTFUNK_MGMT_TOKEN)",
		});
	}
}

/** Relay `event` to `path` (query included) on the management API. The host's bearer overwrites
 * whatever the browser sent, the session cookie stays behind, and a 401 becomes 502. A dead host
 * is `proxyRequest`'s 502. */
export function relayToMgmt(event: H3Event, path: string) {
	const base = mgmtUrl();
	return proxyRequest(event, `${base}${path}`, {
		// `tls` is a Bun.fetch extension, not in the standard RequestInit type.
		fetchOptions: loopbackTls(base) as unknown as RequestInit | undefined,
		headers: { authorization: mgmtBearer(), cookie: "" },
		onResponse: (_event, response) => assertHostTokenAccepted(response),
	});
}
