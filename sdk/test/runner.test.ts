// The runner's contract: discovery (scripts + plugin packages + the sshd refusal), both plugin
// shapes running against a mock host, crash → supervised restart, one-shot scripts, and the
// M5 heart — structured interruption running scoped finalizers on shutdown.
import { afterAll, describe, expect, test } from "bun:test";
import { Effect, Fiber } from "effect";
import * as fs from "node:fs";
import * as path from "node:path";
import {
	adoptNestedState,
	describeFailure,
	discoverUnits,
	inProcessConnect,
	runner,
	spawnAgainIfKilled,
	superviseUnit,
	windowsSddlUnsafeReason,
} from "../src/runner.js";

const TOKEN = "runner-token";
// Fixtures live under sdk/ so the generated plugin files can resolve "effect" and the SDK.
const ROOT = path.join(import.meta.dir, "..", `.runner-fixtures-${process.pid}`);
fs.mkdirSync(ROOT, { recursive: true });
afterAll(() => fs.rmSync(ROOT, { recursive: true, force: true }));

const mkdirs = (name: string) => {
	const dir = path.join(ROOT, name);
	fs.mkdirSync(path.join(dir, "scripts"), { recursive: true });
	fs.mkdirSync(path.join(dir, "plugins", "node_modules"), { recursive: true });
	return {
		scriptsDir: path.join(dir, "scripts"),
		pluginsDir: path.join(dir, "plugins"),
		dir,
	};
};

const write = (file: string, content: string) => {
	fs.mkdirSync(path.dirname(file), { recursive: true });
	fs.writeFileSync(file, content);
	fs.chmodSync(file, 0o644);
};

const mockHost = () =>
	Bun.serve({
		port: 0,
		fetch(req) {
			if (req.headers.get("authorization") !== `Bearer ${TOKEN}`)
				return Response.json({ error: "no" }, { status: 401 });
			const p = new URL(req.url).pathname;
			if (p === "/api/v1/host") return Response.json({ hostname: "mock" });
			if (p === "/api/v1/status") return Response.json({ ok: true });
			return Response.json({ error: "nf" }, { status: 404 });
		},
	});

const waitFor = async (predicate: () => boolean, ms = 5000) => {
	const deadline = Date.now() + ms;
	while (!predicate()) {
		if (Date.now() > deadline) throw new Error("condition never became true");
		await new Promise((r) => setTimeout(r, 25));
	}
};

