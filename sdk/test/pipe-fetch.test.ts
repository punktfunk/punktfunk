// The socket-path fetch, against a node:http server on a unix socket: the same code dials a
// Windows pipe, where bun's own fetch cannot.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as http from "node:http";
import * as os from "node:os";
import * as path from "node:path";
import { socketFetch } from "../src/pipe-fetch.js";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-pipe-"));
const sock = path.join(dir, "h.sock");
let server: http.Server;

beforeAll(async () => {
	server = http.createServer((req, res) => {
		const route = (req.url ?? "").split("?")[0];
		if (route === "/echo") {
			const chunks: Buffer[] = [];
			req.on("data", (c: Buffer) => chunks.push(c));
			req.on("end", () => {
				res.writeHead(200, { "content-type": "application/json", "x-seen-host": req.headers.host ?? "" });
				res.end(
					JSON.stringify({
						method: req.method,
						auth: req.headers.authorization,
						body: Buffer.concat(chunks).toString(),
					}),
				);
			});
			return;
		}
		if (route === "/empty") {
			res.writeHead(204);
			res.end();
			return;
		}
		if (route === "/stream") {
			res.writeHead(200, { "content-type": "text/event-stream" });
			res.write("data: one\n\n");
			setTimeout(() => res.end("data: two\n\n"), 300);
			return;
		}
		res.writeHead(404, { "content-type": "application/json" });
		res.end(JSON.stringify({ error: "no such path" }));
	});
	await new Promise<void>((r) => server.listen(sock, r));
});

afterAll(() => {
	server.close();
	fs.rmSync(dir, { recursive: true, force: true });
});

describe("socketFetch", () => {
	test("carries method, headers, body and the URL's host, and reads the JSON back", async () => {
		const f = socketFetch(sock);
		const resp = await f("http://punktfunk.host/echo?x=1", {
			method: "POST",
			headers: { authorization: "Bearer t", "content-type": "application/json" },
			body: JSON.stringify({ hello: 1 }),
		});
		expect(resp.status).toBe(200);
		expect(resp.headers.get("x-seen-host")).toBe("punktfunk.host");
		expect(await resp.json()).toEqual({ method: "POST", auth: "Bearer t", body: '{"hello":1}' });
	});

	test("a 204 is a Response with no body, and a 404 keeps its error body", async () => {
		const f = socketFetch(sock);
		const empty = await f("http://punktfunk.host/empty");
		expect(empty.status).toBe(204);
		expect(empty.body).toBeNull();
		const missing = await f("http://punktfunk.host/nowhere");
		expect(missing.ok).toBe(false);
		expect(await missing.json()).toEqual({ error: "no such path" });
	});

	test("a streamed body arrives as it is written, not at the end", async () => {
		const f = socketFetch(sock);
		const resp = await f("http://punktfunk.host/stream");
		const reader = resp.body?.getReader();
		if (!reader) throw new Error("no body");
		const started = Date.now();
		const first = await reader.read();
		expect(new TextDecoder().decode(first.value)).toContain("one");
		expect(Date.now() - started).toBeLessThan(250);
		let rest = "";
		for (;;) {
			const { done, value } = await reader.read();
			if (done) break;
			rest += new TextDecoder().decode(value);
		}
		expect(rest).toContain("two");
	});

	test("a socket nobody serves rejects instead of hanging", async () => {
		const f = socketFetch(path.join(dir, "nobody.sock"));
		await expect(f("http://punktfunk.host/echo")).rejects.toThrow();
	});
});
