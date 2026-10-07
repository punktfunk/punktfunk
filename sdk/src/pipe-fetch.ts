// A fetch over `node:http` for a socket path: a Windows named pipe, which bun's `fetch({ unix })`
// does not dial, or a unix socket. The host answers plain HTTP there, so there is nothing to pin,
// and one connection carries one request, so a held event stream never queues the next call.
import * as http from "node:http";
import { Readable } from "node:stream";

/** Statuses a `Response` may not carry a body for. */
const BODYLESS = new Set([101, 204, 205, 304]);

export const socketFetch = (socketPath: string): typeof fetch =>
	(async (input, init) => {
		const req = new Request(input, init);
		const url = new URL(req.url);
		const body = req.body ? Buffer.from(await req.arrayBuffer()) : undefined;
		const headers: Record<string, string> = { host: url.host };
		req.headers.forEach((v, k) => {
			headers[k] = v;
		});
		if (body) headers["content-length"] = String(body.byteLength);
		return new Promise<Response>((resolve, reject) => {
			const r = http.request(
				{
					socketPath,
					method: req.method,
					path: url.pathname + url.search,
					headers,
				},
				(res) => {
					const out = new Headers();
					for (const [k, v] of Object.entries(res.headers)) {
						if (Array.isArray(v)) for (const x of v) out.append(k, x);
						else if (v !== undefined) out.set(k, v);
					}
					const status = res.statusCode ?? 0;
					resolve(
						new Response(
							BODYLESS.has(status)
								? null
								: (Readable.toWeb(res) as unknown as ReadableStream<Uint8Array>),
							{ status, statusText: res.statusMessage ?? "", headers: out },
						),
					);
				},
			);
			r.on("error", reject);
			req.signal.addEventListener("abort", () => r.destroy(new Error("aborted")));
			r.end(body);
		});
	}) as typeof fetch;