describe("discovery", () => {
	test("finds scripts + plugin packages, refuses world-writable files", () => {
		const d = mkdirs("discover");
		write(path.join(d.scriptsDir, "b-script.ts"), "export {};");
		write(path.join(d.scriptsDir, "a-script.ts"), "export {};");
		write(path.join(d.scriptsDir, "notes.txt"), "not code");
		const evil = path.join(d.scriptsDir, "evil.ts");
		write(evil, "export {};");
		fs.chmodSync(evil, 0o777);
		write(
			path.join(d.pluginsDir, "node_modules", "punktfunk-plugin-x", "package.json"),
			JSON.stringify({ name: "punktfunk-plugin-x", main: "index.js" }),
		);
		write(
			path.join(d.pluginsDir, "node_modules", "punktfunk-plugin-x", "index.js"),
			"export default { name: 'x', main: async () => {} };",
		);
		write(
			path.join(d.pluginsDir, "node_modules", "unrelated-pkg", "package.json"),
			JSON.stringify({ name: "unrelated-pkg" }),
		);

		const logs: string[] = [];
		const units = discoverUnits(d, (l) => logs.push(l));
		expect(units.map((u) => u.name)).toEqual([
			"a-script",
			"b-script",
			"punktfunk-plugin-x",
		]);
		expect(logs.join("\n")).toContain("REFUSING");
		expect(logs.join("\n")).toContain("evil.ts");
	});

	test("discovers scoped @punktfunk/plugin-* packages (not other scoped pkgs)", () => {
		const d = mkdirs("discover-scoped");
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "plugin-y", "package.json"),
			JSON.stringify({ name: "@punktfunk/plugin-y", module: "dist/index.js" }),
		);
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "plugin-y", "dist", "index.js"),
			"export default { name: 'y', main: async () => {} };",
		);
		// A non-plugin scoped package (e.g. the SDK itself) is ignored.
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "host", "package.json"),
			JSON.stringify({ name: "@punktfunk/host" }),
		);
		const units = discoverUnits(d);
		expect(units.map((u) => u.name)).toEqual(["@punktfunk/plugin-y"]);
	});

	test("discovers plugins in ANY scope, not just @punktfunk", () => {
		// Plugin-store catalog entries must be scoped so the scope can map to that entry's
		// registry, so a third-party plugin necessarily arrives under its own scope. Discovery
		// limited to @punktfunk would let it install and then never run.
		const d = mkdirs("discover-foreign-scope");
		write(
			path.join(d.pluginsDir, "node_modules", "@retro-hub", "plugin-z", "package.json"),
			JSON.stringify({ name: "@retro-hub/plugin-z", main: "index.js" }),
		);
		write(
			path.join(d.pluginsDir, "node_modules", "@retro-hub", "plugin-z", "index.js"),
			"export default { name: 'z', main: async () => {} };",
		);
		expect(discoverUnits(d).map((u) => u.name)).toEqual(["@retro-hub/plugin-z"]);
	});

	test("a plugin's LIBRARY is not a unit — top-level dependencies win", () => {
		// Regression from a live install: `@punktfunk/plugin-kit` is the framework kit-built
		// plugins depend on. It matches the `plugin-*` convention exactly and lands in
		// node_modules transitively, so a convention-only scan would import and run the framework
		// as if it were a plugin.
		const d = mkdirs("discover-toplevel");
		for (const name of ["plugin-real", "plugin-kit"]) {
			write(
				path.join(d.pluginsDir, "node_modules", "@punktfunk", name, "package.json"),
				JSON.stringify({ name: `@punktfunk/${name}`, main: "index.js" }),
			);
			write(
				path.join(d.pluginsDir, "node_modules", "@punktfunk", name, "index.js"),
				"export default { name: 'p', main: async () => {} };",
			);
		}
		write(
			path.join(d.pluginsDir, "package.json"),
			JSON.stringify({ dependencies: { "@punktfunk/plugin-real": "^1.0.0" } }),
		);
		expect(discoverUnits(d).map((u) => u.name)).toEqual(["@punktfunk/plugin-real"]);
	});

	test("no package.json at all falls back to the naming convention", () => {
		// Hand-assembled or older trees must still run, not silently discover nothing.
		const d = mkdirs("discover-nodeps");
		write(
			path.join(d.pluginsDir, "node_modules", "punktfunk-plugin-legacy", "package.json"),
			JSON.stringify({ name: "punktfunk-plugin-legacy", main: "index.js" }),
		);
		write(
			path.join(d.pluginsDir, "node_modules", "punktfunk-plugin-legacy", "index.js"),
			"export default { name: 'l', main: async () => {} };",
		);
		expect(discoverUnits(d).map((u) => u.name)).toEqual(["punktfunk-plugin-legacy"]);
	});

	test("an emptied dependency list means nothing to run", () => {
		// `bun remove` drops the `dependencies` key when the last plugin goes, while orphaned
		// transitive packages linger in node_modules. Falling back to the naming convention there
		// would start running a plugin's LIBRARY right after the operator removed the only real
		// plugin — seen on-glass.
		const d = mkdirs("discover-emptied");
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "plugin-kit", "package.json"),
			JSON.stringify({ name: "@punktfunk/plugin-kit", main: "index.js" }),
		);
		write(
			path.join(d.pluginsDir, "node_modules", "@punktfunk", "plugin-kit", "index.js"),
			"export default {};",
		);
		write(path.join(d.pluginsDir, "package.json"), JSON.stringify({ name: "plugins" }));
		expect(discoverUnits(d)).toEqual([]);

		write(
			path.join(d.pluginsDir, "package.json"),
			JSON.stringify({ name: "plugins", dependencies: {} }),
		);
		expect(discoverUnits(d)).toEqual([]);
	});
});

describe("describeFailure", () => {
	test("names the rejection a tryPromise wrapped, not the wrapper", async () => {
		const exit = await Effect.runPromiseExit(
			Effect.tryPromise(() => Promise.reject(new Error("connect refused\n\nat host.sock"))),
		);
		if (exit._tag !== "Failure") throw new Error("expected a failure");
		expect(describeFailure(exit.cause)).toBe("connect refused | at host.sock");
	});
});

