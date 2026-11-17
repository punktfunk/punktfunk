import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import * as os from "node:os";
import type { Punktfunk } from "@punktfunk/host";
import { Duration, Effect, Layer, Schema } from "effect";
import {
	localServer,
	reachServer,
	requestAccess,
	unreachable,
} from "../src/access.js";
import { HostRequestError } from "../src/errors.js";
import { HostClient, type HostClientService } from "../src/host-client.js";
import { defineLibraryPlugin } from "../src/library/define.js";

const hostLayer = (request: HostClientService["request"]) =>
	Layer.succeed(HostClient)({ request, facade: {} as Punktfunk });

describe("a server on this computer", () => {
	test("localServer names the port and the loopback address, and nothing elsewhere", () => {
		expect(localServer("http://localhost:8080/romm/")).toEqual({
			port: 8080,
			url: "http://127.0.0.1:8080/romm",
		});
		expect(localServer("http://[::1]:9000")).toEqual({
			port: 9000,
			url: "http://127.0.0.1:9000",
		});
		expect(localServer(`http://${os.hostname()}`)?.port).toBe(80);
		// https keeps its name: the certificate is for it.
		expect(localServer("https://localhost:8443")).toEqual({
			port: 8443,
			url: "https://localhost:8443",
		});
		for (const elsewhere of [
			"http://romm.lan:8080",
			"ftp://localhost",
			"nonsense",
		])
			expect(localServer(elsewhere), elsewhere).toBeUndefined();
	});

	test("reachServer asks for the port and dials the loopback address once granted", async () => {
		const reach = (
			url: string,
			outcomes: unknown,
			seen?: (body: unknown) => void,
		) =>
			Effect.runPromise(
				reachServer(url, "RomM").pipe(
					Effect.provide(
						hostLayer((_method, _path, body) => {
							seen?.(body);
							return Effect.succeed(outcomes);
						}),
					),
				),
			);
		const row = (outcome: string) => [{ path: "127.0.0.1:8080", outcome }];
		let asked: unknown;
		expect(
			await reach("http://localhost:8080", row("granted"), (b) => {
				asked = b;
			}),
		).toBe("http://127.0.0.1:8080");
		expect(asked).toEqual({ paths: [], ports: [8080], reason: "RomM" });
		expect(
			await reach("http://localhost:8080", row("pending")),
		).toBeUndefined();
		expect(await reach("http://localhost:8080", row("denied"))).toBeUndefined();
		// Windows, and a host before ports: the address as typed.
		const typed = "http://localhost:8080";
		expect(await reach(typed, row("refused:unsupported"))).toBe(typed);
		expect(await reach(typed, [])).toBe(typed);
		// A server elsewhere is never asked about.
		expect(
			await reach("http://romm.lan:8080", [], () => {
				throw new Error("asked");
			}),
		).toBe("http://romm.lan:8080");
	});
});

