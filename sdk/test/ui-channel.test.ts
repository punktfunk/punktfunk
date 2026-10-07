// The reverse tunnel a Windows plugin serves its page through, against a stand-in host on a unix
// socket: the plugin dials, asks for the upgrade, and the host sends requests down the parked
// connection, one each. The stand-in is a raw socket server: bun's `node:http` server hands no
// usable socket out of an upgrade, and the real host is hyper.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { redirectUiServeToChannel } from "../src/ui-forward.js";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-ui-"));
const sock = path.join(dir, "host.sock");
const parked: net.Socket[] = [];
let host: net.Server;

beforeAll(async () => {
	host = net.createServer((socket) => {
		let head = Buffer.alloc(0);
		const onHead = (chunk: Buffer) => {
			head = Buffer.concat([head, chunk]);
			const split = head.indexOf("\r\n\r\n");
			if (split < 0) return;
			socket.off("data", onHead);
			const text = head.subarray(0, split).toString("latin1");
			if (!text.startsWith("GET /api/v1/plugins/demo/ui/attach ") || !/upgrade: punktfunk-ui/i.test(text)) {
				socket.end("HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
				return;
			}
			socket.write("HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: punktfunk-ui\r\n\r\n");
			parked.push(socket);
		};
		socket.on("data", onHead);
		socket.on("error", () => {});
	});
	await new Promise<void>((r) => host.listen(sock, r));
});

afterAll(() => {
	host.close();
	fs.rmSync(dir, { recursive: true, force: true });
});

/**
 * The host's side of one request: raw HTTP/1.1 down a parked connection (bun's client cannot
 * take a ready socket), the answer read to the close and its chunks joined.
 */
const down = (socket: net.Socket, method: string, pathname: string, body = "") =>
	new Promise<{ status: number; text: string }>((resolve, reject) => {
		const chunks: Buffer[] = [];
		socket.on("data", (c: Buffer) => {
			chunks.push(c);
		});
		socket.on("error", reject);
		socket.on("close", () => {
			const raw = Buffer.concat(chunks).toString("utf8");
			const split = raw.indexOf("\r\n\r\n");
			const head = raw.slice(0, split);
			const status = Number(head.split(" ")[1]);
			let text = raw.slice(split + 4);
			if (/transfer-encoding:\s*chunked/i.test(head)) {
				let out = "";
				let rest = text;
				for (;;) {
					const line = rest.indexOf("\r\n");
					const size = Number.parseInt(rest.slice(0, line), 16);
					if (!size) break;
					out += rest.slice(line + 2, line + 2 + size);
					rest = rest.slice(line + 2 + size + 2);
				}
				text = out;
			}
			resolve({ status, text });
		});
		socket.write(
			`${method} ${pathname} HTTP/1.1\r\nhost: punktfunk.plugin\r\nauthorization: Bearer s3cret\r\n` +
				`content-length: ${Buffer.byteLength(body)}\r\nconnection: close\r\n\r\n${body}`,
		);
	});

const waitFor = async (ready: () => boolean, ms = 3000): Promise<void> => {
	const deadline = Date.now() + ms;
	while (!ready()) {
		if (Date.now() > deadline) throw new Error("condition never became true");
		await new Promise((r) => setTimeout(r, 20));
	}
};

describe("redirectUiServeToChannel", () => {
	test("a plugin's loopback page is served down connections parked at the host", async () => {
		const serve = Bun.serve;
		redirectUiServeToChannel(sock, "demo");
		const seen: string[] = [];
		// What `servePluginUi` calls, in any SDK copy a plugin tree holds.
		const server = Bun.serve({
			hostname: "127.0.0.1",
			port: 0,
			async fetch(req) {
				seen.push(`${req.method} ${new URL(req.url).pathname} ${req.headers.get("authorization")}`);
				const text = req.method === "POST" ? await req.text() : "";
				return new Response(`hello ${text}`, { status: 200, headers: { "x-plugin": "demo" } });
			},
		});
		// A second one is the plugin's own business and keeps its real port.
		const other = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => new Response("x") });
		(Bun as { serve: typeof Bun.serve }).serve = serve;
		try {
			expect(server.port).toBe(0);
			expect(other.port).toBeGreaterThan(0);
			await waitFor(() => parked.length >= 4);
			const first = await down(parked.shift() as net.Socket, "GET", "/__health");
			expect(first).toEqual({ status: 200, text: "hello " });
			const second = await down(parked.shift() as net.Socket, "POST", "/api/save", "{\"a\":1}");
			expect(second.text).toBe('hello {"a":1}');
			expect(seen).toEqual(["GET /__health Bearer s3cret", "POST /api/save Bearer s3cret"]);
			// Two used, two dialed again: the plugin keeps four parked.
			await waitFor(() => parked.length >= 4);
		} finally {
			server.stop(true);
			other.stop(true);
		}
	});
});