describe("adoptNestedState (0.39 wrote sandboxed state one level down)", () => {
	const write = (p: string, s: string) => {
		fs.mkdirSync(path.dirname(p), { recursive: true });
		fs.writeFileSync(p, s);
	};
	const quiet = () => {};

	test("moves nested files up; the nested copy wins a clash and the old one is kept", () => {
		const state = path.join(ROOT, "adopt", "rom-manager");
		write(path.join(state, "config.json"), "old");
		write(path.join(state, "libretro-db", "a.dat"), "db");
		write(path.join(state, "rom-manager", "config.json"), "new");
		write(path.join(state, "rom-manager", "cache.json"), "cache");
		adoptNestedState(state, "rom-manager", quiet);
		expect(fs.readFileSync(path.join(state, "config.json"), "utf8")).toBe("new");
		expect(fs.readFileSync(path.join(state, "config.json.pre-sandbox"), "utf8")).toBe("old");
		expect(fs.readFileSync(path.join(state, "cache.json"), "utf8")).toBe("cache");
		expect(fs.existsSync(path.join(state, "libretro-db", "a.dat"))).toBe(true);
		expect(fs.existsSync(path.join(state, "rom-manager"))).toBe(false);
	});

	test("leaves a state dir with nothing nested alone", () => {
		const state = path.join(ROOT, "adopt", "steam");
		write(path.join(state, "config.json"), "cfg");
		adoptNestedState(state, "steam", quiet);
		expect(fs.readdirSync(state)).toEqual(["config.json"]);
	});
});

describe("inProcessConnect (Windows, or the sandbox off)", () => {
	const config = path.join(ROOT, "tokens");
	fs.mkdirSync(path.join(config, "plugin-run"), { recursive: true });
	fs.writeFileSync(
		path.join(config, "plugin-run", "plugin-tokens.json"),
		JSON.stringify({ "rom-manager": "own-token" }),
	);
	const shared = { url: "https://127.0.0.1:1", token: TOKEN };
	const options = { configDir: config, connect: shared };

	test("a plugin with a manifest speaks with its own token", () => {
		const unit = { name: "r", file: "/x", manifest: { id: "rom-manager" } };
		expect(inProcessConnect(unit, options)).toEqual({ ...shared, token: "own-token" });
	});

	test("a loose script, or a plugin with no minted token, keeps the runner's", () => {
		expect(inProcessConnect({ name: "s", file: "/x" }, options)).toBe(shared);
		const unknown = { name: "n", file: "/x", manifest: { id: "nope" } };
		expect(inProcessConnect(unknown, options)).toBe(shared);
	});
});

describe("spawnAgainIfKilled (the ACL read's guard against a misfired timeout)", () => {
	const scripted = (...statuses: (number | null)[]) => {
		const seen: (number | null)[] = [];
		const last = spawnAgainIfKilled(() => {
			const status = statuses[seen.length] ?? null;
			seen.push(status);
			return { status };
		});
		return { status: last.status, spawns: seen.length };
	};

	test("a killed spawn runs once more; an exit of any code is final", () => {
		// `status: null` is the kill. A non-zero exit is PowerShell's own answer and stays.
		expect(scripted(null, 0)).toEqual({ status: 0, spawns: 2 });
		expect(scripted(1)).toEqual({ status: 1, spawns: 1 });
		// A real hang is killed twice and the unit is still refused: bounded, fail-closed.
		expect(scripted(null, null, 0)).toEqual({ status: null, spawns: 2 });
	});
});