describe("folder access", () => {
	test("requestAccess batches paths on the host route", async () => {
		let seen: unknown;
		const result = await Effect.runPromise(
			requestAccess(["/one", { path: "/two", write: true }], "libraries").pipe(
				Effect.provide(
					hostLayer((method, path, body) => {
						expect([method, path]).toEqual(["POST", "/plugin-access/requests"]);
						seen = body;
						return Effect.succeed([
							{ path: "/one", outcome: "pending" },
							{ path: "/two", outcome: "pending" },
						]);
					}),
				),
			),
		);
		expect(seen).toEqual({
			paths: [{ path: "/one" }, { path: "/two", write: true }],
			reason: "libraries",
		});
		expect(result).toHaveLength(2);
	});

	test("an old host is a no-op, not a plugin failure", async () => {
		const result = await Effect.runPromise(
			requestAccess(["/one"]).pipe(
				Effect.provide(
					hostLayer((method, path) =>
						Effect.fail(
							new HostRequestError({ method, path, cause: { status: 404 } }),
						),
					),
				),
			),
		);
		expect(result).toEqual([]);
	});

	test("a host that refuses the request is never a plugin failure", async () => {
		for (const status of [403, 500]) {
			const result = await Effect.runPromise(
				requestAccess(["/one"]).pipe(
					Effect.provide(
						hostLayer((method, path) =>
							Effect.fail(
								new HostRequestError({ method, path, cause: { status } }),
							),
						),
					),
				),
			);
			expect([status, result]).toEqual([status, []]);
		}
	});

	test("~ expands before the host sees it", async () => {
		let seen: unknown;
		await Effect.runPromise(
			requestAccess(["~/Games"]).pipe(
				Effect.provide(
					hostLayer((_method, _path, body) => {
						seen = body;
						return Effect.succeed([]);
					}),
				),
			),
		);
		expect(seen).toEqual({ paths: [{ path: `${os.homedir()}/Games` }] });
	});

	test("missing counts only inside the sandbox", () => {
		const missing = `/definitely-missing-punktfunk-${process.pid}`;
		const previous = process.env.PUNKTFUNK_MGMT_UNIX;
		try {
			delete process.env.PUNKTFUNK_MGMT_UNIX;
			expect(unreachable([missing])).toEqual([]);
			process.env.PUNKTFUNK_MGMT_UNIX = "/run/punktfunk/host.sock";
			expect(unreachable([missing, missing])).toEqual([missing]);
		} finally {
			if (previous === undefined) delete process.env.PUNKTFUNK_MGMT_UNIX;
			else process.env.PUNKTFUNK_MGMT_UNIX = previous;
		}
	});

	test("a folder asked for by its files keeps that through the check", () => {
		const missing = `/definitely-missing-punktfunk-${process.pid}`;
		const previous = process.env.PUNKTFUNK_MGMT_UNIX;
		try {
			process.env.PUNKTFUNK_MGMT_UNIX = "/run/punktfunk/host.sock";
			expect(unreachable([{ path: missing, files: true }, missing])).toEqual([
				{ path: missing, files: true },
			]);
		} finally {
			if (previous === undefined) delete process.env.PUNKTFUNK_MGMT_UNIX;
			else process.env.PUNKTFUNK_MGMT_UNIX = previous;
		}
	});

	test("defineLibraryPlugin asks once for one unreachable set", async () => {
		const previousConfig = process.env.PUNKTFUNK_CONFIG_DIR;
		const previousSocket = process.env.PUNKTFUNK_MGMT_UNIX;
		const config = `${import.meta.dir}/.access-${process.pid}`;
		process.env.PUNKTFUNK_CONFIG_DIR = config;
		process.env.PUNKTFUNK_MGMT_UNIX = "/run/punktfunk/host.sock";
		const posts: unknown[] = [];
		const plugin = defineLibraryPlugin({
			name: "access-test",
			title: "Access test",
			configSchema: Schema.Struct({}),
			detect: () => Effect.succeed(true),
			scan: () => Effect.succeed([]),
			wants: () => ["/missing-one", "/missing-two"],
			pollInterval: Duration.millis(20),
		});
		const pf = {
			request: async (method: string, path: string, body?: unknown) => {
				if (method === "POST" && path === "/plugin-access/requests") {
					posts.push(body);
					return [
						{ path: "/missing-one", outcome: "pending" },
						{ path: "/missing-two", outcome: "pending" },
					];
				}
				if (method === "GET" && path === "/plugins")
					return [{ id: "access-test", category: "library" }];
				return [];
			},
		} as unknown as Punktfunk;
		let running: Promise<void> | undefined;
		try {
			running = (plugin.def.main as (pf: Punktfunk) => Promise<void>)(pf);
			const deadline = Date.now() + 5000;
			while (posts.length === 0 && Date.now() < deadline)
				await new Promise((resolve) => setTimeout(resolve, 10));
			expect(posts).toHaveLength(1);
			expect(posts[0]).toEqual({
				paths: [{ path: "/missing-one" }, { path: "/missing-two" }],
				reason: "Access test",
			});
			await new Promise((resolve) => setTimeout(resolve, 80));
			expect(posts).toHaveLength(1);
		} finally {
			if (running) {
				process.emit("SIGTERM", "SIGTERM");
				await running;
			}
			if (previousConfig === undefined) delete process.env.PUNKTFUNK_CONFIG_DIR;
			else process.env.PUNKTFUNK_CONFIG_DIR = previousConfig;
			if (previousSocket === undefined) delete process.env.PUNKTFUNK_MGMT_UNIX;
			else process.env.PUNKTFUNK_MGMT_UNIX = previousSocket;
			fs.rmSync(config, { recursive: true, force: true });
		}
	});

	test("a refused request still lets the scan reach the library", async () => {
		const previousConfig = process.env.PUNKTFUNK_CONFIG_DIR;
		const previousSocket = process.env.PUNKTFUNK_MGMT_UNIX;
		const config = `${import.meta.dir}/.access-refused-${process.pid}`;
		process.env.PUNKTFUNK_CONFIG_DIR = config;
		process.env.PUNKTFUNK_MGMT_UNIX = "/run/punktfunk/host.sock";
		const reconciles: string[] = [];
		const plugin = defineLibraryPlugin({
			name: "refused-test",
			title: "Refused test",
			configSchema: Schema.Struct({}),
			detect: () => Effect.succeed(true),
			scan: () => Effect.succeed([]),
			wants: () => ["/missing-one"],
			pollInterval: Duration.millis(20),
		});
		const pf = {
			request: async (method: string, path: string) => {
				// What the host answers a plugin that has no token of its own.
				if (method === "POST" && path === "/plugin-access/requests")
					throw Object.assign(new Error("forbidden"), { status: 403 });
				if (
					method === "PUT" &&
					path.startsWith("/library/provider/refused-test")
				)
					reconciles.push(path);
				if (method === "GET" && path === "/plugins")
					return [{ id: "refused-test", category: "library" }];
				return [];
			},
		} as unknown as Punktfunk;
		let running: Promise<void> | undefined;
		try {
			running = (plugin.def.main as (pf: Punktfunk) => Promise<void>)(pf);
			const deadline = Date.now() + 5000;
			while (reconciles.length === 0 && Date.now() < deadline)
				await new Promise((resolve) => setTimeout(resolve, 10));
			expect(reconciles.length).toBeGreaterThan(0);
		} finally {
			if (running) {
				process.emit("SIGTERM", "SIGTERM");
				await running;
			}
			if (previousConfig === undefined) delete process.env.PUNKTFUNK_CONFIG_DIR;
			else process.env.PUNKTFUNK_CONFIG_DIR = previousConfig;
			if (previousSocket === undefined) delete process.env.PUNKTFUNK_MGMT_UNIX;
			else process.env.PUNKTFUNK_MGMT_UNIX = previousSocket;
			fs.rmSync(config, { recursive: true, force: true });
		}
	});
});
