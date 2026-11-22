// What a macOS plugin can reach is decided by its Seatbelt profile. The profile is pinned as text
// on any OS; on a Mac a probe plugin also runs under `sandbox-exec` through the real runner bundle,
// and each thing it reaches has a negative control beside it.
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { Effect } from "effect";
import * as fs from "node:fs";
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { runSeatbelt } from "../src/runner-macos.js";
import {
	type PluginManifest,
	SANDBOX_EXEC,
	SEATBELT_BASE,
	type SeatbeltPaths,
	sandboxProbe,
	seatbeltEnv,
	seatbeltProfile,
} from "../src/sandbox.js";

const paths: SeatbeltPaths = {
	stateDir: "/Users/u/.config/punktfunk/plugin-state/demo",
	view: "/pf-view/p-ab12cd34",
	pluginsDir: "/Users/u/.config/punktfunk/plugins",
	bun: "/Applications/Punktfunk Host.app/Contents/Helpers/bun",
	runner: "/Applications/Punktfunk Host.app/Contents/Resources/scripting/runner-cli.js",
	home: "/Users/u",
	configDir: "/Users/u/.config/punktfunk",
	ui: false,
};

const manifest = (over: Partial<PluginManifest> = {}): PluginManifest => ({
	schema: 1,
	id: "demo",
	reads: ["~/Library/Application Support/Steam"],
	...over,
});