describe("windowsSddlUnsafeReason (the sshd rule's Windows half, pure)", () => {
	// What a file under the host's ACL'd %ProgramData%\punktfunk actually looks like: owned by
	// Administrators, protected DACL, admin/SYSTEM/OWNER-RIGHTS full + Users read-execute.
	const LOCKED =
		"O:BAG:SYD:PAI(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)(A;;0x1200a9;;;BU)";

	test("accepts the host's locked-down layout", () => {
		expect(windowsSddlUnsafeReason(LOCKED)).toBeNull();
	});

	test("accepts inherited allow ACEs spelled with token runs", () => {
		expect(
			windowsSddlUnsafeReason("O:SYG:SYD:(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;ID;FR;;;BU)"),
		).toBeNull();
	});

	test("refuses an untrusted owner", () => {
		expect(
			windowsSddlUnsafeReason("O:BUG:SYD:(A;;FA;;;SY)(A;;FA;;;BA)"),
		).toContain("owner");
		expect(
			windowsSddlUnsafeReason(
				"O:S-1-5-21-1111111111-2222222222-3333333333-1001G:SYD:(A;;FA;;;SY)",
			),
		).toContain("owner");
	});

	test("accepts the running account as owner and writer (the Unix 'your own file' rule)", () => {
		const me = "S-1-5-21-1111111111-2222222222-3333333333-1001";
		const sddl = `O:${me}G:SYD:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;${me})`;
		expect(windowsSddlUnsafeReason(sddl)).toContain("owner");
		expect(windowsSddlUnsafeReason(sddl, me)).toBeNull();
	});

	test("refuses write-capable ACEs for non-admin principals, by token and by hex", () => {
		// BUILTIN\Users with modify (hex, as Windows renders 0x1301bf)
		expect(
			windowsSddlUnsafeReason(`${LOCKED}(A;;0x1301bf;;;BU)`),
		).toContain("BU");
		// Everyone with generic write
		expect(windowsSddlUnsafeReason(`${LOCKED}(A;;GW;;;WD)`)).toContain("WD");
		// Authenticated Users with file-write
		expect(windowsSddlUnsafeReason(`${LOCKED}(A;;FW;;;AU)`)).toContain("AU");
		// DELETE alone is enough (delete + recreate)
		expect(windowsSddlUnsafeReason(`${LOCKED}(A;;SD;;;BU)`)).toContain("BU");
		// WRITE_DAC alone is enough (rewrite the protection)
		expect(windowsSddlUnsafeReason(`${LOCKED}(A;;WD;;;BU)`)).toContain("BU");
		// an explicit user SID
		expect(
			windowsSddlUnsafeReason(
				`${LOCKED}(A;;FA;;;S-1-5-21-1111111111-2222222222-3333333333-1001)`,
			),
		).toContain("S-1-5-21");
	});

	test("accepts the runner principal's RX+WA grant, refuses real writes for it", () => {
		// `plugins enable` grants LocalService (RX,WA) on the unit dirs — bun's module loader
		// opens files requesting FILE_WRITE_ATTRIBUTES on top of read+execute (0x1201a9).
		// WA can't alter content, so it must not read as tamper-capable…
		expect(
			windowsSddlUnsafeReason(`${LOCKED}(A;OICIID;0x1201a9;;;LS)`),
		).toBeNull();
		// …but actual write-data for the service principal is still refused.
		expect(windowsSddlUnsafeReason(`${LOCKED}(A;;FW;;;LS)`)).toContain("LS");
	});

	test("inherit-only ACEs and deny ACEs don't trip it; unknown shapes fail closed", () => {
		// inherit-only: a template for children, grants nothing on this file
		expect(
			windowsSddlUnsafeReason(`${LOCKED}(A;OICIIO;FA;;;BU)`),
		).toBeNull();
		// deny ACEs only tighten
		expect(windowsSddlUnsafeReason(`${LOCKED}(D;;FA;;;BU)`)).toBeNull();
		// unknown rights token → treated as write-capable
		expect(windowsSddlUnsafeReason(`${LOCKED}(A;;ZZ;;;BU)`)).toContain("BU");
		// no DACL at all → refused
		expect(windowsSddlUnsafeReason("O:BAG:SY")).toContain("DACL");
	});
});

