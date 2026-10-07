// A sandboxed plugin's page is no loopback listener the console can dial. On Linux without network
// it listens on a unix socket the runner forwards a port to; in a Windows AppContainer it is served
// over the host's channel on the plugin's pipe. Pins: `ui-forward.test.ts`, `ui-channel.test.ts`.
import * as net from "node:net";

/** Where the sandbox sees the runner's UI dir. */
export const UI_DIR = "/run/punktfunk/ui";
export const UI_SOCKET = `${UI_DIR}/ui.sock`;

export interface UiForward {
	/** The host loopback port a plugin UI registers. */
	readonly port: number;
	close(): void;
}

/**
 * Runner side: listen on `127.0.0.1:<ephemeral>` and pipe each connection to `socket`. Bun binds
 * inside `listen()`, so the port is known on return.
 */
export const forwardUi = (socket: string): UiForward => {
	const server = net.createServer((client) => {
		const upstream = net.connect(socket);
		client.pipe(upstream).pipe(client);
		client.on("error", () => upstream.destroy());
		upstream.on("error", () => client.destroy());
	});
	server.listen(0, "127.0.0.1");
	const address = server.address();
	if (address === null || typeof address === "string") {
		server.close();
		throw new Error("the plugin UI forward has no port");
	}
	return { port: address.port, close: () => server.close() };
};

type Serve = typeof Bun.serve;
type ServeOptions = {
	hostname?: string;
	port?: number | string;
	unix?: string;
	fetch?: (req: Request, server: unknown) => Response | Promise<Response>;
};

/** `servePluginUi`'s listener: an ephemeral port on loopback. */
const ephemeralLoopback = (o: ServeOptions): boolean =>
	o.unix === undefined &&
	(o.hostname === undefined || ["127.0.0.1", "localhost", "::1"].includes(o.hostname)) &&
	Number(o.port ?? 0) === 0;

/**
 * Sandbox side, before the plugin loads: the first `Bun.serve` on an ephemeral loopback port
 * binds `socket` instead and reports `port` — which is what `servePluginUi` registers. Nothing
 * else could reach that loopback, so no plugin loses a listener it could use.
 */
export const redirectUiServe = (port: number, socket = UI_SOCKET): void => {
	const bun = globalThis.Bun as { serve: Serve };
	const serve = bun.serve.bind(bun) as Serve;
	let used = false;
	bun.serve = ((options: Parameters<Serve>[0]) => {
		const o = options as ServeOptions;
		if (used || !ephemeralLoopback(o)) return serve(options);
		used = true;
		const { hostname: _hostname, port: _port, ...rest } = o;
		const server = serve({ ...rest, unix: socket } as Parameters<Serve>[0]);
		return new Proxy(server, {
			get(target, key) {
				if (key === "port") return port;
				if (key === "hostname") return "127.0.0.1";
				if (key === "url") return new URL(`http://127.0.0.1:${port}/`);
				const value = Reflect.get(target, key, target);
				return typeof value === "function" ? value.bind(target) : value;
			},
		});
	}) as Serve;
};

/**
 * Windows container side, before the plugin loads: the first `Bun.serve` on an ephemeral loopback
 * port is served over the host's channel on `pipe` and reports port 0, which the host accepts
 * only from a registration over that pipe. It lives here and not in `servePluginUi`, so a plugin
 * tree whose shared SDK the runner cannot refresh still gets it.
 */
export const redirectUiServeToChannel = (pipe: string, id: string): void => {
	const bun = globalThis.Bun as { serve: Serve };
	const serve = bun.serve.bind(bun) as Serve;
	let used = false;
	bun.serve = ((options: Parameters<Serve>[0]) => {
		const o = options as ServeOptions;
		const fetch = o.fetch;
		if (used || !ephemeralLoopback(o) || typeof fetch !== "function") return serve(options);
		used = true;
		const server = {
			port: 0,
			hostname: "127.0.0.1",
			url: new URL("http://127.0.0.1:0/"),
			stop: () => channel.stop(),
		};
		const channel = attachOverPipe(pipe, id, async (req) => fetch.call(server, req, server));
		return server as unknown as ReturnType<Serve>;
	}) as Serve;
};

const warn = (m: string) => console.warn(`[punktfunk] UI channel: ${m}`);

/** Connections a plugin keeps parked at the host; each one used is dialed again. */
const PARKED = 4;

/**
 * The reverse tunnel: dial the plugin's own pipe, ask the host to keep the connection, and serve
 * one HTTP/1.1 request on it. The host closes each connection after its response, so a page that
 * takes its time never holds the next one; bun's `node:http` server takes no socket it did not
 * accept, so the request is read and the response written here.
 */
