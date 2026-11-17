// The way a sandboxed plugin reaches the host.
//
// A plugin's network namespace is its own, so it cannot dial the loopback management API. The
// supervisor listens on a unix socket bound into each sandbox and forwards what arrives to the host
// over the connection it already makes — the pinned TLS one.
//
// It forwards, it does not vouch: the plugin presents its OWN bearer, and a request without one is
// refused by the host exactly as it would be on loopback. Nothing here adds a credential.
import * as fs from "node:fs";
import * as path from "node:path";

export interface HostProxyOptions {
	/** Where to listen. One socket per plugin, inside the runtime dir. */
	socket: string;
	/** The host's management API, e.g. `https://127.0.0.1:47990`. */
	url: string;
	/** The pinned fetch the SDK built for that URL. */
	fetch: typeof globalThis.fetch;
}

export interface HostProxy {
	readonly socket: string;
	close(): void;
}

/** Headers that belong to the hop, not to the request. */
const HOP_BY_HOP = new Set([
	"connection",
	"keep-alive",
	"proxy-authenticate",
	"proxy-authorization",
	"te",
	"trailer",
	"transfer-encoding",
	"upgrade",
	"host",
]);

/**
 * Serve `options.socket`, forwarding every request to the host. Bun only: `Bun.serve({unix})` is
 * the listener, and the runner is bun.
 */
export const serveHostProxy = (options: HostProxyOptions): HostProxy => {
	const bun = (globalThis as { Bun?: { serve: (o: unknown) => { stop: (b?: boolean) => void } } })
		.Bun;
	if (!bun) throw new Error("the host proxy needs the Bun runtime");
	fs.mkdirSync(path.dirname(options.socket), { recursive: true });
	fs.rmSync(options.socket, { force: true });
	const base = options.url.replace(/\/+$/, "");
	const server = bun.serve({
		unix: options.socket,
		idleTimeout: 0,
		async fetch(req: Request): Promise<Response> {
			const url = new URL(req.url);
			const headers = new Headers();
			req.headers.forEach((value, key) => {
				if (!HOP_BY_HOP.has(key.toLowerCase())) headers.set(key, value);
			});
			const init: RequestInit = { method: req.method, headers };
			if (req.method !== "GET" && req.method !== "HEAD") {
				init.body = await req.arrayBuffer();
			}
			try {
				const upstream = await options.fetch(`${base}${url.pathname}${url.search}`, init);
				// Streamed as it arrives: `/api/v1/events` is an SSE feed a plugin subscribes to.
				return new Response(upstream.body, {
					status: upstream.status,
					headers: upstream.headers,
				});
			} catch (cause) {
				return Response.json(
					{ error: "the host is not reachable", issue: String(cause) },
					{ status: 502 },
				);
			}
		},
	});
	// Owner-only: the socket is the plugin's way in, and every other account on the box is not
	// the operator. (Same uid still reaches it — the sandbox, not the mode, is the boundary.)
	try {
		fs.chmodSync(options.socket, 0o600);
	} catch {
		// A runtime dir that refuses chmod is already owner-only.
	}
	return {
		socket: options.socket,
		close: () => {
			server.stop(true);
			fs.rmSync(options.socket, { force: true });
		},
	};
};