describe("supervision", () => {
	test("async-fn plugin runs with a facade client; clean return completes", async () => {
		const server = mockHost();
		const d = mkdirs("fn-plugin");
		const out = path.join(d.dir, "out.txt");
		write(
			path.join(d.scriptsDir, "fn.ts"),
			`export default { name: "fn", main: async (pf) => {
				const host = await pf.request("GET", "/host");
				require("node:fs").writeFileSync(${JSON.stringify(out)}, host.hostname);
			}};`,
		);
		const logs: string[] = [];
		try {
			const fiber = Effect.runFork(
				runner({
					...d,
					connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
					log: (l) => logs.push(l),
				}),
			);
			await waitFor(() => fs.existsSync(out));
			expect(fs.readFileSync(out, "utf8")).toBe("mock");
			await waitFor(() => logs.some((l) => l.includes("[fn] plugin completed")));
			await Effect.runPromise(Fiber.interrupt(fiber));
		} finally {
			server.stop(true);
		}
	});

	test("a crashing plugin is restarted with backoff", async () => {
		const server = mockHost();
		const d = mkdirs("crashy");
		const counter = path.join(d.dir, "count.txt");
		write(
			path.join(d.scriptsDir, "crashy.ts"),
			`import * as fs from "node:fs";
			export default { name: "crashy", main: async () => {
				const n = fs.existsSync(${JSON.stringify(counter)}) ? Number(fs.readFileSync(${JSON.stringify(counter)}, "utf8")) : 0;
				fs.writeFileSync(${JSON.stringify(counter)}, String(n + 1));
				throw new Error("boom " + n);
			}};`,
		);
		const logs: string[] = [];
		try {
			const fiber = Effect.runFork(
				runner({
					...d,
					connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
					restartBase: "20 millis",
					log: (l) => logs.push(l),
				}),
			);
			await waitFor(() => {
				try {
					return Number(fs.readFileSync(counter, "utf8")) >= 3;
				} catch {
					return false;
				}
			});
			expect(logs.some((l) => l.includes("[crashy] failed: "))).toBe(true);
			expect(logs.some((l) => l.includes("restarting (attempt 2)"))).toBe(true);
			await Effect.runPromise(Fiber.interrupt(fiber));
		} finally {
			server.stop(true);
		}
	});

	test("a grants change restarts only the affected sandboxed plugin", async () => {
		const d = mkdirs("grant-restart");
		const packages = ["one", "two"];
		for (const id of packages) {
			const pkg = path.join(d.pluginsDir, "node_modules", `punktfunk-plugin-${id}`);
			write(
				path.join(pkg, "package.json"),
				JSON.stringify({
					name: `punktfunk-plugin-${id}`,
					main: "index.js",
					punktfunk: { schema: 1, id },
				}),
			);
			write(path.join(pkg, "index.js"), "export default {};");
		}
		write(
			path.join(d.pluginsDir, "package.json"),
			JSON.stringify({
				dependencies: Object.fromEntries(packages.map((id) => [`punktfunk-plugin-${id}`, "1.0.0"])),
			}),
		);
		const grants = (one: unknown[] = []) =>
			JSON.stringify({ one: { grants: one }, two: { grants: [] } });
		const grantsFile = path.join(d.dir, "plugin-run", "plugin-grants.json");
		fs.mkdirSync(path.dirname(grantsFile), { recursive: true });
		fs.writeFileSync(grantsFile, grants());

		const starts: Record<string, number> = { one: 0, two: 0 };
		const active: Record<string, number> = { one: 0, two: 0 };
		const logs: string[] = [];
		const fiber = Effect.runFork(
			runner({
				...d,
				sandbox: "on",
				configDir: d.dir,
				grantPollInterval: "20 millis",
				log: (line) => logs.push(line),
				sandboxRun: (_unit, manifest) => {
					const id = manifest.id as "one" | "two";
					return Effect.scoped(
						Effect.acquireRelease(
							Effect.sync(() => {
								starts[id] += 1;
								active[id] += 1;
							}),
							() => Effect.sync(() => void (active[id] -= 1)),
						).pipe(Effect.andThen(Effect.never)),
					).pipe(Effect.as("plugin" as const));
				},
			}),
		);
		try {
			await waitFor(() => starts.one === 1 && starts.two === 1);
			const changed = grants([{ path: "/mnt/one", write: false }]);
			fs.writeFileSync(grantsFile, changed);
			await waitFor(() => starts.one === 2);
			expect(starts.two).toBe(1);
			expect(active).toEqual({ one: 1, two: 1 });
			const restartLines = () =>
				logs.filter((line) => line.includes("[runner] one: folder access changed — restarting"));
			expect(restartLines()).toHaveLength(1);
			expect(logs.some((line) => line.includes("[one] restarting (attempt 2)"))).toBe(false);
			fs.writeFileSync(grantsFile, changed);
			await new Promise((resolve) => setTimeout(resolve, 100));
			expect(starts).toEqual({ one: 2, two: 1 });
			expect(restartLines()).toHaveLength(1);
		} finally {
			await Effect.runPromise(Fiber.interrupt(fiber));
		}
		expect(active).toEqual({ one: 0, two: 0 });
	});

	test("a bare script is one-shot: runs on import, never restarts", async () => {
		const d = mkdirs("bare");
		const counter = path.join(d.dir, "ran.txt");
		write(
			path.join(d.scriptsDir, "once.ts"),
			`import * as fs from "node:fs";
			const n = fs.existsSync(${JSON.stringify(counter)}) ? Number(fs.readFileSync(${JSON.stringify(counter)}, "utf8")) : 0;
			fs.writeFileSync(${JSON.stringify(counter)}, String(n + 1));`,
		);
		const logs: string[] = [];
		const fiber = Effect.runFork(
			runner({ ...d, restartBase: "20 millis", log: (l) => logs.push(l) }),
		);
		await waitFor(() => logs.some((l) => l.includes("[once] script completed")));
		await new Promise((r) => setTimeout(r, 200)); // would have restarted by now
		expect(fs.readFileSync(counter, "utf8")).toBe("1");
		await Effect.runPromise(Fiber.interrupt(fiber));
	});

	test("a plugin package without a manifest does not run while sandboxing is on", async () => {
		const d = mkdirs("no-manifest");
		const ran = path.join(d.dir, "ran.txt");
		const pkg = path.join(d.pluginsDir, "node_modules", "punktfunk-plugin-bare");
		write(
			path.join(pkg, "package.json"),
			JSON.stringify({ name: "punktfunk-plugin-bare", main: "index.js" }),
		);
		write(
			path.join(pkg, "index.js"),
			`import * as fs from "node:fs"; fs.writeFileSync(${JSON.stringify(ran)}, "1");`,
		);
		const logs: string[] = [];
		const fiber = Effect.runFork(runner({ ...d, sandbox: "on", log: (l) => logs.push(l) }));
		await waitFor(() => logs.some((l) => l.includes("not starting punktfunk-plugin-bare")));
		await new Promise((r) => setTimeout(r, 200)); // would have imported by now
		await Effect.runPromise(Fiber.interrupt(fiber));
		expect(logs.some((l) => l.includes("starting punktfunk-plugin-bare ("))).toBe(false);
		expect(fs.existsSync(ran)).toBe(false);
	});

	test("shutdown interrupts an Effect plugin STRUCTURALLY — its finalizer runs", async () => {
		const server = mockHost();
		const d = mkdirs("finalizer");
		const acquired = path.join(d.dir, "acquired.txt");
		const released = path.join(d.dir, "released.txt");
		write(
			path.join(d.scriptsDir, "holder.ts"),
			`import { Effect } from "effect";
			import * as fs from "node:fs";
			export default { name: "holder", main: Effect.scoped(Effect.gen(function* () {
				yield* Effect.acquireRelease(
					Effect.sync(() => fs.writeFileSync(${JSON.stringify(acquired)}, "1")),
					() => Effect.sync(() => fs.writeFileSync(${JSON.stringify(released)}, "1")),
				);
				yield* Effect.never; // hold the resource for the plugin's lifetime
			})) };`,
		);
		try {
			const fiber = Effect.runFork(
				runner({
					...d,
					connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
					log: () => {},
				}),
			);
			await waitFor(() => fs.existsSync(acquired));
			expect(fs.existsSync(released)).toBe(false);
			await Effect.runPromise(Fiber.interrupt(fiber)); // the SIGTERM path
			await waitFor(() => fs.existsSync(released));
		} finally {
			server.stop(true);
		}
	});

	test("supervised unit ends cleanly when its plugin completes (no spin)", async () => {
		const server = mockHost();
		const d = mkdirs("done");
		write(
			path.join(d.scriptsDir, "done.ts"),
			`export default { name: "done", main: async () => {} };`,
		);
		const logs: string[] = [];
		try {
			await Effect.runPromise(
				superviseUnit(
					{ name: "done", file: path.join(d.scriptsDir, "done.ts") },
					{
						connect: { url: `http://127.0.0.1:${server.port}`, token: TOKEN },
						log: (l) => logs.push(l),
						restartBase: "10 millis",
					},
				),
			);
			expect(logs.filter((l) => l.includes("restarting")).length).toBe(0);
		} finally {
			server.stop(true);
		}
	});
});