export const attachOverPipe = (
	pipe: string,
	id: string,
	handle: (req: Request) => Promise<Response>,
): { stop(): void } => {
	const live = new Set<net.Socket>();
	let closed = false;
	let parked = 0;
	// The handshake by hand on a raw socket: bun's HTTP client hands no upgraded socket back.
	const dial = (): void => {
		if (closed || parked >= PARKED) return;
		parked++;
		const socket = net.connect({ path: pipe });
		let head = Buffer.alloc(0);
		let attached = false;
		socket.on("connect", () => {
			socket.write(
				`GET /api/v1/plugins/${id}/ui/attach HTTP/1.1\r\nhost: punktfunk.host\r\n` +
					"connection: Upgrade\r\nupgrade: punktfunk-ui\r\n\r\n",
			);
		});
		const onHead = (chunk: Buffer): void => {
			head = Buffer.concat([head, chunk]);
			const split = head.indexOf("\r\n\r\n");
			if (split < 0) return;
			socket.off("data", onHead);
			const status = head.subarray(0, split).toString("latin1").split(" ")[1];
			if (status !== "101") {
				warn(`the host refused the UI channel: ${status}`);
				socket.destroy();
				return;
			}
			attached = true;
			live.add(socket);
			serveOne(socket, handle, head.subarray(split + 4));
		};
		socket.on("data", onHead);
		socket.on("error", (e) => {
			if (!attached) warn(`the UI channel did not attach: ${e.message}`);
		});
		socket.on("close", () => {
			live.delete(socket);
			parked--;
			if (!closed) setTimeout(dial, attached ? 50 : 2000).unref();
		});
	};
	for (let i = 0; i < PARKED; i++) dial();
	return {
		stop: () => {
			closed = true;
			for (const socket of live) socket.destroy();
		},
	};
};

/** Statuses that carry no body. */
const BODYLESS = new Set([204, 304]);

/**
 * One request off a parked connection: head and `content-length` body read whole (a page's
 * requests are small), the response written chunked so a stream reaches the console as it is
 * made, then the connection closed.
 */
const serveOne = (
	socket: net.Socket,
	handle: (req: Request) => Promise<Response>,
	initial: Buffer,
): void => {
	const chunks: Buffer[] = [];
	const onData = (chunk: Buffer): void => {
		chunks.push(chunk);
		const raw = Buffer.concat(chunks);
		const split = raw.indexOf("\r\n\r\n");
		if (split < 0) return;
		const lines = raw.subarray(0, split).toString("latin1").split("\r\n");
		const [method = "GET", target = "/"] = lines[0]?.split(" ") ?? [];
		const headers = new Headers();
		for (const line of lines.slice(1)) {
			const colon = line.indexOf(":");
			if (colon > 0) headers.append(line.slice(0, colon).trim(), line.slice(colon + 1).trim());
		}
		const length = Number(headers.get("content-length") ?? 0);
		if (raw.length < split + 4 + length) return;
		socket.off("data", onData);
		const body = raw.subarray(split + 4, split + 4 + length);
		void respond(socket, method, target, headers, body, handle);
	};
	socket.on("data", onData);
	socket.on("error", () => {});
	if (initial.length > 0) onData(initial);
};

const respond = async (
	socket: net.Socket,
	method: string,
	target: string,
	headers: Headers,
	body: Buffer,
	handle: (req: Request) => Promise<Response>,
): Promise<void> => {
	let response: Response;
	try {
		response = await handle(
			new Request(new URL(target, "http://punktfunk.plugin"), {
				method,
				headers,
				body: body.length > 0 ? new Uint8Array(body) : undefined,
			}),
		);
	} catch (e) {
		warn(`page request failed: ${e}`);
		response = new Response("plugin error", { status: 500 });
	}
	const bodyless = BODYLESS.has(response.status) || method === "HEAD";
	const head = [`HTTP/1.1 ${response.status} ${response.statusText || "OK"}`];
	response.headers.forEach((v, k) => {
		if (!["content-length", "transfer-encoding", "connection"].includes(k)) head.push(`${k}: ${v}`);
	});
	head.push("connection: close");
	if (!bodyless) head.push("transfer-encoding: chunked");
	socket.write(`${head.join("\r\n")}\r\n\r\n`);
	if (bodyless || !response.body) {
		socket.end(bodyless ? "" : "0\r\n\r\n");
		return;
	}
	try {
		const reader = response.body.getReader();
		for (;;) {
			const { done, value } = await reader.read();
			if (done) break;
			if (value && value.length > 0) {
				socket.write(`${value.length.toString(16)}\r\n`);
				socket.write(value);
				socket.write("\r\n");
			}
		}
	} catch {
		// The console went away mid-stream; the close below is all that is left to do.
	}
	socket.end("0\r\n\r\n");
};
