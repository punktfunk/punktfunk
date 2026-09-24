// A plugin without network has a loopback of its own, which the host's console proxy cannot dial.
// Its UI listens on a unix socket in a dir the runner binds in, and the runner forwards a port on
// the host's loopback to that socket. Pin: `ui-forward.test.ts`.
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
		const o = options as { hostname?: string; port?: number | string; unix?: string };
		const loopback =
			o.hostname === undefined || ["127.0.0.1", "localhost", "::1"].includes(o.hostname);
		if (used || o.unix !== undefined || !loopback || Number(o.port ?? 0) !== 0) {
			return serve(options);
		}
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