/** The filters of every rule that allows exactly `ops`. */
const allowed = (profile: string, ops: string): string[] =>
	profile
		.split("\n")
		.filter((l) => l.startsWith(`(allow ${ops} (`))
		.flatMap((l) => [...l.matchAll(/\((?:subpath|literal) "([^"]*)"\)/g)].map((m) => m[1] as string));

describe("seatbeltProfile", () => {
	test("allows nothing without a filter but what the base allows every process", () => {
		const profile = seatbeltProfile(manifest({ network: true }), { ...paths, ui: true }, [], [8080]);
		const bare = profile.split("\n").filter((l) => /^\(allow [\w*-]+(?: [\w*-]+)*\)$/.test(l));
		expect(bare).toEqual(["(allow process-fork)", "(allow sysctl-read)"]);
		expect(profile.startsWith(SEATBELT_BASE)).toBe(true);
	});

	test("reads its code and what it declared, writes only its state and its temp dir", () => {
		const profile = seatbeltProfile(manifest(), paths);
		const read = allowed(profile, "file-read* process-exec");
		expect(read).toEqual(
			expect.arrayContaining([
				paths.pluginsDir,
				paths.bun,
				paths.runner,
				paths.view,
				"/Users/u/Library/Application Support/Steam",
			]),
		);
		expect(allowed(profile, "file-read* file-write* process-exec")).toEqual([
			paths.stateDir,
			`${paths.view}/tmp`,
		]);
		// No rule names the home, the config dir or anything above them.
		const every = [...profile.matchAll(/"([^"]*)"/g)].map((m) => m[1]);
		for (const p of ["/Users/u", "/Users/u/.config", paths.configDir, "/Users/u/Library"]) {
			expect(allowed(profile, "file-read* process-exec")).not.toContain(p);
			expect(allowed(profile, "file-read* file-write* process-exec")).not.toContain(p);
		}
		expect(every).not.toContain(`${paths.configDir}/mgmt-token`);
	});

	test("never shares keys, keychains, browser profiles, privacy grants or the config dir", () => {
		const reads = [
			"~/.ssh",
			"~/.config/punktfunk/plugin-run",
			"~/Library",
			"~/Library/Application Support",
			"~/Library/Keychains",
			"~/Library/Cookies",
			"~/Library/Containers/com.example.app",
			"~/Library/Application Support/com.apple.TCC",
			"~/Library/Application Support/Google/Chrome/Default",
			"/tmp",
			"/tmp/com.apple.launchd.abc/Listeners",
			"/dev",
		];
		const profile = seatbeltProfile(manifest({ reads }), paths, [
			{ path: "/Users/u/Library/Safari", write: false },
			{ path: "/Users/u", write: true },
		]);
		const named = [...allowed(profile, "file-read* process-exec"), ...allowed(profile, "file-read* file-write* process-exec")];
		for (const p of reads) expect(named).not.toContain(p.replace("~", "/Users/u"));
		expect(named).not.toContain("/Users/u/Library/Safari");
		expect(named).not.toContain("/Users/u");
	});

	test("a write binds through the owner's grant, except under /tmp, and never where the session starts", () => {
		const profile = seatbeltProfile(
			manifest({ writes: ["~/Emu", "/tmp/vhclient"] }),
			paths,
			[
				{ path: "/Volumes/Games", write: true },
				{ path: "/Users/u/Library/LaunchAgents", write: true },
				{ path: "/Users/u/Library/Preferences", write: true },
				{ path: "/Volumes/Roms", write: false },
			],
		);
		const writable = allowed(profile, "file-read* file-write* process-exec");
		expect(writable).toContain("/tmp/vhclient");
		expect(writable).toContain("/Volumes/Games");
		expect(writable).not.toContain("/Users/u/Emu");
		expect(writable).not.toContain("/Users/u/Library/LaunchAgents");
		expect(writable).not.toContain("/Users/u/Library/Preferences");
		expect(allowed(profile, "file-read* process-exec")).toContain("/Volumes/Roms");
		expect(writable).not.toContain("/Volumes/Roms");
	});

	test("leaves out a path that could end its string, rather than escape it", () => {
		const evil = '/Volumes/x") (allow default) ("';
		const profile = seatbeltProfile(manifest({ reads: [evil, "/Volumes/back\\slash"] }), paths, [
			{ path: "/Volumes/new\nline", write: false },
		]);
		expect(profile).not.toContain("(allow default)");
		expect(profile).not.toContain("back\\slash");
		expect(profile).not.toContain("new\nline");
	});

	test("its only socket is the host's until it declared network", () => {
		const plain = seatbeltProfile(manifest(), paths, [], [8080]);
		expect(plain).not.toContain("remote");
		expect(plain).not.toContain("mach-lookup (global-name");
		expect(plain).not.toContain("AF_INET");
		expect(plain.split("\n").filter((l) => l.startsWith("(allow network-outbound"))).toEqual([
			`(allow network-outbound (literal "${paths.view}/host.sock"))`,
		]);
		const net = seatbeltProfile(manifest({ network: true }), paths, [], [8080, 0, 1.5]).split("\n");
		const at = (rule: string) => net.indexOf(rule);
		// The last matching rule wins: loopback is refused after the internet, and only granted
		// ports come after that.
		expect(at('(allow network-outbound (remote ip "*:*"))')).toBeGreaterThan(0);
		expect(at('(deny network-outbound (remote ip "localhost:*"))')).toBeGreaterThan(
			at('(allow network-outbound (remote ip "*:*"))'),
		);
		expect(at('(allow network-outbound (remote tcp "localhost:8080"))')).toBeGreaterThan(
			at('(deny network-outbound (remote ip "localhost:*"))'),
		);
		expect(net.join("\n")).not.toContain("localhost:0");
		expect(net.join("\n")).not.toContain("localhost:1.5");
	});

	test("binds its page's socket only when the runner forwards one", () => {
		expect(seatbeltProfile(manifest(), paths)).not.toContain("network-bind");
		const ui = seatbeltProfile(manifest(), { ...paths, ui: true });
		expect(ui).toContain(`(allow network-bind (local unix-socket (path-literal "${paths.view}/ui.sock")))`);
		expect(ui).toContain(`(allow file-write-create file-write-unlink (literal "${paths.view}/ui.sock"))`);
	});

	test("a link the plugin asked through is readable as a link, its target through the grant", () => {
		const profile = seatbeltProfile(manifest(), paths, [
			{
				path: "/Volumes/SSD/games",
				write: false,
				links: [
					{ path: "/Users/u/Games/SteamLibrary", target: "/Volumes/SSD/games/lib" },
					{ path: "/Users/u/x/../.ssh", target: "/Volumes/SSD/games" },
				],
			},
		]);
		expect(allowed(profile, "file-read-metadata")).toContain("/Users/u/Games/SteamLibrary");
		expect(profile).not.toContain(".ssh");
	});

	test("the environment points the SDK at the view", () => {
		const env = seatbeltEnv("/Users/u", paths.view, 41234);
		expect(env.PUNKTFUNK_CONFIG_DIR).toBe(paths.view);
		expect(env.PUNKTFUNK_MGMT_UNIX).toBe(`${paths.view}/host.sock`);
		expect(env.TMPDIR).toBe(`${paths.view}/tmp`);
		expect(env.PUNKTFUNK_UI_SOCKET).toBe(`${paths.view}/ui.sock`);
		expect(env.PUNKTFUNK_MGMT_TOKEN).toBeUndefined();
		expect(seatbeltEnv("/Users/u", paths.view).PUNKTFUNK_UI_PORT).toBeUndefined();
	});
});

describe("sandboxProbe on macOS", () => {
	test("probes the base profile with sandbox-exec, and says why it can't", () => {
		let asked: [string, string[]] = ["", []];
		expect(
			sandboxProbe((cmd, args) => {
				asked = [cmd, args];
				return { status: 0 };
			}, "darwin"),
		).toEqual({ ok: true });
		expect(asked).toEqual([SANDBOX_EXEC, ["-p", SEATBELT_BASE, "/usr/bin/true"]]);
		const missing = sandboxProbe(() => ({ status: null }), "darwin");
		expect(!missing.ok && missing.reason).toContain("PUNKTFUNK_PLUGIN_SANDBOX=off");
		const refused = sandboxProbe(() => ({ status: 65, stderr: "sandbox-exec: syntax error\n" }), "darwin");
		expect(!refused.ok && refused.reason).toBe("sandbox-exec: syntax error");
	});

	test.skipIf(process.platform !== "darwin")("calls this Mac capable", () => {
		expect(sandboxProbe()).toEqual({ ok: true });
	});
});

// ---- real runs ---------------------------------------------------------------------------------

/** Each probe reports `key=value` into its state dir; the keys are what the tests assert on. */
const PROBE = `import fs from "node:fs";
import net from "node:net";
import dns from "node:dns/promises";
import { spawnSync } from "node:child_process";
const P = __PATHS__;
const o = {};
const t = async (k, f) => { try { o[k] = String(await f()); } catch (e) { o[k] = "ERR:" + (e.code ?? e.message); } };
const tcp = (port) => new Promise((res) => {
	const s = net.connect({ host: "127.0.0.1", port });
	s.on("connect", () => { res("open"); s.destroy(); });
	s.on("error", (e) => res("ERR:" + e.code));
});
const token = fs.readFileSync(process.env.PUNKTFUNK_CONFIG_DIR + "/plugin-token", "utf8").trim().split("=")[1];
await t("host", async () => (await fetch("http://punktfunk.host/api/v1/host", { unix: process.env.PUNKTFUNK_MGMT_UNIX, headers: { authorization: "Bearer " + token } })).text());
await t("other_socket", async () => (await fetch("http://x/", { unix: P.otherSocket })).text());
await t("declared", () => fs.readFileSync(P.declared + "/marker", "utf8").trim());
await t("declared_write", () => { fs.writeFileSync(P.declared + "/w", "x"); return "written"; });
await t("sibling", () => fs.readFileSync(P.sibling + "/marker", "utf8").trim());
await t("documents", () => fs.readdirSync(P.documents).length);
await t("mgmt_token", () => fs.readFileSync(P.mgmtToken, "utf8").trim());
await t("granted", () => fs.readFileSync(P.granted + "/marker", "utf8").trim());
await t("granted_write", () => { fs.writeFileSync(P.granted + "/w", "x"); return "written"; });
await t("granted_rw", () => { fs.writeFileSync(P.grantedRw + "/w", "x"); return "written"; });
await t("outside_write", () => { fs.writeFileSync(P.root + "/escaped", "x"); return "written"; });
await t("state", () => { fs.writeFileSync(process.env.PUNKTFUNK_CONFIG_DIR + "/plugin-state/" + P.id + "/w", "x"); return "written"; });
await t("tmpdir", () => { fs.writeFileSync(process.env.TMPDIR + "/t", "x"); return "written"; });
await t("tcp_granted", () => tcp(P.granted_port));
await t("tcp_other", () => tcp(P.other_port));
await t("dns", async () => (await dns.lookup("localhost", { family: 4 })).address);
await t("signal", () => { process.kill(P.sleeper, 0); return "reached"; });
await t("setuid", () => { const r = spawnSync("/bin/ps", ["-A"]); return r.error ? "ERR:" + r.error.code : "ran:" + r.status; });
await t("shell", () => spawnSync("/bin/sh", ["-c", "echo ok"]).stdout.toString().trim());
fs.writeFileSync(process.env.PUNKTFUNK_CONFIG_DIR + "/plugin-state/" + P.id + "/probe.json", JSON.stringify(o));
`;

describe.skipIf(process.platform !== "darwin")("a plugin under sandbox-exec", () => {
	const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "pf-seatbelt-")));
	const config = path.join(root, "cfg");
	const pluginsDir = path.join(config, "plugins");
	const at = (rel: string) => path.join(root, rel);
	const servers: { stop(force?: boolean): void }[] = [];
	const listeners: net.Server[] = [];
	let sleeper: ReturnType<typeof Bun.spawn> | undefined;
	const entry = process.env.PUNKTFUNK_RUNNER_ENTRY;
	const ports = { granted: 0, other: 0 };

	const listen = async (): Promise<number> => {
		const server = net.createServer((s) => s.end());
		listeners.push(server);
		await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
		return (server.address() as net.AddressInfo).port;
	};

	beforeAll(async () => {
		for (const d of ["steamlike", "sibling", "Documents", "granted", "granted-rw"]) {
			fs.mkdirSync(at(d), { recursive: true });
			fs.writeFileSync(at(`${d}/marker`), `${d}-data`);
		}
		fs.mkdirSync(path.join(config, "plugin-run"), { recursive: true });
		fs.writeFileSync(path.join(config, "mgmt-token"), "secret-admin-token");
		// The shipped runner is one bundle the sandbox reads; test what ships.
		const built = await Bun.build({
			entrypoints: [path.join(import.meta.dir, "..", "src", "runner-cli.ts")],
			target: "bun",
			outdir: at("scripting"),
		});
		if (!built.success) throw new Error(String(built.logs));
		process.env.PUNKTFUNK_RUNNER_ENTRY = at("scripting/runner-cli.js");
		ports.granted = await listen();
		ports.other = await listen();
		fs.rmSync(at("other.sock"), { force: true });
		servers.push(Bun.serve({ unix: at("other.sock"), fetch: () => new Response("other-socket") }));
		sleeper = Bun.spawn(["/bin/sleep", "60"]);
	});

	afterAll(() => {
		for (const s of servers) s.stop(true);
		for (const l of listeners) l.close();
		sleeper?.kill();
		if (entry === undefined) delete process.env.PUNKTFUNK_RUNNER_ENTRY;
		else process.env.PUNKTFUNK_RUNNER_ENTRY = entry;
		fs.rmSync(root, { recursive: true, force: true });
	});

	/** Install probe plugin `id`, run it once through `runSeatbelt`, and read what it reported. */
	const probe = async (id: string, network: boolean): Promise<Record<string, string>> => {
		const pkg = path.join(pluginsDir, "node_modules", `punktfunk-plugin-${id}`);
		fs.mkdirSync(pkg, { recursive: true });
		const m: PluginManifest = { schema: 1, id, reads: [at("steamlike")], network };
		fs.writeFileSync(
			path.join(pkg, "package.json"),
			JSON.stringify({ name: `punktfunk-plugin-${id}`, main: "index.js", punktfunk: m }),
		);
		const probePaths = {
			id,
			root,
			declared: at("steamlike"),
			sibling: at("sibling"),
			documents: at("Documents"),
			granted: at("granted"),
			grantedRw: at("granted-rw"),
			mgmtToken: path.join(config, "mgmt-token"),
			otherSocket: at("other.sock"),
			granted_port: ports.granted,
			other_port: ports.other,
			sleeper: sleeper?.pid,
		};
		fs.writeFileSync(path.join(pkg, "index.js"), PROBE.replace("__PATHS__", JSON.stringify(probePaths)));
		const tokens = path.join(config, "plugin-run", "plugin-tokens.json");
		const known = fs.existsSync(tokens) ? JSON.parse(fs.readFileSync(tokens, "utf8")) : {};
		fs.writeFileSync(tokens, JSON.stringify({ ...known, [id]: `token-${id}` }));
		const grants = path.join(config, "plugin-run", "plugin-grants.json");
		const granted = fs.existsSync(grants) ? JSON.parse(fs.readFileSync(grants, "utf8")) : {};
		fs.writeFileSync(
			grants,
			JSON.stringify({
				...granted,
				[id]: {
					grants: [
						{ path: at("granted"), write: false },
						{ path: at("granted-rw"), write: true },
					],
					ports: [ports.granted],
				},
			}),
		);
		const host = Bun.serve({
			hostname: "127.0.0.1",
			port: 0,
			fetch: (req) => new Response(`host:${req.headers.get("authorization")}`),
		});
		try {
			const logs: string[] = [];
			await Effect.runPromise(
				runSeatbelt(
					{ name: `punktfunk-plugin-${id}`, file: path.join(pkg, "index.js") },
					m,
					{
						configDir: config,
						pluginsDir,
						connect: { url: `http://127.0.0.1:${host.port}` },
						sandboxFetch: fetch,
					},
					(line) => logs.push(line),
				),
			);
		} finally {
			host.stop(true);
		}
		return JSON.parse(fs.readFileSync(path.join(config, "plugin-state", id, "probe.json"), "utf8"));
	};

	test("reaches its host, its declared and granted folders and its own state, and nothing else", async () => {
		const got = await probe("probe", false);
		// The channel works with its own token; another socket next to it does not.
		expect(got.host).toBe("host:Bearer token-probe");
		expect(got.other_socket).toStartWith("ERR:");
		expect(await (await fetch("http://x/", { unix: at("other.sock") })).text()).toBe("other-socket");
		// Declared: readable, not writable. A sibling and a home folder it never named: unreadable.
		expect(got.declared).toBe("steamlike-data");
		expect(got.declared_write).toBe("ERR:EPERM");
		expect(got.sibling).toBe("ERR:EPERM");
		expect(got.documents).toBe("ERR:EPERM");
		expect(fs.readFileSync(at("sibling/marker"), "utf8")).toBe("sibling-data");
		expect(fs.readdirSync(at("Documents"))).toEqual(["marker"]);
		// The host's credentials stay out.
		expect(got.mgmt_token).toBe("ERR:EPERM");
		expect(fs.readFileSync(path.join(config, "mgmt-token"), "utf8")).toBe("secret-admin-token");
		// Grants: read-only unless marked write.
		expect(got.granted).toBe("granted-data");
		expect(got.granted_write).toBe("ERR:EPERM");
		expect(got.granted_rw).toBe("written");
		// Writes land in its state and its temp dir only.
		expect(got.outside_write).toBe("ERR:EPERM");
		expect(fs.existsSync(at("escaped"))).toBe(false);
		expect(got.state).toBe("written");
		expect(fs.readFileSync(path.join(config, "plugin-state", "probe", "w"), "utf8")).toBe("x");
		expect(got.tmpdir).toBe("written");
	});

	test("opens no connection and resolves no name without network, signals nothing it did not start", async () => {
		const got = await probe("quiet", false);
		expect(got.tcp_granted).toStartWith("ERR:");
		expect(got.tcp_other).toStartWith("ERR:");
		expect(got.dns).toStartWith("ERR:");
		// Outside, both ports answer.
		for (const port of [ports.granted, ports.other]) {
			await new Promise<void>((done, fail) =>
				net.connect({ host: "127.0.0.1", port }).on("connect", done).on("error", fail),
			);
		}
		expect(got.signal).toBe("ERR:EPERM");
		expect(() => process.kill(sleeper?.pid as number, 0)).not.toThrow();
		// A setuid program does not run; a plain one does.
		expect(got.setuid).toBe("ERR:EPERM");
		expect(got.shell).toBe("ok");
	});

	test("with network, reaches a granted port on this computer and no other", async () => {
		const got = await probe("netprobe", true);
		expect(got.tcp_granted).toBe("open");
		expect(got.tcp_other).toStartWith("ERR:");
		expect(got.dns).toBe("127.0.0.1");
		expect(got.host).toBe("host:Bearer token-netprobe");
		expect(got.mgmt_token).toBe("ERR:EPERM");
	});

	test("leaves its state behind and its view gone", async () => {
		const runtime = path.join(process.env.XDG_RUNTIME_DIR ?? os.tmpdir(), "punktfunk");
		const before = fs.existsSync(runtime) ? fs.readdirSync(runtime) : [];
		await probe("tidy", false);
		const after = fs.existsSync(runtime) ? fs.readdirSync(runtime) : [];
		expect(after.filter((d) => !before.includes(d))).toEqual([]);
		expect(fs.existsSync(path.join(config, "plugin-state", "tidy", "probe.json"))).toBe(true);
	});
});
